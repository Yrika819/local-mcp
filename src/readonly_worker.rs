use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent::AgentError;
use crate::config;
use crate::external_model_work::{
    IssueBlock, ModelWorkItem, OUTPUT_CONTRACT_READONLY_V1, SubmissionDecision, SubmissionId,
    SubmissionReceipt, WorkId, WorkItemState, WorkRole,
};
use crate::failure_class::FailureClass;
use crate::fallback::{SideEffectClass, SideEffectState};
use crate::goal::{Goal, GoalId, GoalStatus};
use crate::orchestrator_error::OrchestratorError;
use crate::task::{
    MAX_READONLY_TRANSPORT_INTERRUPTS_PER_TASK, ReadonlyTransportInterruptionKind, ReplaySafety,
    TaskBlocker, TaskEvidence, TaskId, TaskOperationKind, TaskStatus, TaskTransitionContext,
    VerificationSpec, WorkerKind, WorkerReport,
};
use crate::task_store::TaskStore;

/// Provenance recorded on evidence produced by the Codex readonly worker.
pub(crate) const CODEX_READONLY_SOURCE: &str = "CODEX_READONLY";

/// Provenance recorded on evidence produced by an external agent.
///
/// Kept distinct on purpose: host verification and any later audit must be able
/// to tell a host-launched worker's observation from an untrusted external
/// agent's claim, even though both pass the same validators.
pub(crate) const EXTERNAL_READONLY_SOURCE: &str = "EXTERNAL_READONLY_AGENT";

const MAX_READONLY_RESULT_BYTES: usize = 256 * 1024;
const MAX_READONLY_SUMMARY_BYTES: usize = crate::planner::MAX_READONLY_SUMMARY_BYTES;
const MAX_READONLY_EVIDENCE_ITEMS: usize = crate::planner::MAX_READONLY_EVIDENCE_ITEMS;
const MAX_READONLY_EVIDENCE_FIELD_BYTES: usize = crate::planner::MAX_READONLY_EVIDENCE_FIELD_BYTES;
const MAX_READONLY_EVIDENCE_TOTAL_BYTES: usize = crate::planner::MAX_READONLY_EVIDENCE_TOTAL_BYTES;

#[derive(Debug)]
#[expect(
    dead_code,
    reason = "Frozen read-only backend error variant remains part of the staged worker contract."
)]
pub(crate) enum ReadonlyError {
    Model(AgentError),
    Backend(String),
    OutputInvalid(String),
    AuthorityViolation(String),
    Store(OrchestratorError),
}

impl std::fmt::Display for ReadonlyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Model(error) => write!(f, "readonly model invocation failed: {error}"),
            Self::Backend(detail) => write!(f, "readonly backend failed: {detail}"),
            Self::OutputInvalid(detail) => write!(f, "readonly output is invalid: {detail}"),
            Self::AuthorityViolation(detail) => write!(f, "readonly authority violation: {detail}"),
            Self::Store(error) => write!(f, "readonly durable-state error: {error}"),
        }
    }
}

impl std::error::Error for ReadonlyError {}

impl From<OrchestratorError> for ReadonlyError {
    fn from(value: OrchestratorError) -> Self {
        Self::Store(value)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadonlyRequest {
    goal_id: String,
    task_id: String,
    attempt_id: String,
    goal_revision: u64,
    plan_revision: u32,
    goal_cwd: PathBuf,
    task_title: String,
    task_objective: String,
    allowed_paths: Vec<PathBuf>,
    forbidden_paths: Vec<PathBuf>,
    verification: Vec<VerificationSpec>,
}

#[expect(
    dead_code,
    reason = "Frozen read-only request identity accessors are retained for staged model-backend consumers."
)]
impl ReadonlyRequest {
    pub(crate) fn goal_id(&self) -> &str {
        &self.goal_id
    }
    pub(crate) fn task_id(&self) -> &str {
        &self.task_id
    }
    pub(crate) fn attempt_id(&self) -> &str {
        &self.attempt_id
    }
    pub(crate) fn goal_revision(&self) -> u64 {
        self.goal_revision
    }
    pub(crate) fn plan_revision(&self) -> u32 {
        self.plan_revision
    }
    pub(crate) fn goal_cwd(&self) -> &std::path::Path {
        &self.goal_cwd
    }
}

pub(crate) trait ReadonlyBackend {
    fn investigate(&self, request: &ReadonlyRequest) -> Result<Vec<u8>, ReadonlyError>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReadonlyStatus {
    CandidateComplete,
    Blocked,
    NeedsReplan,
    Failed,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadonlyEvidence {
    kind: String,
    value: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadonlyResult {
    goal_id: String,
    task_id: String,
    attempt_id: String,
    goal_revision: u64,
    plan_revision: u32,
    status: ReadonlyStatus,
    summary: String,
    evidence: Vec<ReadonlyEvidence>,
}

#[expect(
    dead_code,
    reason = "Frozen read-only backend fixture constructor is retained for staged adapter tests."
)]
pub(crate) fn readonly_request_for_model_backend_test(goal_cwd: PathBuf) -> ReadonlyRequest {
    ReadonlyRequest {
        goal_id: "00000000-0000-4000-8000-000000000001".to_owned(),
        task_id: "00000000-0000-4000-8000-000000000002".to_owned(),
        attempt_id: "00000000-0000-4000-8000-000000000003".to_owned(),
        goal_revision: 2,
        plan_revision: 1,
        goal_cwd,
        task_title: "readonly test".to_owned(),
        task_objective: "inspect only".to_owned(),
        allowed_paths: vec![PathBuf::from(".")],
        forbidden_paths: Vec::new(),
        verification: vec![VerificationSpec::StructuredEvidence {
            requirement_id: "readonly".to_owned(),
        }],
    }
}

pub(crate) fn begin_readonly_attempt(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    expected_revision: u64,
) -> Result<ReadonlyRequest, ReadonlyError> {
    let snapshot =
        store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
            validate_goal_session_binding(goal, session)?;
            crate::planner::execution_root_for_goal(goal, session).map_err(|error| {
                OrchestratorError::InvalidDag(format!(
                    "readonly execution-root gate refused: {error}"
                ))
            })?;
            ensure_readonly_task_ready(goal, task_id)?;
            goal.transition_task(
                task_id,
                TaskStatus::Running,
                TaskTransitionContext::default(),
                now,
            )
        })?;
    build_request(&snapshot, task_id, session)
}

