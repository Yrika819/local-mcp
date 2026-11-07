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

use crate::external_model_work::{
    ExternalModelState, ExternalStop, ModelWorkItem, SubmissionReceipt,
};
use crate::goal::{Goal, GoalId};
use crate::orchestrator_error::OrchestratorError;
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
