use crate::config;
use crate::goal::{Goal, GoalId, GoalStatus};
use crate::goal_verifier::{self, GoalVerifierError};
use crate::orchestrator_error::OrchestratorError;
use crate::planner::{self, PlannerBackend, PlannerError};
use crate::readonly_worker::{self, ReadonlyBackend, ReadonlyError};
use crate::replanner::{self, ReplannerBackend, ReplannerError};
use crate::task::{Task, TaskId, TaskStatus, WorkerKind};
use crate::task_store::TaskStore;
use crate::verifier::{self, VerifierError};
use crate::worker_capability::{self, ReadyWorkerRoute};
use crate::writer::{self, ReviewerBackend, WriterBackend, WriterError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SchedulerAuthority {
    Planner,
    Readonly,
    Writer,
    Verifier,
    GoalVerifier,
    Replanner,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SchedulerNoActionReason {
    Pausing,
    Paused,
    Cancelling,
    TerminalGoal(GoalStatus),
    GoalBlocked,
    GoalFinalizationRequired,
    ReplanningWithoutTrigger,
    ActiveWork,
    WaitingForDependencies,
    ReadinessPropagationDeferred,
    RetryPolicyDeferred,
    BlockedTasks,
    FailedTasks,
    WriterLeaseHeld { holder: TaskId },
    NoEligibleAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SchedulerDecision {
    PlanInitial,
    VerifyTask { task_id: TaskId },
    VerifyGoal,
    Replan { trigger_task_id: TaskId },
    RunReadonly { task_id: TaskId },
    RunWriter { task_id: TaskId },
    UnsupportedWorker { task_id: TaskId, worker: WorkerKind },
    NoAction { reason: SchedulerNoActionReason },
}

impl SchedulerDecision {
    fn task_id(&self) -> Option<&TaskId> {
        match self {
            Self::VerifyTask { task_id }
            | Self::Replan {
                trigger_task_id: task_id,
            }
            | Self::RunReadonly { task_id }
            | Self::RunWriter { task_id }
            | Self::UnsupportedWorker { task_id, .. } => Some(task_id),
            Self::PlanInitial | Self::VerifyGoal | Self::NoAction { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SchedulerAction {
    PlanInitial,
    VerifyTask,
    VerifyGoal,
    Replan,
    RunReadonly,
    RunWriter,
    UnsupportedWorker,
    NoAction,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SchedulerStepOutcome {
    Applied,
    NoAction(SchedulerNoActionReason),
    UnsupportedWorker(WorkerKind),
    RevisionConflict {
        expected: u64,
        actual: u64,
    },
    LowerAuthorityError {
        authority: SchedulerAuthority,
        detail: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SchedulerStepResult {
    pub(crate) action: SchedulerAction,
    pub(crate) goal_id: GoalId,
    pub(crate) task_id: Option<TaskId>,
    pub(crate) revision_before: u64,
    pub(crate) revision_after: u64,
    pub(crate) outcome: SchedulerStepOutcome,
}

#[derive(Debug)]
pub(crate) enum SchedulerError {
    Store(OrchestratorError),
    InvalidDurableState(String),
}

impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "scheduler durable-state error: {error}"),
            Self::InvalidDurableState(detail) => {
                write!(f, "scheduler rejected invalid durable state: {detail}")
            }
        }
    }
}

impl std::error::Error for SchedulerError {}

impl From<OrchestratorError> for SchedulerError {
    fn from(value: OrchestratorError) -> Self {
        Self::Store(value)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SchedulerSelection {
    snapshot: Goal,
    decision: SchedulerDecision,
}

impl SchedulerSelection {
    pub(crate) fn revision(&self) -> u64 {
        self.snapshot.revision()
    }

    pub(crate) fn plan_revision(&self) -> u32 {
        self.snapshot.plan_revision()
    }

    pub(crate) fn decision(&self) -> &SchedulerDecision {
        &self.decision
    }
}

/// Pure, side-effect-free action selection derived only from durable Goal state.
///
/// The frozen durable schema records `created_plan_revision`, but it does not
/// retain an ordinal for tasks created within the same plan revision. Therefore
/// Phase 8 uses `(mandatory-first, created_plan_revision, TaskId)` as the stable
/// durable ordering key. This never depends on HashMap order, filesystem order,
/// wall-clock races, backend preference, or randomness at scheduling time.
pub(crate) fn select_next_action(goal: &Goal) -> Result<SchedulerDecision, SchedulerError> {
    goal.validate()
        .map_err(|error| SchedulerError::InvalidDurableState(error.to_string()))?;

    match goal.status() {
        GoalStatus::Pausing => {
            return Ok(SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::Pausing,
            });
        }
        GoalStatus::Paused => {
            return Ok(SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::Paused,
            });
        }
        GoalStatus::Cancelling => {
            return Ok(SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::Cancelling,
            });
        }
        GoalStatus::Completed | GoalStatus::Failed | GoalStatus::Cancelled => {
            return Ok(SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::TerminalGoal(goal.status()),
            });
        }
        GoalStatus::Blocked => {
            return Ok(SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::GoalBlocked,
            });
        }
        GoalStatus::Verifying => {
            return Ok(SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::GoalFinalizationRequired,
            });
        }
        GoalStatus::Planning | GoalStatus::Running | GoalStatus::Replanning => {}
    }

    if let Some((task_id, _)) = ordered_tasks(goal, TaskStatus::Verifying).next() {
        return Ok(SchedulerDecision::VerifyTask {
            task_id: task_id.clone(),
        });
    }

    if let Some((task_id, task)) = ordered_tasks(goal, TaskStatus::NeedsReplan).next() {
        if task.has_unknown_side_effect() {
            return Ok(SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::BlockedTasks,
            });
        }
        return Ok(SchedulerDecision::Replan {
            trigger_task_id: task_id.clone(),
        });
    }

    if goal.status() == GoalStatus::Planning {
        if goal.plan_revision() == 0 && goal.tasks().is_empty() {
            return Ok(SchedulerDecision::PlanInitial);
        }
        return Err(SchedulerError::InvalidDurableState(
            "PLANNING requires plan_revision 0 and an empty Task DAG".to_owned(),
        ));
    }

    if goal.status() == GoalStatus::Replanning {
        return Ok(SchedulerDecision::NoAction {
            reason: SchedulerNoActionReason::ReplanningWithoutTrigger,
        });
    }

    // Once all mandatory work has completed with current authoritative Task
    // verification, Goal verification outranks optional READY work. Active work,
    // blockers, UNKNOWN side effects, VERIFYING, and NEEDS_REPLAN remain higher
    // priority durable gates and prevent final-verification entry.
    let goal_verification_eligible = goal.status() == GoalStatus::Running
        && goal.final_verification_spec().is_some()
        && goal
            .tasks()
            .values()
            .filter(|task| task.mandatory())
            .all(|task| {
                task.status() == TaskStatus::Completed
                    && task.blockers().is_empty()
                    && !task.has_unknown_side_effect()
                    && task.verification_results().last().is_some_and(|result| {
                        result.outcome() == crate::task::VerificationOutcome::Passed
                    })
            })
        && !goal.blockers().iter().any(|blocker| blocker.mandatory())
        && !goal
            .tasks()
            .values()
            .any(|task| task.has_unknown_side_effect())
        && !goal.has_task_status(TaskStatus::Running)
        && !goal.has_task_status(TaskStatus::Verifying)
        && !goal.has_task_status(TaskStatus::NeedsReplan);
    if goal_verification_eligible {
        return Ok(SchedulerDecision::VerifyGoal);
    }

    if let Some((task_id, task)) = ordered_ready_tasks(goal).next() {
        if !dependencies_completed(goal, task) {
            return Ok(SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::WaitingForDependencies,
            });
        }
        if !task.blockers().is_empty() || task.attempts().len() >= task.max_attempts() as usize {
            return Ok(SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::BlockedTasks,
            });
        }
        match worker_capability::ready_worker_route(task.worker()) {
            Some(ReadyWorkerRoute::Readonly) => {
                return Ok(SchedulerDecision::RunReadonly {
                    task_id: task_id.clone(),
                });
            }
            Some(ReadyWorkerRoute::Writer) => {
                if let Some((holder, _)) = ordered_workspace_leases(goal, task_id).next() {
                    return Ok(SchedulerDecision::NoAction {
                        reason: SchedulerNoActionReason::WriterLeaseHeld {
                            holder: holder.clone(),
                        },
                    });
                }
                return Ok(SchedulerDecision::RunWriter {
                    task_id: task_id.clone(),
                });
            }
            None => {
                return Ok(SchedulerDecision::UnsupportedWorker {
                    task_id: task_id.clone(),
                    worker: task.worker(),
                });
            }
        }
    }

    if goal.has_task_status(TaskStatus::Running) {
        return Ok(SchedulerDecision::NoAction {
            reason: SchedulerNoActionReason::ActiveWork,
        });
    }
    if goal.has_task_status(TaskStatus::Retryable) {
        return Ok(SchedulerDecision::NoAction {
            reason: SchedulerNoActionReason::RetryPolicyDeferred,
        });
    }
    if goal.has_task_status(TaskStatus::Blocked) {
        return Ok(SchedulerDecision::NoAction {
            reason: SchedulerNoActionReason::BlockedTasks,
        });
    }
    if goal.has_task_status(TaskStatus::Failed) {
        return Ok(SchedulerDecision::NoAction {
            reason: SchedulerNoActionReason::FailedTasks,
        });
    }
    if goal.has_task_status(TaskStatus::Pending) {
        if goal
            .tasks()
            .values()
            .filter(|task| task.status() == TaskStatus::Pending)
            .any(|task| dependencies_completed(goal, task))
        {
            return Ok(SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::ReadinessPropagationDeferred,
            });
        }
        return Ok(SchedulerDecision::NoAction {
            reason: SchedulerNoActionReason::WaitingForDependencies,
        });
    }

    Ok(SchedulerDecision::NoAction {
        reason: SchedulerNoActionReason::NoEligibleAction,
    })
}

