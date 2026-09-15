use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::config;
use crate::fallback::{SideEffectClass, SideEffectState};
use crate::goal::{CheckpointReason, Goal, GoalId, GoalStatus};
use crate::orchestrator_error::OrchestratorError;
use crate::task::{
    ReplaySafety, TaskOperationKind, TaskStatus, TaskTransitionContext, VerificationOutcome,
    WorkerKind,
};
use crate::task_store::{TaskStore, utc_now_rfc3339};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct GoalIdentityView {
    goal_id: String,
    status: GoalStatus,
    revision: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct GoalApiError {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_goal: Option<GoalIdentityView>,
}

impl GoalApiError {
    pub(crate) fn code(&self) -> &'static str {
        self.code
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "INVALID_ARGUMENT",
            message: message.into(),
            active_goal: None,
        }
    }

    fn idempotency_conflict(goal: &Goal) -> Self {
        Self {
            code: "IDEMPOTENCY_CONFLICT",
            message: "idempotency_key is already bound to a different goal_start payload"
                .to_owned(),
            active_goal: Some(identity_view(goal)),
        }
    }

    pub(crate) fn from_orchestrator(error: OrchestratorError) -> Self {
        Self::from_orchestrator_with_active(error, None)
    }

    fn from_orchestrator_with_active(
        error: OrchestratorError,
        active_goal: Option<GoalIdentityView>,
    ) -> Self {
        let (code, message) = match error {
            OrchestratorError::GoalNotFound => (
                "GOAL_NOT_FOUND",
                "goal was not found for this session".to_owned(),
            ),
            OrchestratorError::ActiveGoalAlreadyExists => (
                "ACTIVE_GOAL_ALREADY_EXISTS",
                "a non-terminal goal already exists for this session".to_owned(),
            ),
            OrchestratorError::InvalidTransition {
                entity,
                from,
                to,
                reason,
            } => (
                "GOAL_STATE_CONFLICT",
                format!("invalid {entity} transition {from} -> {to}: {reason}"),
            ),
            OrchestratorError::RevisionConflict { expected, actual } => (
                "REVISION_CONFLICT",
                format!(
                    "goal revision conflict: expected {expected}, current revision is {actual}"
                ),
            ),
            OrchestratorError::CorruptGoal(reason) => (
                "GOAL_STATE_CORRUPT",
                format!("durable goal state is corrupt: {reason}"),
            ),
            OrchestratorError::UnsupportedSchema(version) => (
                "UNSUPPORTED_GOAL_SCHEMA",
                format!("unsupported durable goal schema version {version}"),
            ),
            OrchestratorError::SchemaUpgradeRequired(version) => (
                "SCHEMA_UPGRADE_REQUIRED",
                format!(
                    "goal schema version {version} requires explicit structured authority upgrade"
                ),
            ),
            OrchestratorError::UnsafeIdentifier(kind) => {
                ("INVALID_ARGUMENT", format!("unsafe {kind} identifier"))
            }
            OrchestratorError::PersistenceIo(_) | OrchestratorError::Serialization(_) => (
                "GOAL_PERSISTENCE_ERROR",
                "durable goal persistence failed".to_owned(),
            ),
            OrchestratorError::InvalidDag(reason) => (
                "INVALID_GOAL_STATE",
                format!("invalid durable task DAG: {reason}"),
            ),
            OrchestratorError::RecoveryBlocked(reason) => (
                "RECOVERY_BLOCKED",
                format!("goal recovery is blocked: {reason}"),
            ),
        };
        Self {
            code,
            message,
            active_goal,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalStartRequest {
    session_id: String,
    objective: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    constraints: Vec<String>,
    #[serde(default)]
    completion_criteria: Vec<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalLookupRequest {
    session_id: String,
    #[serde(default)]
    goal_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalReasonRequest {
    session_id: String,
    #[serde(default)]
    goal_id: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalResultRequest {
    session_id: String,
    goal_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct GoalStartView {
    goal_id: String,
    session_id: String,
    status: GoalStatus,
    plan_revision: u32,
    revision: u64,
    created_at: String,
    idempotent_replay: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TaskSummaryView {
    task_id: String,
    title: String,
    status: TaskStatus,
    mandatory: bool,
    dependencies: Vec<String>,
    blocker_count: usize,
    verification_results: usize,
    evidence_items: usize,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct BlockerView {
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    code: String,
    detail: String,
    mandatory: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct VerificationSummaryView {
    final_outcome: Option<VerificationOutcome>,
    final_verification_id: Option<String>,
    final_check_count: usize,
    final_started_at: Option<String>,
    final_finished_at: Option<String>,
    task_verification_results: usize,
    passed_task_verifications: usize,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CheckpointSummaryView {
    checkpoint_id: String,
    goal_revision: u64,
    plan_revision: u32,
    at: String,
    reason: CheckpointReason,
    goal_status: GoalStatus,
    active_task_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct GoalStatusView {
    goal_id: String,
    session_id: String,
    status: GoalStatus,
    objective: String,
    title: Option<String>,
    revision: u64,
    plan_revision: u32,
    created_at: String,
    updated_at: String,
    terminal: bool,
    result_available: bool,
    task_counts: BTreeMap<&'static str, usize>,
    tasks: Vec<TaskSummaryView>,
    active_tasks: Vec<String>,
    next_runnable_tasks: Vec<String>,
    blockers: Vec<BlockerView>,
    verification: VerificationSummaryView,
    latest_checkpoint: Option<CheckpointSummaryView>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct GoalResultView {
    goal_id: String,
    session_id: String,
    status: GoalStatus,
    terminal: bool,
    result_state: &'static str,
    objective: String,
    revision: u64,
    plan_revision: u32,
    mandatory_task_outcomes: Vec<TaskSummaryView>,
    blockers: Vec<BlockerView>,
    verification: VerificationSummaryView,
    evidence_items: usize,
    latest_checkpoint: Option<CheckpointSummaryView>,
    created_at: String,
    completed_at: Option<String>,
}

pub(crate) fn goal_start(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<GoalStartView, GoalApiError> {
    let request: GoalStartRequest = parse_request(args)?;
    validate_session_binding(&request.session_id, session)?;
    validate_start_request(&request)?;

    let deterministic_id = request
        .idempotency_key
        .as_deref()
        .map(|key| idempotent_goal_id(&session.id, key));

    if let Some(goal_id) = deterministic_id.as_ref() {
        match store.load_goal(&session.id, goal_id) {
            Ok(existing) => {
                if same_start_payload(&existing, &request) {
                    return Ok(start_view(&existing, true));
                }
                return Err(GoalApiError::idempotency_conflict(&existing));
            }
            Err(OrchestratorError::GoalNotFound) => {}
            Err(error) => return Err(GoalApiError::from_orchestrator(error)),
        }
    }

    let now = utc_now_rfc3339();
    let goal = match deterministic_id.clone() {
        Some(goal_id) => Goal::new_with_id(
            goal_id,
            session.id.clone(),
            session.cwd.clone(),
            request.objective.clone(),
            request.title.clone(),
            request.constraints.clone(),
            request.completion_criteria.clone(),
            &now,
        ),
        None => Goal::new(
            session.id.clone(),
            session.cwd.clone(),
            request.objective.clone(),
            request.title.clone(),
            request.constraints.clone(),
            request.completion_criteria.clone(),
            &now,
        ),
    }
    .map_err(GoalApiError::from_orchestrator)?;

    if let Err(error) = store.create_goal(&goal) {
        if matches!(error, OrchestratorError::ActiveGoalAlreadyExists)
            && let Some(goal_id) = deterministic_id.as_ref()
        {
            match store.load_goal(&session.id, goal_id) {
                Ok(existing) if same_start_payload(&existing, &request) => {
                    return Ok(start_view(&existing, true));
                }
                Ok(existing) => return Err(GoalApiError::idempotency_conflict(&existing)),
                Err(OrchestratorError::GoalNotFound) => {}
                Err(load_error) => return Err(GoalApiError::from_orchestrator(load_error)),
            }
        }
        let active_goal = if matches!(error, OrchestratorError::ActiveGoalAlreadyExists) {
            store
                .load_active_goal(&session.id)
                .ok()
                .flatten()
                .map(|active| identity_view(&active))
        } else {
            None
        };
        return Err(GoalApiError::from_orchestrator_with_active(
            error,
            active_goal,
        ));
    }

    let durable = store
        .load_goal(&session.id, goal.id())
        .map_err(GoalApiError::from_orchestrator)?;
    Ok(start_view(&durable, false))
}

pub(crate) fn goal_status(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<GoalStatusView, GoalApiError> {
    let request: GoalLookupRequest = parse_request(args)?;
    validate_session_binding(&request.session_id, session)?;
    let goal = resolve_goal(store, &session.id, request.goal_id.as_deref())?;
    Ok(status_view(&goal))
}

pub(crate) fn goal_pause(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<GoalStatusView, GoalApiError> {
    let request: GoalReasonRequest = parse_request(args)?;
    validate_session_binding(&request.session_id, session)?;
    validate_reason(request.reason.as_deref())?;
    let current = resolve_goal(store, &session.id, request.goal_id.as_deref())?;
    let goal_id = current.id().clone();
    let expected_revision = current.revision();
    let durable = store
        .mutate_goal_snapshot(&session.id, &goal_id, expected_revision, |goal, now| {
            pause_goal(goal, now)
        })
        .map_err(GoalApiError::from_orchestrator)?;
    Ok(status_view(&durable))
}

pub(crate) fn goal_resume(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<GoalStatusView, GoalApiError> {
    let request: GoalLookupRequest = parse_request(args)?;
    validate_session_binding(&request.session_id, session)?;
    let current = resolve_goal(store, &session.id, request.goal_id.as_deref())?;
    let goal_id = current.id().clone();
    let expected_revision = current.revision();
    let durable = store
        .mutate_goal_snapshot(&session.id, &goal_id, expected_revision, |goal, now| {
            resume_goal(goal, now)
        })
        .map_err(GoalApiError::from_orchestrator)?;
    Ok(status_view(&durable))
}

pub(crate) fn goal_cancel(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<GoalStatusView, GoalApiError> {
    let request: GoalReasonRequest = parse_request(args)?;
    validate_session_binding(&request.session_id, session)?;
    validate_reason(request.reason.as_deref())?;
    let current = resolve_goal(store, &session.id, request.goal_id.as_deref())?;
    let goal_id = current.id().clone();
    let expected_revision = current.revision();
    let durable = store
        .mutate_goal_snapshot(&session.id, &goal_id, expected_revision, |goal, now| {
            cancel_goal(goal, now)
        })
        .map_err(GoalApiError::from_orchestrator)?;
    Ok(status_view(&durable))
}

pub(crate) fn goal_result(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<GoalResultView, GoalApiError> {
    let request: GoalResultRequest = parse_request(args)?;
    validate_session_binding(&request.session_id, session)?;
    let goal_id = parse_goal_id(&request.goal_id)?;
    let goal = store
        .load_goal(&session.id, &goal_id)
        .map_err(GoalApiError::from_orchestrator)?;
    Ok(result_view(&goal))
}

fn parse_request<T: DeserializeOwned>(args: &Value) -> Result<T, GoalApiError> {
    serde_json::from_value(args.clone())
        .map_err(|error| GoalApiError::invalid(format!("invalid goal tool arguments: {error}")))
}

fn validate_session_binding(
    request_session_id: &str,
    session: &config::Session,
) -> Result<(), GoalApiError> {
    config::validate_session_id(request_session_id)
        .map_err(|_| GoalApiError::invalid("unsafe session identifier"))?;
    if request_session_id != session.id {
        return Err(GoalApiError::invalid(
            "session_id does not match the resolved Local MCP session",
        ));
    }
    Ok(())
}

fn validate_start_request(request: &GoalStartRequest) -> Result<(), GoalApiError> {
    if request.objective.trim().is_empty() {
        return Err(GoalApiError::invalid("objective must not be empty"));
    }
    ensure_max_chars("objective", &request.objective, 131_072)?;
    if let Some(title) = request.title.as_deref() {
        ensure_max_chars("title", title, 256)?;
    }
    validate_string_list("constraints", &request.constraints, 64, 8_192)?;
    validate_string_list(
        "completion_criteria",
        &request.completion_criteria,
        64,
        8_192,
    )?;
    if let Some(key) = request.idempotency_key.as_deref() {
        ensure_max_chars("idempotency_key", key, 128)?;
    }
    Ok(())
}

fn validate_reason(reason: Option<&str>) -> Result<(), GoalApiError> {
    if let Some(reason) = reason {
        ensure_max_chars("reason", reason, 8_192)?;
    }
    Ok(())
}

fn ensure_max_chars(name: &str, value: &str, max: usize) -> Result<(), GoalApiError> {
    if value.chars().count() > max {
        return Err(GoalApiError::invalid(format!(
            "{name} exceeds maximum length {max}"
        )));
    }
    Ok(())
}

fn validate_string_list(
    name: &str,
    values: &[String],
    max_items: usize,
    max_chars: usize,
) -> Result<(), GoalApiError> {
    if values.len() > max_items {
        return Err(GoalApiError::invalid(format!(
            "{name} exceeds maximum item count {max_items}"
        )));
    }
    for value in values {
        ensure_max_chars(name, value, max_chars)?;
    }
    Ok(())
}

fn parse_goal_id(value: &str) -> Result<GoalId, GoalApiError> {
    let parsed = GoalId::parse(value).map_err(GoalApiError::from_orchestrator)?;
    if parsed.as_str() != value {
        return Err(GoalApiError::invalid(
            "goal_id must be a canonical lowercase UUID",
        ));
    }
    Ok(parsed)
}

fn resolve_goal(
    store: &TaskStore,
    session_id: &str,
    goal_id: Option<&str>,
) -> Result<Goal, GoalApiError> {
    match goal_id {
        Some(value) => {
            let goal_id = parse_goal_id(value)?;
            store
                .load_goal(session_id, &goal_id)
                .map_err(GoalApiError::from_orchestrator)
        }
        None => store
            .load_active_goal(session_id)
            .map_err(GoalApiError::from_orchestrator)?
            .ok_or_else(|| GoalApiError::from_orchestrator(OrchestratorError::GoalNotFound)),
    }
}

fn pause_goal(goal: &mut Goal, now: &str) -> Result<(), OrchestratorError> {
    match goal.status() {
        GoalStatus::Running | GoalStatus::Replanning | GoalStatus::Verifying => {
            goal.transition_to(GoalStatus::Pausing, now)?;
            if goal.has_unknown_side_effects() {
                goal.transition_to(GoalStatus::Blocked, now)?;
            } else if !goal.has_active_tasks() {
                goal.transition_to(GoalStatus::Paused, now)?;
                goal.add_checkpoint(CheckpointReason::Pause, now)?;
            }
        }
        GoalStatus::Pausing => {
            if goal.has_unknown_side_effects() {
                goal.transition_to(GoalStatus::Blocked, now)?;
            } else if !goal.has_active_tasks() {
                goal.transition_to(GoalStatus::Paused, now)?;
                goal.add_checkpoint(CheckpointReason::Pause, now)?;
            }
        }
        GoalStatus::Blocked => {
            if goal.has_active_tasks() {
                return Err(OrchestratorError::invalid_transition(
                    "goal",
                    "BLOCKED",
                    "PAUSED",
                    "active task state must be reconciled before pausing a blocked goal",
                ));
            }
            goal.transition_to(GoalStatus::Paused, now)?;
            goal.add_checkpoint(CheckpointReason::Pause, now)?;
        }
        status => {
            return Err(OrchestratorError::invalid_transition(
                "goal",
                goal_status_name(status),
                "PAUSED",
                "goal is not in a pausable V1 state",
            ));
        }
    }
    Ok(())
}

fn resume_goal(goal: &mut Goal, now: &str) -> Result<(), OrchestratorError> {
    if goal.is_terminal() {
        return Err(OrchestratorError::invalid_transition(
            "goal",
            goal_status_name(goal.status()),
            "RUNNING",
            "terminal goal state is immutable",
        ));
    }

    goal.recover_stale_running(now)?;
    reconcile_safe_readonly_blocked_tasks(goal, now)?;

    match goal.status() {
        GoalStatus::Paused => resume_from_paused(goal, now)?,
        GoalStatus::Pausing => {
            if goal.has_unknown_side_effects() {
                goal.transition_to(GoalStatus::Blocked, now)?;
            } else if !goal.has_active_tasks() {
                goal.transition_to(GoalStatus::Paused, now)?;
                resume_from_paused(goal, now)?;
            }
        }
        GoalStatus::Blocked => {
            if !goal.has_blocking_state() {
                resume_ready_goal(goal, now)?;
            }
        }
        GoalStatus::Planning
        | GoalStatus::Running
        | GoalStatus::Replanning
        | GoalStatus::Verifying
        | GoalStatus::Cancelling => {}
        GoalStatus::Completed | GoalStatus::Failed | GoalStatus::Cancelled => unreachable!(),
    }
    Ok(())
}

fn reconcile_safe_readonly_blocked_tasks(
    goal: &mut Goal,
    now: &str,
) -> Result<(), OrchestratorError> {
    let task_ids = goal
        .tasks()
        .iter()
        .filter_map(|(task_id, task)| {
            let attempt = task.latest_attempt()?;
            let only_readonly_blockers = !task.blockers().is_empty()
                && task
                    .blockers()
                    .iter()
                    .all(|blocker| blocker.code() == "READONLY_BLOCKED");
            let safe_scope = task.worker() == WorkerKind::CodexReadonly
                && task.scope().operation_kind() == TaskOperationKind::ReadOnly
                && task.scope().replay_safety() == ReplaySafety::SafeReadOnly;
            let no_mutation_authority = attempt.operation_id().is_none()
                && attempt.scope_identity().is_none()
                && attempt.side_effect_class() == Some(SideEffectClass::None)
                && attempt.side_effect_state() == Some(SideEffectState::ConfirmedNotPerformed);
            let budget_remains = task.attempts().len() < task.max_attempts() as usize
                && attempt.remaining_attempt_budget().unwrap_or(0) > 0;
            (task.status() == TaskStatus::Blocked
                && only_readonly_blockers
                && safe_scope
                && no_mutation_authority
                && budget_remains)
                .then(|| task_id.clone())
        })
        .collect::<Vec<_>>();

    for task_id in task_ids {
        goal.task_clear_blockers(&task_id)?;
        goal.transition_task(
            &task_id,
            TaskStatus::Ready,
            TaskTransitionContext::default(),
            now,
        )?;
    }
    Ok(())
}

fn resume_from_paused(goal: &mut Goal, now: &str) -> Result<(), OrchestratorError> {
    if goal.has_blocking_state() {
        goal.transition_to(GoalStatus::Blocked, now)
    } else {
        resume_ready_goal(goal, now)
    }
}

fn resume_ready_goal(goal: &mut Goal, now: &str) -> Result<(), OrchestratorError> {
    let next = if goal.has_task_status(TaskStatus::NeedsReplan) {
        GoalStatus::Replanning
    } else {
        GoalStatus::Running
    };
    goal.transition_to(next, now)
}

fn cancel_goal(goal: &mut Goal, now: &str) -> Result<(), OrchestratorError> {
    if goal.is_terminal() {
        return Err(OrchestratorError::invalid_transition(
            "goal",
            goal_status_name(goal.status()),
            "CANCELLING",
            "terminal goal state is immutable",
        ));
    }
    if goal.status() != GoalStatus::Cancelling {
        goal.transition_to(GoalStatus::Cancelling, now)?;
    }
    if goal.has_unknown_side_effects() {
        goal.transition_to(GoalStatus::Blocked, now)?;
    } else if !goal.has_active_tasks() {
        goal.transition_to(GoalStatus::Cancelled, now)?;
    }
    Ok(())
}

fn start_view(goal: &Goal, idempotent_replay: bool) -> GoalStartView {
    GoalStartView {
        goal_id: goal.id().as_str().to_owned(),
        session_id: goal.session_id().to_owned(),
        status: goal.status(),
        plan_revision: goal.plan_revision(),
        revision: goal.revision(),
        created_at: goal.created_at().to_owned(),
        idempotent_replay,
    }
}

fn identity_view(goal: &Goal) -> GoalIdentityView {
    GoalIdentityView {
        goal_id: goal.id().as_str().to_owned(),
        status: goal.status(),
        revision: goal.revision(),
    }
}

fn status_view(goal: &Goal) -> GoalStatusView {
    let tasks = task_views(goal);
    let active_tasks = goal
        .tasks()
        .iter()
        .filter_map(|(id, task)| {
            matches!(task.status(), TaskStatus::Running | TaskStatus::Verifying)
                .then(|| id.as_str().to_owned())
        })
        .collect();
    let next_runnable_tasks = goal
        .tasks()
        .iter()
        .filter_map(|(id, task)| {
            (task.status() == TaskStatus::Ready).then(|| id.as_str().to_owned())
        })
        .collect();
    GoalStatusView {
        goal_id: goal.id().as_str().to_owned(),
        session_id: goal.session_id().to_owned(),
        status: goal.status(),
        objective: goal.objective().to_owned(),
        title: goal.title().map(str::to_owned),
        revision: goal.revision(),
        plan_revision: goal.plan_revision(),
        created_at: goal.created_at().to_owned(),
        updated_at: goal.updated_at().to_owned(),
        terminal: goal.is_terminal(),
        result_available: goal.is_terminal(),
        task_counts: task_counts(goal),
        tasks,
        active_tasks,
        next_runnable_tasks,
        blockers: blocker_views(goal),
        verification: verification_summary(goal),
        latest_checkpoint: latest_checkpoint(goal),
    }
}

fn result_view(goal: &Goal) -> GoalResultView {
    let mandatory_task_outcomes = task_views(goal)
        .into_iter()
        .filter(|task| task.mandatory)
        .collect();
    let evidence_items = goal
        .tasks()
        .values()
        .map(|task| task.evidence_count())
        .sum();
    GoalResultView {
        goal_id: goal.id().as_str().to_owned(),
        session_id: goal.session_id().to_owned(),
        status: goal.status(),
        terminal: goal.is_terminal(),
        result_state: if goal.is_terminal() {
            "TERMINAL"
        } else {
            "NOT_TERMINAL"
        },
        objective: goal.objective().to_owned(),
        revision: goal.revision(),
        plan_revision: goal.plan_revision(),
        mandatory_task_outcomes,
        blockers: blocker_views(goal),
        verification: verification_summary(goal),
        evidence_items,
        latest_checkpoint: latest_checkpoint(goal),
        created_at: goal.created_at().to_owned(),
        completed_at: goal.completed_at().map(str::to_owned),
    }
}

fn task_views(goal: &Goal) -> Vec<TaskSummaryView> {
    goal.tasks()
        .iter()
        .map(|(id, task)| TaskSummaryView {
            task_id: id.as_str().to_owned(),
            title: task.title().to_owned(),
            status: task.status(),
            mandatory: task.mandatory(),
            dependencies: task
                .dependencies()
                .iter()
                .map(|dependency| dependency.task_id().as_str().to_owned())
                .collect(),
            blocker_count: task.blockers().len(),
            verification_results: task.verification_results().len(),
            evidence_items: task.evidence_count(),
        })
        .collect()
}

fn task_counts(goal: &Goal) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for status in [
        TaskStatus::Pending,
        TaskStatus::Ready,
        TaskStatus::Running,
        TaskStatus::Blocked,
        TaskStatus::Retryable,
        TaskStatus::NeedsReplan,
        TaskStatus::Verifying,
        TaskStatus::Completed,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
    ] {
        counts.insert(
            task_status_name(status),
            goal.tasks()
                .values()
                .filter(|task| task.status() == status)
                .count(),
        );
    }
    counts
}

fn blocker_views(goal: &Goal) -> Vec<BlockerView> {
    let mut blockers = goal
        .blockers()
        .iter()
        .map(|blocker| BlockerView {
            source: "GOAL",
            task_id: None,
            code: blocker.code().to_owned(),
            detail: blocker.detail().to_owned(),
            mandatory: blocker.mandatory(),
        })
        .collect::<Vec<_>>();
    for (task_id, task) in goal.tasks() {
        blockers.extend(task.blockers().iter().map(|blocker| BlockerView {
            source: "TASK",
            task_id: Some(task_id.as_str().to_owned()),
            code: blocker.code().to_owned(),
            detail: blocker.detail().to_owned(),
            mandatory: blocker.mandatory(),
        }));
    }
    blockers
}

fn verification_summary(goal: &Goal) -> VerificationSummaryView {
    let task_verification_results = goal
        .tasks()
        .values()
        .map(|task| task.verification_results().len())
        .sum();
    let passed_task_verifications = goal
        .tasks()
        .values()
        .flat_map(|task| task.verification_results())
        .filter(|result| result.outcome() == VerificationOutcome::Passed)
        .count();
    if let Some(result) = goal.latest_applicable_final_verification() {
        return VerificationSummaryView {
            final_outcome: Some(result.outcome()),
            final_verification_id: Some(result.id().as_str().to_owned()),
            final_check_count: result
                .criterion_results()
                .iter()
                .map(|criterion| criterion.observations().len())
                .sum(),
            final_started_at: Some(result.started_at().to_owned()),
            final_finished_at: Some(result.finished_at().to_owned()),
            task_verification_results,
            passed_task_verifications,
        };
    }
    let legacy = goal.legacy_final_verification();
    VerificationSummaryView {
        final_outcome: legacy.map(|result| result.outcome()),
        final_verification_id: legacy.map(|result| result.id().as_str().to_owned()),
        final_check_count: legacy.map_or(0, |result| result.check_count()),
        final_started_at: legacy.map(|result| result.started_at().to_owned()),
        final_finished_at: legacy.map(|result| result.finished_at().to_owned()),
        task_verification_results,
        passed_task_verifications,
    }
}

fn latest_checkpoint(goal: &Goal) -> Option<CheckpointSummaryView> {
    goal.checkpoints()
        .last()
        .map(|checkpoint| CheckpointSummaryView {
            checkpoint_id: checkpoint.id().to_owned(),
            goal_revision: checkpoint.goal_revision(),
            plan_revision: checkpoint.plan_revision(),
            at: checkpoint.at().to_owned(),
            reason: checkpoint.reason(),
            goal_status: checkpoint.goal_status(),
            active_task_ids: checkpoint
                .active_task_ids()
                .iter()
                .map(|id| id.as_str().to_owned())
                .collect(),
        })
}

fn same_start_payload(goal: &Goal, request: &GoalStartRequest) -> bool {
    let expected_criteria = if request.completion_criteria.is_empty() {
        vec![request.objective.clone()]
    } else {
        request.completion_criteria.clone()
    };
    goal.objective() == request.objective
        && goal.title() == request.title.as_deref()
        && goal.constraints() == request.constraints
        && goal.completion_criterion_descriptions() == expected_criteria
}

fn idempotent_goal_id(session_id: &str, key: &str) -> GoalId {
    const FNV_OFFSET_128: u128 = 0x6c62272e07bb014262b821756295c58d;
    const FNV_PRIME_128: u128 = 0x0000000001000000000000000000013b;
    let mut hash = FNV_OFFSET_128;
    for byte in b"local-mcp-goal-idempotency-v1\0"
        .iter()
        .copied()
        .chain(session_id.as_bytes().iter().copied())
        .chain(std::iter::once(0))
        .chain(key.as_bytes().iter().copied())
    {
        hash ^= u128::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME_128);
    }
    let mut bytes = hash.to_be_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    GoalId::parse(&Uuid::from_bytes(bytes).to_string()).expect("generated UUID is canonical")
}

fn goal_status_name(status: GoalStatus) -> &'static str {
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

fn task_status_name(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "PENDING",
        TaskStatus::Ready => "READY",
        TaskStatus::Running => "RUNNING",
        TaskStatus::Blocked => "BLOCKED",
        TaskStatus::Retryable => "RETRYABLE",
        TaskStatus::NeedsReplan => "NEEDS_REPLAN",
        TaskStatus::Verifying => "VERIFYING",
        TaskStatus::Completed => "COMPLETED",
        TaskStatus::Failed => "FAILED",
        TaskStatus::Cancelled => "CANCELLED",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::fallback::{SideEffectClass, SideEffectState};
    use crate::goal::CheckpointReason;
    use crate::task::{
        ReplaySafety, TaskEvidence, TaskOperationKind, TaskScope, TaskStatus,
        TaskTransitionContext, VerificationResult, WorkerKind,
    };
    use crate::task_store::FaultPoint;

    const NOW: &str = "2026-01-01T00:00:00Z";

    fn fixture(name: &str) -> (PathBuf, config::Session, TaskStore) {
        let root = std::env::temp_dir().join(format!("local-mcp-phase3-{name}-{}", Uuid::new_v4()));
        let workspace = root.join("workspace");
        let state = root.join("state");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let session = config::Session {
            id: format!("phase3-{name}-{}", Uuid::new_v4()),
            cwd: workspace.clone(),
            permitted_directories: vec![workspace],
        };
        let store = TaskStore::with_state_root(state);
        (root, session, store)
    }

    fn start_args(session: &config::Session, objective: &str) -> Value {
        serde_json::json!({
            "session_id": session.id,
            "objective": objective,
            "title": "Phase 3 goal",
            "constraints": ["stay inert"],
            "completion_criteria": ["durable state only"]
        })
    }

    fn read_goal_bytes(root: &PathBuf, session: &config::Session, goal_id: &str) -> Vec<u8> {
        std::fs::read(
            root.join("state")
                .join("goals")
                .join(&session.id)
                .join(format!("{goal_id}.json")),
        )
        .unwrap()
    }

    fn read_only_scope() -> TaskScope {
        TaskScope::new(
            vec![PathBuf::from("src")],
            vec![],
            TaskOperationKind::ReadOnly,
            ReplaySafety::SafeReadOnly,
        )
    }

    fn mutation_scope() -> TaskScope {
        TaskScope::new(
            vec![PathBuf::from("src")],
            vec![],
            TaskOperationKind::LocalMutation,
            ReplaySafety::VerifyBeforeRetry,
        )
    }

    fn running_goal(
        session: &config::Session,
        scope: TaskScope,
        worker: WorkerKind,
        task_running: bool,
    ) -> (Goal, crate::task::TaskId) {
        let mut goal = Goal::new(
            session.id.clone(),
            session.cwd.clone(),
            "running objective",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        let task_id = goal
            .add_task(
                "task",
                "task objective",
                true,
                worker,
                scope,
                vec![],
                3,
                NOW,
            )
            .unwrap();
        goal.transition_to(GoalStatus::Running, NOW).unwrap();
        if task_running {
            goal.transition_task(
                &task_id,
                TaskStatus::Ready,
                TaskTransitionContext::default(),
                NOW,
            )
            .unwrap();
            goal.transition_task(
                &task_id,
                TaskStatus::Running,
                TaskTransitionContext::default(),
                NOW,
            )
            .unwrap();
        }
        (goal, task_id)
    }

    fn completed_goal(session: &config::Session) -> Goal {
        let mut goal = Goal::new(
            session.id.clone(),
            session.cwd.clone(),
            "completed objective",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        let task_id = goal
            .add_task(
                "complete task",
                "complete task objective",
                true,
                WorkerKind::Verifier,
                read_only_scope(),
                vec![],
                1,
                NOW,
            )
            .unwrap();
        goal.transition_to(GoalStatus::Running, NOW).unwrap();
        goal.transition_task(
            &task_id,
            TaskStatus::Ready,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
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
        goal.task_record_verification_result(
            &task_id,
            VerificationResult::new(VerificationOutcome::Passed, vec![], NOW, NOW),
        )
        .unwrap();
        goal.transition_task(
            &task_id,
            TaskStatus::Completed,
            TaskTransitionContext {
                active_worker_stopped: true,
                side_effect_reconciled: true,
            },
            NOW,
        )
        .unwrap();
        goal.enter_verifying_for_test(NOW).unwrap();
        goal.record_final_verification(VerificationResult::new(
            VerificationOutcome::Passed,
            vec![],
            NOW,
            NOW,
        ))
        .unwrap();
        goal.add_checkpoint(CheckpointReason::FinalVerification, NOW)
            .unwrap();
        crate::goal_finalizer::complete_goal_for_test(&mut goal, NOW).unwrap();
        goal
    }

    #[test]
    fn start_persists_reloads_and_does_not_touch_workspace() {
        let (root, session, store) = fixture("start");
        let sentinel = session.cwd.join("sentinel.txt");
        std::fs::write(&sentinel, "unchanged").unwrap();

        let started =
            goal_start(&start_args(&session, "durable objective"), &session, &store).unwrap();
        assert_eq!(started.status, GoalStatus::Planning);
        assert_eq!(started.revision, 1);
        assert_eq!(started.plan_revision, 0);
        assert!(!started.idempotent_replay);

        let loaded = store
            .load_goal(&session.id, &GoalId::parse(&started.goal_id).unwrap())
            .unwrap();
        assert_eq!(loaded.objective(), "durable objective");
        assert_eq!(loaded.session_id(), session.id);
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "unchanged");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn start_idempotency_is_durable_and_distinct_active_goal_conflicts() {
        let (root, session, store) = fixture("idempotency");
        let mut args = start_args(&session, "same objective");
        args["idempotency_key"] = Value::String("request-1".to_owned());
        let first = goal_start(&args, &session, &store).unwrap();
        let replay = goal_start(&args, &session, &store).unwrap();
        assert_eq!(first.goal_id, replay.goal_id);
        assert!(replay.idempotent_replay);
        assert_eq!(store.list_goals_for_session(&session.id).unwrap().len(), 1);

        let mut other = start_args(&session, "different objective");
        other["idempotency_key"] = Value::String("request-2".to_owned());
        let error = goal_start(&other, &session, &store).unwrap_err();
        assert_eq!(error.code(), "ACTIVE_GOAL_ALREADY_EXISTS");
        assert!(error.active_goal.is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn idempotency_key_reuse_with_different_payload_is_rejected() {
        let (root, session, store) = fixture("idempotency-conflict");
        let mut first = start_args(&session, "first objective");
        first["idempotency_key"] = Value::String("stable-key".to_owned());
        goal_start(&first, &session, &store).unwrap();

        let mut changed = start_args(&session, "changed objective");
        changed["idempotency_key"] = Value::String("stable-key".to_owned());
        let error = goal_start(&changed, &session, &store).unwrap_err();
        assert_eq!(error.code(), "IDEMPOTENCY_CONFLICT");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn terminal_history_allows_a_new_distinct_goal() {
        let (root, session, store) = fixture("history");
        let first = goal_start(&start_args(&session, "first"), &session, &store).unwrap();
        let cancelled = goal_cancel(
            &serde_json::json!({"session_id": session.id, "goal_id": first.goal_id}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(cancelled.status, GoalStatus::Cancelled);

        let second = goal_start(&start_args(&session, "second"), &session, &store).unwrap();
        assert_ne!(first.goal_id, second.goal_id);
        assert_eq!(store.list_goals_for_session(&session.id).unwrap().len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_and_forged_start_arguments_are_rejected() {
        let (root, session, store) = fixture("validation");
        for args in [
            serde_json::json!({"session_id": session.id, "objective": 7}),
            serde_json::json!({"session_id": session.id, "objective": "   "}),
            serde_json::json!({"session_id": session.id, "objective": "x", "revision": 99}),
            serde_json::json!({"session_id": session.id, "objective": "x", "schema_version": 1}),
        ] {
            assert_eq!(
                goal_start(&args, &session, &store).unwrap_err().code(),
                "INVALID_ARGUMENT"
            );
        }
        let unsafe_id = serde_json::json!({"session_id": session.id, "goal_id": "../escape"});
        assert_eq!(
            goal_status(&unsafe_id, &session, &store)
                .unwrap_err()
                .code(),
            "INVALID_ARGUMENT"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_is_strictly_read_only() {
        let (root, session, store) = fixture("status-readonly");
        let started = goal_start(&start_args(&session, "observe only"), &session, &store).unwrap();
        let before = read_goal_bytes(&root, &session, &started.goal_id);
        let loaded_before = store
            .load_goal(&session.id, &GoalId::parse(&started.goal_id).unwrap())
            .unwrap();

        let status = goal_status(
            &serde_json::json!({"session_id": session.id}),
            &session,
            &store,
        )
        .unwrap();
        let after = read_goal_bytes(&root, &session, &started.goal_id);
        let loaded_after = store
            .load_goal(&session.id, &GoalId::parse(&started.goal_id).unwrap())
            .unwrap();
        assert_eq!(status.revision, 1);
        assert_eq!(before, after);
        assert_eq!(loaded_before.revision(), loaded_after.revision());
        assert_eq!(loaded_before.updated_at(), loaded_after.updated_at());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn no_active_goal_and_cross_session_lookup_are_isolated() {
        let (root, session_a, store) = fixture("cross-session");
        let workspace_b = root.join("workspace-b");
        std::fs::create_dir_all(&workspace_b).unwrap();
        let session_b = config::Session {
            id: format!("phase3-b-{}", Uuid::new_v4()),
            cwd: workspace_b.clone(),
            permitted_directories: vec![workspace_b],
        };
        assert_eq!(
            goal_status(
                &serde_json::json!({"session_id": session_a.id}),
                &session_a,
                &store
            )
            .unwrap_err()
            .code(),
            "GOAL_NOT_FOUND"
        );
        let a = goal_start(&start_args(&session_a, "A"), &session_a, &store).unwrap();
        let b = goal_start(&start_args(&session_b, "B"), &session_b, &store).unwrap();
        assert_ne!(a.goal_id, b.goal_id);
        let leaked = goal_status(
            &serde_json::json!({"session_id": session_b.id, "goal_id": a.goal_id}),
            &session_b,
            &store,
        )
        .unwrap_err();
        assert_eq!(leaked.code(), "GOAL_NOT_FOUND");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pause_and_resume_use_validated_goal_transitions_without_task_execution() {
        let (root, session, store) = fixture("pause-resume");
        let (goal, task_id) = running_goal(
            &session,
            read_only_scope(),
            WorkerKind::CodexReadonly,
            false,
        );
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();

        let paused = goal_pause(
            &serde_json::json!({"session_id": session.id}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(paused.status, GoalStatus::Paused);
        assert_eq!(paused.revision, 2);
        let durable_paused = store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(
            durable_paused.tasks()[&task_id].status(),
            TaskStatus::Pending
        );
        assert!(durable_paused.tasks()[&task_id].attempts().is_empty());
        assert_eq!(
            durable_paused.checkpoints().last().unwrap().reason(),
            CheckpointReason::Pause
        );

        let resumed = goal_resume(
            &serde_json::json!({"session_id": session.id}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(resumed.status, GoalStatus::Running);
        assert_eq!(resumed.revision, 3);
        let durable = store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(durable.tasks()[&task_id].status(), TaskStatus::Pending);
        assert!(durable.tasks()[&task_id].attempts().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pause_with_active_task_stays_pausing_and_does_not_touch_task_state() {
        let (root, session, store) = fixture("pausing");
        let (goal, task_id) =
            running_goal(&session, read_only_scope(), WorkerKind::CodexReadonly, true);
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let paused = goal_pause(
            &serde_json::json!({"session_id": session.id}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(paused.status, GoalStatus::Pausing);
        assert_eq!(
            store.load_goal(&session.id, &goal_id).unwrap().tasks()[&task_id].status(),
            TaskStatus::Running
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_pause_on_terminal_goal_preserves_bytes_and_revision() {
        let (root, session, store) = fixture("pause-terminal");
        let started = goal_start(&start_args(&session, "terminal"), &session, &store).unwrap();
        goal_cancel(
            &serde_json::json!({"session_id": session.id, "goal_id": started.goal_id}),
            &session,
            &store,
        )
        .unwrap();
        let before = read_goal_bytes(&root, &session, &started.goal_id);
        let revision = store
            .load_goal(&session.id, &GoalId::parse(&started.goal_id).unwrap())
            .unwrap()
            .revision();
        let error = goal_pause(
            &serde_json::json!({"session_id": session.id, "goal_id": started.goal_id}),
            &session,
            &store,
        )
        .unwrap_err();
        assert_eq!(error.code(), "GOAL_STATE_CONFLICT");
        assert_eq!(before, read_goal_bytes(&root, &session, &started.goal_id));
        assert_eq!(
            store
                .load_goal(&session.id, &GoalId::parse(&started.goal_id).unwrap())
                .unwrap()
                .revision(),
            revision
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resume_preserves_stale_readonly_recovery_to_retryable() {
        let (root, session, store) = fixture("resume-readonly");
        let (goal, task_id) =
            running_goal(&session, read_only_scope(), WorkerKind::CodexReadonly, true);
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let resumed = goal_resume(
            &serde_json::json!({"session_id": session.id}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(resumed.status, GoalStatus::Running);
        let durable = store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(durable.tasks()[&task_id].status(), TaskStatus::Retryable);
        assert_ne!(durable.tasks()[&task_id].status(), TaskStatus::Completed);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resume_preserves_unknown_mutation_recovery_to_blocked() {
        let (root, session, store) = fixture("resume-unknown");
        let (mut goal, task_id) =
            running_goal(&session, mutation_scope(), WorkerKind::LocalOperation, true);
        goal.task_bind_latest_attempt_execution(
            &task_id,
            Some("operation-unknown".into()),
            Some("scope-unknown".into()),
            Some("request-unknown".into()),
            Some(SideEffectClass::LocalMutation),
            Some(SideEffectState::Unknown),
            Some(1),
            Some(1),
        )
        .unwrap();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let resumed = goal_resume(
            &serde_json::json!({"session_id": session.id}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(resumed.status, GoalStatus::Blocked);
        assert_eq!(
            store.load_goal(&session.id, &goal_id).unwrap().tasks()[&task_id].status(),
            TaskStatus::Blocked
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resume_preserves_proven_mutation_recovery_to_verifying() {
        let (root, session, store) = fixture("resume-proven");
        let (mut goal, task_id) =
            running_goal(&session, mutation_scope(), WorkerKind::LocalOperation, true);
        goal.task_bind_latest_attempt_execution(
            &task_id,
            Some("operation-proven".into()),
            Some("scope-proven".into()),
            Some("request-proven".into()),
            Some(SideEffectClass::LocalMutation),
            Some(SideEffectState::ConfirmedPerformed),
            Some(0),
            Some(0),
        )
        .unwrap();
        goal.task_add_evidence(
            &task_id,
            TaskEvidence::RecoveryReconciliation {
                summary: "independent postcondition proof".into(),
                side_effect_state: SideEffectState::ConfirmedPerformed,
                postcondition_proven: true,
            },
        )
        .unwrap();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        goal_resume(
            &serde_json::json!({"session_id": session.id}),
            &session,
            &store,
        )
        .unwrap();
        let durable = store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(durable.tasks()[&task_id].status(), TaskStatus::Verifying);
        assert_ne!(durable.tasks()[&task_id].status(), TaskStatus::Completed);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn terminal_resume_is_rejected_transactionally() {
        let (root, session, store) = fixture("resume-terminal");
        let started = goal_start(&start_args(&session, "cancel me"), &session, &store).unwrap();
        goal_cancel(
            &serde_json::json!({"session_id": session.id, "goal_id": started.goal_id}),
            &session,
            &store,
        )
        .unwrap();
        let before = read_goal_bytes(&root, &session, &started.goal_id);
        let error = goal_resume(
            &serde_json::json!({"session_id": session.id, "goal_id": started.goal_id}),
            &session,
            &store,
        )
        .unwrap_err();
        assert_eq!(error.code(), "GOAL_STATE_CONFLICT");
        assert_eq!(before, read_goal_bytes(&root, &session, &started.goal_id));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancel_terminalizes_when_safe_and_result_reports_terminal_state() {
        let (root, session, store) = fixture("cancel-safe");
        let sentinel = session.cwd.join("repository-sentinel.txt");
        std::fs::write(&sentinel, "unchanged").unwrap();
        let started = goal_start(&start_args(&session, "cancel safely"), &session, &store).unwrap();
        let cancelled = goal_cancel(
            &serde_json::json!({"session_id": session.id}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(cancelled.status, GoalStatus::Cancelled);
        let result = goal_result(
            &serde_json::json!({"session_id": session.id, "goal_id": started.goal_id}),
            &session,
            &store,
        )
        .unwrap();
        assert!(result.terminal);
        assert_eq!(result.result_state, "TERMINAL");
        assert_eq!(result.status, GoalStatus::Cancelled);
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "unchanged");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancel_does_not_claim_terminal_state_while_goal_task_is_active() {
        let (root, session, store) = fixture("cancel-active");
        let (goal, task_id) =
            running_goal(&session, read_only_scope(), WorkerKind::CodexReadonly, true);
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let cancelled = goal_cancel(
            &serde_json::json!({"session_id": session.id}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(cancelled.status, GoalStatus::Cancelling);
        let durable = store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(durable.tasks()[&task_id].status(), TaskStatus::Running);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancel_unknown_side_effect_becomes_blocked_for_reconciliation() {
        let (root, session, store) = fixture("cancel-unknown");
        let (mut goal, task_id) =
            running_goal(&session, mutation_scope(), WorkerKind::LocalOperation, true);
        goal.task_bind_latest_attempt_execution(
            &task_id,
            Some("operation-cancel".into()),
            Some("scope-cancel".into()),
            Some("request-cancel".into()),
            Some(SideEffectClass::LocalMutation),
            Some(SideEffectState::Unknown),
            Some(1),
            Some(1),
        )
        .unwrap();
        store.create_goal(&goal).unwrap();
        let cancelled = goal_cancel(
            &serde_json::json!({"session_id": session.id}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(cancelled.status, GoalStatus::Blocked);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn result_distinguishes_nonterminal_failed_cancelled_and_completed() {
        let (root, session, store) = fixture("results");
        let started = goal_start(&start_args(&session, "nonterminal"), &session, &store).unwrap();
        let nonterminal = goal_result(
            &serde_json::json!({"session_id": session.id, "goal_id": started.goal_id}),
            &session,
            &store,
        )
        .unwrap();
        assert!(!nonterminal.terminal);
        assert_eq!(nonterminal.result_state, "NOT_TERMINAL");

        goal_cancel(
            &serde_json::json!({"session_id": session.id, "goal_id": started.goal_id}),
            &session,
            &store,
        )
        .unwrap();

        let mut failed = Goal::new(
            session.id.clone(),
            session.cwd.clone(),
            "failed objective",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        failed.transition_to(GoalStatus::Failed, NOW).unwrap();
        let failed_id = failed.id().clone();
        store.create_goal(&failed).unwrap();
        let failed_result = goal_result(
            &serde_json::json!({"session_id": session.id, "goal_id": failed_id.as_str()}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(failed_result.status, GoalStatus::Failed);
        assert!(failed_result.terminal);

        let completed = completed_goal(&session);
        let completed_id = completed.id().clone();
        store.create_goal(&completed).unwrap();
        let completed_result = goal_result(
            &serde_json::json!({"session_id": session.id, "goal_id": completed_id.as_str()}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(completed_result.status, GoalStatus::Completed);
        assert_eq!(
            completed_result.verification.final_outcome,
            Some(VerificationOutcome::Passed)
        );
        assert_eq!(completed_result.mandatory_task_outcomes.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_revision_conflict_does_not_overwrite_newer_bytes() {
        let (root, session, store) = fixture("revision");
        let (goal, _) = running_goal(
            &session,
            read_only_scope(),
            WorkerKind::CodexReadonly,
            false,
        );
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let revision = goal.revision();
        let first = store
            .mutate_goal_snapshot(&session.id, &goal_id, revision, |goal, now| {
                goal.transition_to(GoalStatus::Pausing, now)?;
                Ok(())
            })
            .unwrap();
        assert_eq!(first.revision(), revision + 1);
        let before = read_goal_bytes(&root, &session, goal_id.as_str());
        let error = store
            .mutate_goal_snapshot(&session.id, &goal_id, revision, |goal, now| {
                goal.transition_to(GoalStatus::Cancelling, now)?;
                Ok(())
            })
            .unwrap_err();
        assert!(matches!(error, OrchestratorError::RevisionConflict { .. }));
        assert_eq!(before, read_goal_bytes(&root, &session, goal_id.as_str()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persistence_failure_never_reports_goal_start_success() {
        let (root, session, _) = fixture("persistence-failure");
        let state = root.join("state");
        let store = TaskStore::with_fault(state, FaultPoint::BeforeReplace);
        let error =
            goal_start(&start_args(&session, "must persist"), &session, &store).unwrap_err();
        assert_eq!(error.code(), "GOAL_PERSISTENCE_ERROR");
        assert!(store.load_active_goal(&session.id).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn terminal_goal_tools_reject_all_terminal_states_without_changing_bytes() {
        let (root, session, store) = fixture("terminal-api");

        let completed = completed_goal(&session);
        let mut failed = Goal::new(
            session.id.clone(),
            session.cwd.clone(),
            "failed objective",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        failed.transition_to(GoalStatus::Failed, NOW).unwrap();
        let mut cancelled = Goal::new(
            session.id.clone(),
            session.cwd.clone(),
            "cancelled objective",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        cancelled
            .transition_to(GoalStatus::Cancelling, NOW)
            .unwrap();
        cancelled.transition_to(GoalStatus::Cancelled, NOW).unwrap();

        for goal in [completed, failed, cancelled] {
            let goal_id = goal.id().as_str().to_owned();
            store.create_goal(&goal).unwrap();
            let before = read_goal_bytes(&root, &session, &goal_id);
            let args = serde_json::json!({"session_id": session.id, "goal_id": goal_id});

            assert_eq!(
                goal_pause(&args, &session, &store).unwrap_err().code(),
                "GOAL_STATE_CONFLICT"
            );
            assert_eq!(before, read_goal_bytes(&root, &session, &goal_id));
            assert_eq!(
                goal_resume(&args, &session, &store).unwrap_err().code(),
                "GOAL_STATE_CONFLICT"
            );
            assert_eq!(before, read_goal_bytes(&root, &session, &goal_id));
            assert_eq!(
                goal_cancel(&args, &session, &store).unwrap_err().code(),
                "GOAL_STATE_CONFLICT"
            );
            assert_eq!(before, read_goal_bytes(&root, &session, &goal_id));

            let status = goal_status(&args, &session, &store).unwrap();
            assert!(status.terminal);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_goal_result_is_not_found_and_session_binding_is_enforced() {
        let (root, session, store) = fixture("lookup-errors");
        let unknown = Uuid::new_v4().to_string();
        let error = goal_result(
            &serde_json::json!({"session_id": session.id, "goal_id": unknown}),
            &session,
            &store,
        )
        .unwrap_err();
        assert_eq!(error.code(), "GOAL_NOT_FOUND");

        let mismatch = goal_status(
            &serde_json::json!({"session_id": "different-session"}),
            &session,
            &store,
        )
        .unwrap_err();
        assert_eq!(mismatch.code(), "INVALID_ARGUMENT");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_idempotent_starts_converge_on_one_durable_goal() {
        use std::sync::{Arc, Barrier};

        let (root, session, _) = fixture("idempotent-race");
        let state_root = root.join("state");
        let mut args = start_args(&session, "race objective");
        args["idempotency_key"] = Value::String("race-key".to_owned());
        let barrier = Arc::new(Barrier::new(2));

        let mut handles = Vec::new();
        for _ in 0..2 {
            let session = session.clone();
            let args = args.clone();
            let state_root = state_root.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let store = TaskStore::with_state_root(state_root);
                barrier.wait();
                goal_start(&args, &session, &store)
            }));
        }
        let first = handles.remove(0).join().unwrap().unwrap();
        let second = handles.remove(0).join().unwrap().unwrap();
        assert_eq!(first.goal_id, second.goal_id);
        assert!(first.idempotent_replay || second.idempotent_replay);
        let uuid = Uuid::parse_str(&first.goal_id).unwrap();
        assert_eq!(uuid.get_version_num(), 4);

        let store = TaskStore::with_state_root(state_root);
        assert_eq!(store.list_goals_for_session(&session.id).unwrap().len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn identical_goal_id_in_two_sessions_remains_session_scoped() {
        let (root, session_a, store) = fixture("sameid");
        let workspace_b = root.join("workspace-b");
        std::fs::create_dir_all(&workspace_b).unwrap();
        let session_b = config::Session {
            id: format!("p3-b-{}", Uuid::new_v4()),
            cwd: workspace_b.clone(),
            permitted_directories: vec![workspace_b],
        };
        let shared_id = GoalId::parse(&Uuid::new_v4().to_string()).unwrap();
        let goal_a = Goal::new_with_id(
            shared_id.clone(),
            session_a.id.clone(),
            session_a.cwd.clone(),
            "session A objective",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        let goal_b = Goal::new_with_id(
            shared_id.clone(),
            session_b.id.clone(),
            session_b.cwd.clone(),
            "session B objective",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        store.create_goal(&goal_a).unwrap();
        store.create_goal(&goal_b).unwrap();

        let status_a = goal_status(
            &serde_json::json!({"session_id": session_a.id, "goal_id": shared_id.as_str()}),
            &session_a,
            &store,
        )
        .unwrap();
        let status_b = goal_status(
            &serde_json::json!({"session_id": session_b.id, "goal_id": shared_id.as_str()}),
            &session_b,
            &store,
        )
        .unwrap();
        assert_eq!(status_a.goal_id, status_b.goal_id);
        assert_eq!(status_a.objective, "session A objective");
        assert_eq!(status_b.objective, "session B objective");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn public_pause_reports_persistence_failure_and_preserves_durable_bytes() {
        let (root, session, store) = fixture("pfail");
        let (goal, _) = running_goal(
            &session,
            read_only_scope(),
            WorkerKind::CodexReadonly,
            false,
        );
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let before = read_goal_bytes(&root, &session, goal_id.as_str());
        let faulty_store = TaskStore::with_fault(root.join("state"), FaultPoint::BeforeReplace);

        let error = goal_pause(
            &serde_json::json!({"session_id": session.id, "goal_id": goal_id.as_str()}),
            &session,
            &faulty_store,
        )
        .unwrap_err();
        assert_eq!(error.code(), "GOAL_PERSISTENCE_ERROR");
        assert_eq!(before, read_goal_bytes(&root, &session, goal_id.as_str()));
        let durable = store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(durable.status(), GoalStatus::Running);
        assert_eq!(durable.revision(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn resume_retries_only_mechanically_safe_readonly_worker_blocker() {
        let (root, session, store) = fixture("ro-block-safe");
        let (mut goal, task_id) =
            running_goal(&session, read_only_scope(), WorkerKind::CodexReadonly, true);
        goal.task_bind_latest_attempt_execution(
            &task_id,
            None,
            None,
            None,
            Some(SideEffectClass::None),
            Some(SideEffectState::ConfirmedNotPerformed),
            Some(1),
            Some(0),
        )
        .unwrap();
        goal.task_add_blocker(
            &task_id,
            crate::task::TaskBlocker::new("READONLY_BLOCKED", "worker could not investigate", true),
        )
        .unwrap();
        goal.transition_task(
            &task_id,
            TaskStatus::Blocked,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        pause_goal(&mut goal, NOW).unwrap();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();

        let resumed = goal_resume(
            &serde_json::json!({"session_id": session.id, "goal_id": goal_id.as_str()}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(resumed.status, GoalStatus::Running);
        let durable = store.load_goal(&session.id, &goal_id).unwrap();
        let task = &durable.tasks()[&task_id];
        assert_eq!(task.status(), TaskStatus::Ready);
        assert!(task.blockers().is_empty());
        assert_eq!(task.attempts().len(), 1);
        assert_eq!(
            task.latest_attempt().unwrap().side_effect_state(),
            Some(SideEffectState::ConfirmedNotPerformed)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resume_preserves_readonly_blocker_without_mechanical_retry_proof() {
        let (root, session, store) = fixture("ro-block-keep");
        let (mut goal, task_id) =
            running_goal(&session, read_only_scope(), WorkerKind::CodexReadonly, true);
        goal.task_bind_latest_attempt_execution(
            &task_id,
            None,
            None,
            None,
            Some(SideEffectClass::None),
            Some(SideEffectState::ConfirmedNotPerformed),
            Some(1),
            Some(0),
        )
        .unwrap();
        goal.task_add_blocker(
            &task_id,
            crate::task::TaskBlocker::new("TOOL_MISSING", "external prerequisite", true),
        )
        .unwrap();
        goal.transition_task(
            &task_id,
            TaskStatus::Blocked,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        pause_goal(&mut goal, NOW).unwrap();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();

        let resumed = goal_resume(
            &serde_json::json!({"session_id": session.id, "goal_id": goal_id.as_str()}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(resumed.status, GoalStatus::Blocked);
        let durable = store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(durable.tasks()[&task_id].status(), TaskStatus::Blocked);
        assert_eq!(
            durable.tasks()[&task_id].blockers()[0].code(),
            "TOOL_MISSING"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
