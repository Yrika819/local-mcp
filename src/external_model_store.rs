//! Durable storage boundary for the External Agent Backend.
//!
//! The external state lives *inside* the durable Goal document, so every
//! transition is one `TaskStore` snapshot commit. That reuse is deliberate
//! (ADR-09): the existing session OS lock, the optimistic revision check, the
//! atomic replace, and the schema-migration chain keep applying unchanged.
//!
//! Nothing here holds the session lock across model work. A caller does exactly
//! one of two things:
//!
//! * read the Goal (lock taken and released by `TaskStore`), or
//! * pass a pure mutation closure to [`mutate_external`], which is where the
//!   lock, the revision check, and the single commit happen.
//!
//! A refusal never reports a WorkItem as issued: the mutation must commit
//! successfully before the driver serializes a response (invariant 4).

// Same E3-A staging scope as `external_model_work`: these helpers are the only
// durable write boundary, and E3-B's driver is their production consumer.
#![allow(
    dead_code,
    reason = "E3-A durable write boundary; E3-B driver consumes it and removes this attribute"
)]

use crate::config;
use crate::external_model_work::{
    ExternalModelState, ExternalStop, ModelWorkItem, SubmissionDecision, SubmissionId,
    SubmissionReceipt, WorkId, WorkItemState,
};
use crate::goal::{Goal, GoalId};
use crate::orchestrator_error::OrchestratorError;
use crate::planner;
use crate::task_store::TaskStore;

/// Borrow the external state of an EXTERNAL Goal.
///
/// A CODEX Goal has no external state, and a caller that assumed otherwise gets
/// a durable-state error rather than a silently created empty record.
pub(crate) fn external_state(goal: &Goal) -> Result<&ExternalModelState, OrchestratorError> {
    match (goal.model_backend().is_external(), goal.external_model()) {
        (true, Some(state)) => Ok(state),
        (true, None) => Err(OrchestratorError::CorruptGoal(
            "EXTERNAL goal has no durable external model state".to_owned(),
        )),
        (false, Some(_)) => Err(OrchestratorError::CorruptGoal(
            "CODEX goal must not carry durable external model state".to_owned(),
        )),
        (false, None) => Err(OrchestratorError::CorruptGoal(
            "goal is not an EXTERNAL backend goal".to_owned(),
        )),
    }
}

/// Mutable variant of [`external_state`].
pub(crate) fn external_state_mut(
    goal: &mut Goal,
) -> Result<&mut ExternalModelState, OrchestratorError> {
    if !goal.model_backend().is_external() {
        return Err(OrchestratorError::CorruptGoal(
            "goal is not an EXTERNAL backend goal".to_owned(),
        ));
    }
    goal.external_model_mut().ok_or_else(|| {
        OrchestratorError::CorruptGoal(
            "EXTERNAL goal has no durable external model state".to_owned(),
        )
    })
}

/// Commit one pure mutation of the external state.
///
/// Returns the closure's value together with the committed Goal snapshot. The
/// caller must only report success (for example "the WorkItem was issued")
/// after this function returns `Ok`.
pub(crate) fn mutate_external<T, F>(
    store: &TaskStore,
    session_id: &str,
    goal_id: &GoalId,
    expected_revision: u64,
    mutate: F,
) -> Result<(T, Goal), OrchestratorError>
where
    F: FnOnce(&mut ExternalModelState, &str) -> Result<T, OrchestratorError>,
{
    let mut slot: Option<T> = None;
    let goal =
        store.mutate_goal_snapshot(session_id, goal_id, expected_revision, |goal, now| {
            let state = external_state_mut(goal)?;
            let value = mutate(state, now)?;
            slot = Some(value);
            Ok(())
        })?;
    let value = slot.ok_or_else(|| {
        OrchestratorError::CorruptGoal("external mutation committed without a result".to_owned())
    })?;
    Ok((value, goal))
}