pub(crate) fn select_scheduler_action(goal: &Goal) -> Result<SchedulerSelection, SchedulerError> {
    let decision = select_next_action(goal)?;
    Ok(SchedulerSelection {
        snapshot: goal.clone(),
        decision,
    })
}

pub(crate) async fn scheduler_step<P, RB, W, R, RP>(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    expected_revision: u64,
    planner_backend: &P,
    readonly_backend: &RB,
    writer_backend: &W,
    reviewer_backend: &R,
    replanner_backend: &RP,
) -> Result<SchedulerStepResult, SchedulerError>
where
    P: PlannerBackend,
    RB: ReadonlyBackend,
    W: WriterBackend,
    R: ReviewerBackend,
    RP: ReplannerBackend,
{
    let goal = store.load_goal(&session.id, goal_id)?;
    if goal.revision() != expected_revision {
        return Ok(SchedulerStepResult {
            action: SchedulerAction::NoAction,
            goal_id: goal_id.clone(),
            task_id: None,
            revision_before: expected_revision,
            revision_after: goal.revision(),
            outcome: SchedulerStepOutcome::RevisionConflict {
                expected: expected_revision,
                actual: goal.revision(),
            },
        });
    }
    let selection = select_scheduler_action(&goal)?;
    dispatch_selected_action(
        store,
        session,
        goal_id,
        selection,
        planner_backend,
        readonly_backend,
        writer_backend,
        reviewer_backend,
        replanner_backend,
    )
    .await
}

pub(crate) async fn dispatch_selected_action<P, RB, W, R, RP>(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    selection: SchedulerSelection,
    planner_backend: &P,
    readonly_backend: &RB,
    writer_backend: &W,
    reviewer_backend: &R,
    replanner_backend: &RP,
) -> Result<SchedulerStepResult, SchedulerError>
where
    P: PlannerBackend,
    RB: ReadonlyBackend,
    W: WriterBackend,
    R: ReviewerBackend,
    RP: ReplannerBackend,
{
    let revision_before = selection.revision();
    let action = action_for_decision(&selection.decision);
    let task_id = selection.decision.task_id().cloned();

    let outcome = match &selection.decision {
        SchedulerDecision::PlanInitial => {
            match planner::planner_request_for_goal(&selection.snapshot, session)
                .and_then(|request| planner_backend.propose_initial_plan(&request))
                .and_then(|output| {
                    planner::materialize_initial_plan_output(
                        store,
                        session,
                        goal_id,
                        revision_before,
                        &output,
                    )
                }) {
                Ok(_) => SchedulerStepOutcome::Applied,
                Err(error) => map_planner_error(&error),
            }
        }
        SchedulerDecision::VerifyTask { task_id } => {
            match verifier::verify_task(store, session, goal_id, task_id, revision_before).await {
                Ok(_) => SchedulerStepOutcome::Applied,
                Err(error) => map_verifier_error(&error),
            }
        }
        SchedulerDecision::VerifyGoal => {
            match goal_verifier::verify_goal(store, &session.id, goal_id, revision_before) {
                Ok(_) => SchedulerStepOutcome::Applied,
                Err(error) => map_goal_verifier_error(&error),
            }
        }
        SchedulerDecision::Replan { .. } => {
            match replanner::replanner_request_for_goal(&selection.snapshot, session)
                .and_then(|request| replanner_backend.propose_replan(&request))
                .and_then(|output| {
                    replanner::materialize_replan_output(
                        store,
                        session,
                        goal_id,
                        revision_before,
                        selection.plan_revision(),
                        &output,
                    )
                }) {
                Ok(_) => SchedulerStepOutcome::Applied,
                Err(error) => map_replanner_error(&error),
            }
        }
        SchedulerDecision::RunReadonly { task_id } => {
            match readonly_worker::run_readonly_attempt(
                store,
                session,
                goal_id,
                task_id,
                revision_before,
                readonly_backend,
            ) {
                Ok(_) => SchedulerStepOutcome::Applied,
                Err(error) => map_readonly_error(&error),
            }
        }
        SchedulerDecision::RunWriter { task_id } => {
            match writer::run_writer_attempt(
                store,
                session,
                goal_id,
                task_id,
                revision_before,
                writer_backend,
                reviewer_backend,
            )
            .await
            {
                Ok(_) => SchedulerStepOutcome::Applied,
                Err(error) => map_writer_error(&error),
            }
        }
        SchedulerDecision::UnsupportedWorker { worker, .. } => {
            SchedulerStepOutcome::UnsupportedWorker(*worker)
        }
        SchedulerDecision::NoAction { reason } => SchedulerStepOutcome::NoAction(reason.clone()),
    };

    let revision_after = store
        .load_goal(&session.id, goal_id)
        .map(|goal| goal.revision())
        .unwrap_or(revision_before);

    Ok(SchedulerStepResult {
        action,
        goal_id: goal_id.clone(),
        task_id,
        revision_before,
        revision_after,
        outcome,
    })
}

