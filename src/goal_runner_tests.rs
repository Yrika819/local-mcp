use std::collections::VecDeque;

use crate::goal::{GoalId, GoalStatus};
use crate::goal_finalizer::GoalFinalizationOutcome;
use crate::goal_runner::test_support::*;
use crate::goal_runner::{GoalRunLimitError, MAX_FOREGROUND_GOAL_RUN_STEPS};
use crate::scheduler::{
    SchedulerAction, SchedulerAuthority, SchedulerNoActionReason, SchedulerStepOutcome,
    SchedulerStepResult,
};
use crate::task::WorkerKind;

#[derive(Clone)]
struct SchedulerScript {
    result: Result<SchedulerStepResult, RunnerDriverError>,
    next: Option<RunnerGoalState>,
}

#[derive(Clone)]
struct FinalizerScript {
    result: Result<RunnerFinalizerStepResult, RunnerDriverError>,
    next: Option<RunnerGoalState>,
}

struct FakeAuthorities {
    state: RunnerGoalState,
    scheduler: VecDeque<SchedulerScript>,
    finalizer: VecDeque<FinalizerScript>,
    initial_load_error: Option<RunnerDriverError>,
    scheduler_calls: u32,
    finalizer_calls: u32,
    load_calls: u32,
    expected_revisions: Vec<u64>,
}

impl FakeAuthorities {
    fn new(status: GoalStatus, revision: u64) -> Self {
        Self {
            state: RunnerGoalState { revision, status },
            scheduler: VecDeque::new(),
            finalizer: VecDeque::new(),
            initial_load_error: None,
            scheduler_calls: 0,
            finalizer_calls: 0,
            load_calls: 0,
            expected_revisions: Vec::new(),
        }
    }

    fn scheduler_step(
        mut self,
        action: SchedulerAction,
        revision_before: u64,
        revision_after: u64,
        next_status: GoalStatus,
    ) -> Self {
        self.scheduler.push_back(SchedulerScript {
            result: Ok(step_result(
                action,
                revision_before,
                revision_after,
                SchedulerStepOutcome::Applied,
            )),
            next: Some(RunnerGoalState {
                revision: revision_after,
                status: next_status,
            }),
        });
        self
    }

    fn scheduler_outcome(
        mut self,
        action: SchedulerAction,
        revision_before: u64,
        revision_after: u64,
        outcome: SchedulerStepOutcome,
        next: Option<RunnerGoalState>,
    ) -> Self {
        self.scheduler.push_back(SchedulerScript {
            result: Ok(step_result(action, revision_before, revision_after, outcome)),
            next,
        });
        self
    }

    fn scheduler_error(mut self, error: RunnerDriverError) -> Self {
        self.scheduler.push_back(SchedulerScript {
            result: Err(error),
            next: None,
        });
        self
    }

    fn finalizer_step(
        mut self,
        revision_after: u64,
        outcome: GoalFinalizationOutcome,
        next_status: GoalStatus,
    ) -> Self {
        self.finalizer.push_back(FinalizerScript {
            result: Ok(RunnerFinalizerStepResult {
                revision_after,
                outcome,
            }),
            next: Some(RunnerGoalState {
                revision: revision_after,
                status: next_status,
            }),
        });
        self
    }

    fn finalizer_error(mut self, error: RunnerDriverError) -> Self {
        self.finalizer.push_back(FinalizerScript {
            result: Err(error),
            next: None,
        });
        self
    }
}

impl GoalRunnerAuthorities for FakeAuthorities {
    fn load_goal_state(&mut self) -> Result<RunnerGoalState, RunnerDriverError> {
        self.load_calls += 1;
        if self.load_calls == 1 {
            if let Some(error) = self.initial_load_error.take() {
                return Err(error);
            }
        }
        Ok(self.state)
    }

    async fn scheduler_step(
        &mut self,
        expected_revision: u64,
    ) -> Result<SchedulerStepResult, RunnerDriverError> {
        self.scheduler_calls += 1;
        self.expected_revisions.push(expected_revision);
        let script = self.scheduler.pop_front().expect("unexpected Scheduler call");
        if let Some(next) = script.next {
            self.state = next;
        }
        script.result
    }