pub(crate) fn run_readonly_attempt<B: ReadonlyBackend>(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    expected_revision: u64,
    backend: &B,
) -> Result<Goal, ReadonlyError> {
    let request = begin_readonly_attempt(store, session, goal_id, task_id, expected_revision)?;
    let raw = match backend.investigate(&request) {
        Ok(raw) => raw,
        Err(error) => {
            if matches!(&error, ReadonlyError::Model(AgentError::Timeout)) {
                persist_transport_interruption(
                    store,
                    session,
                    goal_id,
                    task_id,
                    &request,
                    "READONLY_BACKEND_ERROR",
                    &error.to_string(),
                    FailureClass::TransientModelFailure,
                )?;
            } else {
                persist_retryable_failure(
                    store,
                    session,
                    goal_id,
                    task_id,
                    &request,
                    "READONLY_BACKEND_ERROR",
                    &error.to_string(),
                    match &error {
                        ReadonlyError::Model(model) => Some(FailureClass::from_agent_error(*model)),
                        _ => None,
                    },
                )?;
            }
            return Err(error);
        }
    };
    let result = match parse_and_validate(&raw, &request) {
        Ok(result) => result,
        Err(error) => {
            persist_retryable_failure(
                store,
                session,
                goal_id,
                task_id,
                &request,
                "READONLY_OUTPUT_REJECTED",
                &error.to_string(),
                Some(FailureClass::SemanticFailure),
            )?;
            return Err(error);
        }
    };
    let report_digest = sha256_hex(&raw);
    persist_valid_result(
        store,
        session,
        goal_id,
        task_id,
        &request,
        &result,
        &report_digest,
    )
}

fn build_request(
    goal: &Goal,
    task_id: &TaskId,
    session: &config::Session,
) -> Result<ReadonlyRequest, ReadonlyError> {
    build_request_at_revision(goal, task_id, session, goal.revision())
}

/// Build the host-authored readonly request with an explicit goal revision.
///
/// The external backend issues a WorkItem inside a commit that advances the Goal
/// revision, so the request must be bound to the revision the client will
/// actually submit against, not to the pre-commit one.
fn build_request_at_revision(
    goal: &Goal,
    task_id: &TaskId,
    session: &config::Session,
    goal_revision: u64,
) -> Result<ReadonlyRequest, ReadonlyError> {
    let task = goal.tasks().get(task_id).ok_or_else(|| {
        ReadonlyError::AuthorityViolation("readonly Task disappeared after checkpoint".to_owned())
    })?;
    let attempt = task.latest_attempt().ok_or_else(|| {
        ReadonlyError::AuthorityViolation("RUNNING readonly Task lacks an Attempt".to_owned())
    })?;
    let execution_root = crate::planner::execution_root_for_goal(goal, session)
        .map_err(|error| ReadonlyError::AuthorityViolation(error.to_string()))?;
    Ok(ReadonlyRequest {
        goal_id: goal.id().as_str().to_owned(),
        task_id: task_id.as_str().to_owned(),
        attempt_id: attempt.id().as_str().to_owned(),
        goal_revision,
        plan_revision: goal.plan_revision(),
        goal_cwd: execution_root,
        task_title: task.title().to_owned(),
        task_objective: task.objective().to_owned(),
        allowed_paths: task.scope().allowed_paths().to_vec(),
        forbidden_paths: task.scope().forbidden_paths().to_vec(),
        verification: task.verification_specs().to_vec(),
    })
}

fn parse_and_validate(
    raw: &[u8],
    request: &ReadonlyRequest,
) -> Result<ReadonlyResult, ReadonlyError> {
    if raw.len() > MAX_READONLY_RESULT_BYTES {
        return Err(ReadonlyError::OutputInvalid(format!(
            "readonly result exceeds {MAX_READONLY_RESULT_BYTES} bytes"
        )));
    }
    let result: ReadonlyResult = serde_json::from_slice(raw)
        .map_err(|error| ReadonlyError::OutputInvalid(error.to_string()))?;
    if result.goal_id != request.goal_id
        || result.task_id != request.task_id
        || result.attempt_id != request.attempt_id
    {
        return Err(ReadonlyError::AuthorityViolation(
            "readonly result identity does not match host request".to_owned(),
        ));
    }
    if result.goal_revision != request.goal_revision
        || result.plan_revision != request.plan_revision
    {
        return Err(ReadonlyError::AuthorityViolation(
            "readonly result revision does not match host request".to_owned(),
        ));
    }
    validate_text(&result.summary, MAX_READONLY_SUMMARY_BYTES, "summary")?;
    if result.evidence.len() > MAX_READONLY_EVIDENCE_ITEMS {
        return Err(ReadonlyError::OutputInvalid(
            "too many readonly evidence items".to_owned(),
        ));
    }
    let mut total = 0usize;
    for evidence in &result.evidence {
        validate_text(
            &evidence.kind,
            MAX_READONLY_EVIDENCE_FIELD_BYTES,
            "evidence kind",
        )?;
        validate_text(
            &evidence.value,
            MAX_READONLY_EVIDENCE_FIELD_BYTES,
            "evidence value",
        )?;
        total = total
            .checked_add(evidence.kind.len() + evidence.value.len())
            .ok_or_else(|| {
                ReadonlyError::OutputInvalid("readonly evidence size overflow".to_owned())
            })?;
        if total > MAX_READONLY_EVIDENCE_TOTAL_BYTES {
            return Err(ReadonlyError::OutputInvalid(
                "readonly evidence exceeds total size limit".to_owned(),
            ));
        }
    }
    Ok(result)
}

