//! E5-0 — host-only multi-task DAG readiness propagation.
//!
//! The Scheduler only ever *selects* READY Tasks. Before E5, nothing armed a
//! PENDING Task whose predecessors had already reached host-verified
//! `COMPLETED`, so a multi-Task plan reported `READINESS_PROPAGATION_DEFERRED`
//! and stopped. This module adds the single bounded host step that closes that
//! gap:
//!
//! * eligibility is derived from durable state only (PENDING + active plan
//!   authority + no blocker + every dependency active and host-verified
//!   `COMPLETED`), never from a model narrative;
//! * all eligible Tasks are armed in **one** `TaskStore` snapshot commit under
//!   the existing revision CAS, so a partially armed DAG cannot be observed;
//! * the step takes no Session lock while any model runs, and issues no
//!   `ModelWorkItem`, so it can never race a model request;
//! * a superseded, blocked, failed, or replan-pending predecessor simply keeps
//!   its dependents PENDING — the Scheduler's existing typed stop reasons
//!   (`BlockedTasks`, `FailedTasks`, `WaitingForDependencies`, `Replan`) remain
//!   the authority for reporting why.
//!
//! Only EXTERNAL Goals call this today. CODEX Goals keep their existing
//! scheduler behaviour unchanged (see the E5 regression pin
//! `codex_scheduler_selection_is_unchanged`).

use crate::config;
use crate::goal::Goal;
use crate::orchestrator_error::OrchestratorError;
use crate::task::TaskId;
use crate::task_store::TaskStore;

/// Progress code reported when one host step armed at least one Task.
pub(crate) const READINESS_PROPAGATED: &str = "READINESS_PROPAGATED";

/// Bound on armed Tasks per host step. Matches the plan Task cap family so a
/// single step can never arm an unbounded graph.
pub(crate) const MAX_READY_TASKS_PER_STEP: usize = 64;

#[derive(Debug)]
pub(crate) enum ReadinessOutcome {
    /// At least one PENDING Task was armed and the Goal revision advanced.
    ///
    /// Only the bounded count is reported: the armed identities are already
    /// durable in the Task statuses, and a response carries no Task list.
    Propagated { armed: u32 },
    /// Nothing is eligible right now. No write happened.
    NothingEligible,
}

/// Whether a host step would arm anything, evaluated without any write.
///
/// Used by the driver to decide step order: a pending readiness transition is
/// always resolved before a new `ModelWorkItem` is issued.
pub(crate) fn propagation_pending(goal: &Goal) -> Result<bool, OrchestratorError> {
    if !matches!(
        goal.status(),
        crate::goal::GoalStatus::Running | crate::goal::GoalStatus::Replanning
    ) {
        return Ok(false);
    }
    Ok(!goal.ready_candidates()?.is_empty())
}

/// Arm every eligible PENDING Task in one durable commit.
///
/// The pre-check runs on the caller's snapshot so a Goal with nothing to arm
/// performs no write at all. A concurrent writer that moves the revision first
/// wins the CAS; this call then reports `NothingEligible` and the caller re-reads
/// on its next step instead of forcing a second transition set.
pub(crate) fn propagate_ready_tasks(
    store: &TaskStore,
    session: &config::Session,
    goal: &Goal,
) -> Result<ReadinessOutcome, OrchestratorError> {
    let candidates = if propagation_pending(goal)? {
        goal.ready_candidates()?
    } else {
        Vec::new()
    };
    if candidates.is_empty() {
        return Ok(ReadinessOutcome::NothingEligible);
    }
    if candidates.len() > MAX_READY_TASKS_PER_STEP {
        return Err(OrchestratorError::InvalidDag(
            "readiness propagation exceeded the per-step Task bound".to_owned(),
        ));
    }

    let mut armed: Option<Vec<TaskId>> = None;
    let committed =
        store.mutate_goal_snapshot(&session.id, goal.id(), goal.revision(), |goal, now| {
            let task_ids = goal.propagate_ready_tasks(now)?;
            if !task_ids.is_empty() {
                armed = Some(task_ids);
            }
            Ok(())
        });

    match committed {
        Ok(_goal) => match armed {
            Some(task_ids) => Ok(ReadinessOutcome::Propagated {
                armed: u32::try_from(task_ids.len()).unwrap_or(u32::MAX),
            }),
            None => Ok(ReadinessOutcome::NothingEligible),
        },
        Err(OrchestratorError::RevisionConflict { .. }) => Ok(ReadinessOutcome::NothingEligible),
        Err(error) => Err(error),
    }
}

/// Read-only helper for reporting: the PENDING Tasks still waiting and why.
///
/// Returns one `(task_id, blocking_status)` pair per unresolved dependency so a
/// caller can name the reason without guessing. Superseded dependencies are
/// omitted: they no longer exercise plan authority, and the Goal's own
/// validation already refuses a DAG that still depends on them.
pub(crate) fn waiting_reasons(goal: &Goal) -> Vec<(TaskId, TaskId, &'static str)> {
    let mut waiting = Vec::new();
    for (task_id, task) in goal.tasks() {
        if task.status() != crate::task::TaskStatus::Pending || !task.is_active_plan_authority() {
            continue;
        }
        for dependency in task.dependencies() {
            let Some(dep) = goal.tasks().get(dependency.task_id()) else {
                continue;
            };
            if !dep.is_active_plan_authority() {
                continue;
            }
            let reason = match dep.status() {
                crate::task::TaskStatus::Blocked => "BLOCKED_PREDECESSOR",
                crate::task::TaskStatus::Failed => "FAILED_PREDECESSOR",
                crate::task::TaskStatus::NeedsReplan => "REPLAN_PREDECESSOR",
                crate::task::TaskStatus::Completed => continue,
                _ => "PREDECESSOR_NOT_COMPLETED",
            };
            waiting.push((task_id.clone(), dep.id().clone(), reason));
        }
    }
    waiting
}

/// Deterministic, bounded rendering of `waiting_reasons` for a typed stop detail.
pub(crate) fn waiting_detail(goal: &Goal) -> Option<String> {
    let waiting = waiting_reasons(goal);
    if waiting.is_empty() {
        return None;
    }
    let mut rendered = String::new();
    for (task_id, predecessor, reason) in waiting.iter().take(4) {
        if !rendered.is_empty() {
            rendered.push(';');
        }
        rendered.push_str(reason);
        rendered.push(':');
        rendered.push_str(task_id.as_str());
        rendered.push_str("<-");
        rendered.push_str(predecessor.as_str());
    }
    Some(rendered)
}

/// Read-only accessor used by tests and diagnostics: the Tasks a readiness step
/// would arm on this snapshot.
#[cfg(test)]
pub(crate) fn eligible_task_ids(goal: &Goal) -> Result<Vec<TaskId>, OrchestratorError> {
    goal.ready_candidates()
}