    fn finalize_goal(
        &mut self,
        expected_revision: u64,
    ) -> Result<RunnerFinalizerStepResult, RunnerDriverError> {
        self.finalizer_calls += 1;
        self.expected_revisions.push(expected_revision);
        let script = self.finalizer.pop_front().expect("unexpected Finalizer call");
        if let Some(next) = script.next {
            self.state = next;
        }
        script.result
    }
}

fn goal_id() -> GoalId {
    GoalId::parse("00000000-0000-4000-8000-000000000010").unwrap()
}

fn limits(max_steps: u32) -> GoalRunLimits {
    GoalRunLimits::new(max_steps).unwrap()
}

fn step_result(
    action: SchedulerAction,
    revision_before: u64,
    revision_after: u64,
    outcome: SchedulerStepOutcome,
) -> SchedulerStepResult {
    SchedulerStepResult {
        action,
        goal_id: goal_id(),
        task_id: None,
        revision_before,
        revision_after,
        outcome,
    }
}

fn trace_actions(result: &crate::goal_runner::GoalRunResult) -> Vec<GoalRunTraceAction> {
    result.trace.iter().map(|entry| entry.action.clone()).collect()
}

#[test]
fn phase10_limits_reject_zero_and_host_maximum_excess() {
    assert_eq!(GoalRunLimits::new(0), Err(GoalRunLimitError::ZeroSteps));
    assert_eq!(
        GoalRunLimits::new(MAX_FOREGROUND_GOAL_RUN_STEPS + 1),
        Err(GoalRunLimitError::ExceedsHostMaximum {
            requested: MAX_FOREGROUND_GOAL_RUN_STEPS + 1,
            maximum: MAX_FOREGROUND_GOAL_RUN_STEPS,
        })
    );
    assert_eq!(
        GoalRunLimits::new(MAX_FOREGROUND_GOAL_RUN_STEPS)
            .unwrap()
            .max_steps(),
        MAX_FOREGROUND_GOAL_RUN_STEPS
    );
}

#[tokio::test]
async fn phase10_terminal_entry_returns_without_authority_calls() {
    for (status, reason) in [
        (GoalStatus::Completed, GoalRunStopReason::Completed),
        (GoalStatus::Failed, GoalRunStopReason::Failed),
        (GoalStatus::Cancelled, GoalRunStopReason::Cancelled),
    ] {
        let mut fake = FakeAuthorities::new(status, 9);
        let result = run_goal_with_authorities(&mut fake, goal_id(), limits(3)).await;
        assert_eq!(result.steps_attempted, 0);
        assert_eq!(result.steps_applied, 0);
        assert_eq!(result.revision_before, Some(9));
        assert_eq!(result.revision_after, Some(9));
        assert_eq!(result.stop_reason, reason);
        assert_eq!(fake.scheduler_calls, 0);
        assert_eq!(fake.finalizer_calls, 0);
    }
}

#[tokio::test]
async fn phase10_paused_and_control_states_return_without_work() {
    for (status, reason) in [
        (GoalStatus::Paused, GoalRunStopReason::Paused),
        (
            GoalStatus::Pausing,
            GoalRunStopReason::ControlState(GoalStatus::Pausing),
        ),
        (
            GoalStatus::Cancelling,
            GoalRunStopReason::ControlState(GoalStatus::Cancelling),
        ),
    ] {
        let mut fake = FakeAuthorities::new(status, 3);
        let result = run_goal_with_authorities(&mut fake, goal_id(), limits(2)).await;
        assert_eq!(result.stop_reason, reason);
        assert_eq!(result.steps_attempted, 0);
        assert_eq!(fake.scheduler_calls + fake.finalizer_calls, 0);
    }
}

#[tokio::test]
async fn phase10_blocked_entry_does_not_spin() {
    let mut fake = FakeAuthorities::new(GoalStatus::Blocked, 4);
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(4)).await;
    assert_eq!(result.stop_reason, GoalRunStopReason::Blocked);
    assert_eq!(result.steps_attempted, 0);
    assert_eq!(fake.scheduler_calls, 0);
    assert_eq!(fake.finalizer_calls, 0);
}

