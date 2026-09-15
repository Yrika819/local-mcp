use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent::AgentError;
use crate::config;
use crate::fallback::{SideEffectClass, SideEffectState};
use crate::goal::{Goal, GoalId, GoalStatus};
use crate::orchestrator_error::OrchestratorError;
use crate::task::{
    ReplaySafety, TaskBlocker, TaskEvidence, TaskId, TaskOperationKind, TaskStatus,
    TaskTransitionContext, VerificationSpec, WorkerKind, WorkerReport,
};
use crate::task_store::TaskStore;

const MAX_READONLY_RESULT_BYTES: usize = 256 * 1024;
const MAX_READONLY_SUMMARY_BYTES: usize = 16 * 1024;
const MAX_READONLY_EVIDENCE_ITEMS: usize = 32;
const MAX_READONLY_EVIDENCE_FIELD_BYTES: usize = 8 * 1024;
const MAX_READONLY_EVIDENCE_TOTAL_BYTES: usize = 64 * 1024;

#[derive(Debug)]
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
            if goal.status() != GoalStatus::Running {
                return Err(OrchestratorError::InvalidDag(
                    "readonly attempt requires a RUNNING Goal".to_owned(),
                ));
            }
            let task = goal.tasks().get(task_id).ok_or_else(|| {
                OrchestratorError::InvalidDag("readonly Task is missing".to_owned())
            })?;
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
            if task.attempts().len() >= task.max_attempts() as usize {
                return Err(OrchestratorError::InvalidDag(
                    "readonly Task attempt budget is exhausted".to_owned(),
                ));
            }
            goal.transition_task(
                task_id,
                TaskStatus::Running,
                TaskTransitionContext::default(),
                now,
            )
        })?;
    build_request(&snapshot, task_id)
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
            persist_retryable_failure(
                store,
                session,
                goal_id,
                task_id,
                &request,
                "READONLY_BACKEND_ERROR",
                &error.to_string(),
            )?;
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

fn build_request(goal: &Goal, task_id: &TaskId) -> Result<ReadonlyRequest, ReadonlyError> {
    let task = goal.tasks().get(task_id).ok_or_else(|| {
        ReadonlyError::AuthorityViolation("readonly Task disappeared after checkpoint".to_owned())
    })?;
    let attempt = task.latest_attempt().ok_or_else(|| {
        ReadonlyError::AuthorityViolation("RUNNING readonly Task lacks an Attempt".to_owned())
    })?;
    Ok(ReadonlyRequest {
        goal_id: goal.id().as_str().to_owned(),
        task_id: task_id.as_str().to_owned(),
        attempt_id: attempt.id().as_str().to_owned(),
        goal_revision: goal.revision(),
        plan_revision: goal.plan_revision(),
        goal_cwd: goal.cwd().to_path_buf(),
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
            record_non_mutating_metadata(goal, task_id)?;
            goal.task_record_latest_worker_report(
                task_id,
                WorkerReport::new(result.summary.clone(), Vec::new()),
            )?;
            let attempt_id = goal.tasks()[task_id]
                .latest_attempt()
                .expect("active attempt checked")
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
                        source: "CODEX_READONLY".to_owned(),
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
                )?,
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
                    )?;
                }
                ReadonlyStatus::NeedsReplan => goal.transition_task(
                    task_id,
                    TaskStatus::NeedsReplan,
                    TaskTransitionContext::default(),
                    now,
                )?,
                ReadonlyStatus::Failed => goal.transition_task(
                    task_id,
                    TaskStatus::Failed,
                    TaskTransitionContext::default(),
                    now,
                )?,
            }
            Ok(())
        })
        .map_err(ReadonlyError::from)
}

fn persist_retryable_failure(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    request: &ReadonlyRequest,
    code: &str,
    detail: &str,
) -> Result<Goal, ReadonlyError> {
    store
        .mutate_goal_snapshot(&session.id, goal_id, request.goal_revision, |goal, now| {
            ensure_active_attempt(goal, task_id, request)?;
            record_non_mutating_metadata(goal, task_id)?;
            goal.task_record_latest_worker_report(
                task_id,
                WorkerReport::new(format!("{code}: {detail}"), Vec::new()),
            )?;
            let next = if goal.tasks()[task_id].attempts().len()
                < goal.tasks()[task_id].max_attempts() as usize
            {
                TaskStatus::Retryable
            } else {
                TaskStatus::Failed
            };
            goal.transition_task(task_id, next, TaskTransitionContext::default(), now)
        })
        .map_err(ReadonlyError::from)
}

fn record_non_mutating_metadata(
    goal: &mut Goal,
    task_id: &TaskId,
) -> Result<(), OrchestratorError> {
    let remaining = {
        let task = &goal.tasks()[task_id];
        task.max_attempts()
            .saturating_sub(task.attempts().len() as u32)
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
    Ok(())
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
