use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;
use uuid::Uuid;

use crate::config::Session;
use crate::fallback::{SideEffectClass, SideEffectState};
use crate::goal::{CheckpointReason, Goal, GoalBlocker, GoalStatus};
use crate::goal_api;
use crate::goal_finalizer::{
    GoalFinalizationOutcome, GoalFinalizerError, commit_goal_finalization,
    evaluate_goal_finalization, finalize_goal, prepare_goal_finalization,
};
use crate::orchestrator_error::OrchestratorError;
use crate::scheduler::{SchedulerDecision, SchedulerNoActionReason, select_next_action};
use crate::task::{
    ReplaySafety, Task, TaskBlocker, TaskDependency, TaskOperationKind, TaskScope, TaskStatus,
    TaskTransitionContext, VerificationOutcome, VerificationResult, WorkerKind,
};
use crate::task_store::TaskStore;

const NOW: &str = "2026-09-13T13:30:00Z";

fn scope(kind: TaskOperationKind) -> TaskScope {
    TaskScope::new(
        vec![PathBuf::from("src")],
        vec![],
        kind,
        if kind == TaskOperationKind::ReadOnly {
            ReplaySafety::SafeReadOnly
        } else {
            ReplaySafety::VerifyBeforeRetry
        },
    )
}

fn task(mandatory: bool, kind: TaskOperationKind) -> Task {
    Task::new(
        "task",
        "objective",
        mandatory,
        WorkerKind::CodexReadonly,
        scope(kind),
        vec![],
        3,
        1,
        NOW,
    )
    .unwrap()
}

fn new_goal(session_id: &str) -> Goal {
    Goal::new(
        session_id,
        PathBuf::from("/tmp/project"),
        "objective",
        Some("phase9".into()),
        vec![],
        vec!["final criterion".into()],
        NOW,
    )
    .unwrap()
}

fn planned_goal(session_id: &str, tasks: Vec<Task>) -> Goal {
    let mut goal = new_goal(session_id);
    goal.materialize_initial_plan(tasks, NOW).unwrap();
    goal
}

fn safe() -> TaskTransitionContext {
    TaskTransitionContext {
        active_worker_stopped: true,
        side_effect_reconciled: true,
    }
}

fn complete_task(goal: &mut Goal, id: &crate::task::TaskId) {
    if goal.tasks().get(id).unwrap().status() == TaskStatus::Ready {
        goal.transition_task(id, TaskStatus::Running, safe(), NOW)
            .unwrap();
    }
    if goal.tasks().get(id).unwrap().status() == TaskStatus::Running {
        goal.transition_task(id, TaskStatus::Verifying, safe(), NOW)
            .unwrap();
    }
    goal.task_record_verification_result(
        id,
        VerificationResult::new(VerificationOutcome::Passed, vec![], NOW, NOW),
    )
    .unwrap();
    goal.transition_task(id, TaskStatus::Completed, safe(), NOW)
        .unwrap();
}

fn ready_for_finalization(session_id: &str, final_outcome: VerificationOutcome) -> Goal {
    let mandatory = task(true, TaskOperationKind::ReadOnly);
    let id = mandatory.id().clone();
    let mut goal = planned_goal(session_id, vec![mandatory]);
    complete_task(&mut goal, &id);
    goal.enter_verifying_for_test(NOW).unwrap();
    goal.record_final_verification(VerificationResult::new(final_outcome, vec![], NOW, NOW))
        .unwrap();
    goal
}

struct TempState {
    path: PathBuf,
}