fn validate_text(value: &str, max: usize, label: &str) -> Result<(), ReadonlyError> {
    if value.trim().is_empty() || value.len() > max {
        return Err(ReadonlyError::OutputInvalid(format!(
            "{label} must be non-empty and at most {max} bytes"
        )));
    }
    Ok(())
}

fn persist_valid_result(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    request: &ReadonlyRequest,
    result: &ReadonlyResult,
    report_digest: &str,
) -> Result<Goal, ReadonlyError> {
    store
        .mutate_goal_snapshot(&session.id, goal_id, request.goal_revision, |goal, now| {
            ensure_active_attempt(goal, task_id, request)?;
            apply_readonly_result_to_goal(
                goal,
                task_id,
                result,
                report_digest,
                CODEX_READONLY_SOURCE,
                now,
            )
        })
        .map_err(ReadonlyError::from)
}

/// Host-owned application of one already-validated readonly result.
///
/// Extracted so the Codex readonly path and the external readonly path cannot
/// diverge: both record the same evidence shape, consult the same Task state
/// table, and hand the final verdict to the same host Task Verifier. The only
/// difference is the evidence provenance string, which stays explicit so an
/// audit can tell which worker produced an observation.
fn apply_readonly_result_to_goal(
    goal: &mut Goal,
    task_id: &TaskId,
    result: &ReadonlyResult,
    report_digest: &str,
    evidence_source: &str,
    now: &str,
) -> Result<(), OrchestratorError> {
    record_non_mutating_metadata(goal, task_id, false)?;
    goal.task_record_latest_worker_report(
        task_id,
        WorkerReport::new(result.summary.clone(), Vec::new()),
    )?;
    let attempt_id = goal.tasks()[task_id]
        .latest_attempt()
        .ok_or_else(|| OrchestratorError::CorruptGoal("readonly Task lacks an Attempt".to_owned()))?
        .id()
        .clone();
    goal.task_add_evidence(
        task_id,
        TaskEvidence::WorkerReport {
            attempt_id,
            report_digest: report_digest.to_owned(),
        },
    )?;
    for evidence in &result.evidence {
        goal.task_add_evidence(
            task_id,
            TaskEvidence::StructuredObservation {
                requirement_id: evidence.kind.clone(),
                source: evidence_source.to_owned(),
                passed: true,
                detail: evidence.value.clone(),
            },
        )?;
    }
    match result.status {
        ReadonlyStatus::CandidateComplete => goal.transition_task(
            task_id,
            TaskStatus::Verifying,
            TaskTransitionContext::default(),
            now,
        ),
        ReadonlyStatus::Blocked => {
            goal.task_add_blocker(
                task_id,
                TaskBlocker::new("READONLY_BLOCKED", result.summary.clone(), true),
            )?;
            goal.transition_task(
                task_id,
                TaskStatus::Blocked,
                TaskTransitionContext::default(),
                now,
            )
        }
        ReadonlyStatus::NeedsReplan => goal.transition_task(
            task_id,
            TaskStatus::NeedsReplan,
            TaskTransitionContext::default(),
            now,
        ),
        ReadonlyStatus::Failed => goal.transition_task(
            task_id,
            TaskStatus::Failed,
            TaskTransitionContext::default(),
            now,
        ),
    }
}

