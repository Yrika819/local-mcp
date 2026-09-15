use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::fallback::{SideEffectClass, SideEffectState};
use crate::orchestrator_error::OrchestratorError;

macro_rules! durable_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub(crate) struct $name(String);

        impl $name {
            pub(crate) fn new() -> Self {
                Self(Uuid::new_v4().to_string())
            }

            pub(crate) fn parse(value: &str) -> Result<Self, OrchestratorError> {
                let uuid = Uuid::parse_str(value).map_err(|_| {
                    OrchestratorError::UnsafeIdentifier(stringify!($name).to_owned())
                })?;
                Ok(Self(uuid.to_string()))
            }

            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }

            fn validate(&self) -> Result<(), OrchestratorError> {
                let canonical = Self::parse(&self.0)?;
                if canonical.0 != self.0 {
                    return Err(OrchestratorError::UnsafeIdentifier(
                        stringify!($name).to_owned(),
                    ));
                }
                Ok(())
            }
        }
    };
}

durable_id!(TaskId);
durable_id!(AttemptId);
durable_id!(VerificationId);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum TaskStatus {
    Pending,
    Ready,
    Running,
    Blocked,
    Retryable,
    NeedsReplan,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum WorkerKind {
    LocalOperation,
    CodexReadonly,
    CodexWriter,
    CodexReviewer,
    Verifier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum TaskDependencyCondition {
    Completed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskDependency {
    task_id: TaskId,
    condition: TaskDependencyCondition,
}

impl TaskDependency {
    pub(crate) fn completed(task_id: TaskId) -> Self {
        Self {
            task_id,
            condition: TaskDependencyCondition::Completed,
        }
    }

    pub(crate) fn task_id(&self) -> &TaskId {
        &self.task_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum TaskOperationKind {
    ReadOnly,
    LocalMutation,
    HostNativeApproved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ReplaySafety {
    SafeReadOnly,
    VerifyBeforeRetry,
    NeverAutomatic,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskScope {
    allowed_paths: Vec<PathBuf>,
    forbidden_paths: Vec<PathBuf>,
    operation_kind: TaskOperationKind,
    replay_safety: ReplaySafety,
}

impl TaskScope {
    pub(crate) fn new(
        allowed_paths: Vec<PathBuf>,
        forbidden_paths: Vec<PathBuf>,
        operation_kind: TaskOperationKind,
        replay_safety: ReplaySafety,
    ) -> Self {
        Self {
            allowed_paths,
            forbidden_paths,
            operation_kind,
            replay_safety,
        }
    }

    pub(crate) fn operation_kind(&self) -> TaskOperationKind {
        self.operation_kind
    }

    pub(crate) fn replay_safety(&self) -> ReplaySafety {
        self.replay_safety
    }

    pub(crate) fn allowed_paths(&self) -> &[PathBuf] {
        &self.allowed_paths
    }

    pub(crate) fn forbidden_paths(&self) -> &[PathBuf] {
        &self.forbidden_paths
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub(crate) enum VerificationSpec {
    CommandExit {
        command: Vec<String>,
        cwd: Option<PathBuf>,
        accepted_exit_codes: Vec<i32>,
    },
    FileExists {
        path: PathBuf,
        must_be_file: bool,
    },
    FileDigest {
        path: PathBuf,
        expected_sha256: String,
    },
    GitScope {
        allowed_changed_paths: Vec<PathBuf>,
        require_no_other_changes: bool,
    },
    NoForbiddenChanges {
        forbidden_paths: Vec<PathBuf>,
    },
    StructuredEvidence {
        requirement_id: String,
    },
    ReviewGate {
        max_blocking_findings: u32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum VerificationOutcome {
    Passed,
    Failed,
    Indeterminate,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerificationCheckResult {
    verification_index: usize,
    passed: bool,
    detail: Option<String>,
}

impl VerificationCheckResult {
    pub(crate) fn new(verification_index: usize, passed: bool, detail: Option<String>) -> Self {
        Self {
            verification_index,
            passed,
            detail,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerificationResult {
    id: VerificationId,
    outcome: VerificationOutcome,
    checks: Vec<VerificationCheckResult>,
    started_at: String,
    finished_at: String,
}

impl VerificationResult {
    pub(crate) fn id(&self) -> &VerificationId {
        &self.id
    }

    pub(crate) fn check_count(&self) -> usize {
        self.checks.len()
    }

    pub(crate) fn started_at(&self) -> &str {
        &self.started_at
    }

    pub(crate) fn finished_at(&self) -> &str {
        &self.finished_at
    }

    pub(crate) fn new(
        outcome: VerificationOutcome,
        checks: Vec<VerificationCheckResult>,
        started_at: impl Into<String>,
        finished_at: impl Into<String>,
    ) -> Self {
        Self {
            id: VerificationId::new(),
            outcome,
            checks,
            started_at: started_at.into(),
            finished_at: finished_at.into(),
        }
    }

    pub(crate) fn outcome(&self) -> VerificationOutcome {
        self.outcome
    }

    fn validate_for_spec_count(&self, spec_count: usize) -> Result<(), OrchestratorError> {
        self.id.validate()?;
        let mut seen = BTreeSet::new();
        for check in &self.checks {
            if check.verification_index >= spec_count || !seen.insert(check.verification_index) {
                return Err(OrchestratorError::CorruptGoal(
                    "verification check does not match task verification specification".to_owned(),
                ));
            }
        }
        if self.outcome == VerificationOutcome::Passed {
            if seen.len() != spec_count || self.checks.iter().any(|check| !check.passed) {
                return Err(OrchestratorError::CorruptGoal(
                    "passed verification result does not contain a passing result for every specification"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerReport {
    summary: String,
    changed_files: Vec<PathBuf>,
}

impl WorkerReport {
    pub(crate) fn new(summary: impl Into<String>, changed_files: Vec<PathBuf>) -> Self {
        Self {
            summary: summary.into(),
            changed_files,
        }
    }

    pub(crate) fn summary(&self) -> &str {
        &self.summary
    }
}

pub(crate) const MAX_READONLY_TRANSPORT_INTERRUPTS_PER_TASK: u32 = 3;
const LEGACY_READONLY_TIMEOUT_REPORT: &str =
    "READONLY_BACKEND_ERROR: readonly model invocation failed: model invocation timed out";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ReadonlyTransportInterruptionKind {
    ModelTimeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ReadonlyTransportRecoveryKind {
    LegacyTimeoutTerminalization,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ReadonlyTransportRecoveryAuthorityKind {
    GoalResume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum AttemptOutcome {
    CandidateComplete,
    Retryable,
    Blocked,
    NeedsReplan,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskAttempt {
    id: AttemptId,
    number: u32,
    worker: WorkerKind,
    started_at: String,
    finished_at: Option<String>,
    worker_process_ref: Option<String>,
    codex_thread_id: Option<String>,
    low_level_request_ids: Vec<String>,
    operation_id: Option<String>,
    scope_identity: Option<String>,
    side_effect_class: Option<SideEffectClass>,
    side_effect_state: Option<SideEffectState>,
    remaining_attempt_budget: Option<u32>,
    remaining_side_effect_budget: Option<u32>,
    worker_report: Option<WorkerReport>,
    outcome: Option<AttemptOutcome>,
}

impl TaskAttempt {
    fn new(number: u32, worker: WorkerKind, started_at: &str) -> Self {
        Self {
            id: AttemptId::new(),
            number,
            worker,
            started_at: started_at.to_owned(),
            finished_at: None,
            worker_process_ref: None,
            codex_thread_id: None,
            low_level_request_ids: Vec::new(),
            operation_id: None,
            scope_identity: None,
            side_effect_class: None,
            side_effect_state: None,
            remaining_attempt_budget: None,
            remaining_side_effect_budget: None,
            worker_report: None,
            outcome: None,
        }
    }

    pub(crate) fn id(&self) -> &AttemptId {
        &self.id
    }

    pub(crate) fn number(&self) -> u32 {
        self.number
    }

    pub(crate) fn side_effect_class(&self) -> Option<SideEffectClass> {
        self.side_effect_class
    }

    pub(crate) fn side_effect_state(&self) -> Option<SideEffectState> {
        self.side_effect_state
    }

    pub(crate) fn operation_id(&self) -> Option<&str> {
        self.operation_id.as_deref()
    }

    pub(crate) fn scope_identity(&self) -> Option<&str> {
        self.scope_identity.as_deref()
    }

    pub(crate) fn worker(&self) -> WorkerKind {
        self.worker
    }

    pub(crate) fn outcome(&self) -> Option<AttemptOutcome> {
        self.outcome
    }

    pub(crate) fn remaining_attempt_budget(&self) -> Option<u32> {
        self.remaining_attempt_budget
    }

    pub(crate) fn remaining_side_effect_budget(&self) -> Option<u32> {
        self.remaining_side_effect_budget
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub(crate) enum TaskEvidence {
    WorkerReport {
        attempt_id: AttemptId,
        report_digest: String,
    },
    CommandResult {
        request_id: String,
        exit_code: Option<i32>,
        stdout_digest: Option<String>,
        stderr_digest: Option<String>,
    },
    FileSnapshot {
        path: PathBuf,
        exists: bool,
        size: Option<u64>,
        sha256: Option<String>,
    },
    GitSnapshot {
        head: Option<String>,
        changed_paths: Vec<PathBuf>,
        staged_paths: Vec<PathBuf>,
    },
    ReviewResult {
        summary: String,
        blocking_findings: u32,
    },
    RecoveryReconciliation {
        summary: String,
        side_effect_state: SideEffectState,
        postcondition_proven: bool,
    },
    ReadonlyTransportInterruption {
        attempt_id: AttemptId,
        interruption: ReadonlyTransportInterruptionKind,
    },
    ReadonlyTransportRecovery {
        attempt_id: AttemptId,
        classification: ReadonlyTransportRecoveryKind,
        authority: ReadonlyTransportRecoveryAuthorityKind,
        side_effect_state: SideEffectState,
        postcondition_proven: bool,
    },
    StructuredObservation {
        requirement_id: String,
        source: String,
        passed: bool,
        detail: String,
    },
    VerificationObservation {
        verification_id: VerificationId,
        attempt_id: AttemptId,
        verification_index: Option<usize>,
        verification_kind: String,
        observed_fact: String,
        passed: bool,
        observed_at: String,
        source: String,
    },
    Verification {
        verification_id: VerificationId,
        passed: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskBlocker {
    code: String,
    detail: String,
    mandatory: bool,
}

impl TaskBlocker {
    pub(crate) fn new(code: impl Into<String>, detail: impl Into<String>, mandatory: bool) -> Self {
        Self {
            code: code.into(),
            detail: detail.into(),
            mandatory,
        }
    }

    pub(crate) fn code(&self) -> &str {
        &self.code
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }

    pub(crate) fn mandatory(&self) -> bool {
        self.mandatory
    }

    fn recovery(detail: impl Into<String>) -> Self {
        Self::new("RECOVERY_RECONCILIATION_REQUIRED", detail, true)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TaskTransitionContext {
    pub(crate) active_worker_stopped: bool,
    pub(crate) side_effect_reconciled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Task {
    id: TaskId,
    title: String,
    objective: String,
    mandatory: bool,
    status: TaskStatus,
    dependencies: Vec<TaskDependency>,
    worker: WorkerKind,
    scope: TaskScope,
    verification: Vec<VerificationSpec>,
    verification_results: Vec<VerificationResult>,
    max_attempts: u32,
    attempts: Vec<TaskAttempt>,
    evidence: Vec<TaskEvidence>,
    blockers: Vec<TaskBlocker>,
    created_plan_revision: u32,
    updated_at: String,
}

impl Task {
    pub(crate) fn new(
        title: impl Into<String>,
        objective: impl Into<String>,
        mandatory: bool,
        worker: WorkerKind,
        scope: TaskScope,
        verification: Vec<VerificationSpec>,
        max_attempts: u32,
        created_plan_revision: u32,
        now: &str,
    ) -> Result<Self, OrchestratorError> {
        if max_attempts == 0 {
            return Err(OrchestratorError::InvalidDag(
                "task max_attempts must be at least 1".to_owned(),
            ));
        }
        let task = Self {
            id: TaskId::new(),
            title: title.into(),
            objective: objective.into(),
            mandatory,
            status: TaskStatus::Pending,
            dependencies: Vec::new(),
            worker,
            scope,
            verification,
            verification_results: Vec::new(),
            max_attempts,
            attempts: Vec::new(),
            evidence: Vec::new(),
            blockers: Vec::new(),
            created_plan_revision,
            updated_at: now.to_owned(),
        };
        task.validate_local()?;
        Ok(task)
    }

    pub(crate) fn id(&self) -> &TaskId {
        &self.id
    }

    pub(crate) fn status(&self) -> TaskStatus {
        self.status
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn objective(&self) -> &str {
        &self.objective
    }

    pub(crate) fn mandatory(&self) -> bool {
        self.mandatory
    }

    pub(crate) fn dependencies(&self) -> &[TaskDependency] {
        &self.dependencies
    }

    pub(crate) fn worker(&self) -> WorkerKind {
        self.worker
    }

    pub(crate) fn scope(&self) -> &TaskScope {
        &self.scope
    }

    pub(crate) fn attempts(&self) -> &[TaskAttempt] {
        &self.attempts
    }

    pub(crate) fn blockers(&self) -> &[TaskBlocker] {
        &self.blockers
    }

    pub(crate) fn verification_specs(&self) -> &[VerificationSpec] {
        &self.verification
    }

    pub(crate) fn verification_results(&self) -> &[VerificationResult] {
        &self.verification_results
    }

    pub(crate) fn evidence_count(&self) -> usize {
        self.evidence.len()
    }

    pub(crate) fn evidence(&self) -> &[TaskEvidence] {
        &self.evidence
    }

    pub(crate) fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    pub(crate) fn semantic_attempts_consumed(&self) -> u32 {
        self.attempts
            .iter()
            .filter(|attempt| !self.is_readonly_transport_attempt(attempt.id()))
            .count() as u32
    }

    pub(crate) fn semantic_attempts_remaining(&self) -> u32 {
        self.max_attempts
            .saturating_sub(self.semantic_attempts_consumed())
    }

    pub(crate) fn readonly_transport_interruptions(&self) -> u32 {
        let mut attempt_ids = BTreeSet::new();
        for evidence in &self.evidence {
            match evidence {
                TaskEvidence::ReadonlyTransportInterruption { attempt_id, .. }
                | TaskEvidence::ReadonlyTransportRecovery { attempt_id, .. } => {
                    attempt_ids.insert(attempt_id.clone());
                }
                _ => {}
            }
        }
        attempt_ids.len() as u32
    }

    pub(crate) fn readonly_transport_retry_available(&self) -> bool {
        self.readonly_transport_interruptions() < MAX_READONLY_TRANSPORT_INTERRUPTS_PER_TASK
    }

    pub(crate) fn latest_is_readonly_transport_interruption(&self) -> bool {
        self.latest_attempt()
            .is_some_and(|attempt| self.is_readonly_transport_attempt(attempt.id()))
    }

    fn is_readonly_transport_attempt(&self, attempt_id: &AttemptId) -> bool {
        self.evidence.iter().any(|evidence| match evidence {
            TaskEvidence::ReadonlyTransportInterruption {
                attempt_id: evidence_attempt_id,
                ..
            }
            | TaskEvidence::ReadonlyTransportRecovery {
                attempt_id: evidence_attempt_id,
                ..
            } => evidence_attempt_id == attempt_id,
            _ => false,
        })
    }

    pub(crate) fn created_plan_revision(&self) -> u32 {
        self.created_plan_revision
    }

    pub(crate) fn latest_attempt(&self) -> Option<&TaskAttempt> {
        self.attempts.last()
    }

    pub(crate) fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    pub(crate) fn has_unknown_side_effect(&self) -> bool {
        self.attempts
            .iter()
            .any(|attempt| attempt.side_effect_state == Some(SideEffectState::Unknown))
    }

    pub(crate) fn validate_local(&self) -> Result<(), OrchestratorError> {
        self.id.validate()?;
        if self.title.trim().is_empty() || self.objective.trim().is_empty() {
            return Err(OrchestratorError::CorruptGoal(
                "task title/objective must not be empty".to_owned(),
            ));
        }
        if self.max_attempts == 0 {
            return Err(OrchestratorError::CorruptGoal(
                "task max_attempts must be at least 1".to_owned(),
            ));
        }
        let mut attempt_ids = BTreeSet::new();
        let mut budget_by_operation: BTreeMap<&str, (Option<u32>, Option<u32>)> = BTreeMap::new();
        for (index, attempt) in self.attempts.iter().enumerate() {
            attempt.id.validate()?;
            if attempt.number != (index + 1) as u32 || !attempt_ids.insert(attempt.id.clone()) {
                return Err(OrchestratorError::CorruptGoal(
                    "task attempt history is not monotonic and unique".to_owned(),
                ));
            }
            if let Some(operation_id) = attempt.operation_id.as_deref() {
                if operation_id.is_empty() || operation_id.len() > 128 {
                    return Err(OrchestratorError::CorruptGoal(
                        "invalid durable operation identity".to_owned(),
                    ));
                }
                if let Some((previous_attempt, previous_side_effect)) =
                    budget_by_operation.get(operation_id).copied()
                {
                    if increases_budget(previous_attempt, attempt.remaining_attempt_budget)
                        || increases_budget(
                            previous_side_effect,
                            attempt.remaining_side_effect_budget,
                        )
                    {
                        return Err(OrchestratorError::CorruptGoal(
                            "low-level operation budget increased across task attempts".to_owned(),
                        ));
                    }
                }
                budget_by_operation.insert(
                    operation_id,
                    (
                        attempt.remaining_attempt_budget,
                        attempt.remaining_side_effect_budget,
                    ),
                );
            }
        }
        let mut transport_evidence_attempts = BTreeSet::new();
        for evidence in &self.evidence {
            let (attempt_id, requires_failed_legacy) = match evidence {
                TaskEvidence::ReadonlyTransportInterruption { attempt_id, interruption } => {
                    if *interruption != ReadonlyTransportInterruptionKind::ModelTimeout {
                        return Err(OrchestratorError::CorruptGoal(
                            "unsupported readonly transport interruption kind".to_owned(),
                        ));
                    }
                    (attempt_id, false)
                }
                TaskEvidence::ReadonlyTransportRecovery {
                    attempt_id,
                    classification,
                    authority,
                    side_effect_state,
                    postcondition_proven,
                } => {
                    if *classification
                        != ReadonlyTransportRecoveryKind::LegacyTimeoutTerminalization
                        || *authority != ReadonlyTransportRecoveryAuthorityKind::GoalResume
                        || *side_effect_state != SideEffectState::ConfirmedNotPerformed
                        || !*postcondition_proven
                    {
                        return Err(OrchestratorError::CorruptGoal(
                            "invalid readonly transport recovery evidence".to_owned(),
                        ));
                    }
                    (attempt_id, true)
                }
                _ => continue,
            };
            if !transport_evidence_attempts.insert(attempt_id.clone()) {
                return Err(OrchestratorError::CorruptGoal(
                    "duplicate readonly transport evidence for one attempt".to_owned(),
                ));
            }
            let attempt = self
                .attempts
                .iter()
                .find(|attempt| attempt.id() == attempt_id)
                .ok_or_else(|| {
                    OrchestratorError::CorruptGoal(
                        "readonly transport evidence references a missing attempt".to_owned(),
                    )
                })?;
            if attempt.worker != WorkerKind::CodexReadonly
                || attempt.operation_id.is_some()
                || attempt.scope_identity.is_some()
                || attempt.side_effect_class != Some(SideEffectClass::None)
                || attempt.side_effect_state != Some(SideEffectState::ConfirmedNotPerformed)
            {
                return Err(OrchestratorError::CorruptGoal(
                    "readonly transport evidence requires conclusively non-mutating CODEX_READONLY metadata"
                        .to_owned(),
                ));
            }
            if requires_failed_legacy {
                if attempt.outcome != Some(AttemptOutcome::Failed) {
                    return Err(OrchestratorError::CorruptGoal(
                        "legacy readonly transport recovery must preserve the historical FAILED outcome"
                            .to_owned(),
                    ));
                }
            } else if attempt.outcome != Some(AttemptOutcome::Interrupted) {
                return Err(OrchestratorError::CorruptGoal(
                    "readonly transport interruption evidence requires INTERRUPTED outcome"
                        .to_owned(),
                ));
            }
        }
        if self.status == TaskStatus::Running && self.attempts.is_empty() {
            return Err(OrchestratorError::CorruptGoal(
                "RUNNING task has no durable attempt".to_owned(),
            ));
        }
        for result in &self.verification_results {
            result.validate_for_spec_count(self.verification.len())?;
        }
        if self.status == TaskStatus::Completed && !self.latest_verification_passed() {
            return Err(OrchestratorError::CorruptGoal(
                "COMPLETED task lacks a passing host verification result".to_owned(),
            ));
        }
        Ok(())
    }

    pub(crate) fn transition_to(
        &mut self,
        next: TaskStatus,
        dependencies_satisfied: bool,
        context: TaskTransitionContext,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        let from = self.status;
        if from.is_terminal() {
            return Err(invalid_task_transition(
                from,
                next,
                "terminal task state is immutable",
            ));
        }
        let allowed = matches!(
            (from, next),
            (
                TaskStatus::Pending,
                TaskStatus::Ready | TaskStatus::Cancelled
            ) | (
                TaskStatus::Ready,
                TaskStatus::Running | TaskStatus::Cancelled
            ) | (
                TaskStatus::Running,
                TaskStatus::Verifying
                    | TaskStatus::Retryable
                    | TaskStatus::Blocked
                    | TaskStatus::NeedsReplan
                    | TaskStatus::Failed
                    | TaskStatus::Cancelled
            ) | (
                TaskStatus::Verifying,
                TaskStatus::Completed
                    | TaskStatus::Retryable
                    | TaskStatus::Blocked
                    | TaskStatus::NeedsReplan
                    | TaskStatus::Failed
                    | TaskStatus::Cancelled
            ) | (
                TaskStatus::Retryable,
                TaskStatus::Ready | TaskStatus::Failed | TaskStatus::Cancelled
            ) | (
                TaskStatus::Blocked,
                TaskStatus::Ready
                    | TaskStatus::NeedsReplan
                    | TaskStatus::Failed
                    | TaskStatus::Cancelled
            ) | (
                TaskStatus::NeedsReplan,
                TaskStatus::Pending
                    | TaskStatus::Blocked
                    | TaskStatus::Failed
                    | TaskStatus::Cancelled
            )
        );
        #[cfg(not(test))]
        if from == TaskStatus::Verifying && next == TaskStatus::Completed {
            return Err(invalid_task_transition(
                from,
                next,
                "direct completion rejected",
            ));
        }
        if !allowed {
            return Err(invalid_task_transition(
                from,
                next,
                "transition is not in the V1 state table",
            ));
        }
        if next == TaskStatus::Ready {
            if !dependencies_satisfied {
                return Err(invalid_task_transition(
                    from,
                    next,
                    "hard dependencies are incomplete",
                ));
            }
            if !self.blockers.is_empty() {
                return Err(invalid_task_transition(
                    from,
                    next,
                    "task still has blockers",
                ));
            }
            if from == TaskStatus::Retryable && !self.retry_allowed() {
                return Err(invalid_task_transition(
                    from,
                    next,
                    "retry policy/budget does not permit another attempt",
                ));
            }
        }
        if from == TaskStatus::Ready && next == TaskStatus::Running {
            if !dependencies_satisfied {
                return Err(invalid_task_transition(
                    from,
                    next,
                    "hard dependencies are incomplete",
                ));
            }
            if self.semantic_attempts_remaining() == 0 {
                return Err(invalid_task_transition(
                    from,
                    next,
                    "task semantic attempt budget is exhausted",
                ));
            }
            let number = self.attempts.len() as u32 + 1;
            self.attempts
                .push(TaskAttempt::new(number, self.worker, now));
        }
        if from == TaskStatus::Running && next == TaskStatus::Verifying {
            if self
                .latest_attempt()
                .is_some_and(|attempt| attempt.side_effect_state == Some(SideEffectState::Unknown))
            {
                return Err(invalid_task_transition(
                    from,
                    next,
                    "unknown side effect requires BLOCKED reconciliation",
                ));
            }
        }
        if next == TaskStatus::Retryable && !self.retry_allowed() {
            return Err(invalid_task_transition(
                from,
                next,
                "replay safety or remaining budget does not permit retry",
            ));
        }
        if matches!(from, TaskStatus::Running | TaskStatus::Verifying)
            && next == TaskStatus::Cancelled
            && !(context.active_worker_stopped && context.side_effect_reconciled)
        {
            return Err(invalid_task_transition(
                from,
                next,
                "cancellation requires stopped worker and reconciled side effects",
            ));
        }
        if matches!(
            next,
            TaskStatus::Retryable
                | TaskStatus::Blocked
                | TaskStatus::NeedsReplan
                | TaskStatus::Failed
        ) || (from == TaskStatus::Running && next == TaskStatus::Verifying)
        {
            if let Some(attempt) = self.attempts.last_mut() {
                if attempt.finished_at.is_none() {
                    attempt.finished_at = Some(now.to_owned());
                }
                if attempt.outcome.is_none() {
                    attempt.outcome = Some(match next {
                        TaskStatus::Retryable => AttemptOutcome::Retryable,
                        TaskStatus::Blocked => AttemptOutcome::Blocked,
                        TaskStatus::NeedsReplan => AttemptOutcome::NeedsReplan,
                        TaskStatus::Failed => AttemptOutcome::Failed,
                        TaskStatus::Verifying => AttemptOutcome::CandidateComplete,
                        _ => unreachable!(),
                    });
                }
            }
        }
        self.status = next;
        self.updated_at = now.to_owned();
        Ok(())
    }

    pub(crate) fn complete_from_verifier(
        &mut self,
        _authority: &crate::verifier::VerifierCompletionAuthority,
        dependencies_satisfied: bool,
        context: TaskTransitionContext,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        let from = self.status;
        if from != TaskStatus::Verifying {
            return Err(invalid_task_transition(
                from,
                TaskStatus::Completed,
                "verifier completion requires VERIFYING",
            ));
        }
        if !dependencies_satisfied
            || !context.side_effect_reconciled
            || !self.blockers.is_empty()
            || !self.latest_verification_passed()
        {
            return Err(invalid_task_transition(
                from,
                TaskStatus::Completed,
                "completion requires completed dependencies, passing verification, no blocker, and reconciled side effects",
            ));
        }
        self.status = TaskStatus::Completed;
        self.updated_at = now.to_owned();
        Ok(())
    }

    pub(crate) fn bind_latest_attempt_execution(
        &mut self,
        operation_id: Option<String>,
        scope_identity: Option<String>,
        request_id: Option<String>,
        side_effect_class: Option<SideEffectClass>,
        side_effect_state: Option<SideEffectState>,
        remaining_attempt_budget: Option<u32>,
        remaining_side_effect_budget: Option<u32>,
    ) -> Result<(), OrchestratorError> {
        if !matches!(self.status, TaskStatus::Running | TaskStatus::Verifying) {
            return Err(invalid_task_transition(
                self.status,
                self.status,
                "attempt execution metadata may only be updated while active/verifying",
            ));
        }

        let mut candidate = self.clone();
        let attempt = candidate.attempts.last_mut().ok_or_else(|| {
            OrchestratorError::CorruptGoal("active task lacks durable attempt".to_owned())
        })?;

        if let Some(new_id) = operation_id {
            if new_id.is_empty() || new_id.len() > 128 {
                return Err(OrchestratorError::CorruptGoal(
                    "invalid durable operation identity".to_owned(),
                ));
            }
            if attempt
                .operation_id
                .as_ref()
                .is_some_and(|old| old != &new_id)
            {
                return Err(OrchestratorError::InvalidDag(
                    "operation identity cannot be rewritten within an attempt".to_owned(),
                ));
            }
            attempt.operation_id = Some(new_id);
        }
        if let Some(new_scope) = scope_identity {
            if attempt
                .scope_identity
                .as_ref()
                .is_some_and(|old| old != &new_scope)
            {
                return Err(OrchestratorError::InvalidDag(
                    "scope identity cannot be rewritten within an attempt".to_owned(),
                ));
            }
            attempt.scope_identity = Some(new_scope);
        }
        if let Some(request_id) = request_id {
            if !attempt.low_level_request_ids.contains(&request_id) {
                attempt.low_level_request_ids.push(request_id);
            }
        }
        ensure_budget_not_increased(attempt.remaining_attempt_budget, remaining_attempt_budget)?;
        ensure_budget_not_increased(
            attempt.remaining_side_effect_budget,
            remaining_side_effect_budget,
        )?;
        attempt.side_effect_class = side_effect_class.or(attempt.side_effect_class);
        attempt.side_effect_state = side_effect_state.or(attempt.side_effect_state);
        if remaining_attempt_budget.is_some() {
            attempt.remaining_attempt_budget = remaining_attempt_budget;
        }
        if remaining_side_effect_budget.is_some() {
            attempt.remaining_side_effect_budget = remaining_side_effect_budget;
        }

        candidate.validate_local()?;
        *self = candidate;
        Ok(())
    }

    pub(crate) fn mark_latest_attempt_interrupted(
        &mut self,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        if self.status != TaskStatus::Running {
            return Err(invalid_task_transition(
                self.status,
                self.status,
                "attempt interruption may only be recorded while RUNNING",
            ));
        }
        let attempt = self.attempts.last_mut().ok_or_else(|| {
            OrchestratorError::CorruptGoal("RUNNING task lacks durable attempt".to_owned())
        })?;
        if attempt.outcome.is_some() {
            return Err(OrchestratorError::InvalidDag(
                "attempt outcome cannot be rewritten as interrupted".to_owned(),
            ));
        }
        attempt.finished_at.get_or_insert_with(|| now.to_owned());
        attempt.outcome = Some(AttemptOutcome::Interrupted);
        Ok(())
    }

    pub(crate) fn record_latest_worker_report(
        &mut self,
        report: WorkerReport,
    ) -> Result<(), OrchestratorError> {
        if self.status != TaskStatus::Running {
            return Err(invalid_task_transition(
                self.status,
                self.status,
                "worker report may only be recorded while RUNNING",
            ));
        }
        let attempt = self.attempts.last_mut().ok_or_else(|| {
            OrchestratorError::CorruptGoal("RUNNING task lacks durable attempt".to_owned())
        })?;
        if attempt.worker_report.is_some() {
            return Err(OrchestratorError::InvalidDag(
                "worker report cannot be rewritten within an attempt".to_owned(),
            ));
        }
        attempt.worker_report = Some(report);
        Ok(())
    }

    pub(crate) fn record_verification_result(
        &mut self,
        result: VerificationResult,
    ) -> Result<(), OrchestratorError> {
        if self.status != TaskStatus::Verifying {
            return Err(invalid_task_transition(
                self.status,
                self.status,
                "verification results are accepted only in VERIFYING",
            ));
        }
        result.validate_for_spec_count(self.verification.len())?;
        self.verification_results.push(result);
        Ok(())
    }

    pub(crate) fn add_evidence(&mut self, evidence: TaskEvidence) -> Result<(), OrchestratorError> {
        if self.is_terminal() {
            return Err(invalid_task_transition(
                self.status,
                self.status,
                "terminal evidence history is immutable",
            ));
        }
        self.evidence.push(evidence);
        Ok(())
    }

    pub(crate) fn add_blocker(&mut self, blocker: TaskBlocker) -> Result<(), OrchestratorError> {
        if self.is_terminal() {
            return Err(invalid_task_transition(
                self.status,
                self.status,
                "terminal blocker history is immutable",
            ));
        }
        self.blockers.push(blocker);
        Ok(())
    }

    pub(crate) fn clear_blockers(&mut self) -> Result<(), OrchestratorError> {
        if self.is_terminal() {
            return Err(invalid_task_transition(
                self.status,
                self.status,
                "terminal blocker history is immutable",
            ));
        }
        self.blockers.clear();
        Ok(())
    }

    pub(crate) fn strengthen_dependencies(
        &mut self,
        candidate: Vec<TaskDependency>,
        completed_dependencies: &BTreeSet<TaskId>,
    ) -> Result<(), OrchestratorError> {
        if !matches!(
            self.status,
            TaskStatus::Pending | TaskStatus::Ready | TaskStatus::NeedsReplan
        ) {
            return Err(OrchestratorError::InvalidDag(
                "dependencies cannot change for active or terminal tasks".to_owned(),
            ));
        }
        let current = self
            .dependencies
            .iter()
            .map(|dependency| dependency.task_id.clone())
            .collect::<BTreeSet<_>>();
        let proposed = candidate
            .iter()
            .map(|dependency| dependency.task_id.clone())
            .collect::<BTreeSet<_>>();
        if proposed.len() != candidate.len() || !current.is_subset(&proposed) {
            return Err(OrchestratorError::InvalidDag(
                "replanning may add hard dependencies but may not remove or duplicate them"
                    .to_owned(),
            ));
        }
        if proposed.contains(&self.id) {
            return Err(OrchestratorError::InvalidDag(
                "task cannot depend on itself".to_owned(),
            ));
        }
        if self.status == TaskStatus::Ready
            && proposed
                .difference(&current)
                .any(|task_id| !completed_dependencies.contains(task_id))
        {
            return Err(OrchestratorError::InvalidDag(
                "READY task may only gain already-completed dependencies".to_owned(),
            ));
        }
        self.dependencies = candidate;
        Ok(())
    }

    pub(crate) fn strengthen_verification(
        &mut self,
        candidate: Vec<VerificationSpec>,
    ) -> Result<(), OrchestratorError> {
        if !matches!(
            self.status,
            TaskStatus::Pending | TaskStatus::Ready | TaskStatus::NeedsReplan
        ) {
            return Err(OrchestratorError::InvalidDag(
                "verification cannot change for active or terminal tasks".to_owned(),
            ));
        }
        if self
            .verification
            .iter()
            .any(|existing| !candidate.contains(existing))
        {
            return Err(OrchestratorError::InvalidDag(
                "replanning may add verification requirements but may not remove them".to_owned(),
            ));
        }
        self.verification = candidate;
        Ok(())
    }

    pub(crate) fn strengthen_mandatory(&mut self) -> Result<bool, OrchestratorError> {
        if self.is_terminal() {
            return Err(OrchestratorError::InvalidDag(
                "terminal task mandatory flag is immutable".to_owned(),
            ));
        }
        if self.mandatory {
            Ok(false)
        } else {
            self.mandatory = true;
            Ok(true)
        }
    }

    pub(crate) fn recover_stale_running(&mut self, now: &str) -> Result<(), OrchestratorError> {
        if self.status != TaskStatus::Running {
            return Ok(());
        }
        if let Some(attempt) = self.attempts.last_mut() {
            attempt.finished_at.get_or_insert_with(|| now.to_owned());
            attempt.outcome.get_or_insert(AttemptOutcome::Interrupted);
        }
        let can_retry_attempt = self.semantic_attempts_remaining() > 0;
        let proposal_only_worker = matches!(
            self.worker,
            WorkerKind::CodexReadonly | WorkerKind::CodexWriter | WorkerKind::CodexReviewer
        );
        if proposal_only_worker {
            if can_retry_attempt {
                self.status = TaskStatus::Retryable;
            } else {
                self.status = TaskStatus::Blocked;
                self.blockers.push(TaskBlocker::recovery(
                    "stale read-only worker exhausted its task attempt budget",
                ));
            }
            self.updated_at = now.to_owned();
            return Ok(());
        }
        if self.scope.operation_kind == TaskOperationKind::ReadOnly
            && self.scope.replay_safety == ReplaySafety::SafeReadOnly
        {
            if can_retry_attempt {
                self.status = TaskStatus::Retryable;
            } else {
                self.status = TaskStatus::Blocked;
                self.blockers.push(TaskBlocker::recovery(
                    "stale read-only operation exhausted its task attempt budget",
                ));
            }
            self.updated_at = now.to_owned();
            return Ok(());
        }

        let reconciled_postcondition =
            self.evidence
                .iter()
                .rev()
                .find_map(|evidence| match evidence {
                    TaskEvidence::RecoveryReconciliation {
                        side_effect_state,
                        postcondition_proven,
                        ..
                    } => Some((*side_effect_state, *postcondition_proven)),
                    _ => None,
                });
        if matches!(
            reconciled_postcondition,
            Some((SideEffectState::ConfirmedPerformed, true))
        ) {
            self.status = TaskStatus::Verifying;
            self.updated_at = now.to_owned();
            return Ok(());
        }

        let attempt = self.latest_attempt();
        let state = attempt.and_then(|attempt| attempt.side_effect_state);
        let remaining_attempt = attempt.and_then(|attempt| attempt.remaining_attempt_budget);
        let remaining_side_effect =
            attempt.and_then(|attempt| attempt.remaining_side_effect_budget);
        if state == Some(SideEffectState::ConfirmedNotPerformed)
            && can_retry_attempt
            && remaining_attempt.unwrap_or(0) > 0
            && remaining_side_effect.unwrap_or(0) > 0
            && self.scope.replay_safety != ReplaySafety::NeverAutomatic
        {
            self.status = TaskStatus::Retryable;
        } else {
            self.status = TaskStatus::Blocked;
            self.blockers.push(TaskBlocker::recovery(
                "stale mutating operation is not independently safe to replay; reconciliation is required",
            ));
        }
        self.updated_at = now.to_owned();
        Ok(())
    }

    pub(crate) fn recover_legacy_readonly_timeout(
        &mut self,
        _authority: &crate::goal_api::ReadonlyTransportRecoveryAuthority,
        now: &str,
    ) -> Result<bool, OrchestratorError> {
        if self.status != TaskStatus::Failed
            || self.worker != WorkerKind::CodexReadonly
            || self.scope.operation_kind != TaskOperationKind::ReadOnly
            || self.scope.replay_safety != ReplaySafety::SafeReadOnly
            || !self.blockers.is_empty()
            || !self.verification_results.is_empty()
        {
            return Ok(false);
        }
        if self.readonly_transport_interruptions()
            >= MAX_READONLY_TRANSPORT_INTERRUPTS_PER_TASK
        {
            return Ok(false);
        }
        let Some(latest) = self.latest_attempt() else {
            return Ok(false);
        };
        if latest.worker != WorkerKind::CodexReadonly
            || latest.outcome != Some(AttemptOutcome::Failed)
            || latest.operation_id.is_some()
            || latest.scope_identity.is_some()
            || latest.side_effect_class != Some(SideEffectClass::None)
            || latest.side_effect_state != Some(SideEffectState::ConfirmedNotPerformed)
            || latest.remaining_attempt_budget != Some(0)
            || latest
                .worker_report
                .as_ref()
                .map(WorkerReport::summary)
                != Some(LEGACY_READONLY_TIMEOUT_REPORT)
        {
            return Ok(false);
        }
        if self.attempts.iter().any(|attempt| {
            attempt.worker != WorkerKind::CodexReadonly
                || attempt.operation_id.is_some()
                || attempt.scope_identity.is_some()
                || attempt.side_effect_class != Some(SideEffectClass::None)
                || attempt.side_effect_state != Some(SideEffectState::ConfirmedNotPerformed)
        }) {
            return Ok(false);
        }
        if self.evidence.iter().any(|evidence| match evidence {
            TaskEvidence::RecoveryReconciliation {
                side_effect_state: SideEffectState::ConfirmedPerformed | SideEffectState::Unknown,
                ..
            } => true,
            TaskEvidence::GitSnapshot {
                changed_paths,
                staged_paths,
                ..
            } => !changed_paths.is_empty() || !staged_paths.is_empty(),
            _ => false,
        }) {
            return Ok(false);
        }
        let attempt_id = latest.id.clone();
        let consumed_after_recovery = self
            .semantic_attempts_consumed()
            .checked_sub(1)
            .ok_or_else(|| OrchestratorError::CorruptGoal(
                "legacy timeout recovery semantic budget underflow".to_owned(),
            ))?;
        if consumed_after_recovery >= self.max_attempts {
            return Ok(false);
        }
        self.evidence.push(TaskEvidence::ReadonlyTransportRecovery {
            attempt_id,
            classification: ReadonlyTransportRecoveryKind::LegacyTimeoutTerminalization,
            authority: ReadonlyTransportRecoveryAuthorityKind::GoalResume,
            side_effect_state: SideEffectState::ConfirmedNotPerformed,
            postcondition_proven: true,
        });
        self.status = TaskStatus::Retryable;
        self.updated_at = now.to_owned();
        Ok(true)
    }

    fn latest_verification_passed(&self) -> bool {
        self.verification_results
            .last()
            .is_some_and(|result| result.outcome == VerificationOutcome::Passed)
    }

    pub(crate) fn verification_retry_allowed(&self) -> bool {
        self.retry_allowed()
    }

    fn retry_allowed(&self) -> bool {
        if self.semantic_attempts_remaining() == 0 {
            return false;
        }
        if matches!(
            self.worker,
            WorkerKind::CodexReadonly | WorkerKind::CodexWriter | WorkerKind::CodexReviewer
        ) {
            return true;
        }
        if self.scope.operation_kind == TaskOperationKind::ReadOnly {
            return self.scope.replay_safety == ReplaySafety::SafeReadOnly;
        }
        if self.scope.replay_safety == ReplaySafety::NeverAutomatic {
            return false;
        }
        self.latest_attempt().is_some_and(|attempt| {
            attempt.side_effect_state == Some(SideEffectState::ConfirmedNotPerformed)
                && attempt.remaining_attempt_budget.unwrap_or(0) > 0
                && attempt.remaining_side_effect_budget.unwrap_or(0) > 0
        })
    }
}

fn increases_budget(previous: Option<u32>, next: Option<u32>) -> bool {
    matches!((previous, next), (Some(previous), Some(next)) if next > previous)
}

fn ensure_budget_not_increased(
    previous: Option<u32>,
    next: Option<u32>,
) -> Result<(), OrchestratorError> {
    if increases_budget(previous, next) {
        return Err(OrchestratorError::InvalidDag(
            "low-level operation budget cannot be replenished by task retry".to_owned(),
        ));
    }
    Ok(())
}

fn invalid_task_transition(
    from: TaskStatus,
    to: TaskStatus,
    reason: impl Into<String>,
) -> OrchestratorError {
    OrchestratorError::invalid_transition(
        "task",
        format!("{from:?}").to_ascii_uppercase(),
        format!("{to:?}").to_ascii_uppercase(),
        reason,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-01-01T00:00:00Z";

    fn read_only_scope() -> TaskScope {
        TaskScope::new(
            vec![PathBuf::from("src")],
            vec![],
            TaskOperationKind::ReadOnly,
            ReplaySafety::SafeReadOnly,
        )
    }

    fn task(worker: WorkerKind) -> Task {
        Task::new(
            "title",
            "objective",
            true,
            worker,
            read_only_scope(),
            vec![],
            3,
            0,
            NOW,
        )
        .unwrap()
    }

    #[test]
    fn enum_serialization_roundtrip_is_deterministic() {
        let statuses = [
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
        ];
        for status in statuses {
            let encoded = serde_json::to_string(&status).unwrap();
            let decoded: TaskStatus = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, status);
        }
        assert_eq!(
            serde_json::to_string(&WorkerKind::CodexWriter).unwrap(),
            "\"CODEX_WRITER\""
        );
    }

    #[test]
    fn task_serialization_roundtrip_preserves_durable_state() {
        let original = task(WorkerKind::CodexReadonly);
        let encoded = serde_json::to_vec_pretty(&original).unwrap();
        let decoded: Task = serde_json::from_slice(&encoded).unwrap();
        decoded.validate_local().unwrap();
        assert_eq!(decoded.id(), original.id());
        assert_eq!(decoded.status(), TaskStatus::Pending);
    }

    #[test]
    fn legal_task_transition_path_requires_verification() {
        let mut task = task(WorkerKind::CodexReadonly);
        let context = TaskTransitionContext {
            active_worker_stopped: true,
            side_effect_reconciled: true,
        };
        task.transition_to(TaskStatus::Ready, true, context, NOW)
            .unwrap();
        task.transition_to(TaskStatus::Running, true, context, NOW)
            .unwrap();
        assert!(
            task.transition_to(TaskStatus::Completed, true, context, NOW)
                .is_err()
        );
        task.transition_to(TaskStatus::Verifying, true, context, NOW)
            .unwrap();
        task.record_verification_result(VerificationResult::new(
            VerificationOutcome::Passed,
            vec![],
            NOW,
            NOW,
        ))
        .unwrap();
        task.transition_to(TaskStatus::Completed, true, context, NOW)
            .unwrap();
        assert_eq!(task.status(), TaskStatus::Completed);
    }

    #[test]
    fn legal_retry_block_and_replan_transitions_are_enforced() {
        let context = TaskTransitionContext::default();
        for target in [
            TaskStatus::Retryable,
            TaskStatus::Blocked,
            TaskStatus::NeedsReplan,
            TaskStatus::Failed,
        ] {
            let mut task = task(WorkerKind::CodexReadonly);
            task.transition_to(TaskStatus::Ready, true, context, NOW)
                .unwrap();
            task.transition_to(TaskStatus::Running, true, context, NOW)
                .unwrap();
            task.transition_to(target, true, context, NOW).unwrap();
        }
        let mut retry = task(WorkerKind::CodexReadonly);
        retry
            .transition_to(TaskStatus::Ready, true, context, NOW)
            .unwrap();
        retry
            .transition_to(TaskStatus::Running, true, context, NOW)
            .unwrap();
        retry
            .transition_to(TaskStatus::Retryable, true, context, NOW)
            .unwrap();
        retry
            .transition_to(TaskStatus::Ready, true, context, NOW)
            .unwrap();

        let mut blocked = task(WorkerKind::CodexReadonly);
        blocked
            .transition_to(TaskStatus::Ready, true, context, NOW)
            .unwrap();
        blocked
            .transition_to(TaskStatus::Running, true, context, NOW)
            .unwrap();
        blocked
            .transition_to(TaskStatus::Blocked, true, context, NOW)
            .unwrap();
        blocked
            .transition_to(TaskStatus::Ready, true, context, NOW)
            .unwrap();

        let mut replan = task(WorkerKind::CodexReadonly);
        replan
            .transition_to(TaskStatus::Ready, true, context, NOW)
            .unwrap();
        replan
            .transition_to(TaskStatus::Running, true, context, NOW)
            .unwrap();
        replan
            .transition_to(TaskStatus::NeedsReplan, true, context, NOW)
            .unwrap();
        replan
            .transition_to(TaskStatus::Pending, true, context, NOW)
            .unwrap();
    }

    #[test]
    fn representative_illegal_task_transitions_are_rejected() {
        let context = TaskTransitionContext::default();
        let mut pending = task(WorkerKind::CodexReadonly);
        assert!(
            pending
                .transition_to(TaskStatus::Completed, true, context, NOW)
                .is_err()
        );
        pending
            .transition_to(TaskStatus::Ready, true, context, NOW)
            .unwrap();
        assert!(
            pending
                .transition_to(TaskStatus::Completed, true, context, NOW)
                .is_err()
        );
        pending
            .transition_to(TaskStatus::Running, true, context, NOW)
            .unwrap();
        assert!(
            pending
                .transition_to(TaskStatus::Completed, true, context, NOW)
                .is_err()
        );
    }

    #[test]
    fn terminal_tasks_are_immutable() {
        for terminal in [
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            let mut task = task(WorkerKind::CodexReadonly);
            task.status = terminal;
            if terminal == TaskStatus::Completed {
                task.verification_results.push(VerificationResult::new(
                    VerificationOutcome::Passed,
                    vec![],
                    NOW,
                    NOW,
                ));
            }
            assert!(
                task.transition_to(
                    TaskStatus::Running,
                    true,
                    TaskTransitionContext::default(),
                    NOW
                )
                .is_err()
            );
        }
    }

    #[test]
    fn ready_requires_completed_dependencies() {
        let mut task = task(WorkerKind::CodexReadonly);
        assert!(
            task.transition_to(
                TaskStatus::Ready,
                false,
                TaskTransitionContext::default(),
                NOW
            )
            .is_err()
        );
        task.transition_to(
            TaskStatus::Ready,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
    }

    #[test]
    fn active_cancel_requires_worker_stop_and_side_effect_reconciliation() {
        let mut task = task(WorkerKind::CodexReadonly);
        task.transition_to(
            TaskStatus::Ready,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        task.transition_to(
            TaskStatus::Running,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        assert!(
            task.transition_to(
                TaskStatus::Cancelled,
                true,
                TaskTransitionContext::default(),
                NOW
            )
            .is_err()
        );
        task.transition_to(
            TaskStatus::Cancelled,
            true,
            TaskTransitionContext {
                active_worker_stopped: true,
                side_effect_reconciled: true,
            },
            NOW,
        )
        .unwrap();
    }

    #[test]
    fn budget_continuity_rejects_replenishment() {
        let mut task = task(WorkerKind::LocalOperation);
        task.scope = TaskScope::new(
            vec![],
            vec![],
            TaskOperationKind::LocalMutation,
            ReplaySafety::VerifyBeforeRetry,
        );
        task.transition_to(
            TaskStatus::Ready,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        task.transition_to(
            TaskStatus::Running,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        task.bind_latest_attempt_execution(
            Some("operation-1".into()),
            Some("scope-1".into()),
            Some("request-1".into()),
            Some(SideEffectClass::LocalMutation),
            Some(SideEffectState::ConfirmedNotPerformed),
            Some(1),
            Some(1),
        )
        .unwrap();
        assert!(
            task.bind_latest_attempt_execution(None, None, None, None, None, Some(2), Some(1),)
                .is_err()
        );
    }

    fn running_mutation_task() -> Task {
        let mut task = task(WorkerKind::LocalOperation);
        task.scope = TaskScope::new(
            vec![],
            vec![],
            TaskOperationKind::LocalMutation,
            ReplaySafety::VerifyBeforeRetry,
        );
        task.transition_to(
            TaskStatus::Ready,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        task.transition_to(
            TaskStatus::Running,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        task
    }

    #[test]
    fn bind_execution_attempt_budget_failure_leaves_task_exactly_unchanged() {
        let mut task = running_mutation_task();
        task.bind_latest_attempt_execution(
            Some("operation-1".into()),
            Some("scope-1".into()),
            Some("request-1".into()),
            Some(SideEffectClass::LocalMutation),
            Some(SideEffectState::ConfirmedNotPerformed),
            Some(1),
            Some(1),
        )
        .unwrap();
        let before = task.clone();
        assert!(
            task.bind_latest_attempt_execution(
                Some("operation-1".into()),
                Some("scope-1".into()),
                Some("request-2".into()),
                None,
                None,
                Some(2),
                Some(1),
            )
            .is_err()
        );
        assert_eq!(task, before);
    }

    #[test]
    fn bind_execution_side_effect_budget_failure_leaves_task_exactly_unchanged() {
        let mut task = running_mutation_task();
        task.bind_latest_attempt_execution(
            Some("operation-1".into()),
            Some("scope-1".into()),
            Some("request-1".into()),
            Some(SideEffectClass::LocalMutation),
            Some(SideEffectState::ConfirmedNotPerformed),
            Some(1),
            Some(1),
        )
        .unwrap();
        let before = task.clone();
        assert!(
            task.bind_latest_attempt_execution(
                Some("operation-1".into()),
                Some("scope-1".into()),
                Some("request-2".into()),
                None,
                None,
                Some(1),
                Some(2),
            )
            .is_err()
        );
        assert_eq!(task, before);
    }

    #[test]
    fn bind_execution_identity_and_scope_mismatch_leave_task_exactly_unchanged() {
        let mut task = running_mutation_task();
        task.bind_latest_attempt_execution(
            Some("operation-1".into()),
            Some("scope-1".into()),
            Some("request-1".into()),
            Some(SideEffectClass::LocalMutation),
            Some(SideEffectState::ConfirmedNotPerformed),
            Some(1),
            Some(1),
        )
        .unwrap();

        let before_identity = task.clone();
        assert!(
            task.bind_latest_attempt_execution(
                Some("operation-2".into()),
                Some("scope-1".into()),
                None,
                None,
                None,
                Some(1),
                Some(1),
            )
            .is_err()
        );
        assert_eq!(task, before_identity);

        let before_scope = task.clone();
        assert!(
            task.bind_latest_attempt_execution(
                Some("operation-1".into()),
                Some("scope-2".into()),
                None,
                None,
                None,
                Some(1),
                Some(1),
            )
            .is_err()
        );
        assert_eq!(task, before_scope);
    }

    #[test]
    fn bind_execution_cross_attempt_budget_replenishment_is_transactionally_rejected() {
        let mut task = running_mutation_task();
        task.bind_latest_attempt_execution(
            Some("operation-1".into()),
            Some("scope-1".into()),
            None,
            Some(SideEffectClass::LocalMutation),
            Some(SideEffectState::ConfirmedNotPerformed),
            Some(1),
            Some(1),
        )
        .unwrap();
        task.transition_to(
            TaskStatus::Retryable,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        task.transition_to(
            TaskStatus::Ready,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        task.transition_to(
            TaskStatus::Running,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();

        let before = task.clone();
        assert!(
            task.bind_latest_attempt_execution(
                Some("operation-1".into()),
                Some("scope-1".into()),
                None,
                Some(SideEffectClass::LocalMutation),
                Some(SideEffectState::ConfirmedNotPerformed),
                Some(2),
                Some(1),
            )
            .is_err()
        );
        assert_eq!(task, before);
    }

    #[test]
    fn stale_readonly_running_recovers_to_retryable() {
        let mut task = task(WorkerKind::CodexReadonly);
        task.transition_to(
            TaskStatus::Ready,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        task.transition_to(
            TaskStatus::Running,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        task.recover_stale_running("2026-01-02T00:00:00Z").unwrap();
        assert_eq!(task.status(), TaskStatus::Retryable);
    }

    #[test]
    fn stale_unknown_mutation_recovers_to_blocked() {
        let mut task = task(WorkerKind::LocalOperation);
        task.scope = TaskScope::new(
            vec![],
            vec![],
            TaskOperationKind::LocalMutation,
            ReplaySafety::VerifyBeforeRetry,
        );
        task.transition_to(
            TaskStatus::Ready,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        task.transition_to(
            TaskStatus::Running,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        task.bind_latest_attempt_execution(
            Some("op".into()),
            None,
            None,
            Some(SideEffectClass::LocalMutation),
            Some(SideEffectState::Unknown),
            Some(1),
            Some(1),
        )
        .unwrap();
        task.recover_stale_running("2026-01-02T00:00:00Z").unwrap();
        assert_eq!(task.status(), TaskStatus::Blocked);
    }

    #[test]
    fn independently_proven_mutation_recovers_to_verifying_not_completed() {
        let mut task = task(WorkerKind::LocalOperation);
        task.scope = TaskScope::new(
            vec![],
            vec![],
            TaskOperationKind::LocalMutation,
            ReplaySafety::VerifyBeforeRetry,
        );
        task.transition_to(
            TaskStatus::Ready,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        task.transition_to(
            TaskStatus::Running,
            true,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        task.bind_latest_attempt_execution(
            Some("op".into()),
            None,
            None,
            Some(SideEffectClass::LocalMutation),
            Some(SideEffectState::ConfirmedPerformed),
            Some(0),
            Some(0),
        )
        .unwrap();
        task.add_evidence(TaskEvidence::RecoveryReconciliation {
            summary: "postcondition independently proven".into(),
            side_effect_state: SideEffectState::ConfirmedPerformed,
            postcondition_proven: true,
        })
        .unwrap();
        task.recover_stale_running("2026-01-02T00:00:00Z").unwrap();
        assert_eq!(task.status(), TaskStatus::Verifying);
        assert_ne!(task.status(), TaskStatus::Completed);
    }

    #[test]
    fn dependency_and_verification_weakening_are_rejected() {
        let dep_a = TaskId::new();
        let dep_b = TaskId::new();
        let mut task = task(WorkerKind::CodexReadonly);
        task.dependencies = vec![TaskDependency::completed(dep_a.clone())];
        let completed = BTreeSet::from([dep_a.clone(), dep_b.clone()]);
        assert!(task.strengthen_dependencies(vec![], &completed).is_err());
        task.strengthen_dependencies(
            vec![
                TaskDependency::completed(dep_a),
                TaskDependency::completed(dep_b),
            ],
            &completed,
        )
        .unwrap();

        let original = VerificationSpec::ReviewGate {
            max_blocking_findings: 0,
        };
        task.verification = vec![original.clone()];
        assert!(task.strengthen_verification(vec![]).is_err());
        task.strengthen_verification(vec![
            original,
            VerificationSpec::StructuredEvidence {
                requirement_id: "extra".into(),
            },
        ])
        .unwrap();
    }
}