#[tokio::test]
async fn phase10_one_step_budget_runs_exactly_one_authority() {
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 1)
        .scheduler_step(SchedulerAction::VerifyTask, 1, 2, GoalStatus::Running)
        .scheduler_step(SchedulerAction::VerifyGoal, 2, 3, GoalStatus::Verifying);
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(1)).await;
    assert_eq!(result.stop_reason, GoalRunStopReason::StepBudgetExhausted);
    assert_eq!(result.steps_attempted, 1);
    assert_eq!(result.steps_applied, 1);
    assert_eq!(fake.scheduler_calls, 1);
    assert_eq!(fake.finalizer_calls, 0);
    assert_eq!(fake.state.revision, 2);
}

#[tokio::test]
async fn phase10_normal_multistep_success_is_sequential() {
    let mut fake = FakeAuthorities::new(GoalStatus::Planning, 1)
        .scheduler_step(SchedulerAction::PlanInitial, 1, 2, GoalStatus::Running)
        .scheduler_step(SchedulerAction::RunWriter, 2, 3, GoalStatus::Running)
        .scheduler_step(SchedulerAction::VerifyTask, 3, 4, GoalStatus::Running)
        .scheduler_step(SchedulerAction::VerifyGoal, 4, 5, GoalStatus::Verifying)
        .finalizer_step(6, GoalFinalizationOutcome::ReadyToComplete, GoalStatus::Completed);
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(5)).await;
    assert_eq!(result.stop_reason, GoalRunStopReason::Completed);
    assert_eq!(result.steps_attempted, 5);
    assert_eq!(result.steps_applied, 5);
    assert_eq!(result.revision_before, Some(1));
    assert_eq!(result.revision_after, Some(6));
    assert_eq!(
        trace_actions(&result),
        vec![
            GoalRunTraceAction::Scheduler(Some(SchedulerAction::PlanInitial)),
            GoalRunTraceAction::Scheduler(Some(SchedulerAction::RunWriter)),
            GoalRunTraceAction::Scheduler(Some(SchedulerAction::VerifyTask)),
            GoalRunTraceAction::Scheduler(Some(SchedulerAction::VerifyGoal)),
            GoalRunTraceAction::Finalizer,
        ]
    );
    assert_eq!(fake.expected_revisions, vec![1, 2, 3, 4, 5]);
}

#[tokio::test]
async fn phase10_task_and_goal_verifier_are_distinct_steps() {
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 7)
        .scheduler_step(SchedulerAction::VerifyTask, 7, 8, GoalStatus::Running)
        .scheduler_step(SchedulerAction::VerifyGoal, 8, 9, GoalStatus::Verifying);
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(2)).await;
    assert_eq!(result.stop_reason, GoalRunStopReason::StepBudgetExhausted);
    assert_eq!(
        trace_actions(&result),
        vec![
            GoalRunTraceAction::Scheduler(Some(SchedulerAction::VerifyTask)),
            GoalRunTraceAction::Scheduler(Some(SchedulerAction::VerifyGoal)),
        ]
    );
    assert_eq!(fake.finalizer_calls, 0);
}

#[tokio::test]
async fn phase10_goal_verifier_and_finalizer_are_distinct_steps() {
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 10)
        .scheduler_step(SchedulerAction::VerifyGoal, 10, 11, GoalStatus::Verifying)
        .finalizer_step(12, GoalFinalizationOutcome::ReadyToComplete, GoalStatus::Completed);

    let first = run_goal_with_authorities(&mut fake, goal_id(), limits(1)).await;
    assert_eq!(first.stop_reason, GoalRunStopReason::StepBudgetExhausted);
    assert_eq!(fake.finalizer_calls, 0);
    assert_eq!(fake.state.status, GoalStatus::Verifying);

    let second = run_goal_with_authorities(&mut fake, goal_id(), limits(1)).await;
    assert_eq!(second.stop_reason, GoalRunStopReason::Completed);
    assert_eq!(second.steps_attempted, 1);
    assert_eq!(fake.scheduler_calls, 1);
    assert_eq!(fake.finalizer_calls, 1);
}

#[tokio::test]
async fn phase10_verifying_entry_uses_finalizer_directly_as_separate_authority() {
    let mut fake = FakeAuthorities::new(GoalStatus::Verifying, 20)
        .finalizer_step(21, GoalFinalizationOutcome::ReadyToComplete, GoalStatus::Completed);
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(1)).await;
    assert_eq!(result.stop_reason, GoalRunStopReason::Completed);
    assert_eq!(fake.scheduler_calls, 0);
    assert_eq!(fake.finalizer_calls, 1);
    assert_eq!(trace_actions(&result), vec![GoalRunTraceAction::Finalizer]);
}

