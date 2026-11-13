//! E5-C — external Replanner.
//!
//! A Replanner WorkItem is issued **only** for a durable trigger the host already
//! established (the Scheduler's `Replan { trigger_task_id }`, which requires a
//! `NEEDS_REPLAN` Task, an acknowledged failed-task request, or a pre-execution
//! plan rejection). Validation, materialization, receipt, and the WorkItem
//! transition are one `TaskStore` snapshot commit, exactly like the Planner:
//!
//! 1. re-bind session/Goal/WorkItem/revision against the persisted snapshot,
//! 2. validate with the Codex replanner's own validator
//!    (`parse_and_validate_proposal`: scope, criterion coverage, cycles, writer
//!    serialization, supersession pristineness, replacement closure, plan budgets),
//! 3. apply the validated replan (plan revision increment, new/replaced Tasks,
//!    criterion bindings),
//! 4. record the receipt with the revision this commit produces,
//! 5. move the WorkItem to `COMMITTED`.
//!
//! Replays are decided from the existing supersession/replacement records: the same
//! effective proposal is already applied (no new plan revision), a different one
//! under the same request id is a conflict. A new plan is never derived from model
//! narrative, and the model can never delete a required criterion, widen a scope,
//! or reach outside the replacement closure without the shared validator refusing.

use serde::Serialize;

use crate::config;
use crate::external_model_store::{external_state, external_state_mut};
use crate::external_model_work::{
    IssueBlock, MAX_REQUEST_PAYLOAD_BYTES, ModelWorkItem, OUTPUT_CONTRACT_REPLANNER_V1,
    SubmissionId, SubmissionReceipt, WorkId, WorkItemState, WorkRole, sha256_hex,
};
use crate::goal::{Goal, GoalId};
use crate::orchestrator_error::OrchestratorError;
use crate::planner;
use crate::replanner::{self, ReplanReplay};
use crate::task::TaskId;
use crate::task_store::TaskStore;

/// Progress code: a replan was applied and its WorkItem committed.
pub(crate) const REPLAN_APPLIED: &str = "REPLAN_APPLIED";
/// Progress code: the submitted replan was already applied (idempotent replay).
pub(crate) const REPLAN_ALREADY_APPLIED: &str = "REPLAN_ALREADY_APPLIED";

/// Host-authored request handed to the external Replanner client.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ExternalReplanWorkRequest {
    pub(crate) goal_id: String,
    pub(crate) task_id: String,
    pub(crate) goal_revision: u64,
    pub(crate) plan_revision: u32,
    pub(crate) output_contract: &'static str,
    /// The full bounded, host-generated replanner request (criterion ids, active
    /// graph, eligible triggers, budgets, allowed kinds).
    pub(crate) request: serde_json::Value,
}

#[expect(
    clippy::large_enum_variant,
    reason = "The issuance result is short-lived; handing back the durable WorkItem is the point."
)]
pub(crate) enum ReplanIssueOutcome {
    Issued {
        item: ModelWorkItem,
    },
    Refused(IssueBlock),
    /// The trigger disappeared between selection and issuance (another writer
    /// applied a plan first). Nothing was written.
    TriggerGone,
}

pub(crate) enum ReplanApplyOutcome {
    Applied {
        goal: Goal,
        receipt: SubmissionReceipt,
    },
    AlreadyApplied {
        goal: Goal,
        receipt: SubmissionReceipt,
    },
    Rejected {
        goal: Goal,
        receipt: SubmissionReceipt,
    },
    Refused {
        code: &'static str,
    },
}