/// The host gates every readonly attempt passes before an Attempt begins.
///
/// Shared by the Codex path and the external WorkItem issuance so a WorkItem can
/// never be issued for a Task the host would refuse to run.
fn ensure_readonly_task_ready(goal: &Goal, task_id: &TaskId) -> Result<(), OrchestratorError> {
    if goal.status() != GoalStatus::Running {
        return Err(OrchestratorError::InvalidDag(
            "readonly attempt requires a RUNNING Goal".to_owned(),
        ));
    }
    let task = goal
        .tasks()
        .get(task_id)
        .ok_or_else(|| OrchestratorError::InvalidDag("readonly Task is missing".to_owned()))?;
    if task.status() != TaskStatus::Ready || task.worker() != WorkerKind::CodexReadonly {
        return Err(OrchestratorError::InvalidDag(
            "readonly attempt requires a READY CODEX_READONLY Task".to_owned(),
        ));
    }
    if task.scope().operation_kind() != TaskOperationKind::ReadOnly
        || task.scope().replay_safety() != ReplaySafety::SafeReadOnly
    {
        return Err(OrchestratorError::InvalidDag(
            "CODEX_READONLY requires READ_ONLY + SAFE_READ_ONLY scope".to_owned(),
        ));
    }
    if task.semantic_attempts_remaining() == 0 {
        return Err(OrchestratorError::InvalidDag(
            "readonly Task semantic attempt budget is exhausted".to_owned(),
        ));
    }
    if !task.readonly_transport_retry_available() {
        return Err(OrchestratorError::InvalidDag(
            "readonly Task transport interruption budget is exhausted".to_owned(),
        ));
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "Read-only failure persistence keeps store, session, goal, task, request, and report channels explicit."
)]
fn persist_transport_interruption(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    request: &ReadonlyRequest,
    code: &str,
    detail: &str,
    failure_class: FailureClass,
) -> Result<Goal, ReadonlyError> {
    store
        .mutate_goal_snapshot(&session.id, goal_id, request.goal_revision, |goal, now| {
            ensure_active_attempt(goal, task_id, request)?;
            goal.task_record_latest_worker_report(
                task_id,
                WorkerReport::new(format!("{code}: {detail}"), Vec::new()),
            )?;
            goal.task_record_latest_attempt_failure_class(task_id, failure_class)?;
            record_non_mutating_metadata(goal, task_id, true)?;
            goal.task_mark_latest_attempt_interrupted(task_id, now)?;
            let attempt_id = goal.tasks()[task_id]
                .latest_attempt()
                .expect("active attempt checked")
                .id()
                .clone();
            goal.task_add_evidence(
                task_id,
                TaskEvidence::ReadonlyTransportInterruption {
                    attempt_id,
                    interruption: ReadonlyTransportInterruptionKind::ModelTimeout,
                },
            )?;
            let transport_interruptions =
                goal.tasks()[task_id].readonly_transport_interruptions();
            if transport_interruptions < MAX_READONLY_TRANSPORT_INTERRUPTS_PER_TASK
                && goal.tasks()[task_id].semantic_attempts_remaining() > 0
            {
                goal.transition_task(
                    task_id,
                    TaskStatus::Retryable,
                    TaskTransitionContext::default(),
                    now,
                )
            } else {
                goal.task_add_blocker(
                    task_id,
                    TaskBlocker::new(
                        "READONLY_TRANSPORT_UNAVAILABLE",
                        format!(
                            "readonly model transport interruption budget exhausted ({transport_interruptions}/{MAX_READONLY_TRANSPORT_INTERRUPTS_PER_TASK})"
                        ),
                        true,
                    ),
                )?;
                goal.transition_task(
                    task_id,
                    TaskStatus::Blocked,
                    TaskTransitionContext::default(),
                    now,
                )
            }
        })
        .map_err(ReadonlyError::from)
}

/// Persist a failed read-only attempt together with the host-owned structured
/// classification observed at the model boundary.
///
/// The next state is chosen from that classification, not from prose: a
/// failure whose class forbids replaying the identical Task shape is routed
/// into `NEEDS_REPLAN` so the Replanner must change the shape, while its
/// remaining attempt budget stays untouched and unusable.
#[expect(
    clippy::too_many_arguments,
    reason = "Read-only failure persistence keeps store, session, goal, task, request, report, and structured class channels explicit."
)]
fn persist_retryable_failure(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    request: &ReadonlyRequest,
    code: &str,
    detail: &str,
    failure_class: Option<FailureClass>,
) -> Result<Goal, ReadonlyError> {
    store
        .mutate_goal_snapshot(&session.id, goal_id, request.goal_revision, |goal, now| {
            ensure_active_attempt(goal, task_id, request)?;
            apply_readonly_failure_to_goal(goal, task_id, code, detail, failure_class, now)
        })
        .map_err(ReadonlyError::from)
}

/// Host-owned failure handling for one readonly attempt.
///
/// The next Task state is chosen from the structured failure class and the
/// remaining budgets, never from model prose, so an external worker's claim
/// cannot widen the retry policy.
fn apply_readonly_failure_to_goal(
    goal: &mut Goal,
    task_id: &TaskId,
    code: &str,
    detail: &str,
    failure_class: Option<FailureClass>,
    now: &str,
) -> Result<(), OrchestratorError> {
    record_non_mutating_metadata(goal, task_id, false)?;
    goal.task_record_latest_worker_report(
        task_id,
        WorkerReport::new(format!("{code}: {detail}"), Vec::new()),
    )?;
    if let Some(class) = failure_class {
        goal.task_record_latest_attempt_failure_class(task_id, class)?;
    }
    let budget_remains = goal.tasks()[task_id].semantic_attempts_remaining() > 0;
    let unchanged_retry_permitted = goal.tasks()[task_id].unchanged_retry_permitted();
    let next = if !budget_remains {
        TaskStatus::Failed
    } else if unchanged_retry_permitted {
        TaskStatus::Retryable
    } else {
        TaskStatus::NeedsReplan
    };
    goal.transition_task(task_id, next, TaskTransitionContext::default(), now)
}

fn record_non_mutating_metadata(
    goal: &mut Goal,
    task_id: &TaskId,
    transport_interruption: bool,
) -> Result<(), OrchestratorError> {
    let remaining = {
        let task = &goal.tasks()[task_id];
        let consumed = task.semantic_attempts_consumed();
        let consumed = if transport_interruption {
            consumed.saturating_sub(1)
        } else {
            consumed
        };
        task.max_attempts().saturating_sub(consumed)
    };
    goal.task_bind_latest_attempt_execution(
        task_id,
        None,
        None,
        None,
        Some(SideEffectClass::None),
        Some(SideEffectState::ConfirmedNotPerformed),
        Some(remaining),
        Some(0),
    )
}

