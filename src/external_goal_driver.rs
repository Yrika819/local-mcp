//! E3-B — host-only bounded advancement for EXTERNAL Goals.
//!
//! The driver decides the *next* host step for an External Goal and, when that
//! step needs a model, builds and durably issues one bounded [`ModelWorkItem`].
//! It never performs inference, never launches a process, and never applies a
//! model response: applying a response is a later host phase (E4/E5).
//!
//! Two properties are load-bearing:
//!
//! * **Bounded.** One call performs at most one host step. The API enforces the
//!   caller's `max_host_steps` before calling in, and the driver itself never
//!   loops, so a single request cannot hold the session lock while retrying.
//! * **Honest.** A Goal state whose next step needs a role that V1 does not wire
//!   is reported as `EXTERNAL_ROLE_NOT_WIRED`, never as progress and never as
//!   success.
//!
//! A WorkItem is reported as issued only after `commit_issued_work_item`
//! returned `Ok`. If the commit fails, the caller sees an error and no WorkItem
//! exists, so an uncommitted request can never reach a client (invariant 4).

use crate::config;
use crate::external_model_store::{
    commit_cancel_active_work, commit_issued_work_item, external_state,
};
use crate::external_model_work::{
    IssueBlock, MAX_REQUEST_PAYLOAD_BYTES, ModelWorkItem, OUTPUT_CONTRACT_PLANNER_V1, WorkId,
    WorkItemState, WorkRole, sha256_hex,
};
use crate::goal::{Goal, GoalStatus};
use crate::orchestrator_error::OrchestratorError;
use crate::planner;
use crate::task_store::{TaskStore, utc_now_rfc3339};

/// The single host step the driver took (or reused).
///
/// The `ModelRequired` payload is a full durable WorkItem, which is much larger
/// than the other variants. That asymmetry is accepted deliberately: the driver
/// result is short-lived, and boxing would hide the fact that handing back a
/// WorkItem is the expensive branch.
#[expect(
    clippy::large_enum_variant,
    reason = "The driver result is a short-lived local value; the large variant is the point."
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DriverOutcome {
    /// A bound external model request is available. Either it already existed
    /// (same `work_id`, same `request_digest`) or it was just issued and
    /// committed.
    ModelRequired(ModelWorkItem),
    /// Host-only progress was made and the caller can continue immediately.
    /// Carries no model request, so it can never be mistaken for one.
    Progress {
        code: &'static str,
        detail: &'static str,
    },
    /// No model request can be produced right now, with a typed reason.
    Refused(DriverRefusal),
}

/// Why the host produced no model request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DriverRefusal {
    /// The Goal's next host step belongs to a role the External loop does not
    /// wire in V1. Reported, never executed and never claimed as success.
    RoleNotWired { stage: &'static str },
    /// A host budget refused the new WorkItem. Budget is not consumed.
    Budget(IssueBlock),
    /// A response was already accepted and now waits for the host application
    /// phase, which V1 does not implement.
    AwaitingHostApplication { work_id: WorkId, role: WorkRole },
}

/// A driver result plus how many host steps it consumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DriverReport {
    pub(crate) outcome: DriverOutcome,
    pub(crate) host_steps_executed: u32,
}

impl DriverReport {
    fn reused(outcome: DriverOutcome) -> Self {
        Self {
            outcome,
            host_steps_executed: 0,
        }
    }

    fn advanced(outcome: DriverOutcome) -> Self {
        Self {
            outcome,
            host_steps_executed: 1,
        }
    }
}

/// A host-side failure while deciding the next step. Never carries model text.
///
/// `OrchestratorError` is not `Clone`, so this stays a single-use diagnostic.
#[derive(Debug)]
pub(crate) enum DriverError {
    /// The durable store refused the read or the commit.
    Persistence(OrchestratorError),
    /// The host cannot build a bounded, authority-valid request for this Goal.
    RequestUnavailable {
        code: &'static str,
        detail: &'static str,
    },
}