#[tokio::test]
async fn phase10_needs_replan_lifecycle_never_collapses_actions() {
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 1)
        .scheduler_step(SchedulerAction::VerifyTask, 1, 2, GoalStatus::Running)
        .scheduler_step(SchedulerAction::Replan, 2, 3, GoalStatus::Running)
        .scheduler_step(SchedulerAction::RunWriter, 3, 4, GoalStatus::Running);
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(3)).await;
    assert_eq!(result.stop_reason, GoalRunStopReason::StepBudgetExhausted);
    assert_eq!(
        trace_actions(&result),
        vec![
            GoalRunTraceAction::Scheduler(Some(SchedulerAction::VerifyTask)),
            GoalRunTraceAction::Scheduler(Some(SchedulerAction::Replan)),
            GoalRunTraceAction::Scheduler(Some(SchedulerAction::RunWriter)),
        ]
    );
}

#[tokio::test]
async fn phase10_failed_goal_verification_stops_without_finalizer() {
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 2)
        .scheduler_step(SchedulerAction::VerifyGoal, 2, 3, GoalStatus::Failed);
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(3)).await;
    assert_eq!(result.stop_reason, GoalRunStopReason::Failed);
    assert_eq!(result.terminal_status, Some(GoalStatus::Failed));
    assert_eq!(fake.scheduler_calls, 1);
    assert_eq!(fake.finalizer_calls, 0);
}

#[tokio::test]
async fn phase10_indeterminate_goal_verification_stops_blocked() {
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 2)
        .scheduler_step(SchedulerAction::VerifyGoal, 2, 3, GoalStatus::Blocked);
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(3)).await;
    assert_eq!(result.stop_reason, GoalRunStopReason::Blocked);
    assert_eq!(fake.scheduler_calls, 1);
    assert_eq!(fake.finalizer_calls, 0);
}

#[tokio::test]
async fn phase10_follows_scheduler_priority_without_task_selection() {
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 4)
        .scheduler_step(SchedulerAction::VerifyGoal, 4, 5, GoalStatus::Verifying)
        .finalizer_step(6, GoalFinalizationOutcome::ReadyToComplete, GoalStatus::Completed);
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(2)).await;
    assert_eq!(result.stop_reason, GoalRunStopReason::Completed);
    assert_eq!(
        trace_actions(&result),
        vec![
            GoalRunTraceAction::Scheduler(Some(SchedulerAction::VerifyGoal)),
            GoalRunTraceAction::Finalizer,
        ]
    );
}

#[tokio::test]
async fn phase10_unsupported_worker_stops_after_one_scheduler_call() {
    let worker = WorkerKind::LocalOperation;
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 1).scheduler_outcome(
        SchedulerAction::UnsupportedWorker,
        1,
        1,
        SchedulerStepOutcome::UnsupportedWorker(worker),
        None,
    );
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(5)).await;
    assert_eq!(result.stop_reason, GoalRunStopReason::UnsupportedWorker(worker));
    assert_eq!(result.steps_attempted, 1);
    assert_eq!(fake.scheduler_calls, 1);
}

#[tokio::test]
async fn phase10_no_action_stops_without_second_call() {
    let reason = SchedulerNoActionReason::NoEligibleAction;
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 1).scheduler_outcome(
        SchedulerAction::NoAction,
        1,
        1,
        SchedulerStepOutcome::NoAction(reason.clone()),
        None,
    );
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(5)).await;
    assert_eq!(result.stop_reason, GoalRunStopReason::NoAction(reason));
    assert_eq!(fake.scheduler_calls, 1);
    assert_eq!(result.trace.len(), 1);
}

#[tokio::test]
async fn phase10_scheduler_revision_conflict_stops_without_retry() {
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 5).scheduler_outcome(
        SchedulerAction::NoAction,
        5,
        6,
        SchedulerStepOutcome::RevisionConflict {
            expected: 5,
            actual: 6,
        },
        Some(RunnerGoalState {
            revision: 6,
            status: GoalStatus::Running,
        }),
    );
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(5)).await;
    assert_eq!(
        result.stop_reason,
        GoalRunStopReason::RevisionConflict {
            expected: 5,
            actual: 6,
        }
    );
    assert_eq!(fake.scheduler_calls, 1);
}