/// Issue one Replanner WorkItem for a durable trigger (E5-C W0).
pub(crate) fn issue_external_replanner_work_item(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    trigger_task_id: &TaskId,
    expected_revision: u64,
) -> Result<ReplanIssueOutcome, OrchestratorError> {
    let precheck = store.load_goal(&session.id, goal_id)?;
    if replanner::replan_eligible_for_host(&precheck).is_err() {
        return Ok(ReplanIssueOutcome::TriggerGone);
    }
    let mut slot: Option<Result<ModelWorkItem, IssueBlock>> = None;
    store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
        if let Err(block) = external_state(goal)?.check_issue_budget(WorkRole::Replanner) {
            slot = Some(Err(block));
            return Ok(());
        }
        if replanner::replan_eligible_for_host(goal).is_err() {
            slot = Some(Err(IssueBlock::PendingWorkItemExists));
            return Ok(());
        }
        let bound_revision = goal
            .revision()
            .checked_add(1)
            .ok_or_else(|| OrchestratorError::CorruptGoal("goal revision overflow".to_owned()))?;
        let issue_counter = external_state(goal)?
            .issue_counter()
            .checked_add(1)
            .ok_or_else(|| {
                OrchestratorError::CorruptGoal("external work item counter overflow".to_owned())
            })?;
        let request = replanner::replanner_request_for_goal(goal, session).map_err(|_| {
            OrchestratorError::InvalidDag(
                "external replanner request could not be built".to_owned(),
            )
        })?;
        let mut request = serde_json::to_value(&request).map_err(|_| {
            OrchestratorError::InvalidDag(
                "external replanner request could not be serialized".to_owned(),
            )
        })?;
        if let Some(object) = request.as_object_mut() {
            object.insert(
                "goal_revision".to_owned(),
                serde_json::Value::from(bound_revision),
            );
        }
        let payload = serde_json::to_string(&ExternalReplanWorkRequest {
            goal_id: goal_id.as_str().to_owned(),
            task_id: trigger_task_id.as_str().to_owned(),
            goal_revision: bound_revision,
            plan_revision: goal.plan_revision(),
            output_contract: OUTPUT_CONTRACT_REPLANNER_V1,
            request,
        })
        .map_err(|_| {
            OrchestratorError::InvalidDag(
                "external replanner payload could not be serialized".to_owned(),
            )
        })?;
        if payload.len() > MAX_REQUEST_PAYLOAD_BYTES {
            return Err(OrchestratorError::InvalidDag(
                "external replanner request exceeds the durable WorkItem bound".to_owned(),
            ));
        }
        let item = ModelWorkItem::pending(
            WorkId::new(),
            session.id.clone(),
            goal_id.as_str().to_owned(),
            WorkRole::Replanner,
            Some(trigger_task_id.as_str().to_owned()),
            None,
            bound_revision,
            goal.plan_revision(),
            sha256_hex(payload.as_bytes()),
            payload,
            OUTPUT_CONTRACT_REPLANNER_V1,
            issue_counter,
            now,
        )?;
        external_state_mut(goal)?.record_issued(item.clone())?;
        slot = Some(Ok(item));
        Ok(())
    })?;
    Ok(match slot {
        Some(Ok(item)) => ReplanIssueOutcome::Issued { item },
        Some(Err(block)) => ReplanIssueOutcome::Refused(block),
        None => ReplanIssueOutcome::TriggerGone,
    })
}

