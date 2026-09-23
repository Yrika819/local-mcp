use crate::config;
use crate::goal::{GoalId, GoalStatus};
use crate::goal_finalizer::{self, GoalFinalizationOutcome, GoalFinalizerError};
use crate::orchestrator_error::OrchestratorError;
use crate::planner::PlannerBackend;
use crate::readonly_worker::ReadonlyBackend;
use crate::replanner::ReplannerBackend;
use crate::scheduler::{
    self, SchedulerAction, SchedulerAuthority, SchedulerNoActionReason, SchedulerStepOutcome,
    SchedulerStepResult,
};
use crate::task::{TaskId, WorkerKind};
use crate::task_store::TaskStore;
use crate::writer::{ReviewerBackend, WriterBackend};

pub(crate) const MAX_FOREGROUND_GOAL_RUN_STEPS: u32 = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GoalRunLimits {
    max_steps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GoalRunLimitError {
    ZeroSteps,
    ExceedsHostMaximum { requested: u32, maximum: u32 },
}

impl std::fmt::Display for GoalRunLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroSteps => write!(f, "foreground Goal run max_steps must be greater than zero"),
            Self::ExceedsHostMaximum { requested, maximum } => write!(
                f,
                "foreground Goal run max_steps {requested} exceeds host maximum {maximum}"
            ),
        }
    }
}

impl std::error::Error for GoalRunLimitError {}

impl GoalRunLimits {
    pub(crate) fn new(max_steps: u32) -> Result<Self, GoalRunLimitError> {
        if max_steps == 0 {
            return Err(GoalRunLimitError::ZeroSteps);
        }
        if max_steps > MAX_FOREGROUND_GOAL_RUN_STEPS {
            return Err(GoalRunLimitError::ExceedsHostMaximum {
                requested: max_steps,
                maximum: MAX_FOREGROUND_GOAL_RUN_STEPS,
            });
        }
        Ok(Self { max_steps })
    }