#[tokio::test]
async fn phase10_finalizer_revision_conflict_stops_without_retry() {
    let mut fake = FakeAuthorities::new(GoalStatus::Verifying, 8).finalizer_error(
        RunnerDriverError::RevisionConflict {
            expected: 8,
            actual: 9,
        },
    );
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(4)).await;
    assert_eq!(
        result.stop_reason,
        GoalRunStopReason::RevisionConflict {
            expected: 8,
            actual: 9,
        }
    );
    assert_eq!(fake.finalizer_calls, 1);
    assert_eq!(fake.scheduler_calls, 0);
}

#[tokio::test]
async fn phase10_lower_authority_error_stops_immediately() {
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 1).scheduler_outcome(
        SchedulerAction::RunWriter,
        1,
        1,
        SchedulerStepOutcome::LowerAuthorityError {
            authority: SchedulerAuthority::Writer,
            detail: "injected writer failure".into(),
        },
        None,
    );
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(4)).await;
    assert_eq!(
        result.stop_reason,
        GoalRunStopReason::LowerAuthorityError {
            authority: GoalRunnerAuthority::Writer,
            detail: "injected writer failure".into(),
        }
    );
    assert_eq!(fake.scheduler_calls, 1);
}

#[tokio::test]
async fn phase10_driver_error_is_not_retried() {
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 1).scheduler_error(
        RunnerDriverError::LowerAuthority {
            authority: GoalRunnerAuthority::Scheduler,
            detail: "injected scheduler error".into(),
        },
    );
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(4)).await;
    assert_eq!(result.steps_attempted, 1);
    assert_eq!(fake.scheduler_calls, 1);
    assert!(matches!(
        result.stop_reason,
        GoalRunStopReason::LowerAuthorityError {
            authority: GoalRunnerAuthority::Scheduler,
            ..
        }
    ));
}

#[tokio::test]
async fn phase10_applied_without_revision_progress_is_rejected() {
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 3).scheduler_outcome(
        SchedulerAction::VerifyTask,
        3,
        3,
        SchedulerStepOutcome::Applied,
        None,
    );
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(3)).await;
    assert!(matches!(
        result.stop_reason,
        GoalRunStopReason::NoProgress {
            action: GoalRunTraceAction::Scheduler(Some(SchedulerAction::VerifyTask))
        }
    ));
    assert_eq!(result.steps_applied, 0);
    assert_eq!(fake.scheduler_calls, 1);
}

#[tokio::test]
async fn phase10_finalizer_without_revision_progress_is_rejected() {
    let mut fake = FakeAuthorities::new(GoalStatus::Verifying, 3)
        .finalizer_step(3, GoalFinalizationOutcome::ReadyToComplete, GoalStatus::Verifying);
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(3)).await;
    assert!(matches!(
        result.stop_reason,
        GoalRunStopReason::NoProgress {
            action: GoalRunTraceAction::Finalizer
        }
    ));
    assert_eq!(result.steps_applied, 0);
    assert_eq!(fake.finalizer_calls, 1);
}

#[tokio::test]
async fn phase10_exact_step_budget_bounds_trace_and_preserves_progress() {
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 1)
        .scheduler_step(SchedulerAction::RunWriter, 1, 2, GoalStatus::Running)
        .scheduler_step(SchedulerAction::VerifyTask, 2, 3, GoalStatus::Running)
        .scheduler_step(SchedulerAction::Replan, 3, 4, GoalStatus::Running)
        .scheduler_step(SchedulerAction::RunWriter, 4, 5, GoalStatus::Running);
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(3)).await;
    assert_eq!(result.stop_reason, GoalRunStopReason::StepBudgetExhausted);
    assert_eq!(result.steps_attempted, 3);
    assert_eq!(result.steps_applied, 3);
    assert_eq!(result.trace.len(), 3);
    assert_eq!(result.revision_after, Some(4));
    assert_eq!(fake.scheduler_calls, 3);
    assert_eq!(fake.state.revision, 4);
}