fn ensure_active_attempt(
    goal: &Goal,
    task_id: &TaskId,
    request: &ReadonlyRequest,
) -> Result<(), OrchestratorError> {
    if goal.id().as_str() != request.goal_id
        || goal.revision() != request.goal_revision
        || goal.plan_revision() != request.plan_revision
    {
        return Err(OrchestratorError::InvalidDag(
            "readonly durable context changed after invocation".to_owned(),
        ));
    }
    let task = goal
        .tasks()
        .get(task_id)
        .ok_or_else(|| OrchestratorError::InvalidDag("readonly Task disappeared".to_owned()))?;
    if task.worker() != WorkerKind::CodexReadonly || task.status() != TaskStatus::Running {
        return Err(OrchestratorError::InvalidDag(
            "readonly Task is no longer the active CODEX_READONLY attempt".to_owned(),
        ));
    }
    let attempt = task.latest_attempt().ok_or_else(|| {
        OrchestratorError::CorruptGoal("RUNNING readonly Task lacks an Attempt".to_owned())
    })?;
    if attempt.id().as_str() != request.attempt_id {
        return Err(OrchestratorError::InvalidDag(
            "readonly Attempt identity changed".to_owned(),
        ));
    }
    Ok(())
}

fn validate_goal_session_binding(
    goal: &Goal,
    session: &config::Session,
) -> Result<(), OrchestratorError> {
    if goal.session_id() != session.id {
        return Err(OrchestratorError::InvalidDag(
            "readonly Goal/session binding mismatch".to_owned(),
        ));
    }
    let goal_cwd = fs::canonicalize(goal.cwd()).map_err(|_| {
        OrchestratorError::InvalidDag("readonly Goal cwd cannot be canonicalized".to_owned())
    })?;
    let session_cwd = fs::canonicalize(&session.cwd).map_err(|_| {
        OrchestratorError::InvalidDag("readonly session cwd cannot be canonicalized".to_owned())
    })?;
    if goal_cwd != session_cwd {
        return Err(OrchestratorError::InvalidDag(
            "readonly Goal cwd does not match session cwd".to_owned(),
        ));
    }
    crate::planner::execution_root_for_goal(goal, session).map_err(|error| {
        OrchestratorError::InvalidDag(format!("readonly execution-root gate refused: {error}"))
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// E4-B — external readonly integration.
//
// The three functions below are the whole external readonly surface. They reuse
// the request builder, the response parser, the schema checks, the evidence
// checks, and the Task state application above, so an external agent's answer
// travels the same path as a host-launched worker's answer. What differs is
// only *when* the host acts: issuance begins the Attempt in the same commit that
// registers the WorkItem, and submission applies the result and its receipt in
// one commit. No lock is ever held while a model runs.
// ---------------------------------------------------------------------------

/// A bounded, model-free diagnostic for a refused external readonly submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadonlyValidationRefusal {
    code: &'static str,
    detail: &'static str,
}

impl ReadonlyValidationRefusal {
    pub(crate) fn code(self) -> &'static str {
        self.code
    }

    pub(crate) fn detail(self) -> &'static str {
        self.detail
    }
}

/// An untrusted readonly response that passed every host check.
#[derive(Clone, Debug)]
pub(crate) struct ValidatedReadonlyOutput {
    result: ReadonlyResult,
    report_digest: String,
}

impl ValidatedReadonlyOutput {
    pub(crate) fn command_summary_for_receipt(&self) -> &'static str {
        match self.result.status {
            ReadonlyStatus::CandidateComplete => "READONLY_CANDIDATE_COMPLETE",
            ReadonlyStatus::Blocked => "READONLY_BLOCKED",
            ReadonlyStatus::NeedsReplan => "READONLY_NEEDS_REPLAN",
            ReadonlyStatus::Failed => "READONLY_FAILED",
        }
    }
}

/// Result of issuing one external readonly WorkItem.
#[expect(
    clippy::large_enum_variant,
    reason = "The issuance result is short-lived; handing back the durable WorkItem is the point."
)]
pub(crate) enum ReadonlyIssueOutcome {
    /// The Attempt was begun and the WorkItem was durably registered in the same
    /// commit. Only after this returns may a client see the request.
    Issued { item: ModelWorkItem },
    /// A host budget refused the WorkItem. Nothing was written and no Attempt
    /// was begun.
    Refused(IssueBlock),
}

