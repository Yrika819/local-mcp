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
use crate::external_writer::{self, WriterIssueOutcome};
use crate::goal::Goal;
use crate::goal_finalizer::{self, GoalFinalizationOutcome};
use crate::goal_verifier;
use crate::orchestrator_error::OrchestratorError;
use crate::planner;
use crate::readiness;
use crate::readonly_worker::{self, ReadonlyIssueOutcome};
use crate::scheduler::{self, SchedulerDecision, SchedulerNoActionReason};
use crate::task::{TaskId, TaskStatus, WorkerKind};
use crate::task_store::{TaskStore, utc_now_rfc3339};
use crate::verifier;

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
    /// A WorkItem accepted by a pre-E4 host is still active. E3 recorded the
    /// verdict without the model artifact, so the host can neither apply it nor
    /// honestly claim it. Reported as a mandatory blocker and never guessed at.
    UnrecoverableSubmitted { work_id: WorkId },
    /// A host-owned step was attempted and refused by its own validator
    /// (Task Verifier, Goal Verifier, finalizer). Reported, never retried
    /// blindly and never reported as success.
    HostStepRefused {
        code: &'static str,
        detail: &'static str,
    },
    /// The Scheduler selected no host action for this durable state.
    NoAction { code: &'static str },
    /// A PENDING Task is waiting on an active predecessor that cannot complete
    /// without host intervention (BLOCKED / FAILED / NEEDS_REPLAN). Reported as
    /// a non-mandatory blocker so the caller sees the exact predecessor pair
    /// without the Goal being durably marked blocked.
    DependencyWaiting { code: &'static str, detail: String },
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

/// Advance one EXTERNAL Goal by at most one host step.
///
/// The step is chosen by the existing Scheduler (`select_scheduler_action`) from
/// durable state only, so the external loop cannot invent an order the Codex
/// path would not use. Four kinds of step exist:
///
/// * a **model step** — issue one bounded `ModelWorkItem` (Planner, Readonly),
/// * a **host verification step** — the existing Task Verifier, Goal Verifier,
///   or Goal Finalizer,
/// * a **host policy step** — re-arm one RETRYABLE readonly Task under the
///   existing retry gate,
/// * a **refusal** — a role V1 does not wire, a budget, an unrecoverable
///   pre-E4 acceptance, or a validator refusal.
///
/// The function performs exactly one step. The API layer applies the caller's
/// `max_host_steps` budget, so a single call can never loop unbounded, and no
/// Session lock is held across model work (the client owns inference).
pub(crate) async fn advance_external_step(
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
                // A Writer WorkItem is *designed* to rest in SUBMITTED after its
                // proposal was validated and staged: publication is a separate
                // host step. Every other role in SUBMITTED state is a pre-E4
                // acceptance that has no durable artifact and can never be
                // applied.
                if active.role() == WorkRole::Writer
                    && let Some(task_id) =
                        active.task_id().and_then(|value| TaskId::parse(value).ok())
                {
                    let Some(operation_id) =
                        external_writer::pending_prepared_intent(goal, &task_id)
                    else {
                        return Ok(DriverReport::reused(DriverOutcome::Refused(
                            DriverRefusal::HostStepRefused {
                                code: "WRITER_INTENT_NOT_PREPARED",
                                detail: "the accepted writer submission has no prepared intent",
                            },
                        )));
                    };
                    if external_writer::verify_prepared_stage(
                        store,
                        session,
                        goal,
                        &task_id,
                        &operation_id,
                    )
                    .is_err()
                    {
                        return Ok(DriverReport::reused(DriverOutcome::Refused(
                            DriverRefusal::HostStepRefused {
                                code: "WRITER_STAGE_UNUSABLE",
                                detail: "the sealed writer stage cannot be verified against the durable intent",
                            },
                        )));
                    }
                    return Ok(DriverReport::reused(DriverOutcome::Refused(
                        DriverRefusal::HostStepRefused {
                            code: external_writer::WRITER_PUBLICATION_NOT_ENABLED,
                            detail: "the writer intent is durably prepared and verified; host publication is not enabled on this build",
                        },
                    )));
                }
                return Ok(DriverReport::reused(DriverOutcome::Refused(
                    DriverRefusal::UnrecoverableSubmitted {
                        work_id: active.work_id().clone(),
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

    // E5-0: a pending host-only readiness transition is resolved before any new
    // ModelWorkItem is issued, so the client is never asked to work on a graph
    // the host has not finished arming.
    match readiness::propagate_ready_tasks(store, session, goal) {
        Ok(readiness::ReadinessOutcome::Propagated { armed }) => {
            return Ok(DriverReport::advanced(DriverOutcome::Progress {
                code: readiness::READINESS_PROPAGATED,
                detail: if armed == 1 {
                    "the host armed one Task whose active predecessors are verified complete"
                } else {
                    "the host armed every eligible Task whose active predecessors are verified complete"
                },
            }));
        }
        Ok(readiness::ReadinessOutcome::NothingEligible) => {}
        Err(error) => return Err(DriverError::Persistence(error)),
    }
    let selection = scheduler::select_scheduler_action(goal).map_err(|_| {
        DriverError::Persistence(OrchestratorError::CorruptGoal(
            "the scheduler rejected the durable external Goal state".to_owned(),
        ))
    })?;
    let revision = goal.revision();
    match selection.decision().clone() {
        SchedulerDecision::PlanInitial => issue_planner_work_item(store, session, goal),
        SchedulerDecision::RunReadonly { task_id } => {
            issue_readonly_work_item(store, session, goal, &task_id)
        }
        SchedulerDecision::VerifyTask { task_id } => {
            match verifier::verify_task(store, session, goal.id(), &task_id, revision).await {
                Ok(_) => Ok(DriverReport::advanced(DriverOutcome::Progress {
                    code: "TASK_VERIFIED",
                    detail: "the host Task Verifier recorded one verification result",
                })),
                Err(error) => Ok(DriverReport::reused(DriverOutcome::Refused(
                    DriverRefusal::HostStepRefused {
                        code: verifier_failure_code(&error),
                        detail: "the host Task Verifier refused to verify this Task",
                    },
                ))),
            }
        }
        SchedulerDecision::VerifyGoal => {
            match goal_verifier::verify_goal(store, &session.id, goal.id(), revision) {
                Ok(result) => {
                    let code = match result.decision().outcome() {
                        crate::task::VerificationOutcome::Passed => "GOAL_VERIFICATION_PASSED",
                        _ => "GOAL_VERIFICATION_NOT_PASSED",
                    };
                    Ok(DriverReport::advanced(DriverOutcome::Progress {
                        code,
                        detail: "the host Goal Verifier evaluated the Goal final-verification contract",
                    }))
                }
                Err(error) => Ok(DriverReport::reused(DriverOutcome::Refused(
                    DriverRefusal::HostStepRefused {
                        code: goal_verifier_failure_code(&error),
                        detail: "the host Goal Verifier refused to evaluate this Goal",
                    },
                ))),
            }
        }
        SchedulerDecision::Replan { .. } => Ok(DriverReport::reused(DriverOutcome::Refused(
            DriverRefusal::RoleNotWired {
                stage: WorkRole::Replanner.as_str(),
            },
        ))),
        SchedulerDecision::RunWriter { task_id } => {
            issue_writer_work_item(store, session, goal, &task_id)
        }
        SchedulerDecision::RunReviewer { .. } => Ok(DriverReport::reused(DriverOutcome::Refused(
            DriverRefusal::RoleNotWired {
                stage: WorkRole::Reviewer.as_str(),
            },
        ))),
        SchedulerDecision::UnsupportedWorker { worker, .. } => Ok(DriverReport::reused(
            DriverOutcome::Refused(DriverRefusal::RoleNotWired {
                stage: worker_capability_stage(worker),
            }),
        )),
        SchedulerDecision::NoAction { reason } => {
            no_action_step(store, session, goal, reason).await
        }
    }
}

/// Handle one Scheduler "no action" selection.
///
/// Only two of these are host steps the external loop may take: the Goal
/// Finalizer (a Goal that already passed host Goal verification) and the
/// retry-policy step (re-arming a RETRYABLE readonly Task under the existing
/// gate). Everything else is reported, never invented.
async fn no_action_step(
    store: &TaskStore,
    session: &config::Session,
    goal: &Goal,
    reason: SchedulerNoActionReason,
) -> Result<DriverReport, DriverError> {
    match reason {
        SchedulerNoActionReason::GoalFinalizationRequired => {
            match goal_finalizer::finalize_goal_at_revision_with_session(
                store,
                session,
                goal.id(),
                goal.revision(),
            )
            .await
            {
                Ok(result) => match result.decision().outcome() {
                    GoalFinalizationOutcome::ReadyToComplete
                    | GoalFinalizationOutcome::AlreadyCompleted => {
                        Ok(DriverReport::advanced(DriverOutcome::Progress {
                            code: "GOAL_FINALIZED",
                            detail: "the host Goal Finalizer completed the Goal on host-owned evidence",
                        }))
                    }
                    GoalFinalizationOutcome::NotReady => Ok(DriverReport::reused(
                        DriverOutcome::Refused(DriverRefusal::NoAction {
                            code: "GOAL_FINALIZATION_NOT_READY",
                        }),
                    )),
                    GoalFinalizationOutcome::Blocked | GoalFinalizationOutcome::Failed => {
                        Ok(DriverReport::reused(DriverOutcome::Refused(
                            DriverRefusal::HostStepRefused {
                                code: "GOAL_FINALIZATION_BLOCKED",
                                detail: "the host Goal Finalizer refused to complete this Goal",
                            },
                        )))
                    }
                },
                Err(_) => Ok(DriverReport::reused(DriverOutcome::Refused(
                    DriverRefusal::HostStepRefused {
                        code: "GOAL_FINALIZATION_FAILED",
                        detail: "the host Goal Finalizer refused to complete this Goal",
                    },
                ))),
            }
        }
        SchedulerNoActionReason::RetryPolicyDeferred => {
            let Some(task_id) = retryable_readonly_task(goal) else {
                return Ok(DriverReport::reused(DriverOutcome::Refused(
                    DriverRefusal::NoAction {
                        code: "RETRY_POLICY_DEFERRED",
                    },
                )));
            };
            match readonly_worker::rearm_retryable_readonly_task(
                store,
                session,
                goal.id(),
                goal.revision(),
                &task_id,
            ) {
                Ok(_) => Ok(DriverReport::advanced(DriverOutcome::Progress {
                    code: "READONLY_RETRY_REARMED",
                    detail: "the host retry policy re-armed one RETRYABLE readonly Task",
                })),
                Err(_) => Ok(DriverReport::reused(DriverOutcome::Refused(
                    DriverRefusal::NoAction {
                        code: "RETRY_POLICY_DEFERRED",
                    },
                ))),
            }
        }
        other => {
            let code = no_action_code(&other);
            if matches!(
                other,
                SchedulerNoActionReason::WaitingForDependencies
                    | SchedulerNoActionReason::BlockedTasks
                    | SchedulerNoActionReason::FailedTasks
                    | SchedulerNoActionReason::ReadinessPropagationDeferred
            ) && let Some(detail) = readiness::waiting_detail(goal)
            {
                return Ok(DriverReport::reused(DriverOutcome::Refused(
                    DriverRefusal::DependencyWaiting { code, detail },
                )));
            }
            Ok(DriverReport::reused(DriverOutcome::Refused(
                DriverRefusal::NoAction { code },
            )))
        }
    }
}

/// Deterministic selection of the single RETRYABLE readonly Task to re-arm.
///
/// Ordered by `(created_plan_revision, TaskId)` so the choice never depends on
/// hash order or wall-clock races.
fn retryable_readonly_task(goal: &Goal) -> Option<TaskId> {
    goal.tasks()
        .iter()
        .filter(|(_, task)| {
            task.status() == TaskStatus::Retryable
                && task.worker() == WorkerKind::CodexReadonly
                && task.verification_retry_allowed()
                && task.unchanged_retry_permitted()
        })
        .min_by_key(|(task_id, task)| (task.created_plan_revision(), (*task_id).clone()))
        .map(|(task_id, _)| task_id.clone())
}

fn issue_planner_work_item(
    store: &TaskStore,
    session: &config::Session,
    goal: &Goal,
) -> Result<DriverReport, DriverError> {
    let state = external_state(goal).map_err(DriverError::Persistence)?;
    let role = WorkRole::Planner;
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
    let (payload, output_contract) = build_planner_request(goal, session, bound_revision)?;
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

/// Start the Writer Attempt and issue its WorkItem, atomically (E5-A W0).
fn issue_writer_work_item(
    store: &TaskStore,
    session: &config::Session,
    goal: &Goal,
    task_id: &TaskId,
) -> Result<DriverReport, DriverError> {
    match external_writer::issue_external_writer_work_item(
        store,
        session,
        goal.id(),
        task_id,
        goal.revision(),
    ) {
        Ok(WriterIssueOutcome::Issued { item }) => {
            Ok(DriverReport::advanced(DriverOutcome::ModelRequired(item)))
        }
        Ok(WriterIssueOutcome::Refused(block)) => Ok(DriverReport::reused(DriverOutcome::Refused(
            DriverRefusal::Budget(block),
        ))),
        Err(error) => Err(DriverError::Persistence(error)),
    }
}

/// Begin one readonly Attempt and issue its WorkItem, atomically (E4-B).
fn issue_readonly_work_item(
    store: &TaskStore,
    session: &config::Session,
    goal: &Goal,
    task_id: &TaskId,
) -> Result<DriverReport, DriverError> {
    match readonly_worker::issue_external_readonly_work_item(
        store,
        session,
        goal.id(),
        task_id,
        goal.revision(),
    ) {
        Ok(ReadonlyIssueOutcome::Issued { item }) => {
            Ok(DriverReport::advanced(DriverOutcome::ModelRequired(item)))
        }
        Ok(ReadonlyIssueOutcome::Refused(block)) => Ok(DriverReport::reused(
            DriverOutcome::Refused(DriverRefusal::Budget(block)),
        )),
        Err(error) => Err(DriverError::Persistence(error)),
    }
}

fn verifier_failure_code(error: &verifier::VerifierError) -> &'static str {
    match error {
        verifier::VerifierError::Store(OrchestratorError::RevisionConflict { .. }) => {
            "REVISION_CONFLICT"
        }
        verifier::VerifierError::Store(_) => "TASK_VERIFICATION_STORE_ERROR",
        verifier::VerifierError::InvalidState(_) => "TASK_VERIFICATION_STATE_REJECTED",
        verifier::VerifierError::Observation(_) => "TASK_VERIFICATION_OBSERVATION_FAILED",
    }
}

fn goal_verifier_failure_code(error: &goal_verifier::GoalVerifierError) -> &'static str {
    match error {
        goal_verifier::GoalVerifierError::Store(OrchestratorError::RevisionConflict { .. }) => {
            "REVISION_CONFLICT"
        }
        goal_verifier::GoalVerifierError::Store(_) => "GOAL_VERIFICATION_STORE_ERROR",
        goal_verifier::GoalVerifierError::NotApplicable(_) => "GOAL_VERIFICATION_NOT_APPLICABLE",
        goal_verifier::GoalVerifierError::InvalidDurableState(_) => {
            "GOAL_VERIFICATION_STATE_REJECTED"
        }
        goal_verifier::GoalVerifierError::StaleEvaluation(_) => "GOAL_VERIFICATION_STALE",
    }
}

fn worker_capability_stage(worker: WorkerKind) -> &'static str {
    match worker {
        WorkerKind::CodexReadonly => WorkRole::Readonly.as_str(),
        WorkerKind::CodexWriter => WorkRole::Writer.as_str(),
        WorkerKind::CodexReviewer => WorkRole::Reviewer.as_str(),
        WorkerKind::Verifier => "VERIFIER",
        WorkerKind::LocalOperation => "LOCAL_OPERATION",
    }
}

fn no_action_code(reason: &SchedulerNoActionReason) -> &'static str {
    match reason {
        SchedulerNoActionReason::Pausing => "PAUSING",
        SchedulerNoActionReason::Paused => "PAUSED",
        SchedulerNoActionReason::Cancelling => "CANCELLING",
        SchedulerNoActionReason::TerminalGoal(_) => "TERMINAL_GOAL",
        SchedulerNoActionReason::GoalBlocked => "GOAL_BLOCKED",
        SchedulerNoActionReason::GoalFinalizationRequired => "GOAL_FINALIZATION_REQUIRED",
        SchedulerNoActionReason::ReplanningWithoutTrigger => "REPLANNING_WITHOUT_TRIGGER",
        SchedulerNoActionReason::ActiveWork => "ACTIVE_WORK",
        SchedulerNoActionReason::WaitingForDependencies => "WAITING_FOR_DEPENDENCIES",
        SchedulerNoActionReason::ReadinessPropagationDeferred => "READINESS_PROPAGATION_DEFERRED",
        SchedulerNoActionReason::RetryPolicyDeferred => "RETRY_POLICY_DEFERRED",
        SchedulerNoActionReason::BlockedTasks => "BLOCKED_TASKS",
        SchedulerNoActionReason::FailedTasks => "FAILED_TASKS",
        SchedulerNoActionReason::WriterLeaseHeld { .. } => "WRITER_LEASE_HELD",
        SchedulerNoActionReason::NoEligibleAction => "NO_ELIGIBLE_ACTION",
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