impl DriverError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Persistence(OrchestratorError::RevisionConflict { .. }) => "REVISION_CONFLICT",
            Self::Persistence(OrchestratorError::GoalNotFound) => "GOAL_NOT_FOUND",
            Self::Persistence(OrchestratorError::CorruptGoal(_)) => "GOAL_STATE_CORRUPT",
            Self::Persistence(OrchestratorError::UnsupportedSchema(_)) => "UNSUPPORTED_GOAL_SCHEMA",
            Self::Persistence(_) => "GOAL_PERSISTENCE_ERROR",
            Self::RequestUnavailable { code, .. } => code,
        }
    }

    /// Fixed, redacted diagnostic. Deliberately never interpolates a host path,
    /// a Goal objective, or model content.
    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::Persistence(OrchestratorError::GoalNotFound) => {
                "goal was not found for this session"
            }
            Self::Persistence(OrchestratorError::RevisionConflict { .. }) => {
                "the goal changed while the host was deciding the next external step"
            }
            Self::Persistence(_) => "durable goal persistence failed",
            Self::RequestUnavailable { detail, .. } => detail,
        }
    }
}

/// Which role this Goal state needs next, if the host can name one.
///
/// Only initial planning is wired in V1. Every other state reports
/// `RoleNotWired` rather than guessing a role or marking progress.
pub(crate) fn required_role(goal: &Goal) -> Option<WorkRole> {
    if goal.status() == GoalStatus::Planning && goal.plan_revision() == 0 && goal.tasks().is_empty()
    {
        Some(WorkRole::Planner)
    } else {
        None
    }
}

/// Decide and, when a model is needed, durably issue the next bounded WorkItem.
pub(crate) fn next_external_step(
    store: &TaskStore,
    session: &config::Session,
    goal: &Goal,
) -> Result<DriverReport, DriverError> {
    let state = external_state(goal).map_err(DriverError::Persistence)?;
    if let Some(active) = state.active_work() {
        match active.state() {
            WorkItemState::Pending => {
                if active.goal_revision() == goal.revision()
                    && active.plan_revision() == goal.plan_revision()
                {
                    return Ok(DriverReport::reused(DriverOutcome::ModelRequired(
                        active.clone(),
                    )));
                }
                // A host transition advanced the Goal revision while a request
                // was outstanding. The request can no longer be submitted, so it
                // is invalidated durably in this step and re-issued on the next.
                commit_cancel_active_work(
                    store,
                    &session.id,
                    goal.id(),
                    goal.revision(),
                    "WORK_ITEM_REVISION_INVALIDATED",
                )
                .map_err(DriverError::Persistence)?;
                return Ok(DriverReport::advanced(DriverOutcome::Progress {
                    code: "WORK_ITEM_REVISION_INVALIDATED",
                    detail: "the active work item was bound to a superseded goal or plan revision and was cancelled; call goal_next again",
                }));
            }
            WorkItemState::Submitted => {
                return Ok(DriverReport::reused(DriverOutcome::Refused(
                    DriverRefusal::AwaitingHostApplication {
                        work_id: active.work_id().clone(),
                        role: active.role(),
                    },
                )));
            }
            _ => {
                return Err(DriverError::Persistence(OrchestratorError::CorruptGoal(
                    "the active external work slot holds a terminal work item".to_owned(),
                )));
            }
        }
    }

    let Some(role) = required_role(goal) else {
        return Ok(DriverReport::reused(DriverOutcome::Refused(
            DriverRefusal::RoleNotWired {
                stage: goal_status_stage(goal.status()),
            },
        )));
    };

    if let Err(block) = state.check_issue_budget(role) {
        return Ok(DriverReport::reused(DriverOutcome::Refused(
            DriverRefusal::Budget(block),
        )));
    }

    // The issuance commit itself is the durable write that produces the next
    // Goal revision (`TaskStore::mutate_goal_snapshot` increments exactly once),
    // so the request must be bound to that revision, not to the pre-commit one.
    // Binding to the pre-commit revision would make every valid answer look
    // stale the instant the WorkItem became durable.
    let bound_revision = goal.revision().checked_add(1).ok_or_else(|| {
        DriverError::Persistence(OrchestratorError::CorruptGoal(
            "goal revision overflow".to_owned(),
        ))
    })?;

    let (payload, output_contract) = match role {
        WorkRole::Planner => build_planner_request(goal, session, bound_revision)?,
        other => {
            return Ok(DriverReport::reused(DriverOutcome::Refused(
                DriverRefusal::RoleNotWired {
                    stage: other.as_str(),
                },
            )));
        }
    };

    let issue_counter = state.issue_counter().checked_add(1).ok_or_else(|| {
        DriverError::Persistence(OrchestratorError::CorruptGoal(
            "external work item counter overflow".to_owned(),
        ))
    })?;
    let now = utc_now_rfc3339();
    let item = ModelWorkItem::pending(
        WorkId::new(),
        goal.session_id().to_owned(),
        goal.id().as_str().to_owned(),
        role,
        None,
        None,
        bound_revision,
        goal.plan_revision(),
        sha256_hex(payload.as_bytes()),
        payload,
        output_contract,
        issue_counter,
        &now,
    )
    .map_err(DriverError::Persistence)?;

    match commit_issued_work_item(store, &session.id, goal.id(), goal.revision(), item) {
        Ok(committed) => {
            if committed.revision() != bound_revision {
                return Err(DriverError::Persistence(OrchestratorError::CorruptGoal(
                    "the issuance commit produced an unexpected goal revision".to_owned(),
                )));
            }
            let state = external_state(&committed).map_err(DriverError::Persistence)?;
            let active = state.active_work().ok_or_else(|| {
                DriverError::Persistence(OrchestratorError::CorruptGoal(
                    "a committed issuance left no active work item".to_owned(),
                ))
            })?;
            Ok(DriverReport::advanced(DriverOutcome::ModelRequired(
                active.clone(),
            )))
        }
        Err(OrchestratorError::RevisionConflict { .. }) => {
            // Another caller may have issued the WorkItem first. Re-read once
            // and reuse its identity instead of racing a second issuance.
            let reloaded = store
                .load_goal(&session.id, goal.id())
                .map_err(DriverError::Persistence)?;
            let state = external_state(&reloaded).map_err(DriverError::Persistence)?;
            match state.active_work() {
                Some(active) => Ok(DriverReport::reused(DriverOutcome::ModelRequired(
                    active.clone(),
                ))),
                None => Err(DriverError::Persistence(
                    OrchestratorError::RevisionConflict {
                        expected: goal.revision(),
                        actual: reloaded.revision(),
                    },
                )),
            }
        }
        Err(other) => Err(DriverError::Persistence(other)),
    }
}