/// Atomically record a newly issued WorkItem.
///
/// `expected_revision` is the revision the driver read while deciding to issue.
/// A concurrent writer loses the CAS instead of producing a second active
/// WorkItem.
pub(crate) fn commit_issued_work_item(
    store: &TaskStore,
    session_id: &str,
    goal_id: &GoalId,
    expected_revision: u64,
    item: ModelWorkItem,
) -> Result<Goal, OrchestratorError> {
    let (_, goal) = mutate_external(
        store,
        session_id,
        goal_id,
        expected_revision,
        move |state, _now| state.record_issued(item),
    )?;
    Ok(goal)
}

/// Atomically record one submission verdict (acceptance or a refusal).
pub(crate) fn commit_submission(
    store: &TaskStore,
    session_id: &str,
    goal_id: &GoalId,
    expected_revision: u64,
    receipt: SubmissionReceipt,
) -> Result<Goal, OrchestratorError> {
    let (_, goal) = mutate_external(
        store,
        session_id,
        goal_id,
        expected_revision,
        move |state, now| state.record_submission(receipt, now),
    )?;
    Ok(goal)
}

/// Atomically record a host-only stop reason for `goal_next` diagnostics.
pub(crate) fn commit_stop(
    store: &TaskStore,
    session_id: &str,
    goal_id: &GoalId,
    expected_revision: u64,
    stop: ExternalStop,
) -> Result<Goal, OrchestratorError> {
    let (_, goal) = mutate_external(
        store,
        session_id,
        goal_id,
        expected_revision,
        move |state, _now| {
            state.set_stop(stop);
            Ok(())
        },
    )?;
    Ok(goal)
}

/// Atomically invalidate the active WorkItem with a typed reason.
///
/// Used when a host transition advanced the Goal revision while a request was
/// outstanding: the request can no longer be submitted, so it is cancelled
/// durably instead of dead-ending the Goal.
pub(crate) fn commit_cancel_active_work(
    store: &TaskStore,
    session_id: &str,
    goal_id: &GoalId,
    expected_revision: u64,
    code: &'static str,
) -> Result<Goal, OrchestratorError> {
    let (_, goal) = mutate_external(
        store,
        session_id,
        goal_id,
        expected_revision,
        move |state, _now| {
            state.cancel_active_work(code);
            Ok(())
        },
    )?;
    Ok(goal)
}