/// Result of applying one external readonly submission.
#[expect(
    clippy::large_enum_variant,
    reason = "The outcome is a short-lived local value; carrying the committed Goal snapshot is the point."
)]
pub(crate) enum ReadonlyApplyOutcome {
    /// The result, its receipt, and the WorkItem's COMMITTED transition are one
    /// commit.
    Committed {
        goal: Goal,
        receipt: SubmissionReceipt,
    },
    /// The host refused the answer: the refusal receipt, the WorkItem's REJECTED
    /// transition, and the Attempt's structured failure classification are one
    /// commit.
    Rejected {
        goal: Goal,
        receipt: SubmissionReceipt,
    },
    /// Nothing was written: the durable binding no longer matched.
    Refused { code: &'static str },
}

/// Begin one readonly Attempt and register its host-authored request as a
/// durable `ModelWorkItem`, atomically.
///
/// The single commit is what prevents the classic half-issued state: a Task left
/// RUNNING with an Attempt but no WorkItem (which would wedge the Goal), or a
/// WorkItem whose Attempt does not exist (which would make every answer fail
/// attempt identity). The WorkItem is bound to the revision this commit
/// produces, because that is the revision the client will submit against.
pub(crate) fn issue_external_readonly_work_item(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    expected_revision: u64,
) -> Result<ReadonlyIssueOutcome, OrchestratorError> {
    use crate::external_model_store::{external_state, external_state_mut};
    use crate::external_model_work::{MAX_REQUEST_PAYLOAD_BYTES, sha256_hex as work_digest};

    let mut slot: Option<Result<ModelWorkItem, IssueBlock>> = None;
    let _committed =
        store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
            if let Err(block) = external_state(goal)?.check_issue_budget(WorkRole::Readonly) {
                slot = Some(Err(block));
                return Ok(());
            }
            ensure_readonly_task_ready(goal, task_id)?;
            let bound_revision = goal.revision().checked_add(1).ok_or_else(|| {
                OrchestratorError::CorruptGoal("goal revision overflow".to_owned())
            })?;
            let issue_counter = external_state(goal)?
                .issue_counter()
                .checked_add(1)
                .ok_or_else(|| {
                    OrchestratorError::CorruptGoal("external work item counter overflow".to_owned())
                })?;
            let plan_revision = goal.plan_revision();
            // The Attempt must exist before the request can name it, so the
            // transition happens first inside this same commit. A failure after this
            // point aborts the whole commit, so no half-started Attempt survives.
            goal.transition_task(
                task_id,
                TaskStatus::Running,
                TaskTransitionContext::default(),
                now,
            )?;
            let request = build_request_at_revision(goal, task_id, session, bound_revision)
                .map_err(|_| {
                    OrchestratorError::InvalidDag(
                        "external readonly request could not be built".to_owned(),
                    )
                })?;
            let attempt_id = goal.tasks()[task_id]
                .latest_attempt()
                .ok_or_else(|| {
                    OrchestratorError::CorruptGoal(
                        "readonly Attempt was not created for the external request".to_owned(),
                    )
                })?
                .id()
                .as_str()
                .to_owned();
            let payload = serde_json::to_string(&request).map_err(|_| {
                OrchestratorError::InvalidDag(
                    "external readonly request could not be serialized".to_owned(),
                )
            })?;
            if payload.len() > MAX_REQUEST_PAYLOAD_BYTES {
                return Err(OrchestratorError::InvalidDag(
                    "external readonly request exceeds the durable WorkItem bound".to_owned(),
                ));
            }
            let item = ModelWorkItem::pending(
                WorkId::new(),
                session.id.clone(),
                goal_id.as_str().to_owned(),
                WorkRole::Readonly,
                Some(task_id.as_str().to_owned()),
                Some(attempt_id),
                bound_revision,
                plan_revision,
                work_digest(payload.as_bytes()),
                payload,
                OUTPUT_CONTRACT_READONLY_V1,
                issue_counter,
                now,
            )?;
            external_state_mut(goal)?.record_issued(item.clone())?;
            slot = Some(Ok(item));
            Ok(())
        })?;

    match slot {
        Some(Ok(item)) => Ok(ReadonlyIssueOutcome::Issued { item }),
        Some(Err(block)) => Ok(ReadonlyIssueOutcome::Refused(block)),
        None => Err(OrchestratorError::CorruptGoal(
            "external readonly issuance committed without a WorkItem".to_owned(),
        )),
    }
}

/// Validate one untrusted external readonly response against a host request
/// rebuilt from the durable Goal.
///
/// The request is *not* trusted from the WorkItem payload: it is rebuilt from the
/// current durable Task so a WorkItem that has been superseded cannot be
/// answered into a newer Attempt. Identity, revision, size, schema, and evidence
/// bounds are all checked here, before any durable write.
pub(crate) fn validate_external_readonly_output(
    goal: &Goal,
    session: &config::Session,
    task_id: &TaskId,
    attempt_id: &str,
    raw: &[u8],
) -> Result<ValidatedReadonlyOutput, ReadonlyValidationRefusal> {
    let request = build_request(goal, task_id, session).map_err(|_| ReadonlyValidationRefusal {
        code: "READONLY_REQUEST_UNAVAILABLE",
        detail: "the host could not rebuild the readonly request for this Task",
    })?;
    if request.attempt_id != attempt_id {
        return Err(ReadonlyValidationRefusal {
            code: "READONLY_ATTEMPT_STALE",
            detail: "the readonly Attempt this WorkItem was bound to is no longer current",
        });
    }
    if raw.len() > MAX_READONLY_RESULT_BYTES {
        return Err(ReadonlyValidationRefusal {
            code: "READONLY_OUTPUT_TOO_LARGE",
            detail: "the readonly result exceeds the 256 KiB host limit",
        });
    }
    let result = parse_and_validate(raw, &request).map_err(|error| match error {
        ReadonlyError::AuthorityViolation(_) => ReadonlyValidationRefusal {
            code: "READONLY_IDENTITY_MISMATCH",
            detail: "the readonly result identity or revision does not match the host request",
        },
        _ => ReadonlyValidationRefusal {
            code: "READONLY_OUTPUT_INVALID",
            detail: "the readonly result failed the host schema or evidence checks",
        },
    })?;
    Ok(ValidatedReadonlyOutput {
        result,
        report_digest: sha256_hex(raw),
    })
}