#[tokio::test]
async fn phase10_trace_is_ordered_deterministic_and_bounded() {
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 1)
        .scheduler_step(SchedulerAction::RunWriter, 1, 2, GoalStatus::Running)
        .scheduler_step(SchedulerAction::VerifyTask, 2, 3, GoalStatus::Running);
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(2)).await;
    assert_eq!(result.trace.len(), 2);
    assert_eq!(result.trace[0].step_index, 1);
    assert_eq!(result.trace[1].step_index, 2);
    assert_eq!(result.trace[0].revision_before, 1);
    assert_eq!(result.trace[0].revision_after, 2);
    assert_eq!(result.trace[1].revision_before, 2);
    assert_eq!(result.trace[1].revision_after, 3);
    assert!(result.trace.len() <= limits(2).max_steps() as usize);
}

#[tokio::test]
async fn phase10_completed_reentry_is_idempotent() {
    let mut fake = FakeAuthorities::new(GoalStatus::Verifying, 1)
        .finalizer_step(2, GoalFinalizationOutcome::ReadyToComplete, GoalStatus::Completed);
    let first = run_goal_with_authorities(&mut fake, goal_id(), limits(2)).await;
    assert_eq!(first.stop_reason, GoalRunStopReason::Completed);
    assert_eq!(fake.finalizer_calls, 1);

    let second = run_goal_with_authorities(&mut fake, goal_id(), limits(2)).await;
    assert_eq!(second.stop_reason, GoalRunStopReason::Completed);
    assert_eq!(second.steps_attempted, 0);
    assert_eq!(second.revision_before, Some(2));
    assert_eq!(second.revision_after, Some(2));
    assert_eq!(fake.finalizer_calls, 1);
    assert_eq!(fake.scheduler_calls, 0);
}

#[tokio::test]
async fn phase10_finalization_not_ready_stops_without_spin() {
    let mut fake = FakeAuthorities::new(GoalStatus::Verifying, 3)
        .finalizer_step(3, GoalFinalizationOutcome::NotReady, GoalStatus::Verifying);
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(4)).await;
    assert_eq!(
        result.stop_reason,
        GoalRunStopReason::FinalizationNotReady(GoalFinalizationOutcome::NotReady)
    );
    assert_eq!(fake.finalizer_calls, 1);
    assert_eq!(fake.scheduler_calls, 0);
}

#[tokio::test]
async fn phase10_initial_load_error_is_typed_and_dispatches_nothing() {
    let mut fake = FakeAuthorities::new(GoalStatus::Running, 1);
    fake.initial_load_error = Some(RunnerDriverError::LowerAuthority {
        authority: GoalRunnerAuthority::Store,
        detail: "schema load rejected".into(),
    });
    let result = run_goal_with_authorities(&mut fake, goal_id(), limits(2)).await;
    assert!(matches!(
        result.stop_reason,
        GoalRunStopReason::LowerAuthorityError {
            authority: GoalRunnerAuthority::Store,
            ..
        }
    ));
    assert_eq!(result.steps_attempted, 0);
    assert_eq!(result.revision_before, None);
    assert_eq!(fake.scheduler_calls + fake.finalizer_calls, 0);
}

#[test]
fn phase10_static_authority_bypass_and_background_absence() {
    let source = include_str!("goal_runner.rs");
    for forbidden in [
        "planner::materialize",
        "writer::run_writer_attempt",
        "verifier::verify_task",
        "replanner::materialize",
        "goal_verifier::verify_goal",
        "enter_verifying",
        "complete_from_finalizer",
        "GoalFinalizationAuthority",
        "tokio::spawn",
        "thread::spawn",
        "join_all",
        "loop {",
    ] {
        assert!(!source.contains(forbidden), "forbidden Runner token: {forbidden}");
    }
    assert!(source.contains("scheduler::scheduler_step"));
    assert!(source.contains("goal_finalizer::finalize_goal_at_revision"));
}

#[test]
fn phase11_public_mcp_surface_authorizes_only_goal_run_runner_call_site() {
    let mcp = include_str!("mcp.rs");
    assert!(mcp.contains("goal_runner::run_goal_foreground"));
    assert!(mcp.contains("goal_run"));
    assert!(mcp.contains("run_goal_foreground"));
    assert!(!mcp.contains("goal_tick"));
    assert!(!mcp.contains("scheduler_step"));
}