fn action_for_decision(decision: &SchedulerDecision) -> SchedulerAction {
    match decision {
        SchedulerDecision::PlanInitial => SchedulerAction::PlanInitial,
        SchedulerDecision::VerifyTask { .. } => SchedulerAction::VerifyTask,
        SchedulerDecision::VerifyGoal => SchedulerAction::VerifyGoal,
        SchedulerDecision::Replan { .. } => SchedulerAction::Replan,
        SchedulerDecision::RunReadonly { .. } => SchedulerAction::RunReadonly,
        SchedulerDecision::RunWriter { .. } => SchedulerAction::RunWriter,
        SchedulerDecision::UnsupportedWorker { .. } => SchedulerAction::UnsupportedWorker,
        SchedulerDecision::NoAction { .. } => SchedulerAction::NoAction,
    }
}

fn ordered_tasks(goal: &Goal, status: TaskStatus) -> impl Iterator<Item = (&TaskId, &Task)> {
    let mut tasks = goal
        .tasks()
        .iter()
        .filter(|(_, task)| task.status() == status)
        .collect::<Vec<_>>();
    tasks.sort_by(|(left_id, left), (right_id, right)| {
        task_order_key(left_id, left).cmp(&task_order_key(right_id, right))
    });
    tasks.into_iter()
}

fn ordered_ready_tasks(goal: &Goal) -> impl Iterator<Item = (&TaskId, &Task)> {
    let mut tasks = goal
        .tasks()
        .iter()
        .filter(|(_, task)| task.status() == TaskStatus::Ready)
        .collect::<Vec<_>>();
    tasks.sort_by(|(left_id, left), (right_id, right)| {
        task_order_key(left_id, left).cmp(&task_order_key(right_id, right))
    });
    tasks.into_iter()
}

fn ordered_workspace_leases<'a>(
    goal: &'a Goal,
    selected: &TaskId,
) -> impl Iterator<Item = (&'a TaskId, &'a Task)> {
    let mut tasks = goal
        .tasks()
        .iter()
        .filter(|(task_id, task)| {
            *task_id != selected && writer::holds_workspace_mutation_lease(task)
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|(left_id, left), (right_id, right)| {
        task_order_key(left_id, left).cmp(&task_order_key(right_id, right))
    });
    tasks.into_iter()
}

fn task_order_key<'a>(task_id: &'a TaskId, task: &Task) -> (u8, u32, &'a str) {
    (
        if task.mandatory() { 0 } else { 1 },
        task.created_plan_revision(),
        task_id.as_str(),
    )
}

fn dependencies_completed(goal: &Goal, task: &Task) -> bool {
    task.dependencies().iter().all(|dependency| {
        goal.tasks()
            .get(dependency.task_id())
            .is_some_and(|dependency_task| dependency_task.status() == TaskStatus::Completed)
    })
}

fn revision_conflict(error: &OrchestratorError) -> Option<(u64, u64)> {
    match error {
        OrchestratorError::RevisionConflict { expected, actual } => Some((*expected, *actual)),
        _ => None,
    }
}

fn map_planner_error(error: &PlannerError) -> SchedulerStepOutcome {
    match error {
        PlannerError::PlanConflict { expected, actual } => SchedulerStepOutcome::RevisionConflict {
            expected: *expected,
            actual: *actual,
        },
        PlannerError::Store(error) => map_store_or_lower(SchedulerAuthority::Planner, error),
        _ => lower_error(SchedulerAuthority::Planner, error),
    }
}

fn map_readonly_error(error: &ReadonlyError) -> SchedulerStepOutcome {
    match error {
        ReadonlyError::Store(error) => map_store_or_lower(SchedulerAuthority::Readonly, error),
        _ => lower_error(SchedulerAuthority::Readonly, error),
    }
}

fn map_writer_error(error: &WriterError) -> SchedulerStepOutcome {
    match error {
        WriterError::Store(error) => map_store_or_lower(SchedulerAuthority::Writer, error),
        _ => lower_error(SchedulerAuthority::Writer, error),
    }
}

fn map_verifier_error(error: &VerifierError) -> SchedulerStepOutcome {
    match error {
        VerifierError::Store(error) => map_store_or_lower(SchedulerAuthority::Verifier, error),
        _ => lower_error(SchedulerAuthority::Verifier, error),
    }
}

fn map_goal_verifier_error(error: &GoalVerifierError) -> SchedulerStepOutcome {
    match error {
        GoalVerifierError::Store(store_error) => {
            map_store_or_lower(SchedulerAuthority::GoalVerifier, store_error)
        }
        _ => lower_error(SchedulerAuthority::GoalVerifier, error),
    }
}

fn map_replanner_error(error: &ReplannerError) -> SchedulerStepOutcome {
    match error {
        ReplannerError::RevisionConflict { expected, actual } => {
            SchedulerStepOutcome::RevisionConflict {
                expected: *expected,
                actual: *actual,
            }
        }
        ReplannerError::Store(error) => map_store_or_lower(SchedulerAuthority::Replanner, error),
        _ => lower_error(SchedulerAuthority::Replanner, error),
    }
}

fn map_store_or_lower(
    authority: SchedulerAuthority,
    error: &OrchestratorError,
) -> SchedulerStepOutcome {
    if let Some((expected, actual)) = revision_conflict(error) {
        SchedulerStepOutcome::RevisionConflict { expected, actual }
    } else {
        SchedulerStepOutcome::LowerAuthorityError {
            authority,
            detail: error.to_string(),
        }
    }
}