impl TempState {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("local-mcp-phase9-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempState {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn store_goal(goal: &Goal) -> (TempState, TaskStore) {
    let dir = TempState::new();
    let store = TaskStore::with_state_root(dir.path().to_path_buf());
    store.create_goal(goal).unwrap();
    (dir, store)
}

#[test]
fn generic_goal_transition_cannot_bypass_finalizer_completion_authority() {
    let mut goal = ready_for_finalization("phase9-seal-generic", VerificationOutcome::Passed);
    goal.add_checkpoint(CheckpointReason::FinalVerification, NOW)
        .unwrap();
    let before = goal.clone();
    assert!(goal.transition_to(GoalStatus::Completed, NOW).is_err());
    assert_eq!(goal.status(), GoalStatus::Verifying);
    assert_eq!(goal.completed_at(), None);
    assert_eq!(goal.tasks(), before.tasks());
}

#[test]
fn scheduler_stops_at_goal_finalization_boundary() {
    let goal = ready_for_finalization("phase9-seal-scheduler", VerificationOutcome::Passed);
    let decision = select_next_action(&goal).unwrap();
    assert_eq!(
        decision,
        SchedulerDecision::NoAction {
            reason: SchedulerNoActionReason::GoalFinalizationRequired
        }
    );
    assert_eq!(goal.status(), GoalStatus::Verifying);
    assert_eq!(goal.completed_at(), None);
}

#[test]
fn mcp_pause_resume_cancel_cannot_create_completed_goal() {
    for (index, operation) in ["pause", "resume", "cancel"].into_iter().enumerate() {
        let session_id = format!("phase9-mcp-seal-{index}");
        let goal = ready_for_finalization(&session_id, VerificationOutcome::Passed);
        let id = goal.id().clone();
        let (_dir, store) = store_goal(&goal);
        let session = Session {
            id: session_id.clone(),
            cwd: PathBuf::from("/tmp/project"),
            permitted_directories: vec![],
        };
        let args = json!({"session_id": session_id, "goal_id": id.as_str()});
        match operation {
            "pause" => {
                goal_api::goal_pause(&args, &session, &store).unwrap();
            }
            "resume" => {
                goal_api::goal_resume(&args, &session, &store).unwrap();
            }
            "cancel" => {
                goal_api::goal_cancel(&args, &session, &store).unwrap();
            }
            _ => unreachable!(),
        }
        let stored = store.load_goal(&session.id, &id).unwrap();
        assert_ne!(stored.status(), GoalStatus::Completed, "{operation}");
    }
}

#[test]
fn task_completion_alone_does_not_complete_goal() {
    let mandatory = task(true, TaskOperationKind::ReadOnly);
    let task_id = mandatory.id().clone();
    let mut goal = planned_goal("phase9-task-only", vec![mandatory]);
    complete_task(&mut goal, &task_id);
    assert_eq!(goal.tasks()[&task_id].status(), TaskStatus::Completed);
    assert_eq!(goal.status(), GoalStatus::Running);
    assert_eq!(goal.completed_at(), None);
}

#[test]
fn missing_and_indeterminate_final_verification_never_complete() {
    let mandatory = task(true, TaskOperationKind::ReadOnly);
    let task_id = mandatory.id().clone();
    let mut missing = planned_goal("phase9-final-missing", vec![mandatory]);
    complete_task(&mut missing, &task_id);
    missing.enter_verifying_for_test(NOW).unwrap();
    let missing_id = missing.id().clone();
    let (_dir, missing_store) = store_goal(&missing);
    let missing_decision = evaluate_goal_finalization(
        &prepare_goal_finalization(&missing_store, "phase9-final-missing", &missing_id).unwrap(),
    )
    .unwrap();
    assert_eq!(
        missing_decision.outcome(),
        GoalFinalizationOutcome::NotReady
    );
    assert!(
        missing_decision
            .blockers()
            .iter()
            .any(|b| b.code() == "FINAL_VERIFICATION_PENDING")
    );

    let indeterminate = ready_for_finalization(
        "phase9-final-indeterminate",
        VerificationOutcome::Indeterminate,
    );
    let indeterminate_id = indeterminate.id().clone();
    let (_dir2, indeterminate_store) = store_goal(&indeterminate);
    let indeterminate_decision = evaluate_goal_finalization(
        &prepare_goal_finalization(
            &indeterminate_store,
            "phase9-final-indeterminate",
            &indeterminate_id,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        indeterminate_decision.outcome(),
        GoalFinalizationOutcome::Blocked
    );
    assert!(
        indeterminate_decision
            .blockers()
            .iter()
            .any(|b| b.code() == "FINAL_VERIFICATION_INDETERMINATE")
    );
}
#[test]
fn basic_completion_is_atomic_durable_and_task_immutable() {
    let goal = ready_for_finalization("phase9-basic", VerificationOutcome::Passed);
    let id = goal.id().clone();
    let tasks_before = goal.tasks().clone();
    let plan_revision_before = goal.plan_revision();
    let (_dir, store) = store_goal(&goal);
    let result = finalize_goal(&store, "phase9-basic", &id).unwrap();
    let decision = result.decision();
    let completed = result.goal();
    assert_eq!(decision.outcome(), GoalFinalizationOutcome::ReadyToComplete);
    assert_eq!(completed.status(), GoalStatus::Completed);
    assert_eq!(completed.revision(), 2);
    assert_eq!(completed.plan_revision(), plan_revision_before);
    assert!(completed.completed_at().is_some());
    assert_eq!(completed.tasks(), &tasks_before);
    let checkpoint = completed.checkpoints().last().unwrap();
    assert_eq!(checkpoint.reason(), CheckpointReason::FinalVerification);
    assert_eq!(checkpoint.goal_status(), GoalStatus::Verifying);
    assert_eq!(checkpoint.goal_revision(), 1);
    assert_eq!(
        store.load_goal("phase9-basic", &id).unwrap(),
        completed.clone()
    );
}

#[test]
fn completed_tasks_do_not_override_failed_goal_verification() {
    let goal = ready_for_finalization("phase9-final-fail", VerificationOutcome::Failed);
    let id = goal.id().clone();
    let (_dir, store) = store_goal(&goal);
    let result = finalize_goal(&store, "phase9-final-fail", &id).unwrap();
    let decision = result.decision();
    let current = result.goal();
    assert_eq!(decision.outcome(), GoalFinalizationOutcome::Failed);
    assert_eq!(current.status(), GoalStatus::Verifying);
    assert_eq!(current.revision(), 1);
    assert!(current.completed_at().is_none());
}

#[test]
fn finalization_evaluation_is_deterministic() {
    let goal = ready_for_finalization("phase9-deterministic", VerificationOutcome::Passed);
    let id = goal.id().clone();
    let (_dir, store) = store_goal(&goal);
    let snapshot = prepare_goal_finalization(&store, "phase9-deterministic", &id).unwrap();
    assert_eq!(
        evaluate_goal_finalization(&snapshot).unwrap(),
        evaluate_goal_finalization(&snapshot).unwrap()
    );
}

#[test]
fn finalization_requires_verifying_goal_state() {
    let mandatory = task(true, TaskOperationKind::ReadOnly);
    let id = mandatory.id().clone();
    let mut goal = planned_goal("phase9-running", vec![mandatory]);
    complete_task(&mut goal, &id);
    let goal_id = goal.id().clone();
    let (_dir, store) = store_goal(&goal);
    let snapshot = prepare_goal_finalization(&store, "phase9-running", &goal_id).unwrap();
    let decision = evaluate_goal_finalization(&snapshot).unwrap();
    assert_eq!(decision.outcome(), GoalFinalizationOutcome::NotReady);
    assert!(
        decision
            .blockers()
            .iter()
            .any(|b| b.code() == "GOAL_NOT_VERIFYING")
    );
}
fn goal_with_mandatory_status(session: &str, status: TaskStatus) -> Goal {
    if status == TaskStatus::Pending {
        let mut goal = new_goal(session);
        goal.add_task(
            "task",
            "objective",
            true,
            WorkerKind::CodexReadonly,
            scope(TaskOperationKind::ReadOnly),
            vec![],
            3,
            NOW,
        )
        .unwrap();
        return goal;
    }
    let mandatory = task(true, TaskOperationKind::ReadOnly);
    let id = mandatory.id().clone();
    let mut goal = planned_goal(session, vec![mandatory]);
    match status {
        TaskStatus::Ready => {}
        TaskStatus::Running => goal
            .transition_task(&id, TaskStatus::Running, safe(), NOW)
            .unwrap(),
        TaskStatus::Verifying => {
            goal.transition_task(&id, TaskStatus::Running, safe(), NOW)
                .unwrap();
            goal.transition_task(&id, TaskStatus::Verifying, safe(), NOW)
                .unwrap();
        }
        TaskStatus::Blocked
        | TaskStatus::Retryable
        | TaskStatus::NeedsReplan
        | TaskStatus::Failed => {
            goal.transition_task(&id, TaskStatus::Running, safe(), NOW)
                .unwrap();
            goal.transition_task(&id, status, safe(), NOW).unwrap();
        }
        TaskStatus::Cancelled => goal
            .transition_task(&id, TaskStatus::Cancelled, safe(), NOW)
            .unwrap(),
        _ => unreachable!(),
    }
    goal
}

#[test]
fn every_noncompleted_mandatory_task_state_rejects_completion() {
    let states = [
        TaskStatus::Pending,
        TaskStatus::Ready,
        TaskStatus::Running,
        TaskStatus::Blocked,
        TaskStatus::Retryable,
        TaskStatus::NeedsReplan,
        TaskStatus::Verifying,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
    ];
    for (index, status) in states.into_iter().enumerate() {
        let session = format!("phase9-mandatory-{index}");
        let goal = goal_with_mandatory_status(&session, status);
        let id = goal.id().clone();
        let (_dir, store) = store_goal(&goal);
        let snapshot = prepare_goal_finalization(&store, &session, &id).unwrap();
        let decision = evaluate_goal_finalization(&snapshot).unwrap();
        assert_ne!(
            decision.outcome(),
            GoalFinalizationOutcome::ReadyToComplete,
            "{status:?}"
        );
        assert!(
            decision
                .blockers()
                .iter()
                .any(|b| b.code() == "MANDATORY_TASK_NOT_COMPLETED"),
            "{status:?}"
        );
    }
}
fn goal_with_optional_status(session: &str, status: TaskStatus, enter_verifying: bool) -> Goal {
    let mandatory = task(true, TaskOperationKind::ReadOnly);
    let mandatory_id = mandatory.id().clone();
    let mut optional = task(false, TaskOperationKind::ReadOnly);
    let optional_id = optional.id().clone();
    if status == TaskStatus::Pending {
        optional
            .strengthen_dependencies(
                vec![TaskDependency::completed(mandatory_id.clone())],
                &BTreeSet::new(),
            )
            .unwrap();
    }
    let mut goal = planned_goal(session, vec![mandatory, optional]);
    complete_task(&mut goal, &mandatory_id);
    if status != TaskStatus::Pending {
        match status {
            TaskStatus::Ready => {}
            TaskStatus::Running => goal
                .transition_task(&optional_id, TaskStatus::Running, safe(), NOW)
                .unwrap(),
            TaskStatus::Verifying => {
                goal.transition_task(&optional_id, TaskStatus::Running, safe(), NOW)
                    .unwrap();
                goal.transition_task(&optional_id, TaskStatus::Verifying, safe(), NOW)
                    .unwrap();
            }
            TaskStatus::Blocked
            | TaskStatus::Retryable
            | TaskStatus::NeedsReplan
            | TaskStatus::Failed => {
                goal.transition_task(&optional_id, TaskStatus::Running, safe(), NOW)
                    .unwrap();
                goal.transition_task(&optional_id, status, safe(), NOW)
                    .unwrap();
            }
            TaskStatus::Cancelled => goal
                .transition_task(&optional_id, TaskStatus::Cancelled, safe(), NOW)
                .unwrap(),
            TaskStatus::Completed => complete_task(&mut goal, &optional_id),
            _ => unreachable!(),
        }
    }
    if enter_verifying {
        goal.enter_verifying_for_test(NOW).unwrap();
        goal.record_final_verification(VerificationResult::new(
            VerificationOutcome::Passed,
            vec![],
            NOW,
            NOW,
        ))
        .unwrap();
    }
    goal
}
#[test]
fn optional_task_policy_matches_design() {
    let accepted = [
        TaskStatus::Pending,
        TaskStatus::Ready,
        TaskStatus::Blocked,
        TaskStatus::Retryable,
        TaskStatus::Completed,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
    ];
    for (index, status) in accepted.into_iter().enumerate() {
        let session = format!("phase9-optional-ok-{index}");
        let goal = goal_with_optional_status(&session, status, true);
        let id = goal.id().clone();
        let (_dir, store) = store_goal(&goal);
        let decision =
            evaluate_goal_finalization(&prepare_goal_finalization(&store, &session, &id).unwrap())
                .unwrap();
        assert_eq!(
            decision.outcome(),
            GoalFinalizationOutcome::ReadyToComplete,
            "{status:?}"
        );
    }
}

#[test]
fn active_or_needs_replan_optional_task_blocks_finalization_entry() {
    for (index, status) in [
        TaskStatus::Running,
        TaskStatus::Verifying,
        TaskStatus::NeedsReplan,
    ]
    .into_iter()
    .enumerate()
    {
        let session = format!("phase9-optional-no-{index}");
        let goal = goal_with_optional_status(&session, status, false);
        let id = goal.id().clone();
        let (_dir, store) = store_goal(&goal);
        let decision =
            evaluate_goal_finalization(&prepare_goal_finalization(&store, &session, &id).unwrap())
                .unwrap();
        assert_ne!(decision.outcome(), GoalFinalizationOutcome::ReadyToComplete);
        assert!(
            decision
                .blockers()
                .iter()
                .any(|b| b.code() == "TASK_STATE_UNSETTLED_FOR_FINALIZATION")
        );
    }
}
#[test]
fn unresolved_goal_blocker_prevents_completion() {
    let mut goal = ready_for_finalization("phase9-goal-blocker", VerificationOutcome::Passed);
    goal.add_blocker(GoalBlocker::new("HOLD", "unresolved", false))
        .unwrap();
    let id = goal.id().clone();
    let (_dir, store) = store_goal(&goal);
    let result = finalize_goal(&store, "phase9-goal-blocker", &id).unwrap();
    let decision = result.decision();
    let current = result.goal();
    assert_eq!(decision.outcome(), GoalFinalizationOutcome::Blocked);
    assert_eq!(current.status(), GoalStatus::Verifying);
    assert!(
        decision
            .blockers()
            .iter()
            .any(|b| b.code() == "UNRESOLVED_GOAL_BLOCKER")
    );
}

#[test]
fn mandatory_task_and_recovery_blockers_prevent_completion() {
    let mandatory = task(true, TaskOperationKind::ReadOnly);
    let task_id = mandatory.id().clone();
    let mut goal = planned_goal("phase9-task-blocker", vec![mandatory]);
    goal.transition_task(&task_id, TaskStatus::Running, safe(), NOW)
        .unwrap();
    goal.transition_task(&task_id, TaskStatus::Blocked, safe(), NOW)
        .unwrap();
    goal.task_add_blocker(
        &task_id,
        TaskBlocker::new("RECOVERY_RECONCILIATION_REQUIRED", "reconcile", true),
    )
    .unwrap();
    let id = goal.id().clone();
    let (_dir, store) = store_goal(&goal);
    let decision = evaluate_goal_finalization(
        &prepare_goal_finalization(&store, "phase9-task-blocker", &id).unwrap(),
    )
    .unwrap();
    assert_eq!(decision.outcome(), GoalFinalizationOutcome::Blocked);
    assert!(
        decision
            .blockers()
            .iter()
            .any(|b| b.code() == "UNRESOLVED_TASK_BLOCKER")
    );
}
#[test]
fn unknown_side_effect_blocks_goal_even_without_workspace_observation_failure() {
    let mandatory = task(true, TaskOperationKind::LocalMutation);
    let task_id = mandatory.id().clone();
    let mut goal = planned_goal("phase9-unknown", vec![mandatory]);
    goal.transition_task(&task_id, TaskStatus::Running, safe(), NOW)
        .unwrap();
    goal.task_bind_latest_attempt_execution(
        &task_id,
        Some("op-1".into()),
        Some("scope-1".into()),
        None,
        Some(SideEffectClass::LocalMutation),
        Some(SideEffectState::Unknown),
        Some(1),
        Some(1),
    )
    .unwrap();
    goal.transition_task(&task_id, TaskStatus::Blocked, safe(), NOW)
        .unwrap();
    let id = goal.id().clone();
    let (_dir, store) = store_goal(&goal);
    let decision = evaluate_goal_finalization(
        &prepare_goal_finalization(&store, "phase9-unknown", &id).unwrap(),
    )
    .unwrap();
    assert_eq!(decision.outcome(), GoalFinalizationOutcome::Blocked);
    assert!(
        decision
            .blockers()
            .iter()
            .any(|b| b.code() == "UNKNOWN_SIDE_EFFECT")
    );
}
#[test]
fn stale_goal_revision_cannot_commit() {
    let goal = ready_for_finalization("phase9-revision-race", VerificationOutcome::Passed);
    let id = goal.id().clone();
    let (_dir, store) = store_goal(&goal);
    let snapshot = prepare_goal_finalization(&store, "phase9-revision-race", &id).unwrap();
    let decision = evaluate_goal_finalization(&snapshot).unwrap();
    store
        .mutate_goal_snapshot("phase9-revision-race", &id, 1, |goal, _| {
            goal.add_blocker(GoalBlocker::new("RACE", "newer state", true))
        })
        .unwrap();
    let error = commit_goal_finalization(&store, &snapshot, &decision).unwrap_err();
    assert!(matches!(
        error,
        GoalFinalizerError::Store(OrchestratorError::RevisionConflict {
            expected: 1,
            actual: 2
        })
    ));
    let current = store.load_goal("phase9-revision-race", &id).unwrap();
    assert_eq!(current.revision(), 2);
    assert_ne!(current.status(), GoalStatus::Completed);
}
#[test]
fn plan_revision_change_rejects_old_finalization() {
    let goal = ready_for_finalization("phase9-plan-race", VerificationOutcome::Passed);
    let id = goal.id().clone();
    let (_dir, store) = store_goal(&goal);
    let snapshot = prepare_goal_finalization(&store, "phase9-plan-race", &id).unwrap();
    let decision = evaluate_goal_finalization(&snapshot).unwrap();
    store
        .mutate_goal_snapshot("phase9-plan-race", &id, 1, |goal, now| {
            goal.add_task(
                "optional-new",
                "newer plan",
                false,
                WorkerKind::CodexReadonly,
                scope(TaskOperationKind::ReadOnly),
                vec![],
                3,
                now,
            )
            .map(|_| ())
        })
        .unwrap();
    let error = commit_goal_finalization(&store, &snapshot, &decision).unwrap_err();
    assert!(matches!(
        error,
        GoalFinalizerError::Store(OrchestratorError::RevisionConflict {
            expected: 1,
            actual: 2
        })
    ));
    let current = store.load_goal("phase9-plan-race", &id).unwrap();
    assert_eq!(current.plan_revision(), snapshot.plan_revision() + 1);
    assert_eq!(current.status(), GoalStatus::Verifying);
}
#[test]
fn successful_finalization_is_idempotent() {
    let goal = ready_for_finalization("phase9-idempotent", VerificationOutcome::Passed);
    let id = goal.id().clone();
    let (_dir, store) = store_goal(&goal);
    let first = finalize_goal(&store, "phase9-idempotent", &id).unwrap();
    let after_first = first.goal().clone();
    let second = finalize_goal(&store, "phase9-idempotent", &id).unwrap();
    let after_second = second.goal();
    assert_eq!(
        first.decision().outcome(),
        GoalFinalizationOutcome::ReadyToComplete
    );
    assert_eq!(
        second.decision().outcome(),
        GoalFinalizationOutcome::AlreadyCompleted
    );
    assert_eq!(after_second.revision(), after_first.revision());
    assert_eq!(after_second.checkpoints(), after_first.checkpoints());
    assert_eq!(after_second.completed_at(), after_first.completed_at());
    assert_eq!(after_second.tasks(), after_first.tasks());
}

#[test]
fn failed_and_cancelled_goals_are_not_resurrected() {
    for (index, status) in [GoalStatus::Failed, GoalStatus::Cancelled]
        .into_iter()
        .enumerate()
    {
        let session = format!("phase9-terminal-{index}");
        let mandatory = task(true, TaskOperationKind::ReadOnly);
        let task_id = mandatory.id().clone();
        let mut goal = planned_goal(&session, vec![mandatory]);
        complete_task(&mut goal, &task_id);
        if status == GoalStatus::Failed {
            goal.transition_to(GoalStatus::Failed, NOW).unwrap();
        } else {
            goal.transition_to(GoalStatus::Cancelling, NOW).unwrap();
            goal.transition_to(GoalStatus::Cancelled, NOW).unwrap();
        }
        let id = goal.id().clone();
        let (_dir, store) = store_goal(&goal);
        let result = finalize_goal(&store, &session, &id).unwrap();
        assert_eq!(result.decision().outcome(), GoalFinalizationOutcome::Failed);
        assert_eq!(result.goal().status(), status);
        assert_eq!(result.goal().revision(), 1);
    }
}
#[test]
fn paused_and_blocked_goals_do_not_finalize() {
    for (index, status) in [GoalStatus::Paused, GoalStatus::Blocked]
        .into_iter()
        .enumerate()
    {
        let session = format!("phase9-goal-state-{index}");
        let mandatory = task(true, TaskOperationKind::ReadOnly);
        let task_id = mandatory.id().clone();
        let mut goal = planned_goal(&session, vec![mandatory]);
        complete_task(&mut goal, &task_id);
        if status == GoalStatus::Paused {
            goal.transition_to(GoalStatus::Pausing, NOW).unwrap();
            goal.transition_to(GoalStatus::Paused, NOW).unwrap();
        } else {
            goal.transition_to(GoalStatus::Blocked, NOW).unwrap();
        }
        let id = goal.id().clone();
        let (_dir, store) = store_goal(&goal);
        let result = finalize_goal(&store, &session, &id).unwrap();
        assert_ne!(
            result.decision().outcome(),
            GoalFinalizationOutcome::ReadyToComplete
        );
        assert_eq!(result.goal().status(), status);
        assert_eq!(result.goal().revision(), 1);
    }
}
#[test]
fn not_ready_finalization_does_not_mutate_tasks() {
    let goal = goal_with_mandatory_status("phase9-no-task-mutation", TaskStatus::Ready);
    let tasks_before = goal.tasks().clone();
    let id = goal.id().clone();
    let (_dir, store) = store_goal(&goal);
    let result = finalize_goal(&store, "phase9-no-task-mutation", &id).unwrap();
    assert_eq!(result.goal().tasks(), &tasks_before);
    assert_eq!(
        store
            .load_goal("phase9-no-task-mutation", &id)
            .unwrap()
            .tasks(),
        &tasks_before
    );
    assert_eq!(result.goal().revision(), 1);
}

#[test]
fn goal_result_changes_only_after_durable_finalization() {
    let goal = ready_for_finalization("phase9-result", VerificationOutcome::Passed);
    let id = goal.id().clone();
    let (_dir, store) = store_goal(&goal);
    let session = Session {
        id: "phase9-result".into(),
        cwd: PathBuf::from("/tmp/project"),
        permitted_directories: vec![],
    };
    let args = json!({"session_id": session.id, "goal_id": id.as_str()});
    let before =
        serde_json::to_value(goal_api::goal_result(&args, &session, &store).unwrap()).unwrap();
    assert_eq!(before["status"], "VERIFYING");
    assert_eq!(before["terminal"], false);
    assert_eq!(before["result_state"], "NOT_TERMINAL");
    finalize_goal(&store, "phase9-result", &id).unwrap();
    let after =
        serde_json::to_value(goal_api::goal_result(&args, &session, &store).unwrap()).unwrap();
    assert_eq!(after["status"], "COMPLETED");
    assert_eq!(after["terminal"], true);
    assert_eq!(after["result_state"], "TERMINAL");
    assert_eq!(after["verification"]["final_outcome"], "PASSED");
}