/// Build the host-authored Planner request payload, bounded before it is
/// persisted.
///
/// The request is derived from the existing `planner_request_for_goal` builder,
/// so the external loop reuses the same host authority, allowed enum values, and
/// completion-criterion IDs as the Codex path.
///
/// `bound_revision` is the Goal revision the WorkItem will be durably bound to
/// once the issuance commit lands. Only the `goal_revision` field is restated,
/// because every other request field is unaffected by the external-state write.
fn build_planner_request(
    goal: &Goal,
    session: &config::Session,
    bound_revision: u64,
) -> Result<(String, &'static str), DriverError> {
    let request = planner::planner_request_for_goal(goal, session).map_err(|error| {
        DriverError::RequestUnavailable {
            code: planner::planner_failure_code(&error),
            detail: "the host could not build a bounded external Planner request for this Goal",
        }
    })?;
    let mut request =
        serde_json::to_value(&request).map_err(|_| DriverError::RequestUnavailable {
            code: "EXTERNAL_REQUEST_SERIALIZATION_FAILED",
            detail: "the host could not serialize the external Planner request",
        })?;
    if let Some(object) = request.as_object_mut() {
        object.insert(
            "goal_revision".to_owned(),
            serde_json::Value::from(bound_revision),
        );
    }
    let payload = serde_json::to_string(&request).map_err(|_| DriverError::RequestUnavailable {
        code: "EXTERNAL_REQUEST_SERIALIZATION_FAILED",
        detail: "the host could not serialize the external Planner request",
    })?;
    if payload.len() > MAX_REQUEST_PAYLOAD_BYTES {
        return Err(DriverError::RequestUnavailable {
            code: "EXTERNAL_REQUEST_TOO_LARGE",
            detail: "the external Planner request exceeds the durable WorkItem bound",
        });
    }
    Ok((payload, OUTPUT_CONTRACT_PLANNER_V1))
}

fn goal_status_stage(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Planning => "PLANNING",
        GoalStatus::Running => "RUNNING",
        GoalStatus::Replanning => "REPLANNING",
        GoalStatus::Pausing => "PAUSING",
        GoalStatus::Paused => "PAUSED",
        GoalStatus::Blocked => "BLOCKED",
        GoalStatus::Verifying => "VERIFYING",
        GoalStatus::Cancelling => "CANCELLING",
        GoalStatus::Completed => "COMPLETED",
        GoalStatus::Failed => "FAILED",
        GoalStatus::Cancelled => "CANCELLED",
    }
}