/// Validate and apply one external replan in a single durable commit (E5-C W1).
#[expect(
    clippy::too_many_arguments,
    reason = "Goal, work identity, digest, and the untrusted payload are independent authority channels."
)]
pub(crate) fn commit_external_replan(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    expected_revision: u64,
    work_id: &WorkId,
    submission_id: &SubmissionId,
    payload_sha256: &str,
    raw: &[u8],
) -> Result<ReplanApplyOutcome, OrchestratorError> {
    let mut verdict: Option<(ReplanVerdict, SubmissionReceipt)> = None;
    let mut refused: Option<&'static str> = None;
    let committed =
        store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
            // Re-bind on the snapshot that will be persisted.
            let bound = {
                let state = external_state(goal)?;
                match state.active_work() {
                    Some(active)
                        if active.work_id() == work_id
                            && active.goal_id() == goal_id.as_str()
                            && active.state() == WorkItemState::Pending
                            && active.goal_revision() == goal.revision()
                            && active.plan_revision() == goal.plan_revision() =>
                    {
                        Some((active.task_id().map(str::to_owned), active.plan_revision()))
                    }
                    _ => None,
                }
            };
            let Some((_trigger_task, plan_revision)) = bound else {
                refused = Some("WORK_ITEM_NOT_PENDING");
                return Ok(());
            };

            // A replayed plan under a new submission id is decided from the durable
            // supersession/replacement records before any validation.
            if let Some(replay) = replanner::replan_replay_verdict(goal, raw) {
                match replay {
                    ReplanReplay::AlreadyApplied => {
                        let receipt = SubmissionReceipt::new(
                            work_id.clone(),
                            submission_id.clone(),
                            payload_sha256.to_owned(),
                            crate::external_model_work::SubmissionDecision::Committed,
                            Some(goal.revision().checked_add(1).ok_or_else(|| {
                                OrchestratorError::CorruptGoal("goal revision overflow".to_owned())
                            })?),
                            REPLAN_ALREADY_APPLIED,
                            None,
                            now,
                        );
                        external_state_mut(goal)?.record_submission(receipt.clone(), now)?;
                        verdict = Some((ReplanVerdict::AlreadyApplied, receipt));
                        return Ok(());
                    }
                    ReplanReplay::ConflictingDigest => {
                        let receipt = SubmissionReceipt::new(
                            work_id.clone(),
                            submission_id.clone(),
                            payload_sha256.to_owned(),
                            crate::external_model_work::SubmissionDecision::RejectedAuthority,
                            None,
                            "REPLAN_REPLAY_CONFLICT",
                            Some(
                                "a replan request id was replayed with a different proposal"
                                    .to_owned(),
                            ),
                            now,
                        );
                        external_state_mut(goal)?.record_submission(receipt.clone(), now)?;
                        verdict = Some((ReplanVerdict::Rejected, receipt));
                        return Ok(());
                    }
                }
            }

            let goal_root = planner::execution_root_for_goal(goal, session).map_err(|error| {
                OrchestratorError::InvalidDag(format!(
                    "replan execution-root gate refused: {}",
                    planner::planner_failure_code(&error)
                ))
            })?;
            match replanner::validate_replan_output_for_apply(raw, goal, &goal_root) {
                Ok(validated) => {
                    // Dry-run the application on a copy first: the validator checks
                    // the proposal, while the durable DAG invariants (cycles, writer
                    // serialization, criterion coverage of the *resulting* graph) are
                    // checked by the state machine. A model-supplied cycle must be a
                    // typed schema refusal, not a failed commit.
                    let mut probe = goal.clone();
                    if let Err(error) = replanner::apply_validated_replan_to_goal(
                        &mut probe,
                        plan_revision,
                        validated.clone(),
                        now,
                    ) {
                        let receipt = SubmissionReceipt::new(
                            work_id.clone(),
                            submission_id.clone(),
                            payload_sha256.to_owned(),
                            crate::external_model_work::SubmissionDecision::RejectedSchema,
                            None,
                            "REPLAN_APPLY_REJECTED",
                            Some(bounded_replan_detail(&error)),
                            now,
                        );
                        external_state_mut(goal)?.record_submission(receipt.clone(), now)?;
                        verdict = Some((ReplanVerdict::Rejected, receipt));
                        return Ok(());
                    }
                    replanner::apply_validated_replan_to_goal(goal, plan_revision, validated, now)?;
                    let committed_revision = goal.revision().checked_add(1).ok_or_else(|| {
                        OrchestratorError::CorruptGoal("goal revision overflow".to_owned())
                    })?;
                    let receipt = SubmissionReceipt::new(
                        work_id.clone(),
                        submission_id.clone(),
                        payload_sha256.to_owned(),
                        crate::external_model_work::SubmissionDecision::Committed,
                        Some(committed_revision),
                        REPLAN_APPLIED,
                        None,
                        now,
                    );
                    external_state_mut(goal)?.record_submission(receipt.clone(), now)?;
                    verdict = Some((ReplanVerdict::Applied, receipt));
                    Ok(())
                }
                Err(refusal) => {
                    let receipt = SubmissionReceipt::new(
                        work_id.clone(),
                        submission_id.clone(),
                        payload_sha256.to_owned(),
                        crate::external_model_work::SubmissionDecision::RejectedSchema,
                        None,
                        refusal.code(),
                        Some(refusal.detail().to_owned()),
                        now,
                    );
                    external_state_mut(goal)?.record_submission(receipt.clone(), now)?;
                    verdict = Some((ReplanVerdict::Rejected, receipt));
                    Ok(())
                }
            }
        });

    match committed {
        Ok(goal) => {
            if let Some(code) = refused {
                return Ok(ReplanApplyOutcome::Refused { code });
            }
            let (kind, receipt) = verdict.ok_or_else(|| {
                OrchestratorError::CorruptGoal("replan commit produced no verdict".to_owned())
            })?;
            Ok(match kind {
                ReplanVerdict::Applied => ReplanApplyOutcome::Applied { goal, receipt },
                ReplanVerdict::AlreadyApplied => {
                    ReplanApplyOutcome::AlreadyApplied { goal, receipt }
                }
                ReplanVerdict::Rejected => ReplanApplyOutcome::Rejected { goal, receipt },
            })
        }
        Err(OrchestratorError::RevisionConflict { .. }) => Ok(ReplanApplyOutcome::Refused {
            code: "GOAL_REVISION_STALE",
        }),
        Err(error) => Err(error),
    }
}

/// Bounded, content-free description of a durable-DAG refusal.
///
/// The underlying error text can quote a path or an id, so it is truncated and
/// kept within the receipt detail bound.
fn bounded_replan_detail(error: &OrchestratorError) -> String {
    let text = match error {
        OrchestratorError::InvalidDag(reason) => reason.clone(),
        _ => "the replanned graph violates a durable DAG invariant".to_owned(),
    };
    crate::external_model_work::truncate_on_char_boundary(&text, 256)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplanVerdict {
    Applied,
    AlreadyApplied,
    Rejected,
}

/// Whether the external driver should stop because a replan trigger exists but no
/// WorkItem could be issued.
pub(crate) fn replan_unavailable_reason(goal: &Goal) -> Option<&'static str> {
    match replanner::replan_eligible_for_host(goal) {
        Ok(()) => None,
        Err(_) => Some("REPLAN_NOT_ELIGIBLE"),
    }
}