fn lower_error(
    authority: SchedulerAuthority,
    error: &dyn std::fmt::Display,
) -> SchedulerStepOutcome {
    SchedulerStepOutcome::LowerAuthorityError {
        authority,
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    use serde_json::json;
    use uuid::Uuid;

    use crate::goal::CheckpointReason;
    use crate::planner::PlannerRequest;
    use crate::replanner::ReplannerRequest;
    use crate::task::{
        ReplaySafety, TaskDependency, TaskOperationKind, TaskScope, TaskTransitionContext,
        VerificationSpec,
    };
    use crate::writer::{ReviewerRequest, WriterRequest};

    const NOW: &str = "2026-09-13T00:00:00Z";

    struct Fixture {
        root: PathBuf,
        repo: PathBuf,
        session: config::Session,
        store: TaskStore,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn fixture() -> Fixture {
        let root = std::env::temp_dir().join(format!("local-mcp-phase8-{}", Uuid::new_v4()));
        let repo = root.join("repo");
        let state = root.join("state");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("sentinel.txt"), b"ok\n").unwrap();
        let session = config::Session {
            id: format!("phase8-{}", Uuid::new_v4()),
            cwd: repo.clone(),
            permitted_directories: vec![repo.clone()],
        };
        let store = TaskStore::with_state_root(state);
        Fixture {
            root,
            repo,
            session,
            store,
        }
    }

    fn new_goal(fixture: &Fixture) -> Goal {
        Goal::new(
            fixture.session.id.clone(),
            fixture.repo.clone(),
            "advance exactly one durable orchestration action",
            Some("Phase 8 fixture".to_owned()),
            vec!["single-step only".to_owned()],
            vec!["lower authority remains authoritative".to_owned()],
            NOW,
        )
        .unwrap()
    }

    fn read_scope(repo: &Path) -> TaskScope {
        TaskScope::new(
            vec![repo.to_path_buf()],
            vec![],
            TaskOperationKind::ReadOnly,
            ReplaySafety::SafeReadOnly,
        )
    }

    fn writer_scope(repo: &Path) -> TaskScope {
        TaskScope::new(
            vec![repo.to_path_buf()],
            vec![repo.join(".git")],
            TaskOperationKind::LocalMutation,
            ReplaySafety::VerifyBeforeRetry,
        )
    }

    fn task(
        fixture: &Fixture,
        title: &str,
        mandatory: bool,
        worker: WorkerKind,
        verification: Vec<VerificationSpec>,
    ) -> Task {
        let scope = if worker == WorkerKind::CodexWriter {
            writer_scope(&fixture.repo)
        } else {
            read_scope(&fixture.repo)
        };
        Task::new(
            title,
            title,
            mandatory,
            worker,
            scope,
            verification,
            2,
            1,
            NOW,
        )
        .unwrap()
    }

    fn running_goal(fixture: &Fixture, tasks: Vec<Task>) -> Goal {
        let mut goal = new_goal(fixture);
        goal.materialize_initial_plan(tasks, NOW).unwrap();
        goal
    }

    fn persist(fixture: &Fixture, goal: &Goal) -> GoalId {
        let id = goal.id().clone();
        fixture.store.create_goal(goal).unwrap();
        id
    }

    #[derive(Default)]
    struct NeverPlanner {
        calls: Cell<usize>,
    }

    impl PlannerBackend for NeverPlanner {
        fn propose_initial_plan(&self, _request: &PlannerRequest) -> Result<Vec<u8>, PlannerError> {
            self.calls.set(self.calls.get() + 1);
            Err(PlannerError::PlannerUnavailable)
        }
    }

    #[derive(Default)]
    struct NeverReadonly {
        calls: Cell<usize>,
    }

    impl ReadonlyBackend for NeverReadonly {
        fn investigate(
            &self,
            _request: &crate::readonly_worker::ReadonlyRequest,
        ) -> Result<Vec<u8>, ReadonlyError> {
            self.calls.set(self.calls.get() + 1);
            Err(ReadonlyError::Backend(
                "unexpected readonly call".to_owned(),
            ))
        }
    }

    #[derive(Default)]
    struct SuccessfulReadonly {
        calls: Cell<usize>,
    }

    impl ReadonlyBackend for SuccessfulReadonly {
        fn investigate(
            &self,
            request: &crate::readonly_worker::ReadonlyRequest,
        ) -> Result<Vec<u8>, ReadonlyError> {
            self.calls.set(self.calls.get() + 1);
            Ok(json!({
                "goal_id": request.goal_id(),
                "task_id": request.task_id(),
                "attempt_id": request.attempt_id(),
                "goal_revision": request.goal_revision(),
                "plan_revision": request.plan_revision(),
                "status": "candidate_complete",
                "summary": "readonly investigation complete",
                "evidence": [{"kind":"proof","value":"host-bound observation"}]
            })
            .to_string()
            .into_bytes())
        }
    }

    #[derive(Default)]
    struct FailingReadonly {
        calls: Cell<usize>,
    }

    impl ReadonlyBackend for FailingReadonly {
        fn investigate(
            &self,
            _request: &crate::readonly_worker::ReadonlyRequest,
        ) -> Result<Vec<u8>, ReadonlyError> {
            self.calls.set(self.calls.get() + 1);
            Err(ReadonlyError::Backend(
                "synthetic readonly transport failure".to_owned(),
            ))
        }
    }

    #[derive(Default)]
    struct NeverWriter {
        calls: Cell<usize>,
    }

    impl WriterBackend for NeverWriter {
        fn propose(&self, _request: &WriterRequest) -> Result<Vec<u8>, WriterError> {
            self.calls.set(self.calls.get() + 1);
            Err(WriterError::Backend("unexpected writer call".to_owned()))
        }
    }

    #[derive(Default)]
    struct NeverReviewer {
        calls: Cell<usize>,
    }

    impl ReviewerBackend for NeverReviewer {
        fn review(&self, _request: &ReviewerRequest) -> Result<Vec<u8>, WriterError> {
            self.calls.set(self.calls.get() + 1);
            Err(WriterError::Backend("unexpected reviewer call".to_owned()))
        }
    }

    #[derive(Default)]
    struct NeverReplanner {
        calls: Cell<usize>,
    }

    impl ReplannerBackend for NeverReplanner {
        fn propose_replan(&self, _request: &ReplannerRequest) -> Result<Vec<u8>, ReplannerError> {
            self.calls.set(self.calls.get() + 1);
            Err(ReplannerError::ReplannerUnavailable)
        }
    }

    #[derive(Default)]
    struct InitialPlanBackend {
        calls: Cell<usize>,
    }

    impl PlannerBackend for InitialPlanBackend {
        fn propose_initial_plan(&self, request: &PlannerRequest) -> Result<Vec<u8>, PlannerError> {
            self.calls.set(self.calls.get() + 1);
            let request = serde_json::to_value(request).unwrap();
            let criterion_bindings = request["completion_criteria"]
                .as_array()
                .unwrap()
                .iter()
                .map(|criterion| {
                    json!({
                        "criterion_id": criterion["criterion_id"],
                        "task_refs": ["writer"]
                    })
                })
                .collect::<Vec<_>>();
            Ok(serde_json::to_vec(&json!({
                "goal_id": request["goal_id"],
                "goal_revision": request["goal_revision"],
                "summary": "one deterministic writer task",
                "tasks": [{
                    "proposal_id": "writer",
                    "title": "writer",
                    "objective": "write target once",
                    "mandatory": true,
                    "worker": "CODEX_WRITER",
                    "dependencies": [],
                    "scope": {
                        "allowed_paths": ["."],
                        "forbidden_paths": [],
                        "operation_kind": "LOCAL_MUTATION",
                        "replay_safety": "VERIFY_BEFORE_RETRY"
                    },
                    "verification": [{
                        "kind": "FILE_EXISTS",
                        "path": "target.txt",
                        "must_be_file": true
                    }]
                }],
                "criterion_bindings": criterion_bindings
            }))
            .unwrap())
        }
    }

    #[derive(Default)]
    struct CreatingWriter {
        calls: Cell<usize>,
    }

    impl WriterBackend for CreatingWriter {
        fn propose(&self, request: &WriterRequest) -> Result<Vec<u8>, WriterError> {
            self.calls.set(self.calls.get() + 1);
            Ok(json!({
                "goal_id": request.goal_id(),
                "task_id": request.task_id(),
                "attempt_id": request.attempt_id(),
                "goal_revision": request.goal_revision(),
                "plan_revision": request.plan_revision(),
                "status": "candidate_complete",
                "summary": "create target through Phase 5 authority",
                "evidence": [],
                "proposed_operations": [{
                    "kind": "WRITE_UTF8",
                    "path": "target.txt",
                    "expected_preimage": {"kind": "ABSENT"},
                    "content": "created\n"
                }]
            })
            .to_string()
            .into_bytes())
        }
    }

    #[derive(Default)]
    struct PassingReviewer {
        calls: Cell<usize>,
    }

    impl ReviewerBackend for PassingReviewer {
        fn review(&self, request: &ReviewerRequest) -> Result<Vec<u8>, WriterError> {
            self.calls.set(self.calls.get() + 1);
            Ok(json!({
                "goal_id": request.goal_id(),
                "task_id": request.task_id(),
                "attempt_id": request.attempt_id(),
                "goal_revision": request.goal_revision(),
                "plan_revision": request.plan_revision(),
                "summary": "deterministic review passed",
                "blocking_findings": 0,
                "evidence": []
            })
            .to_string()
            .into_bytes())
        }
    }

    #[derive(Default)]
    struct RepairReplanner {
        calls: Cell<usize>,
    }

    impl ReplannerBackend for RepairReplanner {
        fn propose_replan(&self, request: &ReplannerRequest) -> Result<Vec<u8>, ReplannerError> {
            self.calls.set(self.calls.get() + 1);
            let request = serde_json::to_value(request).unwrap();
            let trigger = request["eligible_needs_replan_task_ids"][0]
                .as_str()
                .unwrap();
            Ok(serde_json::to_vec(&json!({
                "goal_id": request["goal_id"],
                "base_goal_revision": request["goal_revision"],
                "base_plan_revision": request["plan_revision"],
                "summary": "add one repair prerequisite",
                "add_tasks": [{
                    "proposal_id": "repair",
                    "title": "repair prerequisite",
                    "objective": "prepare deterministic repair evidence",
                    "mandatory": true,
                    "worker": "CODEX_READONLY",
                    "dependencies": [],
                    "scope": {
                        "allowed_paths": ["."],
                        "forbidden_paths": [],
                        "operation_kind": "READ_ONLY",
                        "replay_safety": "SAFE_READ_ONLY"
                    },
                    "verification": [{
                        "kind": "STRUCTURED_EVIDENCE",
                        "requirement_id": "repair.evidence"
                    }]
                }],
                "add_dependencies": [{
                    "task": {"ref_kind": "EXISTING", "task_id": trigger},
                    "dependency": {"ref_kind": "NEW", "proposal_id": "repair"}
                }],
                "strengthen_verification": [],
                "strengthen_mandatory": [],
                "resolve_needs_replan": [trigger]
            }))
            .unwrap())
        }
    }

    #[test]
    fn planning_goal_selects_initial_planner() {
        let fixture = fixture();
        let goal = new_goal(&fixture);
        assert_eq!(
            select_next_action(&goal).unwrap(),
            SchedulerDecision::PlanInitial
        );
    }

    #[test]
    fn verifying_preempts_needs_replan_and_ready_work() {
        let fixture = fixture();
        let verifying = task(&fixture, "verify", true, WorkerKind::CodexReadonly, vec![]);
        let verifying_id = verifying.id().clone();
        let replan = task(&fixture, "replan", true, WorkerKind::CodexReadonly, vec![]);
        let replan_id = replan.id().clone();
        let ready = task(&fixture, "writer", true, WorkerKind::CodexWriter, vec![]);
        let mut goal = running_goal(&fixture, vec![verifying, replan, ready]);
        goal.transition_task(
            &verifying_id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.transition_task(
            &verifying_id,
            TaskStatus::Verifying,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.transition_task(
            &replan_id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.transition_task(
            &replan_id,
            TaskStatus::NeedsReplan,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        assert_eq!(
            select_next_action(&goal).unwrap(),
            SchedulerDecision::VerifyTask {
                task_id: verifying_id
            }
        );
    }

    #[test]
    fn needs_replan_preempts_ready_work() {
        let fixture = fixture();
        let trigger = task(&fixture, "replan", true, WorkerKind::CodexReadonly, vec![]);
        let trigger_id = trigger.id().clone();
        let ready = task(&fixture, "writer", true, WorkerKind::CodexWriter, vec![]);
        let mut goal = running_goal(&fixture, vec![trigger, ready]);
        goal.transition_task(
            &trigger_id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.transition_task(
            &trigger_id,
            TaskStatus::NeedsReplan,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        assert_eq!(
            select_next_action(&goal).unwrap(),
            SchedulerDecision::Replan {
                trigger_task_id: trigger_id
            }
        );
    }

    #[test]
    fn ready_codex_writer_selects_phase5_authority() {
        let fixture = fixture();
        let writer_task = task(&fixture, "writer", true, WorkerKind::CodexWriter, vec![]);
        let id = writer_task.id().clone();
        let goal = running_goal(&fixture, vec![writer_task]);
        assert_eq!(
            select_next_action(&goal).unwrap(),
            SchedulerDecision::RunWriter { task_id: id }
        );
    }

    #[test]
    fn paused_goal_is_read_only_no_action() {
        let fixture = fixture();
        let writer_task = task(&fixture, "writer", true, WorkerKind::CodexWriter, vec![]);
        let mut goal = running_goal(&fixture, vec![writer_task]);
        goal.transition_to(GoalStatus::Pausing, NOW).unwrap();
        goal.transition_to(GoalStatus::Paused, NOW).unwrap();
        assert_eq!(
            select_next_action(&goal).unwrap(),
            SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::Paused
            }
        );
    }

    #[test]
    fn terminal_goal_is_no_action() {
        let fixture = fixture();
        let mut goal = new_goal(&fixture);
        goal.transition_to(GoalStatus::Failed, NOW).unwrap();
        assert_eq!(
            select_next_action(&goal).unwrap(),
            SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::TerminalGoal(GoalStatus::Failed)
            }
        );
    }

    #[test]
    fn readonly_worker_is_a_typed_ready_runtime_action() {
        let fixture = fixture();
        let readonly = task(
            &fixture,
            "readonly",
            true,
            WorkerKind::CodexReadonly,
            vec![],
        );
        let id = readonly.id().clone();
        let goal = running_goal(&fixture, vec![readonly]);
        assert_eq!(
            select_next_action(&goal).unwrap(),
            SchedulerDecision::RunReadonly { task_id: id }
        );
    }

    #[test]
    fn historical_non_runtime_workers_remain_typed_unsupported() {
        let fixture = fixture();
        for worker in [
            WorkerKind::LocalOperation,
            WorkerKind::CodexReviewer,
            WorkerKind::Verifier,
        ] {
            let candidate = task(&fixture, "historical", true, worker, vec![]);
            let id = candidate.id().clone();
            let goal = running_goal(&fixture, vec![candidate]);
            assert_eq!(
                select_next_action(&goal).unwrap(),
                SchedulerDecision::UnsupportedWorker {
                    task_id: id,
                    worker
                }
            );
        }
    }

    #[test]
    fn cloned_durable_goal_produces_identical_decision() {
        let fixture = fixture();
        let first = task(&fixture, "first", true, WorkerKind::CodexWriter, vec![]);
        let second = task(&fixture, "second", true, WorkerKind::CodexWriter, vec![]);
        let goal = running_goal(&fixture, vec![first, second]);
        let clone = goal.clone();
        assert_eq!(
            select_next_action(&goal).unwrap(),
            select_next_action(&clone).unwrap()
        );
    }

    #[test]
    fn mandatory_ready_task_precedes_optional_ready_task() {
        let fixture = fixture();
        let optional = task(&fixture, "optional", false, WorkerKind::CodexWriter, vec![]);
        let mandatory = task(&fixture, "mandatory", true, WorkerKind::CodexWriter, vec![]);
        let mandatory_id = mandatory.id().clone();
        let goal = running_goal(&fixture, vec![optional, mandatory]);
        assert_eq!(
            select_next_action(&goal).unwrap(),
            SchedulerDecision::RunWriter {
                task_id: mandatory_id
            }
        );
    }

    #[test]
    fn existing_writer_lease_blocks_another_writer() {
        let fixture = fixture();
        let first = task(&fixture, "first", true, WorkerKind::CodexWriter, vec![]);
        let first_id = first.id().clone();
        let second = task(&fixture, "second", true, WorkerKind::CodexWriter, vec![]);
        let mut goal = running_goal(&fixture, vec![first, second]);
        goal.transition_task(
            &first_id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        assert!(matches!(
            select_next_action(&goal).unwrap(),
            SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::WriterLeaseHeld { holder }
            } if holder == first_id
        ));
    }

    #[tokio::test]
    async fn planner_step_materializes_plan_but_does_not_run_root_task() {
        let fixture = fixture();
        let goal = new_goal(&fixture);
        let goal_id = persist(&fixture, &goal);
        let planner = InitialPlanBackend::default();
        let writer = NeverWriter::default();
        let reviewer = NeverReviewer::default();
        let replanner = NeverReplanner::default();
        let result = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            goal.revision(),
            &planner,
            &NeverReadonly::default(),
            &writer,
            &reviewer,
            &replanner,
        )
        .await
        .unwrap();
        assert_eq!(result.action, SchedulerAction::PlanInitial);
        assert_eq!(result.outcome, SchedulerStepOutcome::Applied);
        assert_eq!(planner.calls.get(), 1);
        assert_eq!(writer.calls.get(), 0);
        let stored = fixture
            .store
            .load_goal(&fixture.session.id, &goal_id)
            .unwrap();
        assert_eq!(stored.status(), GoalStatus::Running);
        assert_eq!(stored.tasks().len(), 1);
        assert_eq!(
            stored.tasks().values().next().unwrap().status(),
            TaskStatus::Ready
        );
    }

    #[tokio::test]
    async fn writer_step_runs_phase5_once_and_stops_at_verifying() {
        let fixture = fixture();
        let verification = vec![VerificationSpec::FileExists {
            path: fixture.repo.join("target.txt"),
            must_be_file: true,
        }];
        let writer_task = task(
            &fixture,
            "writer",
            true,
            WorkerKind::CodexWriter,
            verification,
        );
        let task_id = writer_task.id().clone();
        let goal = running_goal(&fixture, vec![writer_task]);
        let goal_id = persist(&fixture, &goal);
        let planner = NeverPlanner::default();
        let writer = CreatingWriter::default();
        let reviewer = PassingReviewer::default();
        let replanner = NeverReplanner::default();
        let result = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            goal.revision(),
            &planner,
            &NeverReadonly::default(),
            &writer,
            &reviewer,
            &replanner,
        )
        .await
        .unwrap();
        assert_eq!(result.action, SchedulerAction::RunWriter);
        assert_eq!(result.outcome, SchedulerStepOutcome::Applied);
        assert_eq!(writer.calls.get(), 1);
        assert_eq!(reviewer.calls.get(), 1);
        let stored = fixture
            .store
            .load_goal(&fixture.session.id, &goal_id)
            .unwrap();
        assert_eq!(stored.tasks()[&task_id].status(), TaskStatus::Verifying);
        assert_eq!(
            fs::read(fixture.repo.join("target.txt")).unwrap(),
            b"created\n"
        );
    }

    #[tokio::test]
    async fn verifier_step_completes_task_but_never_completes_goal() {
        let fixture = fixture();
        let verification = vec![VerificationSpec::FileExists {
            path: fixture.repo.join("sentinel.txt"),
            must_be_file: true,
        }];
        let verify_task = task(
            &fixture,
            "verify",
            true,
            WorkerKind::CodexReadonly,
            verification,
        );
        let task_id = verify_task.id().clone();
        let mut goal = running_goal(&fixture, vec![verify_task]);
        goal.transition_task(
            &task_id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.transition_task(
            &task_id,
            TaskStatus::Verifying,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        let goal_id = persist(&fixture, &goal);
        let result = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            goal.revision(),
            &NeverPlanner::default(),
            &NeverReadonly::default(),
            &NeverWriter::default(),
            &NeverReviewer::default(),
            &NeverReplanner::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.action, SchedulerAction::VerifyTask);
        assert_eq!(result.outcome, SchedulerStepOutcome::Applied);
        let stored = fixture
            .store
            .load_goal(&fixture.session.id, &goal_id)
            .unwrap();
        assert_eq!(stored.tasks()[&task_id].status(), TaskStatus::Completed);
        assert_ne!(stored.status(), GoalStatus::Completed);
    }

    #[tokio::test]
    async fn replan_step_runs_phase7_once_and_does_not_dispatch_new_ready_task() {
        let fixture = fixture();
        let trigger = task(&fixture, "trigger", true, WorkerKind::CodexReadonly, vec![]);
        let trigger_id = trigger.id().clone();
        let mut goal = running_goal(&fixture, vec![trigger]);
        goal.transition_task(
            &trigger_id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.transition_task(
            &trigger_id,
            TaskStatus::NeedsReplan,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        let goal_id = persist(&fixture, &goal);
        let replanner = RepairReplanner::default();
        let writer = NeverWriter::default();
        let result = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            goal.revision(),
            &NeverPlanner::default(),
            &NeverReadonly::default(),
            &writer,
            &NeverReviewer::default(),
            &replanner,
        )
        .await
        .unwrap();
        assert_eq!(result.action, SchedulerAction::Replan);
        assert_eq!(result.outcome, SchedulerStepOutcome::Applied);
        assert_eq!(replanner.calls.get(), 1);
        assert_eq!(writer.calls.get(), 0);
        let stored = fixture
            .store
            .load_goal(&fixture.session.id, &goal_id)
            .unwrap();
        assert_eq!(stored.plan_revision(), 2);
        assert_eq!(stored.tasks().len(), 2);
        assert!(
            stored
                .tasks()
                .values()
                .any(|task| task.status() == TaskStatus::Ready)
        );
    }

    #[tokio::test]
    async fn stale_selected_writer_is_rejected_without_backend_call_or_reselection() {
        let fixture = fixture();
        let writer_task = task(&fixture, "writer", true, WorkerKind::CodexWriter, vec![]);
        let goal = running_goal(&fixture, vec![writer_task]);
        let goal_id = persist(&fixture, &goal);
        let selection = select_scheduler_action(&goal).unwrap();
        fixture
            .store
            .mutate_goal_snapshot(
                &fixture.session.id,
                &goal_id,
                goal.revision(),
                |goal, now| goal.add_checkpoint(CheckpointReason::Recovery, now),
            )
            .unwrap();
        let writer = CreatingWriter::default();
        let result = dispatch_selected_action(
            &fixture.store,
            &fixture.session,
            &goal_id,
            selection,
            &NeverPlanner::default(),
            &NeverReadonly::default(),
            &writer,
            &PassingReviewer::default(),
            &NeverReplanner::default(),
        )
        .await
        .unwrap();
        assert!(matches!(
            result.outcome,
            SchedulerStepOutcome::RevisionConflict { .. }
        ));
        assert_eq!(writer.calls.get(), 0);
        let stored = fixture
            .store
            .load_goal(&fixture.session.id, &goal_id)
            .unwrap();
        assert_eq!(
            stored.tasks().values().next().unwrap().status(),
            TaskStatus::Ready
        );
    }

    #[tokio::test]
    async fn wrong_expected_revision_is_read_only_conflict() {
        let fixture = fixture();
        let writer_task = task(&fixture, "writer", true, WorkerKind::CodexWriter, vec![]);
        let goal = running_goal(&fixture, vec![writer_task]);
        let goal_id = persist(&fixture, &goal);
        let writer = CreatingWriter::default();
        let result = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            goal.revision() + 1,
            &NeverPlanner::default(),
            &NeverReadonly::default(),
            &writer,
            &PassingReviewer::default(),
            &NeverReplanner::default(),
        )
        .await
        .unwrap();
        assert!(matches!(
            result.outcome,
            SchedulerStepOutcome::RevisionConflict { .. }
        ));
        assert_eq!(writer.calls.get(), 0);
        assert_eq!(result.revision_after, goal.revision());
    }

    #[tokio::test]
    async fn paused_scheduler_step_leaves_revision_and_workspace_unchanged() {
        let fixture = fixture();
        let writer_task = task(&fixture, "writer", true, WorkerKind::CodexWriter, vec![]);
        let mut goal = running_goal(&fixture, vec![writer_task]);
        goal.transition_to(GoalStatus::Pausing, NOW).unwrap();
        goal.transition_to(GoalStatus::Paused, NOW).unwrap();
        let goal_id = persist(&fixture, &goal);
        let before = fs::read(fixture.repo.join("sentinel.txt")).unwrap();
        let result = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            goal.revision(),
            &NeverPlanner::default(),
            &NeverReadonly::default(),
            &NeverWriter::default(),
            &NeverReviewer::default(),
            &NeverReplanner::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.outcome,
            SchedulerStepOutcome::NoAction(SchedulerNoActionReason::Paused)
        );
        assert_eq!(result.revision_before, result.revision_after);
        assert_eq!(fs::read(fixture.repo.join("sentinel.txt")).unwrap(), before);
    }

    #[tokio::test]
    async fn one_step_with_two_ready_writers_changes_only_one_task() {
        let fixture = fixture();
        let verification = vec![VerificationSpec::FileExists {
            path: fixture.repo.join("target.txt"),
            must_be_file: true,
        }];
        let first = task(
            &fixture,
            "first",
            true,
            WorkerKind::CodexWriter,
            verification.clone(),
        );
        let second = task(
            &fixture,
            "second",
            true,
            WorkerKind::CodexWriter,
            verification,
        );
        let goal = running_goal(&fixture, vec![first, second]);
        let goal_id = persist(&fixture, &goal);
        let selection = select_scheduler_action(&goal).unwrap();
        let selected_id = selection.decision().task_id().unwrap().clone();
        let writer = CreatingWriter::default();
        let reviewer = PassingReviewer::default();
        let result = dispatch_selected_action(
            &fixture.store,
            &fixture.session,
            &goal_id,
            selection,
            &NeverPlanner::default(),
            &NeverReadonly::default(),
            &writer,
            &reviewer,
            &NeverReplanner::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.outcome, SchedulerStepOutcome::Applied);
        assert_eq!(writer.calls.get(), 1);
        assert_eq!(reviewer.calls.get(), 1);
        let stored = fixture
            .store
            .load_goal(&fixture.session.id, &goal_id)
            .unwrap();
        assert_eq!(stored.tasks()[&selected_id].status(), TaskStatus::Verifying);
        assert_eq!(
            stored
                .tasks()
                .values()
                .filter(|task| task.status() == TaskStatus::Ready)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn completed_dependency_does_not_get_promoted_by_scheduler_without_refresh_authority() {
        let fixture = fixture();
        let root = task(
            &fixture,
            "root",
            true,
            WorkerKind::CodexReadonly,
            vec![VerificationSpec::FileExists {
                path: fixture.repo.join("sentinel.txt"),
                must_be_file: true,
            }],
        );
        let root_id = root.id().clone();
        let mut dependent = task(&fixture, "dependent", true, WorkerKind::CodexWriter, vec![]);
        dependent
            .strengthen_dependencies(
                vec![TaskDependency::completed(root_id.clone())],
                &BTreeSet::new(),
            )
            .unwrap();
        let dependent_id = dependent.id().clone();
        let mut goal = running_goal(&fixture, vec![root, dependent]);
        goal.transition_task(
            &root_id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.transition_task(
            &root_id,
            TaskStatus::Verifying,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        let goal_id = persist(&fixture, &goal);
        let first = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            goal.revision(),
            &NeverPlanner::default(),
            &NeverReadonly::default(),
            &NeverWriter::default(),
            &NeverReviewer::default(),
            &NeverReplanner::default(),
        )
        .await
        .unwrap();
        assert_eq!(first.outcome, SchedulerStepOutcome::Applied);
        let after = fixture
            .store
            .load_goal(&fixture.session.id, &goal_id)
            .unwrap();
        assert_eq!(after.tasks()[&dependent_id].status(), TaskStatus::Pending);
        assert_eq!(
            select_next_action(&after).unwrap(),
            SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::ReadinessPropagationDeferred
            }
        );
    }

    #[tokio::test]
    async fn scheduler_returns_without_background_second_action() {
        let fixture = fixture();
        let goal = new_goal(&fixture);
        let goal_id = persist(&fixture, &goal);
        let planner = InitialPlanBackend::default();
        let writer = CreatingWriter::default();
        let reviewer = PassingReviewer::default();
        let result = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            goal.revision(),
            &planner,
            &NeverReadonly::default(),
            &writer,
            &reviewer,
            &NeverReplanner::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.action, SchedulerAction::PlanInitial);
        assert_eq!(planner.calls.get(), 1);
        assert_eq!(writer.calls.get(), 0);
        assert_eq!(reviewer.calls.get(), 0);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(planner.calls.get(), 1);
        assert_eq!(writer.calls.get(), 0);
        assert_eq!(reviewer.calls.get(), 0);
    }

    #[tokio::test]
    async fn readonly_scheduler_step_invokes_backend_once_and_stops_at_verifying() {
        let fixture = fixture();
        let sentinel = fixture.repo.join("sentinel.txt");
        fs::write(&sentinel, b"unchanged\n").unwrap();
        let readonly = task(
            &fixture,
            "readonly-runtime",
            true,
            WorkerKind::CodexReadonly,
            vec![VerificationSpec::StructuredEvidence {
                requirement_id: "proof".to_owned(),
            }],
        );
        let task_id = readonly.id().clone();
        let goal = running_goal(&fixture, vec![readonly]);
        let goal_id = persist(&fixture, &goal);
        let backend = SuccessfulReadonly::default();
        let result = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            goal.revision(),
            &NeverPlanner::default(),
            &backend,
            &NeverWriter::default(),
            &NeverReviewer::default(),
            &NeverReplanner::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.action, SchedulerAction::RunReadonly);
        assert_eq!(result.outcome, SchedulerStepOutcome::Applied);
        assert_eq!(backend.calls.get(), 1);
        let durable = fixture
            .store
            .load_goal(&fixture.session.id, &goal_id)
            .unwrap();
        assert_eq!(durable.tasks()[&task_id].status(), TaskStatus::Verifying);
        assert_ne!(durable.tasks()[&task_id].status(), TaskStatus::Completed);
        let attempt = durable.tasks()[&task_id].latest_attempt().unwrap();
        assert_eq!(
            attempt.side_effect_class(),
            Some(crate::fallback::SideEffectClass::None)
        );
        assert_eq!(
            attempt.side_effect_state(),
            Some(crate::fallback::SideEffectState::ConfirmedNotPerformed)
        );
        assert!(attempt.operation_id().is_none());
        assert!(attempt.scope_identity().is_none());
        assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged\n");
        assert!(!writer::holds_workspace_mutation_lease(
            &durable.tasks()[&task_id]
        ));
    }

    #[tokio::test]
    async fn verifier_is_a_separate_next_step_and_alone_completes_readonly_task() {
        let fixture = fixture();
        let readonly = task(
            &fixture,
            "readonly-verify",
            true,
            WorkerKind::CodexReadonly,
            vec![VerificationSpec::StructuredEvidence {
                requirement_id: "proof".to_owned(),
            }],
        );
        let task_id = readonly.id().clone();
        let goal = running_goal(&fixture, vec![readonly]);
        let goal_id = persist(&fixture, &goal);
        let backend = SuccessfulReadonly::default();
        let first = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            goal.revision(),
            &NeverPlanner::default(),
            &backend,
            &NeverWriter::default(),
            &NeverReviewer::default(),
            &NeverReplanner::default(),
        )
        .await
        .unwrap();
        assert_eq!(first.action, SchedulerAction::RunReadonly);
        let mid = fixture
            .store
            .load_goal(&fixture.session.id, &goal_id)
            .unwrap();
        assert_eq!(mid.tasks()[&task_id].status(), TaskStatus::Verifying);
        let second = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            mid.revision(),
            &NeverPlanner::default(),
            &backend,
            &NeverWriter::default(),
            &NeverReviewer::default(),
            &NeverReplanner::default(),
        )
        .await
        .unwrap();
        assert_eq!(second.action, SchedulerAction::VerifyTask);
        assert_eq!(backend.calls.get(), 1);
        let done = fixture
            .store
            .load_goal(&fixture.session.id, &goal_id)
            .unwrap();
        assert_eq!(done.tasks()[&task_id].status(), TaskStatus::Completed);
    }

    #[tokio::test]
    async fn stale_revision_prevents_readonly_backend_invocation() {
        let fixture = fixture();
        let readonly = task(&fixture, "stale", true, WorkerKind::CodexReadonly, vec![]);
        let goal = running_goal(&fixture, vec![readonly]);
        let goal_id = persist(&fixture, &goal);
        let backend = SuccessfulReadonly::default();
        let result = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            goal.revision() + 1,
            &NeverPlanner::default(),
            &backend,
            &NeverWriter::default(),
            &NeverReviewer::default(),
            &NeverReplanner::default(),
        )
        .await
        .unwrap();
        assert!(matches!(
            result.outcome,
            SchedulerStepOutcome::RevisionConflict { .. }
        ));
        assert_eq!(backend.calls.get(), 0);
    }

    #[test]
    fn readonly_ready_work_does_not_wait_for_writer_mutation_lease() {
        let fixture = fixture();
        let writer_task = task(
            &fixture,
            "writer-active",
            true,
            WorkerKind::CodexWriter,
            vec![],
        );
        let writer_id = writer_task.id().clone();
        let readonly = task(
            &fixture,
            "readonly-ready",
            true,
            WorkerKind::CodexReadonly,
            vec![],
        );
        let readonly_id = readonly.id().clone();
        let mut goal = running_goal(&fixture, vec![writer_task, readonly]);
        goal.transition_task(
            &writer_id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        assert!(writer::holds_workspace_mutation_lease(
            &goal.tasks()[&writer_id]
        ));
        assert_eq!(
            select_next_action(&goal).unwrap(),
            SchedulerDecision::RunReadonly {
                task_id: readonly_id
            }
        );
    }

    #[tokio::test]
    async fn readonly_transport_failure_is_non_mutating_and_bounded() {
        let fixture = fixture();
        let readonly = task(
            &fixture,
            "readonly-fail",
            true,
            WorkerKind::CodexReadonly,
            vec![],
        );
        let task_id = readonly.id().clone();
        let goal = running_goal(&fixture, vec![readonly]);
        let goal_id = persist(&fixture, &goal);
        let backend = FailingReadonly::default();
        let first = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            goal.revision(),
            &NeverPlanner::default(),
            &backend,
            &NeverWriter::default(),
            &NeverReviewer::default(),
            &NeverReplanner::default(),
        )
        .await
        .unwrap();
        assert!(matches!(
            first.outcome,
            SchedulerStepOutcome::LowerAuthorityError {
                authority: SchedulerAuthority::Readonly,
                ..
            }
        ));
        let retryable = fixture
            .store
            .load_goal(&fixture.session.id, &goal_id)
            .unwrap();
        assert_eq!(retryable.tasks()[&task_id].status(), TaskStatus::Retryable);
        let first_attempt = retryable.tasks()[&task_id].latest_attempt().unwrap();
        assert_eq!(
            first_attempt.side_effect_state(),
            Some(crate::fallback::SideEffectState::ConfirmedNotPerformed)
        );
        assert_eq!(
            first_attempt.side_effect_class(),
            Some(crate::fallback::SideEffectClass::None)
        );
        let revision = retryable.revision();
        fixture
            .store
            .mutate_goal_snapshot(&fixture.session.id, &goal_id, revision, |goal, now| {
                goal.transition_task(
                    &task_id,
                    TaskStatus::Ready,
                    TaskTransitionContext::default(),
                    now,
                )
            })
            .unwrap();
        let ready = fixture
            .store
            .load_goal(&fixture.session.id, &goal_id)
            .unwrap();
        let second = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            ready.revision(),
            &NeverPlanner::default(),
            &backend,
            &NeverWriter::default(),
            &NeverReviewer::default(),
            &NeverReplanner::default(),
        )
        .await
        .unwrap();
        assert!(matches!(
            second.outcome,
            SchedulerStepOutcome::LowerAuthorityError {
                authority: SchedulerAuthority::Readonly,
                ..
            }
        ));
        let failed = fixture
            .store
            .load_goal(&fixture.session.id, &goal_id)
            .unwrap();
        assert_eq!(failed.tasks()[&task_id].status(), TaskStatus::Failed);
        assert_eq!(failed.tasks()[&task_id].attempts().len(), 2);
        assert_eq!(backend.calls.get(), 2);
        assert!(
            failed.tasks()[&task_id]
                .attempts()
                .iter()
                .all(|a| a.side_effect_state()
                    == Some(crate::fallback::SideEffectState::ConfirmedNotPerformed))
        );
    }

    #[tokio::test]
    async fn multiple_ready_readonly_tasks_dispatch_exactly_one_in_durable_order() {
        let fixture = fixture();
        let one = task(&fixture, "one", true, WorkerKind::CodexReadonly, vec![]);
        let two = task(&fixture, "two", true, WorkerKind::CodexReadonly, vec![]);
        let goal = running_goal(&fixture, vec![one, two]);
        let selected = match select_next_action(&goal).unwrap() {
            SchedulerDecision::RunReadonly { task_id } => task_id,
            other => panic!("unexpected decision: {other:?}"),
        };
        let goal_id = persist(&fixture, &goal);
        let backend = SuccessfulReadonly::default();
        let result = scheduler_step(
            &fixture.store,
            &fixture.session,
            &goal_id,
            goal.revision(),
            &NeverPlanner::default(),
            &backend,
            &NeverWriter::default(),
            &NeverReviewer::default(),
            &NeverReplanner::default(),
        )
        .await
        .unwrap();
        assert_eq!(result.task_id.as_ref(), Some(&selected));
        assert_eq!(backend.calls.get(), 1);
        let durable = fixture
            .store
            .load_goal(&fixture.session.id, &goal_id)
            .unwrap();
        assert_eq!(durable.tasks()[&selected].status(), TaskStatus::Verifying);
        assert_eq!(
            durable
                .tasks()
                .values()
                .filter(|task| task.status() == TaskStatus::Ready)
                .count(),
            1
        );
    }
}
