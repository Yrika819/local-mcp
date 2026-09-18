use std::collections::BTreeMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::config;
use crate::fallback::{SideEffectClass, SideEffectState};
use crate::goal::{CheckpointReason, Goal, GoalId, GoalStatus};
use crate::mutation_recovery;
use crate::orchestrator_error::OrchestratorError;
use crate::task::{
    ReplaySafety, TaskOperationKind, TaskStatus, TaskTransitionContext, VerificationOutcome,
    WorkerKind,
};
use crate::task_store::{TaskStore, utc_now_rfc3339};

const READONLY_RESPONSE_LIMIT_REPORT: &str =
    "READONLY_BACKEND_ERROR: readonly model invocation failed: model response exceeded the host limit";

pub(crate) struct ReadonlyTransportRecoveryAuthority {
    _private: (),
}

impl ReadonlyTransportRecoveryAuthority {
    fn for_goal_resume() -> Self {
        Self { _private: () }
    }
}

pub(crate) struct LegacyWriterPreMutationReconciliationAuthority {
    _private: (),
}

impl LegacyWriterPreMutationReconciliationAuthority {
    fn for_goal_resume() -> Self {
        Self { _private: () }
    }
}

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

    fn pre_execution_rejection_conflict(goal: &Goal) -> Self {
        Self {
            code: "IDEMPOTENCY_CONFLICT",
            message: "pre-execution plan rejection request conflicts with durable history"
                .to_owned(),
            active_goal: Some(identity_view(goal)),
        }
    }

    fn pre_execution_plan_revision_conflict(goal: &Goal, expected: u32) -> Self {
        Self {
            code: "REVISION_CONFLICT",
            message: format!(
                "expected plan revision {expected}, current plan revision is {}",
                goal.plan_revision()
            ),
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
struct GoalResumeRequest {
    session_id: String,
    #[serde(default)]
    goal_id: Option<String>,
    #[serde(default)]
    pre_execution_plan_rejection: Option<PreExecutionPlanRejectionRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreExecutionPlanRejectionRequest {
    request_id: String,
    expected_goal_revision: u64,
    expected_plan_revision: u32,
    trigger_task_id: String,
    reason: String,
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
    let request: GoalResumeRequest = parse_request(args)?;
    validate_session_binding(&request.session_id, session)?;
    let current = resolve_goal(store, &session.id, request.goal_id.as_deref())?;
    let goal_id = current.id().clone();

    if let Some(rejection) = request.pre_execution_plan_rejection {
        let trigger_task_id = validate_pre_execution_plan_rejection(&rejection)?;
        if let Some(existing) = current.find_pre_execution_plan_rejection(&rejection.request_id) {
            if existing.matches(
                &rejection.request_id,
                rejection.expected_goal_revision,
                rejection.expected_plan_revision,
                &trigger_task_id,
                &rejection.reason,
            ) {
                return Ok(status_view(&current));
            }
            return Err(GoalApiError::pre_execution_rejection_conflict(&current));
        }
        if current.has_pre_execution_plan_rejection_for_plan(rejection.expected_plan_revision) {
            return Err(GoalApiError::pre_execution_rejection_conflict(&current));
        }
        if current.revision() != rejection.expected_goal_revision {
            return Err(GoalApiError::from_orchestrator(
                OrchestratorError::RevisionConflict {
                    expected: rejection.expected_goal_revision,
                    actual: current.revision(),
                },
            ));
        }
        if current.plan_revision() != rejection.expected_plan_revision {
            return Err(GoalApiError::pre_execution_plan_revision_conflict(
                &current,
                rejection.expected_plan_revision,
            ));
        }
        let result =
            store.mutate_goal_snapshot(&session.id, &goal_id, current.revision(), |goal, now| {
                goal.reject_pre_execution_plan(
                    rejection.request_id.clone(),
                    rejection.expected_goal_revision,
                    rejection.expected_plan_revision,
                    trigger_task_id.clone(),
                    rejection.reason.clone(),
                    now,
                )
            });
        match result {
            Ok(durable) => return Ok(status_view(&durable)),
            Err(error @ OrchestratorError::RevisionConflict { .. }) => {
                return recover_rejection_after_cas_conflict(
                    store,
                    session,
                    &goal_id,
                    &rejection,
                    &trigger_task_id,
                    error,
                );
            }
            Err(error) => return Err(GoalApiError::from_orchestrator(error)),
        }
    }

    let expected_revision = current.revision();
    let durable = store
        .mutate_goal_snapshot(&session.id, &goal_id, expected_revision, |goal, now| {
            resume_goal(goal, now)
        })
        .map_err(GoalApiError::from_orchestrator)?;
    Ok(status_view(&durable))
}

fn validate_pre_execution_plan_rejection(
    request: &PreExecutionPlanRejectionRequest,
) -> Result<crate::task::TaskId, GoalApiError> {
    if request.expected_goal_revision == 0 || request.expected_plan_revision == 0 {
        return Err(GoalApiError::invalid(
            "expected Goal and plan revisions must be positive",
        ));
    }
    if request.request_id.trim().is_empty() {
        return Err(GoalApiError::invalid("request_id must not be empty"));
    }
    ensure_max_chars("request_id", &request.request_id, 128)?;
    if request.reason.trim().is_empty() {
        return Err(GoalApiError::invalid("reason must not be empty"));
    }
    ensure_max_chars("reason", &request.reason, 8_192)?;
    ensure_max_chars("trigger_task_id", &request.trigger_task_id, 36)?;
    let task_id = crate::task::TaskId::parse(&request.trigger_task_id)
        .map_err(GoalApiError::from_orchestrator)?;
    if task_id.as_str() != request.trigger_task_id {
        return Err(GoalApiError::invalid(
            "trigger_task_id must be a canonical lowercase UUID",
        ));
    }
    Ok(task_id)
}

fn recover_rejection_after_cas_conflict(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    rejection: &PreExecutionPlanRejectionRequest,
    trigger_task_id: &crate::task::TaskId,
    error: OrchestratorError,
) -> Result<GoalStatusView, GoalApiError> {
    let latest = resolve_goal(store, &session.id, Some(goal_id.as_str()))?;
    if let Some(existing) = latest.find_pre_execution_plan_rejection(&rejection.request_id) {
        if existing.matches(
            &rejection.request_id,
            rejection.expected_goal_revision,
            rejection.expected_plan_revision,
            trigger_task_id,
            &rejection.reason,
        ) {
            return Ok(status_view(&latest));
        }
        return Err(GoalApiError::pre_execution_rejection_conflict(&latest));
    }
    if latest.has_pre_execution_plan_rejection_for_plan(rejection.expected_plan_revision) {
        return Err(GoalApiError::pre_execution_rejection_conflict(&latest));
    }
    Err(GoalApiError::from_orchestrator(error))
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

    let mutation_reconciled = mutation_recovery::reconcile_goal_mutations(goal, now)?;
    goal.recover_stale_running(now)?;
    reconcile_legacy_readonly_timeout_failures(goal, now)?;
    reconcile_safe_readonly_blocked_tasks(goal, now)?;
    let legacy_writer_reconciled = reconcile_legacy_writer_blocked_tasks(goal, now)?;
    reconcile_safe_writer_blocked_tasks(goal, now)?;
    reconcile_readonly_transport_retryable_tasks(goal, now)?;
    let readonly_plan_reconciled = reconcile_readonly_plan_contract_failures(goal, now)?;
    if mutation_reconciled || legacy_writer_reconciled || readonly_plan_reconciled {
        goal.add_checkpoint(CheckpointReason::Recovery, now)?;
    }

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

fn reconcile_legacy_readonly_timeout_failures(
    goal: &mut Goal,
    now: &str,
) -> Result<(), OrchestratorError> {
    let task_ids = goal
        .tasks()
        .iter()
        .filter_map(|(task_id, task)| {
            (task.status() == TaskStatus::Failed).then(|| task_id.clone())
        })
        .collect::<Vec<_>>();
    let authority = ReadonlyTransportRecoveryAuthority::for_goal_resume();
    for task_id in task_ids {
        let _ = goal.recover_legacy_readonly_timeout_task(&task_id, &authority, now)?;
    }
    Ok(())
}

fn reconcile_readonly_transport_retryable_tasks(
    goal: &mut Goal,
    now: &str,
) -> Result<(), OrchestratorError> {
    let task_ids = goal
        .tasks()
        .iter()
        .filter_map(|(task_id, task)| {
            let attempt = task.latest_attempt()?;
            let safe_scope = task.worker() == WorkerKind::CodexReadonly
                && task.scope().operation_kind() == TaskOperationKind::ReadOnly
                && task.scope().replay_safety() == ReplaySafety::SafeReadOnly;
            let safe_attempt = attempt.operation_id().is_none()
                && attempt.scope_identity().is_none()
                && attempt.side_effect_class() == Some(SideEffectClass::None)
                && attempt.side_effect_state() == Some(SideEffectState::ConfirmedNotPerformed);
            (task.status() == TaskStatus::Retryable
                && task.blockers().is_empty()
                && safe_scope
                && safe_attempt
                && task.latest_is_readonly_transport_interruption()
                && task.semantic_attempts_remaining() > 0
                && task.readonly_transport_retry_available())
            .then(|| task_id.clone())
        })
        .collect::<Vec<_>>();

    for task_id in task_ids {
        goal.transition_task(
            &task_id,
            TaskStatus::Ready,
            TaskTransitionContext::default(),
            now,
        )?;
    }
    Ok(())
}

fn reconcile_readonly_plan_contract_failures(
    goal: &mut Goal,
    now: &str,
) -> Result<bool, OrchestratorError> {
    let task_ids = goal
        .tasks()
        .iter()
        .filter_map(|(task_id, task)| {
            let attempt = task.latest_attempt()?;
            let safe_scope = task.worker() == WorkerKind::CodexReadonly
                && task.scope().operation_kind() == TaskOperationKind::ReadOnly
                && task.scope().replay_safety() == ReplaySafety::SafeReadOnly;
            let safe_attempt = attempt.operation_id().is_none()
                && attempt.scope_identity().is_none()
                && attempt.side_effect_class() == Some(SideEffectClass::None)
                && attempt.side_effect_state() == Some(SideEffectState::ConfirmedNotPerformed);
            let response_limit = attempt
                .worker_report()
                .is_some_and(|report| report.summary() == READONLY_RESPONSE_LIMIT_REPORT);
            (task.status() == TaskStatus::Retryable
                && task.blockers().is_empty()
                && safe_scope
                && safe_attempt
                && response_limit)
                .then(|| task_id.clone())
        })
        .collect::<Vec<_>>();

    for task_id in &task_ids {
        goal.transition_task(
            task_id,
            TaskStatus::NeedsReplan,
            TaskTransitionContext::default(),
            now,
        )?;
    }
    Ok(!task_ids.is_empty())
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
            let budget_remains = task.semantic_attempts_remaining() > 0
                && attempt.remaining_attempt_budget().unwrap_or(0) > 0
                && task.readonly_transport_retry_available();
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

fn reconcile_safe_writer_blocked_tasks(
    goal: &mut Goal,
    now: &str,
) -> Result<(), OrchestratorError> {
    let task_ids = goal
        .tasks()
        .iter()
        .filter_map(|(task_id, task)| {
            let latest = task.latest_attempt()?;
            let pre_mutation_blockers = !task.blockers().is_empty()
                && task.blockers().iter().all(|blocker| {
                    matches!(
                        blocker.code(),
                        "WRITER_BLOCKED"
                            | "WRITER_BACKEND_ERROR"
                            | "WRITER_OUTPUT_REJECTED"
                            | "WRITER_OPERATION_REJECTED"
                    )
                });
            let safe_attempt_history = task.attempts().iter().all(|attempt| {
                attempt.operation_id().is_none()
                    && attempt.scope_identity().is_none()
                    && attempt.side_effect_class() == Some(SideEffectClass::None)
                    && attempt.side_effect_state() == Some(SideEffectState::ConfirmedNotPerformed)
            });
            let safe_scope = task.worker() == WorkerKind::CodexWriter
                && task.scope().operation_kind() != TaskOperationKind::ReadOnly
                && task.scope().replay_safety() == ReplaySafety::VerifyBeforeRetry;
            (task.status() == TaskStatus::Blocked
                && pre_mutation_blockers
                && safe_scope
                && safe_attempt_history
                && latest.operation_id().is_none()
                && latest.scope_identity().is_none()
                && latest.side_effect_class() == Some(SideEffectClass::None)
                && latest.side_effect_state() == Some(SideEffectState::ConfirmedNotPerformed)
                && latest.remaining_attempt_budget().unwrap_or(0) > 0
                && latest.remaining_side_effect_budget().unwrap_or(0) > 0
                && task.semantic_attempts_remaining() > 0)
                .then_some(task_id.clone())
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

fn reconcile_legacy_writer_blocked_tasks(
    goal: &mut Goal,
    now: &str,
) -> Result<bool, OrchestratorError> {
    let task_ids = goal
        .tasks()
        .iter()
        .filter_map(|(task_id, task)| {
            task.can_reconcile_legacy_writer_pre_mutation()
                .then_some(task_id.clone())
        })
        .collect::<Vec<_>>();
    let authority = LegacyWriterPreMutationReconciliationAuthority::for_goal_resume();
    let mut reconciled = false;
    for task_id in task_ids {
        if goal.reconcile_legacy_writer_pre_mutation_task(&task_id, &authority, now)? {
            goal.task_clear_blockers(&task_id)?;
            goal.transition_task(
                &task_id,
                TaskStatus::Ready,
                TaskTransitionContext::default(),
                now,
            )?;
            reconciled = true;
        }
    }
    Ok(reconciled)
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
    use crate::mutation::{MutationIntent, MutationIntentState, MutationOperationIntent, MutationPreimage};
    use crate::planner::{PlannerBackend, PlannerError, PlannerRequest};
    use crate::readonly_worker::{ReadonlyBackend, ReadonlyError};
    use crate::replanner::{ReplannerBackend, ReplannerError, ReplannerRequest};
    use crate::task::{
        AttemptOutcome, ReadonlyTransportRecoveryAuthorityKind, ReadonlyTransportRecoveryKind,
        ReplaySafety, TaskEvidence, TaskOperationKind, TaskScope, TaskStatus,
        TaskTransitionContext, VerificationResult, WorkerKind, WorkerReport,
    };
    use crate::task_store::FaultPoint;
    use crate::writer::{
        ReviewerBackend, ReviewerRequest, WriterBackend, WriterError, WriterRequest,
    };

    const NOW: &str = "2026-01-01T00:00:00Z";

    struct UnusedPlanner;

    impl PlannerBackend for UnusedPlanner {
        fn propose_initial_plan(&self, _request: &PlannerRequest) -> Result<Vec<u8>, PlannerError> {
            panic!("initial planner must not run during pre-execution rejection replan")
        }
    }

    struct UnusedReadonly;

    impl ReadonlyBackend for UnusedReadonly {
        fn investigate(
            &self,
            _request: &crate::readonly_worker::ReadonlyRequest,
        ) -> Result<Vec<u8>, ReadonlyError> {
            panic!("readonly worker must not run before the Replanner")
        }
    }

    struct UnusedWriter {
        calls: std::cell::Cell<usize>,
    }

    impl WriterBackend for UnusedWriter {
        fn propose(&self, _request: &WriterRequest) -> Result<Vec<u8>, WriterError> {
            self.calls.set(self.calls.get() + 1);
            panic!("Writer must not run before the Replanner")
        }
    }

    struct UnusedReviewer;

    impl ReviewerBackend for UnusedReviewer {
        fn review(&self, _request: &ReviewerRequest) -> Result<Vec<u8>, WriterError> {
            panic!("Reviewer must not run during pre-execution rejection replan")
        }
    }

    struct ResolvingReplanner {
        calls: std::cell::Cell<usize>,
    }

    impl ReplannerBackend for ResolvingReplanner {
        fn propose_replan(&self, request: &ReplannerRequest) -> Result<Vec<u8>, ReplannerError> {
            self.calls.set(self.calls.get() + 1);
            let request: Value = serde_json::to_value(request).unwrap();
            Ok(serde_json::to_vec(&serde_json::json!({
                "goal_id": request["goal_id"],
                "base_goal_revision": request["goal_revision"],
                "base_plan_revision": request["plan_revision"],
                "summary": "resolve the rejected plan trigger",
                "add_tasks": [{
                    "proposal_id": "repair",
                    "title": "repair prerequisite",
                    "objective": "provide a safe replanning prerequisite",
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
                    "task": {"ref_kind": "EXISTING", "task_id": request["eligible_needs_replan_task_ids"][0]},
                    "dependency": {"ref_kind": "NEW", "proposal_id": "repair"}
                }],
                "strengthen_verification": [],
                "strengthen_mandatory": [],
                "strengthen_criterion_bindings": [],
                "resolve_needs_replan": [request["eligible_needs_replan_task_ids"][0]]
            }))
            .unwrap())
        }
    }

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

    fn pre_execution_goal(
        name: &str,
    ) -> (
        PathBuf,
        config::Session,
        TaskStore,
        GoalId,
        crate::task::TaskId,
        crate::task::TaskId,
    ) {
        let (root, session, store) = fixture(name);
        let mut goal = Goal::new(
            session.id.clone(),
            session.cwd.clone(),
            "reject an accepted plan before execution",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        let preserved_task = goal
            .add_task(
                "preserved task",
                "preserve the old task history",
                true,
                WorkerKind::CodexReadonly,
                read_only_scope(),
                vec![],
                1,
                NOW,
            )
            .unwrap();
        let trigger_task = goal
            .add_task(
                "rejected task",
                "the accepted plan is invalid",
                true,
                WorkerKind::CodexWriter,
                mutation_scope(),
                vec![],
                1,
                NOW,
            )
            .unwrap();
        goal.transition_to(GoalStatus::Running, NOW).unwrap();
        goal.transition_task(
            &preserved_task,
            TaskStatus::Ready,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.transition_task(
            &trigger_task,
            TaskStatus::Ready,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        (root, session, store, goal_id, preserved_task, trigger_task)
    }

    fn pre_execution_rejection_args(
        session: &config::Session,
        goal_id: &GoalId,
        expected_goal_revision: u64,
        expected_plan_revision: u32,
        trigger_task_id: &crate::task::TaskId,
        request_id: &str,
        reason: &str,
    ) -> Value {
        serde_json::json!({
            "session_id": session.id,
            "goal_id": goal_id.as_str(),
            "pre_execution_plan_rejection": {
                "request_id": request_id,
                "expected_goal_revision": expected_goal_revision,
                "expected_plan_revision": expected_plan_revision,
                "trigger_task_id": trigger_task_id.as_str(),
                "reason": reason
            }
        })
    }

    fn legacy_writer_goal(
        session: &config::Session,
    ) -> (Goal, crate::task::TaskId) {
        legacy_writer_goal_with_changed_files(session, vec![])
    }

    fn legacy_writer_goal_with_changed_files(
        session: &config::Session,
        changed_files: Vec<PathBuf>,
    ) -> (Goal, crate::task::TaskId) {
        let mut goal = Goal::new(
            session.id.clone(),
            session.cwd.clone(),
            "legacy writer objective",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        let task_id = goal
            .add_task(
                "legacy writer",
                "legacy writer objective",
                true,
                WorkerKind::CodexWriter,
                mutation_scope(),
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
        let attempt_id = goal.tasks()[&task_id].latest_attempt().unwrap().id().clone();
        goal.task_record_latest_worker_report(
            &task_id,
            WorkerReport::new("legacy writer was blocked before mutation", changed_files),
        )
        .unwrap();
        goal.task_add_evidence(
            &task_id,
            TaskEvidence::WorkerReport {
                attempt_id,
                report_digest: "legacy-report".to_owned(),
            },
        )
        .unwrap();
        (goal, task_id)
    }

    fn blocked_legacy_writer_goal(
        session: &config::Session,
        changed_files: Vec<PathBuf>,
        blocker_code: &str,
    ) -> (Goal, crate::task::TaskId) {
        let (mut goal, task_id) = legacy_writer_goal_with_changed_files(session, changed_files);
        block_writer_goal(&mut goal, &task_id, blocker_code);
        (goal, task_id)
    }

    fn block_writer_goal(goal: &mut Goal, task_id: &crate::task::TaskId, blocker_code: &str) {
        goal.task_add_blocker(
            task_id,
            crate::task::TaskBlocker::new(blocker_code, "historical writer blocker", true),
        )
        .unwrap();
        goal.transition_task(
            task_id,
            TaskStatus::Blocked,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.transition_to(GoalStatus::Blocked, NOW).unwrap();
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
    fn pre_execution_plan_rejection_moves_explicit_pristine_trigger_to_replanning() {
        let (root, session, store) = fixture("preplan-reject");
        let sentinel = session.cwd.join("sentinel.txt");
        std::fs::write(&sentinel, "unchanged").unwrap();
        let mut goal = Goal::new(
            session.id.clone(),
            session.cwd.clone(),
            "reject the accepted plan before execution",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        let earlier_task = goal
            .add_task(
                "earlier read",
                "preserve earlier plan history",
                true,
                WorkerKind::CodexReadonly,
                read_only_scope(),
                vec![],
                1,
                NOW,
            )
            .unwrap();
        let trigger_task = goal
            .add_task(
                "rejected writer",
                "this accepted writer plan is invalid",
                true,
                WorkerKind::CodexWriter,
                mutation_scope(),
                vec![],
                1,
                NOW,
            )
            .unwrap();
        goal.transition_to(GoalStatus::Running, NOW).unwrap();
        goal.transition_task(
            &earlier_task,
            TaskStatus::Ready,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.transition_task(
            &trigger_task,
            TaskStatus::Ready,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        assert_eq!(goal.plan_revision(), 2);
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();

        let status = goal_resume(
            &serde_json::json!({
                "session_id": session.id,
                "goal_id": goal_id.as_str(),
                "pre_execution_plan_rejection": {
                    "request_id": "reject-plan-1",
                    "expected_goal_revision": 1,
                    "expected_plan_revision": 2,
                    "trigger_task_id": trigger_task.as_str(),
                    "reason": "host rejected the accepted plan before execution"
                }
            }),
            &session,
            &store,
        )
        .unwrap();

        assert_eq!(status.status, GoalStatus::Replanning);
        let durable = store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(durable.plan_revision(), 2);
        assert_eq!(durable.tasks()[&earlier_task].status(), TaskStatus::Ready);
        assert_eq!(
            durable.tasks()[&trigger_task].status(),
            TaskStatus::NeedsReplan
        );
        assert!(durable.tasks()[&trigger_task].attempts().is_empty());
        let selection = crate::scheduler::select_scheduler_action(&durable).unwrap();
        assert!(matches!(
            selection.decision(),
            crate::scheduler::SchedulerDecision::Replan { trigger_task_id }
                if trigger_task_id == &trigger_task
        ));
        assert_eq!(
            durable.checkpoints().last().unwrap().reason(),
            CheckpointReason::PreExecutionPlanRejected
        );
        let bytes = read_goal_bytes(&root, &session, goal_id.as_str());
        let persisted: Value = serde_json::from_slice(&bytes).unwrap();
        let record = &persisted["pre_execution_plan_rejections"][0];
        assert_eq!(record["rejected_plan_revision"], 2);
        assert_eq!(record["trigger_task_id"], trigger_task.as_str());
        assert_eq!(record["authority"], "GOAL_RESUME");
        assert_eq!(record["expected_goal_revision"], 1);
        assert_eq!(record["observed_goal_revision"], 1);
        assert_eq!(record["observed_plan_revision"], 2);
        assert_eq!(
            record["reason"],
            "host rejected the accepted plan before execution"
        );
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "unchanged");
        let serialized = String::from_utf8(bytes).unwrap();
        assert!(serialized.contains("PRE_EXECUTION_PLAN_REJECTED"));
        assert!(serialized.contains("reject-plan-1"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pre_execution_plan_rejection_persistence_failure_is_non_mutating() {
        let (root, session, store, goal_id, _preserved_task, trigger_task) =
            pre_execution_goal("preplan-pfail");
        let before = read_goal_bytes(&root, &session, goal_id.as_str());
        let faulty_store = TaskStore::with_fault(root.join("state"), FaultPoint::BeforeReplace);
        let request = pre_execution_rejection_args(
            &session,
            &goal_id,
            1,
            2,
            &trigger_task,
            "reject-plan-persistence-failure",
            "the durable write must fail safely",
        );

        let error = goal_resume(&request, &session, &faulty_store).unwrap_err();
        assert_eq!(error.code(), "GOAL_PERSISTENCE_ERROR");
        assert_eq!(before, read_goal_bytes(&root, &session, goal_id.as_str()));
        let durable = store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(durable.status(), GoalStatus::Running);
        assert_eq!(durable.revision(), 1);
        assert_eq!(durable.tasks()[&trigger_task].status(), TaskStatus::Ready);
        assert!(durable.pre_execution_plan_rejections().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn pre_execution_plan_rejection_enters_replanner_without_worker_and_reloads() {
        let (root, session, store, goal_id, _preserved_task, trigger_task) =
            pre_execution_goal("preplan-replan");
        std::fs::write(session.cwd.join("sentinel.txt"), "unchanged").unwrap();
        let request = pre_execution_rejection_args(
            &session,
            &goal_id,
            1,
            2,
            &trigger_task,
            "reject-plan-replan",
            "replanner must repair this accepted plan",
        );
        goal_resume(&request, &session, &store).unwrap();

        let reloaded_store = TaskStore::with_state_root(root.join("state"));
        let rejected = reloaded_store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(rejected.status(), GoalStatus::Replanning);
        assert_eq!(rejected.revision(), 2);
        let replanner = ResolvingReplanner {
            calls: std::cell::Cell::new(0),
        };
        let writer = UnusedWriter {
            calls: std::cell::Cell::new(0),
        };
        let result = crate::scheduler::scheduler_step(
            &reloaded_store,
            &session,
            &goal_id,
            rejected.revision(),
            &UnusedPlanner,
            &UnusedReadonly,
            &writer,
            &UnusedReviewer,
            &replanner,
        )
        .await
        .unwrap();
        assert_eq!(result.action, crate::scheduler::SchedulerAction::Replan);
        assert_eq!(
            result.outcome,
            crate::scheduler::SchedulerStepOutcome::Applied
        );
        assert_eq!(replanner.calls.get(), 1);
        assert_eq!(writer.calls.get(), 0);

        let repaired = reloaded_store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(repaired.status(), GoalStatus::Running);
        assert_eq!(repaired.plan_revision(), 3);
        assert_eq!(
            repaired.tasks()[&trigger_task].status(),
            TaskStatus::Pending
        );
        assert!(repaired.tasks().values().any(
            |task| task.title() == "repair prerequisite" && task.status() == TaskStatus::Ready
        ));
        assert_eq!(repaired.pre_execution_plan_rejections().len(), 1);
        assert!(repaired.tasks()[&trigger_task].attempts().is_empty());
        assert!(repaired.tasks()[&trigger_task].evidence().is_empty());
        assert_eq!(
            std::fs::read_to_string(session.cwd.join("sentinel.txt")).unwrap(),
            "unchanged"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pre_execution_plan_rejection_is_idempotent_and_conflicts_are_non_mutating() {
        let (root, session, store, goal_id, _preserved_task, trigger_task) =
            pre_execution_goal("preplan-idempotency");
        let args = pre_execution_rejection_args(
            &session,
            &goal_id,
            1,
            2,
            &trigger_task,
            "reject-plan-1",
            "host rejected this plan",
        );
        let first = goal_resume(&args, &session, &store).unwrap();
        let first_bytes = read_goal_bytes(&root, &session, goal_id.as_str());
        assert_eq!(first.revision, 2);
        assert_eq!(
            store
                .load_goal(&session.id, &goal_id)
                .unwrap()
                .pre_execution_plan_rejections()
                .len(),
            1
        );

        let replay = goal_resume(&args, &session, &store).unwrap();
        assert_eq!(replay.revision, first.revision);
        assert_eq!(
            read_goal_bytes(&root, &session, goal_id.as_str()),
            first_bytes
        );

        let changed_reason = pre_execution_rejection_args(
            &session,
            &goal_id,
            1,
            2,
            &trigger_task,
            "reject-plan-1",
            "different host reason",
        );
        let conflict = goal_resume(&changed_reason, &session, &store).unwrap_err();
        assert_eq!(conflict.code(), "IDEMPOTENCY_CONFLICT");
        assert_eq!(
            read_goal_bytes(&root, &session, goal_id.as_str()),
            first_bytes
        );

        let other_request = pre_execution_rejection_args(
            &session,
            &goal_id,
            1,
            2,
            &trigger_task,
            "reject-plan-2",
            "another host reason",
        );
        assert_eq!(
            goal_resume(&other_request, &session, &store)
                .unwrap_err()
                .code(),
            "IDEMPOTENCY_CONFLICT"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pre_execution_plan_rejection_cas_loser_reloads_identical_commit() {
        let (root, session, store, goal_id, _preserved_task, trigger_task) =
            pre_execution_goal("preplan-cas-loser");
        let args = pre_execution_rejection_args(
            &session,
            &goal_id,
            1,
            2,
            &trigger_task,
            "reject-plan-cas-loser",
            "the winning concurrent request committed this rejection",
        );
        goal_resume(&args, &session, &store).unwrap();
        let before = read_goal_bytes(&root, &session, goal_id.as_str());
        let rejection = PreExecutionPlanRejectionRequest {
            request_id: "reject-plan-cas-loser".to_owned(),
            expected_goal_revision: 1,
            expected_plan_revision: 2,
            trigger_task_id: trigger_task.as_str().to_owned(),
            reason: "the winning concurrent request committed this rejection".to_owned(),
        };
        let replay = recover_rejection_after_cas_conflict(
            &store,
            &session,
            &goal_id,
            &rejection,
            &trigger_task,
            OrchestratorError::RevisionConflict {
                expected: 1,
                actual: 2,
            },
        )
        .unwrap();
        assert_eq!(replay.status, GoalStatus::Replanning);
        assert_eq!(replay.revision, 2);
        assert_eq!(before, read_goal_bytes(&root, &session, goal_id.as_str()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pre_execution_plan_rejection_requires_current_revisions_and_pristine_explicit_trigger() {
        let (root, session, store, goal_id, _preserved_task, trigger_task) =
            pre_execution_goal("preplan-validation");
        let stale_plan = pre_execution_rejection_args(
            &session,
            &goal_id,
            1,
            1,
            &trigger_task,
            "reject-plan-stale-plan",
            "stale plan",
        );
        assert_eq!(
            goal_resume(&stale_plan, &session, &store)
                .unwrap_err()
                .code(),
            "REVISION_CONFLICT"
        );

        let stale_goal = pre_execution_rejection_args(
            &session,
            &goal_id,
            2,
            2,
            &trigger_task,
            "reject-plan-stale-goal",
            "stale goal",
        );
        assert_eq!(
            goal_resume(&stale_goal, &session, &store)
                .unwrap_err()
                .code(),
            "REVISION_CONFLICT"
        );

        let missing_trigger = pre_execution_rejection_args(
            &session,
            &goal_id,
            1,
            2,
            &crate::task::TaskId::new(),
            "reject-plan-missing-trigger",
            "missing trigger",
        );
        assert_eq!(
            goal_resume(&missing_trigger, &session, &store)
                .unwrap_err()
                .code(),
            "INVALID_GOAL_STATE"
        );

        let noncanonical = serde_json::json!({
            "session_id": session.id,
            "goal_id": goal_id.as_str(),
            "pre_execution_plan_rejection": {
                "request_id": "reject-plan-invalid-reason",
                "expected_goal_revision": 1,
                "expected_plan_revision": 2,
                "trigger_task_id": trigger_task.as_str(),
                "reason": " "
            }
        });
        assert_eq!(
            goal_resume(&noncanonical, &session, &store)
                .unwrap_err()
                .code(),
            "INVALID_ARGUMENT"
        );

        let before = read_goal_bytes(&root, &session, goal_id.as_str());
        store
            .mutate_goal_snapshot(&session.id, &goal_id, 1, |goal, now| {
                goal.transition_task(
                    &trigger_task,
                    TaskStatus::Running,
                    TaskTransitionContext::default(),
                    now,
                )
            })
            .unwrap();
        let current = store.load_goal(&session.id, &goal_id).unwrap();
        let nonpristine = pre_execution_rejection_args(
            &session,
            &goal_id,
            current.revision(),
            2,
            &trigger_task,
            "reject-plan-nonpristine",
            "non-pristine trigger",
        );
        assert_eq!(
            goal_resume(&nonpristine, &session, &store)
                .unwrap_err()
                .code(),
            "INVALID_GOAL_STATE"
        );
        assert_ne!(read_goal_bytes(&root, &session, goal_id.as_str()), before);
        assert_eq!(
            store.load_goal(&session.id, &goal_id).unwrap().tasks()[&trigger_task]
                .attempts()
                .len(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pre_execution_plan_rejection_denies_running_verifying_and_recovery_states() {
        let (root, session, store, goal_id, _preserved_task, trigger_task) =
            pre_execution_goal("preplan-running");
        store
            .mutate_goal_snapshot(&session.id, &goal_id, 1, |goal, now| {
                goal.transition_task(
                    &trigger_task,
                    TaskStatus::Running,
                    TaskTransitionContext::default(),
                    now,
                )
            })
            .unwrap();
        let current = store.load_goal(&session.id, &goal_id).unwrap();
        let request = pre_execution_rejection_args(
            &session,
            &goal_id,
            current.revision(),
            2,
            &trigger_task,
            "reject-plan-running",
            "running task must be denied",
        );
        assert_eq!(
            goal_resume(&request, &session, &store).unwrap_err().code(),
            "INVALID_GOAL_STATE"
        );

        let (root_verify, session_verify, store_verify, goal_verify, _preserved, trigger_verify) =
            pre_execution_goal("preplan-verifying");
        store_verify
            .mutate_goal_snapshot(&session_verify.id, &goal_verify, 1, |goal, now| {
                goal.transition_task(
                    &trigger_verify,
                    TaskStatus::Running,
                    TaskTransitionContext::default(),
                    now,
                )?;
                goal.transition_task(
                    &trigger_verify,
                    TaskStatus::Verifying,
                    TaskTransitionContext::default(),
                    now,
                )
            })
            .unwrap();
        let current_verify = store_verify
            .load_goal(&session_verify.id, &goal_verify)
            .unwrap();
        let verifying = pre_execution_rejection_args(
            &session_verify,
            &goal_verify,
            current_verify.revision(),
            2,
            &trigger_verify,
            "reject-plan-verifying",
            "verifying task must be denied",
        );
        assert_eq!(
            goal_resume(&verifying, &session_verify, &store_verify)
                .unwrap_err()
                .code(),
            "INVALID_GOAL_STATE"
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(root_verify).unwrap();
    }

    #[test]
    fn pre_execution_plan_rejection_denies_active_mutation_intent_and_unknown_side_effects() {
        let (root, session, store, goal_id, _preserved_task, trigger_task) =
            pre_execution_goal("preplan-intent");
        store
            .mutate_goal_snapshot(&session.id, &goal_id, 1, |goal, now| {
                goal.transition_task(
                    &trigger_task,
                    TaskStatus::Running,
                    TaskTransitionContext::default(),
                    now,
                )?;
                let intent = MutationIntent::new(
                    "operation-preplan".to_owned(),
                    "scope-preplan".to_owned(),
                    vec![MutationOperationIntent::new(
                        0,
                        session.cwd.join("target.txt"),
                        MutationPreimage::Absent,
                        crate::mutation::FileObservation::absent(),
                        "0000000000000000000000000000000000000000000000000000000000000000"
                            .to_owned(),
                        "request-preplan".to_owned(),
                    )],
                )?;
                goal.task_prepare_latest_mutation_intent(&trigger_task, intent)
            })
            .unwrap();
        let current = store.load_goal(&session.id, &goal_id).unwrap();
        let request = pre_execution_rejection_args(
            &session,
            &goal_id,
            current.revision(),
            2,
            &trigger_task,
            "reject-plan-intent",
            "active intent must be denied",
        );
        assert_eq!(
            goal_resume(&request, &session, &store).unwrap_err().code(),
            "INVALID_GOAL_STATE"
        );
        std::fs::remove_dir_all(root).unwrap();

        let (
            root_unknown,
            session_unknown,
            store_unknown,
            goal_unknown,
            _preserved,
            trigger_unknown,
        ) = pre_execution_goal("preplan-unknown");
        store_unknown
            .mutate_goal_snapshot(&session_unknown.id, &goal_unknown, 1, |goal, now| {
                goal.transition_task(
                    &trigger_unknown,
                    TaskStatus::Running,
                    TaskTransitionContext::default(),
                    now,
                )?;
                goal.task_bind_latest_attempt_execution(
                    &trigger_unknown,
                    Some("unknown-operation".to_owned()),
                    Some("unknown-scope".to_owned()),
                    None,
                    Some(SideEffectClass::LocalMutation),
                    Some(SideEffectState::Unknown),
                    Some(1),
                    Some(1),
                )
            })
            .unwrap();
        let current_unknown = store_unknown
            .load_goal(&session_unknown.id, &goal_unknown)
            .unwrap();
        let unknown = pre_execution_rejection_args(
            &session_unknown,
            &goal_unknown,
            current_unknown.revision(),
            2,
            &trigger_unknown,
            "reject-plan-unknown",
            "unknown side effect must be denied",
        );
        assert_eq!(
            goal_resume(&unknown, &session_unknown, &store_unknown)
                .unwrap_err()
                .code(),
            "INVALID_GOAL_STATE"
        );
        std::fs::remove_dir_all(root_unknown).unwrap();
    }

    #[test]
    fn pre_execution_plan_rejection_denies_terminal_goal_without_mutating_history() {
        let (root, session, store) = fixture("preplan-terminal");
        let goal = completed_goal(&session);
        let goal_id = goal.id().clone();
        let trigger_task = goal.tasks().keys().next().unwrap().clone();
        store.create_goal(&goal).unwrap();
        let before = read_goal_bytes(&root, &session, goal_id.as_str());
        let request = pre_execution_rejection_args(
            &session,
            &goal_id,
            goal.revision(),
            goal.plan_revision(),
            &trigger_task,
            "reject-plan-terminal",
            "terminal goal must be denied",
        );
        assert_eq!(
            goal_resume(&request, &session, &store).unwrap_err().code(),
            "INVALID_GOAL_STATE"
        );
        assert_eq!(read_goal_bytes(&root, &session, goal_id.as_str()), before);
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
    fn resume_reconciles_safe_readonly_response_limit_for_replan() {
        const RESPONSE_LIMIT_REPORT: &str =
            "READONLY_BACKEND_ERROR: readonly model invocation failed: model response exceeded the host limit";
        let (root, session, store) = fixture("resume-resp-limit");
        let (mut goal, task_id) =
            running_goal(&session, read_only_scope(), WorkerKind::CodexReadonly, true);
        goal.task_record_latest_worker_report(
            &task_id,
            WorkerReport::new(RESPONSE_LIMIT_REPORT, Vec::new()),
        )
        .unwrap();
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
        goal.transition_task(
            &task_id,
            TaskStatus::Retryable,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        let attempt_id = goal.tasks()[&task_id].latest_attempt().unwrap().id().clone();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let before_attempt = store
            .load_goal(&session.id, &goal_id)
            .unwrap()
            .tasks()[&task_id]
            .latest_attempt()
            .unwrap()
            .clone();

        let resumed = goal_resume(
            &serde_json::json!({"session_id": session.id, "goal_id": goal_id.as_str()}),
            &session,
            &store,
        )
        .unwrap();
        let durable = store.load_goal(&session.id, &goal_id).unwrap();
        let task = &durable.tasks()[&task_id];
        assert_eq!(resumed.status, GoalStatus::Running);
        assert_eq!(task.status(), TaskStatus::NeedsReplan);
        assert_eq!(task.attempts().len(), 1);
        assert_eq!(task.attempts()[0], before_attempt);
        assert_eq!(task.latest_attempt().unwrap().id(), &attempt_id);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resume_does_not_replan_unrelated_readonly_retryable_failure() {
        let (root, session, store) = fixture("resume-unrelated");
        let (mut goal, task_id) =
            running_goal(&session, read_only_scope(), WorkerKind::CodexReadonly, true);
        goal.task_record_latest_worker_report(
            &task_id,
            WorkerReport::new("READONLY_BACKEND_ERROR: unrelated failure", Vec::new()),
        )
        .unwrap();
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
        goal.transition_task(
            &task_id,
            TaskStatus::Retryable,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();

        goal_resume(
            &serde_json::json!({"session_id": session.id, "goal_id": goal_id.as_str()}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(
            store.load_goal(&session.id, &goal_id).unwrap().tasks()[&task_id].status(),
            TaskStatus::Retryable
        );
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
    fn resume_reconciles_writer_afterimage_and_is_idempotent_before_reviewer() {
        use sha2::{Digest, Sha256};

        let (root, session, store) = fixture("rw-after");
        let target_dir = session.cwd.join("src");
        std::fs::create_dir_all(&target_dir).unwrap();
        let target = target_dir.join("recovered.txt");
        let content = b"recovered\n";
        let digest = Sha256::digest(content)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let (mut goal, task_id) = running_goal(
            &session,
            mutation_scope(),
            WorkerKind::CodexWriter,
            true,
        );
        let intent = MutationIntent::new(
            "generic-writer-recovery".to_owned(),
            "generic-writer-scope".to_owned(),
            vec![MutationOperationIntent::new(
                0,
                target.clone(),
                MutationPreimage::Absent,
                crate::mutation::FileObservation::absent(),
                digest,
                "request-1".to_owned(),
            )],
        )
        .unwrap();
        goal.task_prepare_latest_mutation_intent(&task_id, intent).unwrap();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let prepared = store.load_goal(&session.id, &goal_id).unwrap();
        store
            .mutate_goal_snapshot(&session.id, &goal_id, prepared.revision(), |goal, _| {
                goal.task_advance_latest_mutation_intent(
                    &task_id,
                    "generic-writer-recovery",
                    crate::mutation::MutationIntentUpdate::BeginOperation { index: 0 },
                )
            })
            .unwrap();
        std::fs::write(&target, content).unwrap();

        let first = goal_resume(
            &serde_json::json!({"session_id": session.id, "goal_id": goal_id.as_str()}),
            &session,
            &store,
        )
        .unwrap();
        let after_first = store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(first.status, GoalStatus::Running);
        assert_eq!(after_first.tasks()[&task_id].status(), TaskStatus::Running);
        assert_eq!(
            after_first.tasks()[&task_id]
                .latest_attempt()
                .unwrap()
                .mutation_intent()
                .unwrap()
                .state(),
            MutationIntentState::ReconciledPerformed
        );
        assert!(after_first.tasks()[&task_id].needs_reviewer_recovery());
        let revision_after_first = after_first.revision();

        let second = goal_resume(
            &serde_json::json!({"session_id": session.id, "goal_id": goal_id.as_str()}),
            &session,
            &store,
        )
        .unwrap();
        let after_second = store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(second.status, GoalStatus::Running);
        assert_eq!(after_second.revision(), revision_after_first);
        assert_eq!(
            after_second.tasks()[&task_id].evidence().len(),
            after_first.tasks()[&task_id].evidence().len()
        );
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

    #[test]
    fn resume_retries_only_mechanically_safe_writer_blocker_before_mutation() {
        let (root, session, store) = fixture("writer-block-safe");
        let (mut goal, task_id) =
            running_goal(&session, mutation_scope(), WorkerKind::CodexWriter, true);
        goal.task_bind_latest_attempt_execution(
            &task_id,
            None,
            None,
            None,
            Some(SideEffectClass::None),
            Some(SideEffectState::ConfirmedNotPerformed),
            Some(2),
            Some(1),
        )
        .unwrap();
        goal.task_add_blocker(
            &task_id,
            crate::task::TaskBlocker::new("WRITER_BLOCKED", "writer could not inspect source", true),
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
        let attempt_id = goal.tasks()[&task_id].attempts()[0].id().clone();

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
        assert_eq!(task.attempts()[0].id(), &attempt_id);
        assert_eq!(task.latest_attempt().unwrap().operation_id(), None);
        assert_eq!(
            task.latest_attempt().unwrap().side_effect_state(),
            Some(SideEffectState::ConfirmedNotPerformed)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resume_reconciles_legacy_writer_pre_mutation_attempt() {
        let (root, session, store) = fixture("legacy-writer");
        let (mut goal, task_id) = legacy_writer_goal(&session);
        goal.task_add_blocker(
            &task_id,
            crate::task::TaskBlocker::new(
                "WRITER_BLOCKED",
                "Cannot inspect or modify the approved file in this read-only writer context.",
                true,
            ),
        )
        .unwrap();
        goal.transition_task(
            &task_id,
            TaskStatus::Blocked,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.transition_to(GoalStatus::Blocked, NOW).unwrap();
        let goal_id = goal.id().clone();
        let historical_attempt = goal.tasks()[&task_id].attempts()[0].clone();
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
        assert_eq!(task.attempts()[0], historical_attempt);
        assert_eq!(task.evidence().len(), 2);
        assert_eq!(task.semantic_attempts_consumed(), 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_writer_reconciliation_rejects_ambiguous_or_unrelated_state() {
        let (root, session, _store) = fixture("legacy-writer-reject");

        let (mut operation_goal, operation_task) = legacy_writer_goal(&session);
        operation_goal
            .task_bind_latest_attempt_execution(
                &operation_task,
                Some("operation".to_owned()),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        block_writer_goal(&mut operation_goal, &operation_task, "WRITER_BLOCKED");
        assert!(!operation_goal.tasks()[&operation_task]
            .can_reconcile_legacy_writer_pre_mutation());

        let (mut scope_goal, scope_task) = legacy_writer_goal(&session);
        scope_goal
            .task_bind_latest_attempt_execution(
                &scope_task,
                None,
                Some("scope".to_owned()),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        block_writer_goal(&mut scope_goal, &scope_task, "WRITER_BLOCKED");
        assert!(!scope_goal.tasks()[&scope_task]
            .can_reconcile_legacy_writer_pre_mutation());

        let (mut request_goal, request_task) = legacy_writer_goal(&session);
        request_goal
            .task_bind_latest_attempt_execution(
                &request_task,
                None,
                None,
                Some("request".to_owned()),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        block_writer_goal(&mut request_goal, &request_task, "WRITER_BLOCKED");
        assert!(!request_goal.tasks()[&request_task]
            .can_reconcile_legacy_writer_pre_mutation());

        let (changed_goal, changed_task) =
            blocked_legacy_writer_goal(&session, vec![PathBuf::from("changed.py")], "WRITER_BLOCKED");
        assert!(!changed_goal.tasks()[&changed_task]
            .can_reconcile_legacy_writer_pre_mutation());

        let (mut snapshot_goal, snapshot_task) = legacy_writer_goal(&session);
        snapshot_goal
            .task_add_evidence(
                &snapshot_task,
                TaskEvidence::FileSnapshot {
                    path: PathBuf::from("changed.py"),
                    exists: true,
                    size: Some(1),
                    sha256: Some("digest".to_owned()),
                },
            )
            .unwrap();
        block_writer_goal(&mut snapshot_goal, &snapshot_task, "WRITER_BLOCKED");
        assert!(!snapshot_goal.tasks()[&snapshot_task]
            .can_reconcile_legacy_writer_pre_mutation());

        let (mut review_goal, review_task) = legacy_writer_goal(&session);
        review_goal
            .task_add_evidence(
                &review_task,
                TaskEvidence::ReviewResult {
                    summary: "reviewer ran after mutation".to_owned(),
                    blocking_findings: 0,
                },
            )
            .unwrap();
        block_writer_goal(&mut review_goal, &review_task, "WRITER_BLOCKED");
        assert!(!review_goal.tasks()[&review_task]
            .can_reconcile_legacy_writer_pre_mutation());

        let (mut performed_goal, performed_task) = legacy_writer_goal(&session);
        performed_goal
            .task_bind_latest_attempt_execution(
                &performed_task,
                Some("operation".to_owned()),
                Some("scope".to_owned()),
                Some("request".to_owned()),
                Some(SideEffectClass::LocalMutation),
                Some(SideEffectState::ConfirmedPerformed),
                None,
                None,
            )
            .unwrap();
        block_writer_goal(&mut performed_goal, &performed_task, "WRITER_BLOCKED");
        assert!(!performed_goal.tasks()[&performed_task]
            .can_reconcile_legacy_writer_pre_mutation());

        let (mut unknown_goal, unknown_task) = legacy_writer_goal(&session);
        unknown_goal
            .task_bind_latest_attempt_execution(
                &unknown_task,
                Some("operation".to_owned()),
                Some("scope".to_owned()),
                None,
                Some(SideEffectClass::LocalMutation),
                Some(SideEffectState::Unknown),
                None,
                None,
            )
            .unwrap();
        block_writer_goal(&mut unknown_goal, &unknown_task, "WRITER_BLOCKED");
        assert!(!unknown_goal.tasks()[&unknown_task]
            .can_reconcile_legacy_writer_pre_mutation());

        let (post_mutation_goal, post_mutation_task) =
            blocked_legacy_writer_goal(&session, vec![], "WRITER_MUTATION_FAILED");
        assert!(!post_mutation_goal.tasks()[&post_mutation_task]
            .can_reconcile_legacy_writer_pre_mutation());

        let (unrelated_goal, unrelated_task) =
            blocked_legacy_writer_goal(&session, vec![], "TOOL_MISSING");
        assert!(!unrelated_goal.tasks()[&unrelated_task]
            .can_reconcile_legacy_writer_pre_mutation());

        let (mut readonly_goal, readonly_task) =
            running_goal(&session, read_only_scope(), WorkerKind::CodexReadonly, true);
        block_writer_goal(&mut readonly_goal, &readonly_task, "WRITER_BLOCKED");
        assert!(!readonly_goal.tasks()[&readonly_task]
            .can_reconcile_legacy_writer_pre_mutation());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_writer_reconciliation_is_append_only_and_idempotent() {
        let (root, session, store) = fixture("legacy-idem");
        let (goal, task_id) = blocked_legacy_writer_goal(&session, vec![], "WRITER_BLOCKED");
        let goal_id = goal.id().clone();
        let historical_attempt = goal.tasks()[&task_id].attempts()[0].clone();
        store.create_goal(&goal).unwrap();

        let first = goal_resume(
            &serde_json::json!({"session_id": session.id, "goal_id": goal_id.as_str()}),
            &session,
            &store,
        )
        .unwrap();
        let after_first = store.load_goal(&session.id, &goal_id).unwrap();
        let bytes_after_first = read_goal_bytes(&root, &session, goal_id.as_str());
        assert_eq!(first.status, GoalStatus::Running);
        assert_eq!(after_first.tasks()[&task_id].attempts()[0], historical_attempt);
        assert!(after_first.tasks()[&task_id].evidence().iter().any(|evidence| {
            matches!(
                evidence,
                TaskEvidence::LegacyWriterPreMutationReconciliation { reason, .. }
                    if reason == crate::task::LEGACY_WRITER_PRE_MUTATION_RECONCILIATION
            )
        }));
        assert!(after_first
            .checkpoints()
            .iter()
            .any(|checkpoint| checkpoint.reason() == CheckpointReason::Recovery));

        let second = goal_resume(
            &serde_json::json!({"session_id": session.id, "goal_id": goal_id.as_str()}),
            &session,
            &store,
        )
        .unwrap();
        let after_second = store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(second.status, GoalStatus::Running);
        assert_eq!(after_second.revision(), after_first.revision());
        assert_eq!(after_second.tasks()[&task_id].evidence().len(), after_first.tasks()[&task_id].evidence().len());
        assert_eq!(bytes_after_first, read_goal_bytes(&root, &session, goal_id.as_str()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reconciled_writer_reaches_running_once_under_scheduler_budget() {
        let (root, session, store) = fixture("legacy-running");
        let (goal, task_id) = blocked_legacy_writer_goal(&session, vec![], "WRITER_BLOCKED");
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        goal_resume(
            &serde_json::json!({"session_id": session.id, "goal_id": goal_id.as_str()}),
            &session,
            &store,
        )
        .unwrap();

        let ready = store.load_goal(&session.id, &goal_id).unwrap();
        assert!(matches!(
            crate::scheduler::select_next_action(&ready).unwrap(),
            crate::scheduler::SchedulerDecision::RunWriter { task_id: selected }
                if selected == task_id
        ));

        let request = crate::writer::begin_writer_attempt(
            &store,
            &session,
            &goal_id,
            &task_id,
            ready.revision(),
        )
        .unwrap();
        let running = store.load_goal(&session.id, &goal_id).unwrap();
        let task = &running.tasks()[&task_id];
        assert_eq!(task.status(), TaskStatus::Running);
        assert_eq!(task.attempts().len(), 2);
        assert_eq!(task.semantic_attempts_consumed(), 1);
        assert_eq!(task.semantic_attempts_remaining(), 0);
        assert_eq!(task.max_attempts(), 1);
        assert_eq!(request.attempt_id(), task.latest_attempt().unwrap().id().as_str());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_writer_after_mutation_blocks_replay_until_reconciled() {
        let (root, session, _store) = fixture("wsam");
        let (mut goal, task_id) =
            running_goal(&session, mutation_scope(), WorkerKind::CodexWriter, true);
        goal.task_bind_latest_attempt_execution(
            &task_id,
            Some("writer-operation".into()),
            Some("writer-scope".into()),
            Some("write-request".into()),
            Some(SideEffectClass::LocalMutation),
            Some(SideEffectState::ConfirmedPerformed),
            Some(2),
            Some(1),
        )
        .unwrap();

        goal.recover_stale_running(NOW).unwrap();

        let task = &goal.tasks()[&task_id];
        assert_eq!(task.status(), TaskStatus::Blocked);
        assert!(task.blockers().iter().any(|blocker| {
            blocker.code() == "RECOVERY_RECONCILIATION_REQUIRED"
                && blocker.detail().contains("writer")
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resume_recovers_legacy_readonly_timeout_append_only_and_allows_attempt_three() {
        let (root, session, store) = fixture("lto");
        let mut goal = Goal::new(
            session.id.clone(),
            session.cwd.clone(),
            "legacy timeout recovery",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        let task_id = goal
            .add_task(
                "readonly",
                "inspect",
                true,
                WorkerKind::CodexReadonly,
                read_only_scope(),
                vec![],
                2,
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
        goal.task_record_latest_worker_report(
            &task_id,
            WorkerReport::new("first semantic readonly blocker", Vec::new()),
        )
        .unwrap();
        goal.task_add_blocker(
            &task_id,
            crate::task::TaskBlocker::new("READONLY_BLOCKED", "first blocker", true),
        )
        .unwrap();
        goal.transition_task(
            &task_id,
            TaskStatus::Blocked,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        let attempt_1_id = goal.tasks()[&task_id].attempts()[0].id().clone();
        goal.task_clear_blockers(&task_id).unwrap();
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
        goal.task_bind_latest_attempt_execution(
            &task_id,
            None,
            None,
            None,
            Some(SideEffectClass::None),
            Some(SideEffectState::ConfirmedNotPerformed),
            Some(0),
            Some(0),
        )
        .unwrap();
        goal.task_record_latest_worker_report(
            &task_id,
            WorkerReport::new(
                "READONLY_BACKEND_ERROR: readonly model invocation failed: model invocation timed out",
                Vec::new(),
            ),
        )
        .unwrap();
        goal.transition_task(
            &task_id,
            TaskStatus::Failed,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        let attempt_2_id = goal.tasks()[&task_id].attempts()[1].id().clone();
        assert!(
            goal.transition_task(
                &task_id,
                TaskStatus::Ready,
                TaskTransitionContext::default(),
                NOW,
            )
            .is_err()
        );
        pause_goal(&mut goal, NOW).unwrap();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let revision_before = goal.revision();

        let resumed = goal_resume(
            &serde_json::json!({"session_id": session.id, "goal_id": goal_id.as_str()}),
            &session,
            &store,
        )
        .unwrap();
        assert_eq!(resumed.status, GoalStatus::Running);
        let recovered = store.load_goal(&session.id, &goal_id).unwrap();
        assert_eq!(recovered.id(), &goal_id);
        assert!(recovered.revision() > revision_before);
        let task = &recovered.tasks()[&task_id];
        assert_eq!(task.status(), TaskStatus::Ready);
        assert_eq!(task.max_attempts(), 2);
        assert_eq!(task.attempts().len(), 2);
        assert_eq!(task.attempts()[0].id(), &attempt_1_id);
        assert_eq!(task.attempts()[1].id(), &attempt_2_id);
        assert_eq!(task.attempts()[0].number(), 1);
        assert_eq!(task.attempts()[1].number(), 2);
        assert_eq!(task.attempts()[0].outcome(), Some(AttemptOutcome::Blocked));
        assert_eq!(task.attempts()[1].outcome(), Some(AttemptOutcome::Failed));
        assert_eq!(task.semantic_attempts_consumed(), 1);
        assert_eq!(task.semantic_attempts_remaining(), 1);
        assert_eq!(task.readonly_transport_interruptions(), 1);
        assert!(task.evidence().iter().any(|evidence| matches!(
            evidence,
            TaskEvidence::ReadonlyTransportRecovery {
                attempt_id,
                classification: ReadonlyTransportRecoveryKind::LegacyTimeoutTerminalization,
                authority: ReadonlyTransportRecoveryAuthorityKind::GoalResume,
                side_effect_state: SideEffectState::ConfirmedNotPerformed,
                postcondition_proven: true,
            } if attempt_id == &attempt_2_id
        )));

        store
            .mutate_goal_snapshot(
                &session.id,
                &goal_id,
                recovered.revision(),
                |goal, now| {
                    goal.transition_task(
                        &task_id,
                        TaskStatus::Running,
                        TaskTransitionContext::default(),
                        now,
                    )
                },
            )
            .unwrap();
        let attempt_three = store.load_goal(&session.id, &goal_id).unwrap();
        let task = &attempt_three.tasks()[&task_id];
        assert_eq!(task.attempts().len(), 3);
        assert_eq!(task.attempts()[2].number(), 3);
        assert_ne!(task.attempts()[2].id(), &attempt_1_id);
        assert_ne!(task.attempts()[2].id(), &attempt_2_id);
        assert_eq!(task.max_attempts(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sealed_legacy_recovery_rejects_writer_and_unsafe_side_effects() {
        let (root, session, _store) = fixture("lrr");
        for (worker, scope, class, state) in [
            (
                WorkerKind::CodexWriter,
                mutation_scope(),
                SideEffectClass::None,
                SideEffectState::ConfirmedNotPerformed,
            ),
            (
                WorkerKind::CodexReadonly,
                read_only_scope(),
                SideEffectClass::None,
                SideEffectState::Unknown,
            ),
            (
                WorkerKind::CodexReadonly,
                read_only_scope(),
                SideEffectClass::None,
                SideEffectState::ConfirmedPerformed,
            ),
        ] {
            let mut goal = Goal::new(
                session.id.clone(),
                session.cwd.clone(),
                "reject unsafe recovery",
                None,
                vec![],
                vec![],
                NOW,
            )
            .unwrap();
            let task_id = goal
                .add_task("task", "task", true, worker, scope, vec![], 2, NOW)
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
            goal.task_bind_latest_attempt_execution(
                &task_id,
                None,
                None,
                None,
                Some(class),
                Some(state),
                Some(0),
                Some(0),
            )
            .unwrap();
            goal.task_record_latest_worker_report(
                &task_id,
                WorkerReport::new(
                    "READONLY_BACKEND_ERROR: readonly model invocation failed: model invocation timed out",
                    Vec::new(),
                ),
            )
            .unwrap();
            goal.transition_task(
                &task_id,
                TaskStatus::Failed,
                TaskTransitionContext::default(),
                NOW,
            )
            .unwrap();
            let before = goal.clone();
            let authority = ReadonlyTransportRecoveryAuthority::for_goal_resume();
            assert!(!goal
                .recover_legacy_readonly_timeout_task(&task_id, &authority, NOW)
                .unwrap());
            assert_eq!(goal, before);
            assert_eq!(goal.tasks()[&task_id].status(), TaskStatus::Failed);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

}