/// Apply one validated external readonly result, its receipt, and the WorkItem's
/// COMMITTED transition in a single durable commit.
#[expect(
    clippy::too_many_arguments,
    reason = "Goal, work identity, digest, and the validated result are independent authority channels."
)]
pub(crate) fn commit_external_readonly_result(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    expected_revision: u64,
    work_id: &WorkId,
    submission_id: &SubmissionId,
    payload_sha256: &str,
    output: ValidatedReadonlyOutput,
) -> Result<ReadonlyApplyOutcome, OrchestratorError> {
    use crate::external_model_store::{external_state, external_state_mut};

    let mut applied: Option<SubmissionReceipt> = None;
    let mut refused: Option<&'static str> = None;
    let committed =
        store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
            let binding_ok = {
                let state = external_state(goal)?;
                match state.active_work() {
                    Some(active)
                        if active.work_id() == work_id
                            && active.state() == WorkItemState::Pending
                            && active.goal_revision() == goal.revision()
                            && active.plan_revision() == goal.plan_revision()
                            && active.task_id().is_some() =>
                    {
                        Some(active.clone())
                    }
                    _ => None,
                }
            };
            let Some(active) = binding_ok else {
                refused = Some("WORK_ITEM_NOT_PENDING");
                return Ok(());
            };
            let Some(task_id) = active.task_id().and_then(|value| TaskId::parse(value).ok()) else {
                refused = Some("WORK_ITEM_TASK_INVALID");
                return Ok(());
            };
            let Some(attempt_id) = active.attempt_id() else {
                refused = Some("WORK_ITEM_ATTEMPT_INVALID");
                return Ok(());
            };
            let request = match build_request(goal, &task_id, session) {
                Ok(request) => request,
                Err(_) => {
                    refused = Some("READONLY_REQUEST_UNAVAILABLE");
                    return Ok(());
                }
            };
            if request.attempt_id != attempt_id {
                refused = Some("READONLY_ATTEMPT_STALE");
                return Ok(());
            }
            ensure_active_attempt(goal, &task_id, &request)?;
            apply_readonly_result_to_goal(
                goal,
                &task_id,
                &output.result,
                &output.report_digest,
                EXTERNAL_READONLY_SOURCE,
                now,
            )?;
            let committed_revision = goal.revision().checked_add(1).ok_or_else(|| {
                OrchestratorError::CorruptGoal("goal revision overflow".to_owned())
            })?;
            let receipt = SubmissionReceipt::new(
                work_id.clone(),
                submission_id.clone(),
                payload_sha256.to_owned(),
                SubmissionDecision::Committed,
                Some(committed_revision),
                output.command_summary_for_receipt(),
                None,
                now,
            );
            external_state_mut(goal)?.record_submission(receipt.clone(), now)?;
            applied = Some(receipt);
            Ok(())
        });

    match committed {
        Ok(goal) => {
            if let Some(code) = refused {
                return Ok(ReadonlyApplyOutcome::Refused { code });
            }
            let receipt = applied.ok_or_else(|| {
                OrchestratorError::CorruptGoal(
                    "readonly submission committed without a receipt".to_owned(),
                )
            })?;
            Ok(ReadonlyApplyOutcome::Committed { goal, receipt })
        }
        Err(OrchestratorError::RevisionConflict { .. }) => Ok(ReadonlyApplyOutcome::Refused {
            code: "GOAL_REVISION_STALE",
        }),
        Err(error) => Err(error),
    }
}

/// Record one schema-invalid external readonly answer.
///
/// The refusal receipt, the WorkItem's REJECTED transition, and the host's
/// structured failure classification for the Attempt are one commit. The Task
/// therefore never sits RUNNING with no owner, and the ModelWorkItem is not left
/// PENDING forever behind an answer the host already rejected.
#[expect(
    clippy::too_many_arguments,
    reason = "The refusal keeps Goal, work identity, digest, and bounded verdict channels explicit."
)]
pub(crate) fn commit_external_readonly_refusal(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    expected_revision: u64,
    work_id: &WorkId,
    submission_id: &SubmissionId,
    payload_sha256: &str,
    code: &'static str,
    detail: &'static str,
) -> Result<ReadonlyApplyOutcome, OrchestratorError> {
    use crate::external_model_store::{external_state, external_state_mut};

    let mut applied: Option<SubmissionReceipt> = None;
    let mut refused: Option<&'static str> = None;
    let committed =
        store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
            let binding_ok = {
                let state = external_state(goal)?;
                match state.active_work() {
                    Some(active)
                        if active.work_id() == work_id
                            && active.state() == WorkItemState::Pending
                            && active.goal_revision() == goal.revision() =>
                    {
                        Some(active.clone())
                    }
                    _ => None,
                }
            };
            let Some(active) = binding_ok else {
                refused = Some("WORK_ITEM_NOT_PENDING");
                return Ok(());
            };
            let Some(task_id) = active.task_id().and_then(|value| TaskId::parse(value).ok()) else {
                refused = Some("WORK_ITEM_TASK_INVALID");
                return Ok(());
            };
            let Some(attempt_id) = active.attempt_id() else {
                refused = Some("WORK_ITEM_ATTEMPT_INVALID");
                return Ok(());
            };
            let request = match build_request(goal, &task_id, session) {
                Ok(request) => request,
                Err(_) => {
                    refused = Some("READONLY_REQUEST_UNAVAILABLE");
                    return Ok(());
                }
            };
            if request.attempt_id != attempt_id {
                refused = Some("READONLY_ATTEMPT_STALE");
                return Ok(());
            }
            ensure_active_attempt(goal, &task_id, &request)?;
            apply_readonly_failure_to_goal(
                goal,
                &task_id,
                code,
                detail,
                Some(FailureClass::SemanticFailure),
                now,
            )?;
            let receipt = SubmissionReceipt::new(
                work_id.clone(),
                submission_id.clone(),
                payload_sha256.to_owned(),
                SubmissionDecision::RejectedSchema,
                None,
                code,
                Some(detail.to_owned()),
                now,
            );
            external_state_mut(goal)?.record_submission(receipt.clone(), now)?;
            applied = Some(receipt);
            Ok(())
        });

    match committed {
        Ok(goal) => {
            if let Some(code) = refused {
                return Ok(ReadonlyApplyOutcome::Refused { code });
            }
            let receipt = applied.ok_or_else(|| {
                OrchestratorError::CorruptGoal(
                    "readonly refusal committed without a receipt".to_owned(),
                )
            })?;
            Ok(ReadonlyApplyOutcome::Rejected { goal, receipt })
        }
        Err(OrchestratorError::RevisionConflict { .. }) => Ok(ReadonlyApplyOutcome::Refused {
            code: "GOAL_REVISION_STALE",
        }),
        Err(error) => Err(error),
    }
}