/// Outcome of one atomic Planner submission.
///
/// Every variant was decided against a single durable snapshot, and every
/// variant except `Refused` describes durable state that this call committed
/// (or that already existed for a replay).
#[derive(Debug)]
pub(crate) enum PlannerSubmissionOutcome {
    /// The validated plan and its receipt were committed together, and the
    /// WorkItem moved to COMMITTED. Not a Goal completion.
    Committed {
        goal: Goal,
        receipt: SubmissionReceipt,
    },
    /// The host refused the proposal. The refusal receipt and the WorkItem's
    /// REJECTED transition are the same durable commit, exactly as in E3.
    Rejected {
        goal: Goal,
        receipt: SubmissionReceipt,
    },
    /// Nothing was written: the Goal moved on between validation and commit, or
    /// the WorkItem binding no longer holds.
    Refused { code: &'static str },
}

/// Atomically validate and apply one external Planner submission (H1-2).
///
/// One `TaskStore` mutation closure performs the whole sequence, so the durable
/// document can never contain a Task DAG without a receipt, a receipt without
/// its DAG, or a second copy of either:
///
/// 1. re-check the Session/Goal/WorkItem/revision binding against the snapshot
///    the commit will actually persist,
/// 2. validate the untrusted proposal with the same validator the Codex path
///    uses,
/// 3. materialize the Task DAG and the criterion bindings,
/// 4. advance the plan revision,
/// 5. finalize the receipt with the revision this commit produces,
/// 6. move the WorkItem to COMMITTED.
///
/// The lock is taken exactly once. Model inference already happened in the
/// client, so no lock is ever held across model work.
#[expect(
    clippy::too_many_arguments,
    reason = "Goal, work identity, digest, and untrusted payload are independent authority channels."
)]
pub(crate) fn commit_planner_submission(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    expected_revision: u64,
    work_id: &WorkId,
    submission_id: &SubmissionId,
    payload_sha256: &str,
    response: &[u8],
) -> Result<PlannerSubmissionOutcome, OrchestratorError> {
    let mut verdict: Option<(bool, SubmissionReceipt)> = None;
    let mut refused: Option<&'static str> = None;
    let committed =
        store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
            // Re-bind on the snapshot that will be persisted. The API layer
            // already refused a foreign or stale request; this second check is
            // what makes the commit itself authoritative. The borrow ends before
            // the plan is applied, so no two exclusive borrows of `goal` overlap.
            let pending = matches!(
                external_state(goal)?.active_work(),
                Some(active)
                    if active.work_id() == work_id
                        && active.goal_id() == goal_id.as_str()
                        && active.state() == WorkItemState::Pending
                        && active.goal_revision() == goal.revision()
                        && active.plan_revision() == goal.plan_revision()
            );
            if !pending {
                refused = Some("WORK_ITEM_NOT_PENDING");
                return Ok(());
            }

            let goal_root = planner::execution_root_for_goal(goal, session).map_err(|error| {
                OrchestratorError::InvalidDag(format!(
                    "planner execution-root gate refused: {}",
                    planner::planner_failure_code(&error)
                ))
            })?;

            match planner::validate_plan_output_for_apply(response, goal, &goal_root) {
                Ok(plan) => {
                    planner::apply_validated_plan_to_goal(goal, plan, now)?;
                    let committed_revision = goal.revision().checked_add(1).ok_or_else(|| {
                        OrchestratorError::CorruptGoal("goal revision overflow".to_owned())
                    })?;
                    let receipt = SubmissionReceipt::new(
                        work_id.clone(),
                        submission_id.clone(),
                        payload_sha256.to_owned(),
                        SubmissionDecision::Committed,
                        Some(committed_revision),
                        "PLAN_APPLIED",
                        None,
                        now,
                    );
                    external_state_mut(goal)?.record_submission(receipt.clone(), now)?;
                    verdict = Some((true, receipt));
                    Ok(())
                }
                Err(refusal) => {
                    let receipt = SubmissionReceipt::new(
                        work_id.clone(),
                        submission_id.clone(),
                        payload_sha256.to_owned(),
                        SubmissionDecision::RejectedSchema,
                        None,
                        refusal.code(),
                        Some(refusal.detail().to_owned()),
                        now,
                    );
                    external_state_mut(goal)?.record_submission(receipt.clone(), now)?;
                    verdict = Some((false, receipt));
                    Ok(())
                }
            }
        });

    match committed {
        Ok(goal) => {
            if let Some(code) = refused {
                return Ok(PlannerSubmissionOutcome::Refused { code });
            }
            let (applied, receipt) = verdict.ok_or_else(|| {
                OrchestratorError::CorruptGoal(
                    "planner submission committed without a verdict".to_owned(),
                )
            })?;
            if applied {
                Ok(PlannerSubmissionOutcome::Committed { goal, receipt })
            } else {
                Ok(PlannerSubmissionOutcome::Rejected { goal, receipt })
            }
        }
        Err(OrchestratorError::RevisionConflict { .. }) => Ok(PlannerSubmissionOutcome::Refused {
            code: "GOAL_REVISION_STALE",
        }),
        Err(error) => Err(error),
    }
}

/// Invalidate the active WorkItem during a host-owned cancellation.
///
/// Called from the existing `goal_cancel` mutation closure so cancellation and
/// WorkItem invalidation are the same durable commit (invariant 10).
pub(crate) fn cancel_active_work_in_place(goal: &mut Goal, code: &str) {
    if goal.model_backend().is_external()
        && let Some(state) = goal.external_model_mut()
    {
        state.cancel_active_work(code);
    }
}