    pub(crate) fn max_steps(self) -> u32 {
        self.max_steps
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GoalRunnerAuthority {
    Store,
    Scheduler,
    Planner,
    Readonly,
    Writer,
    Verifier,
    GoalVerifier,
    Replanner,
    Finalizer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GoalRunStopReason {
    Completed,
    Failed,
    Cancelled,
    Paused,
    ControlState(GoalStatus),
    Blocked,
    NoAction(SchedulerNoActionReason),
    UnsupportedWorker(WorkerKind),
    RevisionConflict {
        expected: u64,
        actual: u64,
    },
    LowerAuthorityError {
        authority: GoalRunnerAuthority,
        detail: String,
    },
    StepBudgetExhausted,
    NoProgress {
        action: GoalRunTraceAction,
    },
    FinalizationNotReady(GoalFinalizationOutcome),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GoalRunTraceAction {
    Scheduler(Option<SchedulerAction>),
    Finalizer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GoalRunTraceOutcome {
    Applied,
    NoAction(SchedulerNoActionReason),
    UnsupportedWorker(WorkerKind),
    RevisionConflict,
    LowerAuthorityError,
    Finalization(GoalFinalizationOutcome),
    NoProgress,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GoalRunTraceEntry {
    pub(crate) step_index: u32,
    pub(crate) action: GoalRunTraceAction,
    pub(crate) task_id: Option<TaskId>,
    pub(crate) revision_before: u64,
    pub(crate) revision_after: u64,
    pub(crate) outcome: GoalRunTraceOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GoalRunResult {
    pub(crate) goal_id: GoalId,
    pub(crate) revision_before: Option<u64>,
    pub(crate) revision_after: Option<u64>,
    pub(crate) steps_attempted: u32,
    pub(crate) steps_applied: u32,
    pub(crate) terminal_status: Option<GoalStatus>,
    pub(crate) stop_reason: GoalRunStopReason,
    pub(crate) trace: Vec<GoalRunTraceEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RunnerGoalState {
    pub(crate) revision: u64,
    pub(crate) status: GoalStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RunnerFinalizerStepResult {
    pub(crate) revision_after: u64,
    pub(crate) outcome: GoalFinalizationOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RunnerDriverError {
    RevisionConflict {
        expected: u64,
        actual: u64,
    },
    LowerAuthority {
        authority: GoalRunnerAuthority,
        detail: String,
    },
}

pub(crate) trait GoalRunnerAuthorities {
    fn load_goal_state(&mut self) -> Result<RunnerGoalState, RunnerDriverError>;

    async fn scheduler_step(
        &mut self,
        expected_revision: u64,
    ) -> Result<SchedulerStepResult, RunnerDriverError>;

    fn finalize_goal(
        &mut self,
        expected_revision: u64,
    ) -> Result<RunnerFinalizerStepResult, RunnerDriverError>;
}

struct ProductionRunnerAuthorities<'a, P, RB, W, R, RP> {
    store: &'a TaskStore,
    session: &'a config::Session,
    goal_id: &'a GoalId,
    planner_backend: &'a P,
    readonly_backend: &'a RB,
    writer_backend: &'a W,
    reviewer_backend: &'a R,
    replanner_backend: &'a RP,
}

impl<P, RB, W, R, RP> GoalRunnerAuthorities for ProductionRunnerAuthorities<'_, P, RB, W, R, RP>
where
    P: PlannerBackend,
    RB: ReadonlyBackend,
    W: WriterBackend,
    R: ReviewerBackend,
    RP: ReplannerBackend,
{
    fn load_goal_state(&mut self) -> Result<RunnerGoalState, RunnerDriverError> {
        self.store
            .load_goal(&self.session.id, self.goal_id)
            .map(|goal| RunnerGoalState {
                revision: goal.revision(),
                status: goal.status(),
            })
            .map_err(|error| RunnerDriverError::LowerAuthority {
                authority: GoalRunnerAuthority::Store,
                detail: error.to_string(),
            })
    }

    async fn scheduler_step(
        &mut self,
        expected_revision: u64,
    ) -> Result<SchedulerStepResult, RunnerDriverError> {
        scheduler::scheduler_step(
            self.store,
            self.session,
            self.goal_id,
            expected_revision,
            self.planner_backend,
            self.readonly_backend,
            self.writer_backend,
            self.reviewer_backend,
            self.replanner_backend,
        )
        .await
        .map_err(|error| RunnerDriverError::LowerAuthority {
            authority: GoalRunnerAuthority::Scheduler,
            detail: error.to_string(),
        })
    }

    fn finalize_goal(
        &mut self,
        expected_revision: u64,
    ) -> Result<RunnerFinalizerStepResult, RunnerDriverError> {
        goal_finalizer::finalize_goal_at_revision(
            self.store,
            &self.session.id,
            self.goal_id,
            expected_revision,
        )
        .map(|result| RunnerFinalizerStepResult {
            revision_after: result.goal().revision(),
            outcome: result.decision().outcome(),
        })
        .map_err(map_finalizer_error)
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Runner authority backends and execution limits remain explicit at this frozen boundary."
)]
pub(crate) async fn run_goal_foreground<P, RB, W, R, RP>(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    limits: GoalRunLimits,
    planner_backend: &P,
    readonly_backend: &RB,
    writer_backend: &W,
    reviewer_backend: &R,
    replanner_backend: &RP,
) -> GoalRunResult
where
    P: PlannerBackend,
    RB: ReadonlyBackend,
    W: WriterBackend,
    R: ReviewerBackend,
    RP: ReplannerBackend,
{
    let mut authorities = ProductionRunnerAuthorities {
        store,
        session,
        goal_id,
        planner_backend,
        readonly_backend,
        writer_backend,
        reviewer_backend,
        replanner_backend,
    };
    run_goal_with_authorities(&mut authorities, goal_id.clone(), limits).await
}

pub(crate) async fn run_goal_with_authorities<A>(
    authorities: &mut A,
    goal_id: GoalId,
    limits: GoalRunLimits,
) -> GoalRunResult
where
    A: GoalRunnerAuthorities,
{
    let initial = match authorities.load_goal_state() {
        Ok(state) => state,
        Err(error) => return result_for_initial_error(goal_id, error),
    };
    let revision_before = initial.revision;
    let mut current = initial;
    let mut trace = Vec::with_capacity(limits.max_steps() as usize);
    let mut steps_applied = 0_u32;

    if let Some(stop_reason) = stop_for_state(current.status) {
        return build_result(
            goal_id,
            Some(revision_before),
            Some(current.revision),
            steps_applied,
            stop_reason,
            current.status,
            trace,
        );
    }

    for step_index in 0..limits.max_steps() {
        let step_number = step_index + 1;

        if current.status == GoalStatus::Verifying {
            let before = current.revision;
            match authorities.finalize_goal(before) {
                Err(error) => {
                    let (after, stop_reason) = stop_from_driver_error(before, &error);
                    trace.push(GoalRunTraceEntry {
                        step_index: step_number,
                        action: GoalRunTraceAction::Finalizer,
                        task_id: None,
                        revision_before: before,
                        revision_after: after,
                        outcome: trace_outcome_from_driver_error(&error),
                    });
                    return build_result(
                        goal_id,
                        Some(revision_before),
                        Some(after),
                        steps_applied,
                        stop_reason,
                        current.status,
                        trace,
                    );
                }
                Ok(step) => {
                    let action = GoalRunTraceAction::Finalizer;
                    let outcome = step.outcome;
                    let after = step.revision_after;
                    match outcome {
                        GoalFinalizationOutcome::ReadyToComplete => {
                            if after <= before {
                                trace.push(GoalRunTraceEntry {
                                    step_index: step_number,
                                    action: action.clone(),
                                    task_id: None,
                                    revision_before: before,
                                    revision_after: after,
                                    outcome: GoalRunTraceOutcome::NoProgress,
                                });
                                return build_result(
                                    goal_id,
                                    Some(revision_before),
                                    Some(after),
                                    steps_applied,
                                    GoalRunStopReason::NoProgress { action },
                                    current.status,
                                    trace,
                                );
                            }
                            steps_applied += 1;
                            trace.push(GoalRunTraceEntry {
                                step_index: step_number,
                                action,
                                task_id: None,
                                revision_before: before,
                                revision_after: after,
                                outcome: GoalRunTraceOutcome::Finalization(outcome),
                            });
                        }
                        GoalFinalizationOutcome::AlreadyCompleted => {
                            trace.push(GoalRunTraceEntry {
                                step_index: step_number,
                                action,
                                task_id: None,
                                revision_before: before,
                                revision_after: after,
                                outcome: GoalRunTraceOutcome::Finalization(outcome),
                            });
                            return build_result(
                                goal_id,
                                Some(revision_before),
                                Some(after),
                                steps_applied,
                                GoalRunStopReason::Completed,
                                GoalStatus::Completed,
                                trace,
                            );
                        }
                        GoalFinalizationOutcome::Blocked
                        | GoalFinalizationOutcome::Failed
                        | GoalFinalizationOutcome::NotReady => {
                            trace.push(GoalRunTraceEntry {
                                step_index: step_number,
                                action,
                                task_id: None,
                                revision_before: before,
                                revision_after: after,
                                outcome: GoalRunTraceOutcome::Finalization(outcome),
                            });
                            return build_result(
                                goal_id,
                                Some(revision_before),
                                Some(after),
                                steps_applied,
                                GoalRunStopReason::FinalizationNotReady(outcome),
                                current.status,
                                trace,
                            );
                        }
                    }
                }
            }
        } else {
            let before = current.revision;
            let step = match authorities.scheduler_step(before).await {
                Ok(step) => step,
                Err(error) => {
                    let (after, stop_reason) = stop_from_driver_error(before, &error);
                    trace.push(GoalRunTraceEntry {
                        step_index: step_number,
                        action: GoalRunTraceAction::Scheduler(None),
                        task_id: None,
                        revision_before: before,
                        revision_after: after,
                        outcome: trace_outcome_from_driver_error(&error),
                    });
                    return build_result(
                        goal_id,
                        Some(revision_before),
                        Some(after),
                        steps_applied,
                        stop_reason,
                        current.status,
                        trace,
                    );
                }
            };
            let action = GoalRunTraceAction::Scheduler(Some(step.action));

            if step.revision_before != before {
                trace.push(GoalRunTraceEntry {
                    step_index: step_number,
                    action: action.clone(),
                    task_id: step.task_id.clone(),
                    revision_before: before,
                    revision_after: step.revision_after,
                    outcome: GoalRunTraceOutcome::LowerAuthorityError,
                });
                return build_result(
                    goal_id,
                    Some(revision_before),
                    Some(step.revision_after),
                    steps_applied,
                    GoalRunStopReason::LowerAuthorityError {
                        authority: GoalRunnerAuthority::Scheduler,
                        detail: format!(
                            "scheduler result revision_before {} did not match requested revision {before}",
                            step.revision_before
                        ),
                    },
                    current.status,
                    trace,
                );
            }

            if matches!(
                &step.outcome,
                SchedulerStepOutcome::NoAction(_) | SchedulerStepOutcome::UnsupportedWorker(_)
            ) && step.revision_after != before
            {
                trace.push(GoalRunTraceEntry {
                    step_index: step_number,
                    action,
                    task_id: step.task_id.clone(),
                    revision_before: before,
                    revision_after: step.revision_after,
                    outcome: GoalRunTraceOutcome::RevisionConflict,
                });
                return build_result(
                    goal_id,
                    Some(revision_before),
                    Some(step.revision_after),
                    steps_applied,
                    GoalRunStopReason::RevisionConflict {
                        expected: before,
                        actual: step.revision_after,
                    },
                    current.status,
                    trace,
                );
            }

            match &step.outcome {
                SchedulerStepOutcome::Applied => {
                    if step.revision_after <= before {
                        trace.push(GoalRunTraceEntry {
                            step_index: step_number,
                            action: action.clone(),
                            task_id: step.task_id.clone(),
                            revision_before: before,
                            revision_after: step.revision_after,
                            outcome: GoalRunTraceOutcome::NoProgress,
                        });
                        return build_result(
                            goal_id,
                            Some(revision_before),
                            Some(step.revision_after),
                            steps_applied,
                            GoalRunStopReason::NoProgress { action },
                            current.status,
                            trace,
                        );
                    }
                    steps_applied += 1;
                    trace.push(GoalRunTraceEntry {
                        step_index: step_number,
                        action,
                        task_id: step.task_id.clone(),
                        revision_before: before,
                        revision_after: step.revision_after,
                        outcome: GoalRunTraceOutcome::Applied,
                    });
                }
                SchedulerStepOutcome::NoAction(reason) => {
                    trace.push(GoalRunTraceEntry {
                        step_index: step_number,
                        action,
                        task_id: step.task_id.clone(),
                        revision_before: before,
                        revision_after: step.revision_after,
                        outcome: GoalRunTraceOutcome::NoAction(reason.clone()),
                    });
                    return build_result(
                        goal_id,
                        Some(revision_before),
                        Some(step.revision_after),
                        steps_applied,
                        stop_from_no_action(reason),
                        current.status,
                        trace,
                    );
                }
                SchedulerStepOutcome::UnsupportedWorker(worker) => {
                    trace.push(GoalRunTraceEntry {
                        step_index: step_number,
                        action,
                        task_id: step.task_id.clone(),
                        revision_before: before,
                        revision_after: step.revision_after,
                        outcome: GoalRunTraceOutcome::UnsupportedWorker(*worker),
                    });
                    return build_result(
                        goal_id,
                        Some(revision_before),
                        Some(step.revision_after),
                        steps_applied,
                        GoalRunStopReason::UnsupportedWorker(*worker),
                        current.status,
                        trace,
                    );
                }
                SchedulerStepOutcome::RevisionConflict { expected, actual } => {
                    trace.push(GoalRunTraceEntry {
                        step_index: step_number,
                        action,
                        task_id: step.task_id.clone(),
                        revision_before: before,
                        revision_after: step.revision_after,
                        outcome: GoalRunTraceOutcome::RevisionConflict,
                    });
                    return build_result(
                        goal_id,
                        Some(revision_before),
                        Some(*actual),
                        steps_applied,
                        GoalRunStopReason::RevisionConflict {
                            expected: *expected,
                            actual: *actual,
                        },
                        current.status,
                        trace,
                    );
                }
                SchedulerStepOutcome::LowerAuthorityError { authority, detail } => {
                    let authority = runner_authority_from_scheduler(*authority);
                    trace.push(GoalRunTraceEntry {
                        step_index: step_number,
                        action,
                        task_id: step.task_id.clone(),
                        revision_before: before,
                        revision_after: step.revision_after,
                        outcome: GoalRunTraceOutcome::LowerAuthorityError,
                    });
                    return build_result(
                        goal_id,
                        Some(revision_before),
                        Some(step.revision_after),
                        steps_applied,
                        GoalRunStopReason::LowerAuthorityError {
                            authority,
                            detail: detail.clone(),
                        },
                        current.status,
                        trace,
                    );
                }
            }
        }

        current = match authorities.load_goal_state() {
            Ok(state) => state,
            Err(error) => {
                let (after, stop_reason) = stop_from_driver_error(current.revision, &error);
                return build_result(
                    goal_id,
                    Some(revision_before),
                    Some(after),
                    steps_applied,
                    stop_reason,
                    current.status,
                    trace,
                );
            }
        };

        let recorded_after = trace
            .last()
            .map(|entry| entry.revision_after)
            .unwrap_or(current.revision);
        if current.revision != recorded_after {
            return build_result(
                goal_id,
                Some(revision_before),
                Some(current.revision),
                steps_applied,
                GoalRunStopReason::RevisionConflict {
                    expected: recorded_after,
                    actual: current.revision,
                },
                current.status,
                trace,
            );
        }

        if trace.last().is_some_and(|entry| {
            matches!(
                (&entry.action, &entry.outcome),
                (
                    GoalRunTraceAction::Finalizer,
                    GoalRunTraceOutcome::Finalization(GoalFinalizationOutcome::ReadyToComplete)
                )
            )
        }) && current.status != GoalStatus::Completed
        {
            return build_result(
                goal_id,
                Some(revision_before),
                Some(current.revision),
                steps_applied,
                GoalRunStopReason::LowerAuthorityError {
                    authority: GoalRunnerAuthority::Finalizer,
                    detail:
                        "Phase 9 Finalizer reported ReadyToComplete without durable COMPLETED state"
                            .to_owned(),
                },
                current.status,
                trace,
            );
        }

        if let Some(stop_reason) = stop_for_state(current.status) {
            return build_result(
                goal_id,
                Some(revision_before),
                Some(current.revision),
                steps_applied,
                stop_reason,
                current.status,
                trace,
            );
        }
    }

    build_result(
        goal_id,
        Some(revision_before),
        Some(current.revision),
        steps_applied,
        GoalRunStopReason::StepBudgetExhausted,
        current.status,
        trace,
    )
}

fn result_for_initial_error(goal_id: GoalId, error: RunnerDriverError) -> GoalRunResult {
    let stop_reason = match error {
        RunnerDriverError::RevisionConflict { expected, actual } => {
            GoalRunStopReason::RevisionConflict { expected, actual }
        }
        RunnerDriverError::LowerAuthority { authority, detail } => {
            GoalRunStopReason::LowerAuthorityError { authority, detail }
        }
    };
    GoalRunResult {
        goal_id,
        revision_before: None,
        revision_after: None,
        steps_attempted: 0,
        steps_applied: 0,
        terminal_status: None,
        stop_reason,
        trace: Vec::new(),
    }
}

fn build_result(
    goal_id: GoalId,
    revision_before: Option<u64>,
    revision_after: Option<u64>,
    steps_applied: u32,
    stop_reason: GoalRunStopReason,
    current_status: GoalStatus,
    trace: Vec<GoalRunTraceEntry>,
) -> GoalRunResult {
    GoalRunResult {
        goal_id,
        revision_before,
        revision_after,
        steps_attempted: trace.len() as u32,
        steps_applied,
        terminal_status: current_status.is_terminal().then_some(current_status),
        stop_reason,
        trace,
    }
}

fn stop_for_state(status: GoalStatus) -> Option<GoalRunStopReason> {
    match status {
        GoalStatus::Completed => Some(GoalRunStopReason::Completed),
        GoalStatus::Failed => Some(GoalRunStopReason::Failed),
        GoalStatus::Cancelled => Some(GoalRunStopReason::Cancelled),
        GoalStatus::Paused => Some(GoalRunStopReason::Paused),
        GoalStatus::Pausing | GoalStatus::Cancelling => {
            Some(GoalRunStopReason::ControlState(status))
        }
        GoalStatus::Blocked => Some(GoalRunStopReason::Blocked),
        GoalStatus::Planning
        | GoalStatus::Running
        | GoalStatus::Replanning
        | GoalStatus::Verifying => None,
    }
}

fn stop_from_no_action(reason: &SchedulerNoActionReason) -> GoalRunStopReason {
    match reason {
        SchedulerNoActionReason::Paused => GoalRunStopReason::Paused,
        SchedulerNoActionReason::Pausing => GoalRunStopReason::ControlState(GoalStatus::Pausing),
        SchedulerNoActionReason::Cancelling => {
            GoalRunStopReason::ControlState(GoalStatus::Cancelling)
        }
        SchedulerNoActionReason::TerminalGoal(GoalStatus::Completed) => {
            GoalRunStopReason::Completed
        }
        SchedulerNoActionReason::TerminalGoal(GoalStatus::Failed) => GoalRunStopReason::Failed,
        SchedulerNoActionReason::TerminalGoal(GoalStatus::Cancelled) => {
            GoalRunStopReason::Cancelled
        }
        SchedulerNoActionReason::GoalBlocked => GoalRunStopReason::Blocked,
        _ => GoalRunStopReason::NoAction(reason.clone()),
    }
}

fn runner_authority_from_scheduler(authority: SchedulerAuthority) -> GoalRunnerAuthority {
    match authority {
        SchedulerAuthority::Planner => GoalRunnerAuthority::Planner,
        SchedulerAuthority::Readonly => GoalRunnerAuthority::Readonly,
        SchedulerAuthority::Writer => GoalRunnerAuthority::Writer,
        SchedulerAuthority::Verifier => GoalRunnerAuthority::Verifier,
        SchedulerAuthority::GoalVerifier => GoalRunnerAuthority::GoalVerifier,
        SchedulerAuthority::Replanner => GoalRunnerAuthority::Replanner,
    }
}

fn map_finalizer_error(error: GoalFinalizerError) -> RunnerDriverError {
    match error {
        GoalFinalizerError::Store(OrchestratorError::RevisionConflict { expected, actual }) => {
            RunnerDriverError::RevisionConflict { expected, actual }
        }
        other => RunnerDriverError::LowerAuthority {
            authority: GoalRunnerAuthority::Finalizer,
            detail: other.to_string(),
        },
    }
}

fn stop_from_driver_error(
    fallback_revision: u64,
    error: &RunnerDriverError,
) -> (u64, GoalRunStopReason) {
    match error {
        RunnerDriverError::RevisionConflict { expected, actual } => (
            *actual,
            GoalRunStopReason::RevisionConflict {
                expected: *expected,
                actual: *actual,
            },
        ),
        RunnerDriverError::LowerAuthority { authority, detail } => (
            fallback_revision,
            GoalRunStopReason::LowerAuthorityError {
                authority: *authority,
                detail: detail.clone(),
            },
        ),
    }
}

fn trace_outcome_from_driver_error(error: &RunnerDriverError) -> GoalRunTraceOutcome {
    match error {
        RunnerDriverError::RevisionConflict { .. } => GoalRunTraceOutcome::RevisionConflict,
        RunnerDriverError::LowerAuthority { .. } => GoalRunTraceOutcome::LowerAuthorityError,
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) use super::{
        GoalRunLimits, GoalRunStopReason, GoalRunTraceAction, GoalRunnerAuthorities,
        GoalRunnerAuthority, RunnerDriverError, RunnerFinalizerStepResult, RunnerGoalState,
        run_goal_with_authorities,
    };
}