/// Re-arm one RETRYABLE readonly Task when the host retry policy allows it.
///
/// `SchedulerNoActionReason::RetryPolicyDeferred` means "a host policy step owns
/// this decision". For the external backend that step is this function, and it
/// only ever executes the transition the Task state table already permits
/// (RETRYABLE -> READY), gated by the Task's own attempt budget and unchanged-
/// retry permission. Nothing here can grant an extra attempt.
pub(crate) fn rearm_retryable_readonly_task(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    expected_revision: u64,
    task_id: &TaskId,
) -> Result<Goal, OrchestratorError> {
    store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
        let task = goal.tasks().get(task_id).ok_or_else(|| {
            OrchestratorError::InvalidDag("retryable readonly Task is missing".to_owned())
        })?;
        if task.status() != TaskStatus::Retryable || task.worker() != WorkerKind::CodexReadonly {
            return Err(OrchestratorError::InvalidDag(
                "only a RETRYABLE readonly Task may be re-armed".to_owned(),
            ));
        }
        if !task.verification_retry_allowed() || !task.unchanged_retry_permitted() {
            return Err(OrchestratorError::InvalidDag(
                "the readonly retry policy does not permit another identical attempt".to_owned(),
            ));
        }
        goal.transition_task(
            task_id,
            TaskStatus::Ready,
            TaskTransitionContext::default(),
            now,
        )
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn request() -> ReadonlyRequest {
        readonly_request_for_model_backend_test(std::env::current_dir().unwrap())
    }

    fn valid_value(request: &ReadonlyRequest) -> Value {
        json!({
            "goal_id": request.goal_id(),
            "task_id": request.task_id(),
            "attempt_id": request.attempt_id(),
            "goal_revision": request.goal_revision(),
            "plan_revision": request.plan_revision(),
            "status": "candidate_complete",
            "summary": "bounded readonly finding",
            "evidence": [{"kind":"proof","value":"observed"}]
        })
    }

    #[test]
    fn strict_result_accepts_only_host_bound_identity() {
        let request = request();
        assert_eq!(
            parse_and_validate(
                &serde_json::to_vec(&valid_value(&request)).unwrap(),
                &request
            )
            .unwrap()
            .status,
            ReadonlyStatus::CandidateComplete
        );
    }

    #[test]
    fn wrong_task_identity_is_rejected() {
        let request = request();
        let mut value = valid_value(&request);
        value["task_id"] = json!("00000000-0000-4000-8000-000000000099");
        assert!(matches!(
            parse_and_validate(&serde_json::to_vec(&value).unwrap(), &request),
            Err(ReadonlyError::AuthorityViolation(_))
        ));
    }

    #[test]
    fn wrong_attempt_identity_is_rejected() {
        let request = request();
        let mut value = valid_value(&request);
        value["attempt_id"] = json!("00000000-0000-4000-8000-000000000099");
        assert!(matches!(
            parse_and_validate(&serde_json::to_vec(&value).unwrap(), &request),
            Err(ReadonlyError::AuthorityViolation(_))
        ));
    }

    #[test]
    fn wrong_goal_or_plan_revision_is_rejected() {
        let request = request();
        for field in ["goal_revision", "plan_revision"] {
            let mut value = valid_value(&request);
            value[field] = json!(999);
            assert!(matches!(
                parse_and_validate(&serde_json::to_vec(&value).unwrap(), &request),
                Err(ReadonlyError::AuthorityViolation(_))
            ));
        }
    }

    #[test]
    fn unknown_authority_fields_are_rejected() {
        let request = request();
        for (field, value) in [
            ("task_status", json!("COMPLETED")),
            ("goal_status", json!("COMPLETED")),
            ("attempt_budget", json!(999)),
            ("side_effect_state", json!("CONFIRMED_PERFORMED")),
        ] {
            let mut candidate = valid_value(&request);
            candidate[field] = value;
            assert!(matches!(
                parse_and_validate(&serde_json::to_vec(&candidate).unwrap(), &request),
                Err(ReadonlyError::OutputInvalid(_))
            ));
        }
    }

    #[test]
    fn proposed_operations_are_rejected() {
        let request = request();
        let mut value = valid_value(&request);
        value["proposed_operations"] = json!([{"kind":"WRITE_UTF8","path":"x","content":"x"}]);
        assert!(matches!(
            parse_and_validate(&serde_json::to_vec(&value).unwrap(), &request),
            Err(ReadonlyError::OutputInvalid(_))
        ));
    }

    #[test]
    fn oversized_summary_and_evidence_are_rejected() {
        let request = request();
        let mut summary = valid_value(&request);
        summary["summary"] = json!("x".repeat(MAX_READONLY_SUMMARY_BYTES + 1));
        assert!(matches!(
            parse_and_validate(&serde_json::to_vec(&summary).unwrap(), &request),
            Err(ReadonlyError::OutputInvalid(_))
        ));
        let mut evidence = valid_value(&request);
        evidence["evidence"] = json!(
            (0..=MAX_READONLY_EVIDENCE_ITEMS)
                .map(|_| json!({"kind":"k","value":"v"}))
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            parse_and_validate(&serde_json::to_vec(&evidence).unwrap(), &request),
            Err(ReadonlyError::OutputInvalid(_))
        ));
    }

    #[test]
    fn readonly_schema_has_no_completion_or_mutation_authority_fields() {
        let request = request();
        let serialized = serde_json::to_value(&request).unwrap();
        for forbidden in [
            "proposed_operations",
            "task_status",
            "goal_status",
            "side_effect_state",
            "attempt_budget",
        ] {
            assert!(serialized.get(forbidden).is_none());
        }
    }
}
