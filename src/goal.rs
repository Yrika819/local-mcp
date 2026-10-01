use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

use serde::de::Error as _;
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config;
use crate::failure_class::FailureClass;
use crate::fallback::SideEffectState;
use crate::managed_worktree::{
    ManagedWorktreeCreationIntent, ManagedWorktreeRecord, WorkspaceMode,
};
use crate::mutation::{MutationIntent, MutationIntentState, MutationIntentUpdate};
use crate::orchestrator_error::OrchestratorError;
use crate::task::{
    AttemptId, Task, TaskDependency, TaskId, TaskOperationKind, TaskStatus, TaskTransitionContext,
    VerificationId, VerificationOutcome, VerificationResult, VerificationSpec, WorkerReport,
};
#[cfg(test)]
use crate::task::{ReplaySafety, TaskScope, WorkerKind};

pub(crate) const GOAL_STORE_FORMAT: &str = "local-mcp-goal";

/// Schema 4 adds explicit Managed Worktrees V1 workspace identity
/// (`workspace_mode`, `managed_worktree`, `managed_worktree_creation_intent`).
///
/// Managed workspace identity changes Goal authority/evidence semantics, so
/// `docs/MANAGED_WORKTREES_V1_DESIGN.md` section 9 forbids smuggling it in as
/// an unversioned assumption. Schema 1/2/3 Goals stay readable and migrate to
/// `PRIMARY` with no invented worktree ownership.
///
/// Schema 5 adds the durable lifetime creation-attempt budget. It is an explicit
/// advance because the new field changes durable retry authority: a schema-4
/// managed record has no attempt count, and the migration must decide one
/// without inventing permission to mutate Git again.
pub(crate) const GOAL_SCHEMA_VERSION: u32 = 5;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct GoalId(String);

impl GoalId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub(crate) fn parse(value: &str) -> Result<Self, OrchestratorError> {
        let uuid = Uuid::parse_str(value)
            .map_err(|_| OrchestratorError::UnsafeIdentifier("GoalId".to_owned()))?;
        Ok(Self(uuid.to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
    pub(crate) fn validate(&self) -> Result<(), OrchestratorError> {
        if Self::parse(&self.0)?.0 != self.0 {
            return Err(OrchestratorError::UnsafeIdentifier("GoalId".to_owned()));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct CompletionCriterionId(String);

impl CompletionCriterionId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }
    pub(crate) fn parse(value: &str) -> Result<Self, OrchestratorError> {
        let uuid = Uuid::parse_str(value)
            .map_err(|_| OrchestratorError::UnsafeIdentifier("CompletionCriterionId".to_owned()))?;
        Ok(Self(uuid.to_string()))
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
    fn validate(&self) -> Result<(), OrchestratorError> {
        if Self::parse(&self.0)?.0 != self.0 {
            return Err(OrchestratorError::UnsafeIdentifier(
                "CompletionCriterionId".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoalCompletionCriterion {
    id: CompletionCriterionId,
    description: String,
    required: bool,
}

impl GoalCompletionCriterion {
    fn new(description: String) -> Result<Self, OrchestratorError> {
        if description.trim().is_empty() {
            return Err(OrchestratorError::CorruptGoal(
                "completion criterion description must not be empty".to_owned(),
            ));
        }
        Ok(Self {
            id: CompletionCriterionId::new(),
            description,
            required: true,
        })
    }
    pub(crate) fn id(&self) -> &CompletionCriterionId {
        &self.id
    }
    pub(crate) fn description(&self) -> &str {
        &self.description
    }
    pub(crate) fn required(&self) -> bool {
        self.required
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub(crate) enum GoalVerificationRequirement {
    TaskVerified { task_id: TaskId },
}

impl GoalVerificationRequirement {
    pub(crate) fn task_id(&self) -> &TaskId {
        match self {
            Self::TaskVerified { task_id } => task_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoalCriterionBinding {
    criterion_id: CompletionCriterionId,
    requirements: Vec<GoalVerificationRequirement>,
}

impl GoalCriterionBinding {
    pub(crate) fn new(
        criterion_id: CompletionCriterionId,
        requirements: Vec<GoalVerificationRequirement>,
    ) -> Self {
        Self {
            criterion_id,
            requirements,
        }
    }
    pub(crate) fn criterion_id(&self) -> &CompletionCriterionId {
        &self.criterion_id
    }
    pub(crate) fn requirements(&self) -> &[GoalVerificationRequirement] {
        &self.requirements
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoalFinalVerificationSpec {
    plan_revision: u32,
    criterion_bindings: Vec<GoalCriterionBinding>,
}

#[expect(
    dead_code,
    reason = "Frozen final-verification accessors are retained for staged Goal Orchestrator authority wiring."
)]
impl GoalFinalVerificationSpec {
    pub(crate) fn new(plan_revision: u32, criterion_bindings: Vec<GoalCriterionBinding>) -> Self {
        Self {
            plan_revision,
            criterion_bindings,
        }
    }
    pub(crate) fn plan_revision(&self) -> u32 {
        self.plan_revision
    }
    pub(crate) fn criterion_bindings(&self) -> &[GoalCriterionBinding] {
        &self.criterion_bindings
    }
    pub(crate) fn canonical_digest(&self) -> String {
        let mut bindings = self.criterion_bindings.clone();
        bindings.sort_by(|a, b| a.criterion_id.cmp(&b.criterion_id));
        let mut canonical = String::from("goal-final-verification-v1\n");
        for binding in bindings {
            canonical.push_str(binding.criterion_id.as_str());
            canonical.push('\n');
            let mut requirements = binding.requirements;
            requirements.sort();
            for requirement in requirements {
                match requirement {
                    GoalVerificationRequirement::TaskVerified { task_id } => {
                        canonical.push_str("TASK_VERIFIED:");
                        canonical.push_str(task_id.as_str());
                        canonical.push('\n');
                    }
                }
            }
        }
        format!("{:x}", Sha256::digest(canonical.as_bytes()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum VerificationOrigin {
    HostDeterministicGoalVerifier,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub(crate) enum GoalRequirementObservation {
    TaskVerified {
        task_id: TaskId,
        task_status: TaskStatus,
        verification_id: Option<VerificationId>,
        verification_outcome: Option<VerificationOutcome>,
        outcome: VerificationOutcome,
    },
}

#[expect(
    dead_code,
    reason = "Frozen verification-observation accessors are retained for staged verifier consumers."
)]
impl GoalRequirementObservation {
    pub(crate) fn task_verification_identity(&self) -> (&TaskId, Option<&VerificationId>) {
        match self {
            Self::TaskVerified {
                task_id,
                verification_id,
                ..
            } => (task_id, verification_id.as_ref()),
        }
    }
    pub(crate) fn outcome(&self) -> VerificationOutcome {
        match self {
            Self::TaskVerified { outcome, .. } => *outcome,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoalCriterionVerificationResult {
    criterion_id: CompletionCriterionId,
    outcome: VerificationOutcome,
    observations: Vec<GoalRequirementObservation>,
}

#[expect(
    dead_code,
    reason = "Frozen criterion-result accessors are retained for staged final-verification consumers."
)]
impl GoalCriterionVerificationResult {
    pub(crate) fn new(
        criterion_id: CompletionCriterionId,
        outcome: VerificationOutcome,
        observations: Vec<GoalRequirementObservation>,
    ) -> Self {
        Self {
            criterion_id,
            outcome,
            observations,
        }
    }
    pub(crate) fn criterion_id(&self) -> &CompletionCriterionId {
        &self.criterion_id
    }
    pub(crate) fn outcome(&self) -> VerificationOutcome {
        self.outcome
    }
    pub(crate) fn observations(&self) -> &[GoalRequirementObservation] {
        &self.observations
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoalFinalVerificationRecord {
    id: VerificationId,
    outcome: VerificationOutcome,
    source: VerificationOrigin,
    goal_id: GoalId,
    evaluated_goal_revision: u64,
    committed_goal_revision: u64,
    plan_revision: u32,
    contract_digest: String,
    criterion_results: Vec<GoalCriterionVerificationResult>,
    started_at: String,
    finished_at: String,
}

#[expect(
    dead_code,
    reason = "Frozen durable verification-record accessors are retained for staged authority consumers."
)]
#[expect(
    clippy::too_many_arguments,
    reason = "Durable verification identity, revisions, digest, criteria, and timestamps remain explicit."
)]
impl GoalFinalVerificationRecord {
    pub(crate) fn new(
        outcome: VerificationOutcome,
        goal_id: GoalId,
        evaluated_goal_revision: u64,
        committed_goal_revision: u64,
        plan_revision: u32,
        contract_digest: String,
        criterion_results: Vec<GoalCriterionVerificationResult>,
        started_at: impl Into<String>,
        finished_at: impl Into<String>,
    ) -> Self {
        Self {
            id: VerificationId::new(),
            outcome,
            source: VerificationOrigin::HostDeterministicGoalVerifier,
            goal_id,
            evaluated_goal_revision,
            committed_goal_revision,
            plan_revision,
            contract_digest,
            criterion_results,
            started_at: started_at.into(),
            finished_at: finished_at.into(),
        }
    }
    pub(crate) fn id(&self) -> &VerificationId {
        &self.id
    }
    pub(crate) fn outcome(&self) -> VerificationOutcome {
        self.outcome
    }
    pub(crate) fn source(&self) -> VerificationOrigin {
        self.source
    }
    pub(crate) fn goal_id(&self) -> &GoalId {
        &self.goal_id
    }
    pub(crate) fn evaluated_goal_revision(&self) -> u64 {
        self.evaluated_goal_revision
    }
    pub(crate) fn committed_goal_revision(&self) -> u64 {
        self.committed_goal_revision
    }
    pub(crate) fn plan_revision(&self) -> u32 {
        self.plan_revision
    }
    pub(crate) fn contract_digest(&self) -> &str {
        &self.contract_digest
    }
    pub(crate) fn criterion_results(&self) -> &[GoalCriterionVerificationResult] {
        &self.criterion_results
    }
    pub(crate) fn started_at(&self) -> &str {
        &self.started_at
    }
    pub(crate) fn finished_at(&self) -> &str {
        &self.finished_at
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum GoalStatus {
    Planning,
    Running,
    Replanning,
    Pausing,
    Paused,
    Blocked,
    Verifying,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

impl GoalStatus {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// Whether the foreground runner performs no step at all for this state.
    ///
    /// This is the single source of truth for that decision. `goal_runner`
    /// maps it to a stop reason, and managed workspace preparation consults it,
    /// so a new state can never be "runnable" to one and "stopped" to the other.
    /// Managed preparation is an authority call: a Goal the runner would return
    /// from without doing any work must not have a linked worktree and a local
    /// branch created for it.
    pub(crate) fn blocks_foreground_run(self) -> bool {
        self.is_terminal()
            || matches!(
                self,
                Self::Paused | Self::Pausing | Self::Cancelling | Self::Blocked
            )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoalBlocker {
    code: String,
    detail: String,
    mandatory: bool,
}

impl GoalBlocker {
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum CheckpointReason {
    PlanCommitted,
    ReplanCommitted,
    PreExecutionPlanRejected,
    FailedTaskReplanRequested,
    Pause,
    Recovery,
    FinalVerification,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum PreExecutionPlanRejectionAuthorityKind {
    GoalResume,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum PreExecutionPlanReplanPolicy {
    #[default]
    Normal,
    RequireReadonlyReassessment,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreExecutionPlanRejection {
    request_id: String,
    expected_goal_revision: u64,
    observed_goal_revision: u64,
    rejected_plan_revision: u32,
    observed_plan_revision: u32,
    trigger_task_id: TaskId,
    reason: String,
    #[serde(default)]
    replan_policy: PreExecutionPlanReplanPolicy,
    authority: PreExecutionPlanRejectionAuthorityKind,
    rejected_at: String,
}

impl PreExecutionPlanRejection {
    pub(crate) fn matches(
        &self,
        request_id: &str,
        expected_goal_revision: u64,
        expected_plan_revision: u32,
        trigger_task_id: &TaskId,
        reason: &str,
        replan_policy: PreExecutionPlanReplanPolicy,
    ) -> bool {
        self.request_id == request_id
            && self.expected_goal_revision == expected_goal_revision
            && self.rejected_plan_revision == expected_plan_revision
            && self.trigger_task_id == *trigger_task_id
            && self.reason == reason
            && self.replan_policy == replan_policy
    }

    pub(crate) fn request_id(&self) -> &str {
        &self.request_id
    }
    pub(crate) fn rejected_plan_revision(&self) -> u32 {
        self.rejected_plan_revision
    }

    pub(crate) fn observed_goal_revision(&self) -> u64 {
        self.observed_goal_revision
    }

    pub(crate) fn trigger_task_id(&self) -> &TaskId {
        &self.trigger_task_id
    }

    pub(crate) fn reason(&self) -> &str {
        &self.reason
    }

    pub(crate) fn replan_policy(&self) -> PreExecutionPlanReplanPolicy {
        self.replan_policy
    }

    pub(crate) fn rejected_at(&self) -> &str {
        &self.rejected_at
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PristinePlanSupersessionRecord {
    rejection_request_id: String,
    rejected_plan_revision: u32,
    committed_plan_revision: u32,
    canonical_proposal_digest: String,
    affected_task_ids: Vec<TaskId>,
    replacement_task_ids: Vec<TaskId>,
    rebound_criterion_ids: Vec<CompletionCriterionId>,
}

#[expect(
    dead_code,
    reason = "Frozen plan-supersession accessors are retained for staged replanner consumers."
)]
impl PristinePlanSupersessionRecord {
    pub(crate) fn rejection_request_id(&self) -> &str {
        &self.rejection_request_id
    }
    pub(crate) fn canonical_proposal_digest(&self) -> &str {
        &self.canonical_proposal_digest
    }
    pub(crate) fn rejected_plan_revision(&self) -> u32 {
        self.rejected_plan_revision
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoalCheckpoint {
    id: String,
    goal_revision: u64,
    plan_revision: u32,
    at: String,
    reason: CheckpointReason,
    goal_status: GoalStatus,
    active_task_ids: Vec<TaskId>,
}

impl GoalCheckpoint {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn goal_revision(&self) -> u64 {
        self.goal_revision
    }

    pub(crate) fn plan_revision(&self) -> u32 {
        self.plan_revision
    }

    pub(crate) fn at(&self) -> &str {
        &self.at
    }

    pub(crate) fn reason(&self) -> CheckpointReason {
        self.reason
    }

    pub(crate) fn goal_status(&self) -> GoalStatus {
        self.goal_status
    }

    pub(crate) fn active_task_ids(&self) -> &[TaskId] {
        &self.active_task_ids
    }

    fn validate(&self) -> Result<(), OrchestratorError> {
        let parsed = Uuid::parse_str(&self.id).map_err(|_| {
            OrchestratorError::CorruptGoal("checkpoint id is not a UUID".to_owned())
        })?;
        if parsed.to_string() != self.id {
            return Err(OrchestratorError::CorruptGoal(
                "checkpoint id is not canonical".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReplanMutation {
    pub(crate) new_tasks: Vec<Task>,
    pub(crate) add_dependencies: Vec<(TaskId, Vec<TaskId>)>,
    pub(crate) add_verification: Vec<(TaskId, Vec<VerificationSpec>)>,
    pub(crate) strengthen_mandatory: Vec<TaskId>,
    pub(crate) resolve_needs_replan: Vec<TaskId>,
    pub(crate) add_criterion_requirements:
        Vec<(CompletionCriterionId, Vec<GoalVerificationRequirement>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum FailedTaskReplanAuthorityKind {
    #[default]
    GoalResume,
}

/// Why a Task is being routed into the failed-task replacement path.
///
/// The two kinds are distinct durable concepts. `PostAttemptFailure` is
/// derived from structured failure evidence on a consumed attempt;
/// `PreExecutionRejection` reuses an existing pre-execution plan rejection as
/// its authority. Neither is a `pristine_plan_supersession`, which supersedes
/// a whole rejected plan revision and remains a separate mechanism.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum FailedTaskReplanTriggerKind {
    #[default]
    PostAttemptFailure,
    PreExecutionRejection,
}

/// Recovery policy requested for a failed Task.
///
/// `RequireDecomposition` additionally binds the host to accept only a
/// materially decomposed replacement closure; `RequireReplacement` accepts any
/// host-validated bounded closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum FailedTaskReplanPolicy {
    #[default]
    RequireReplacement,
    RequireDecomposition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FailedTaskReplanRequest {
    request_id: String,
    expected_goal_revision: u64,
    observed_goal_revision: u64,
    trigger_plan_revision: u32,
    observed_plan_revision: u32,
    trigger_task_id: TaskId,
    reason: String,
    policy: FailedTaskReplanPolicy,
    trigger_kind: FailedTaskReplanTriggerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    authority_request_id: Option<String>,
    trigger_failure_class: FailureClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trigger_side_effect_state: Option<SideEffectState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    observed_attempt_id: Option<AttemptId>,
    observed_consumed_attempts: u32,
    observed_max_attempts: u32,
    #[serde(default)]
    authority: FailedTaskReplanAuthorityKind,
    requested_at: String,
}

#[expect(
    dead_code,
    reason = "Failed-task replan request accessors back the staged MCP and replanner surfaces."
)]
impl FailedTaskReplanRequest {
    pub(crate) fn matches(
        &self,
        request_id: &str,
        trigger_task_id: &TaskId,
        policy: FailedTaskReplanPolicy,
        reason: &str,
    ) -> bool {
        self.request_id == request_id
            && self.trigger_task_id == *trigger_task_id
            && self.policy == policy
            && self.reason == reason
    }

    pub(crate) fn request_id(&self) -> &str {
        &self.request_id
    }

    pub(crate) fn trigger_task_id(&self) -> &TaskId {
        &self.trigger_task_id
    }

    pub(crate) fn trigger_plan_revision(&self) -> u32 {
        self.trigger_plan_revision
    }

    pub(crate) fn policy(&self) -> FailedTaskReplanPolicy {
        self.policy
    }

    pub(crate) fn trigger_kind(&self) -> FailedTaskReplanTriggerKind {
        self.trigger_kind
    }

    pub(crate) fn authority_request_id(&self) -> Option<&str> {
        self.authority_request_id.as_deref()
    }

    pub(crate) fn trigger_failure_class(&self) -> FailureClass {
        self.trigger_failure_class
    }

    pub(crate) fn trigger_side_effect_state(&self) -> Option<SideEffectState> {
        self.trigger_side_effect_state
    }

    pub(crate) fn reason(&self) -> &str {
        &self.reason
    }

    pub(crate) fn requested_at(&self) -> &str {
        &self.requested_at
    }
}

/// Durable record of one committed generic failed-task replacement.
///
/// The record is the replacement's replay identity: the same
/// `replan_request_id` carrying the same canonical proposal digest is a no-op
/// replay, and a different effective proposal under the same identity is
/// rejected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FailedTaskReplacementRecord {
    replan_request_id: String,
    replaced_task_id: TaskId,
    replaced_plan_revision: u32,
    committed_plan_revision: u32,
    canonical_proposal_digest: String,
    replaced_attempt_ids: Vec<AttemptId>,
    preserved_max_attempts: u32,
    preserved_consumed_attempts: u32,
    replacement_task_ids: Vec<TaskId>,
    completion_closure_task_ids: Vec<TaskId>,
    rewired_dependent_task_ids: Vec<TaskId>,
    rebound_criterion_ids: Vec<CompletionCriterionId>,
}

#[expect(
    dead_code,
    reason = "Failed-task replacement accessors back the staged MCP and replanner surfaces."
)]
impl FailedTaskReplacementRecord {
    pub(crate) fn replan_request_id(&self) -> &str {
        &self.replan_request_id
    }

    pub(crate) fn canonical_proposal_digest(&self) -> &str {
        &self.canonical_proposal_digest
    }

    pub(crate) fn replaced_task_id(&self) -> &TaskId {
        &self.replaced_task_id
    }

    pub(crate) fn completion_closure_task_ids(&self) -> &[TaskId] {
        &self.completion_closure_task_ids
    }

    pub(crate) fn preserved_max_attempts(&self) -> u32 {
        self.preserved_max_attempts
    }

    pub(crate) fn preserved_consumed_attempts(&self) -> u32 {
        self.preserved_consumed_attempts
    }

    pub(crate) fn replaced_attempt_ids(&self) -> &[AttemptId] {
        &self.replaced_attempt_ids
    }

    pub(crate) fn rebound_criterion_ids(&self) -> &[CompletionCriterionId] {
        &self.rebound_criterion_ids
    }
}

/// One durable request to replace a structurally invalid Task with a bounded
/// replacement closure.
#[derive(Clone, Debug)]
pub(crate) struct FailedTaskReplacementMutation {
    pub(crate) replan_request_id: String,
    pub(crate) replaced_task_id: TaskId,
    pub(crate) replaced_plan_revision: u32,
    pub(crate) committed_plan_revision: u32,
    pub(crate) canonical_proposal_digest: String,
    pub(crate) completion_closure_task_ids: Vec<TaskId>,
    pub(crate) criterion_rebindings: Vec<(CompletionCriterionId, Vec<TaskId>)>,
    pub(crate) new_tasks: Vec<Task>,
}

#[derive(Clone, Debug)]
pub(crate) struct PristinePlanSupersessionMutation {
    pub(crate) rejection_request_id: String,
    pub(crate) rejected_plan_revision: u32,
    pub(crate) committed_plan_revision: u32,
    pub(crate) canonical_proposal_digest: String,
    pub(crate) affected_task_ids: Vec<TaskId>,
    pub(crate) new_tasks: Vec<Task>,
    pub(crate) criterion_rebindings: Vec<(CompletionCriterionId, Vec<TaskId>)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Goal {
    store_format: String,
    schema_version: u32,
    revision: u64,
    #[serde(rename = "goal_id")]
    id: GoalId,
    session_id: String,
    cwd: PathBuf,
    objective: String,
    title: Option<String>,
    constraints: Vec<String>,
    completion_criteria: Vec<GoalCompletionCriterion>,
    #[serde(default)]
    final_verification_spec: Option<GoalFinalVerificationSpec>,
    status: GoalStatus,
    plan_revision: u32,
    #[serde(
        serialize_with = "serialize_tasks",
        deserialize_with = "deserialize_tasks"
    )]
    tasks: BTreeMap<TaskId, Task>,
    blockers: Vec<GoalBlocker>,
    #[serde(default)]
    final_verifications: Vec<GoalFinalVerificationRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    legacy_completion_criteria: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_final_verification: Option<VerificationResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pre_execution_plan_rejections: Vec<PreExecutionPlanRejection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pristine_plan_supersessions: Vec<PristinePlanSupersessionRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    failed_task_replan_requests: Vec<FailedTaskReplanRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    failed_task_replacements: Vec<FailedTaskReplacementRecord>,
    #[serde(default, skip_serializing_if = "WorkspaceMode::is_primary")]
    workspace_mode: WorkspaceMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    managed_worktree: Option<ManagedWorktreeRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    managed_worktree_creation_intent: Option<ManagedWorktreeCreationIntent>,
    checkpoints: Vec<GoalCheckpoint>,
    created_at: String,
    updated_at: String,
    completed_at: Option<String>,
}

#[expect(
    dead_code,
    reason = "Goal state-machine methods include the frozen orchestrator contract and are not all reachable from the current read-only public surface."
)]
#[expect(
    clippy::too_many_arguments,
    reason = "Goal authority, revision, scope, verification, budget, and timestamp channels remain explicit."
)]
impl Goal {
    pub(crate) fn new(
        session_id: impl Into<String>,
        cwd: PathBuf,
        objective: impl Into<String>,
        title: Option<String>,
        constraints: Vec<String>,
        completion_criteria: Vec<String>,
        now: &str,
    ) -> Result<Self, OrchestratorError> {
        Self::new_with_id(
            GoalId::new(),
            session_id,
            cwd,
            objective,
            title,
            constraints,
            completion_criteria,
            now,
        )
    }

    pub(crate) fn new_with_id(
        id: GoalId,
        session_id: impl Into<String>,
        cwd: PathBuf,
        objective: impl Into<String>,
        title: Option<String>,
        constraints: Vec<String>,
        completion_criteria: Vec<String>,
        now: &str,
    ) -> Result<Self, OrchestratorError> {
        Self::build_with_id(
            id,
            session_id,
            cwd,
            objective,
            title,
            constraints,
            completion_criteria,
            now,
            None,
        )
    }

    /// Host-owned constructor for an explicitly requested managed worktree.
    ///
    /// The only difference from [`Goal::new_with_id`] is the durable
    /// `MANAGED_WORKTREE` identity: the workspace mode plus the `REQUESTED`
    /// record whose path, branch, base commit, and common directory were all
    /// derived by the host. `PRIMARY` Goals never pass a record, so their
    /// construction and behavior are unchanged.
    #[expect(
        clippy::too_many_arguments,
        reason = "Mirrors new_with_id so managed construction differs only by the record."
    )]
    pub(crate) fn new_managed_with_id(
        id: GoalId,
        session_id: impl Into<String>,
        cwd: PathBuf,
        objective: impl Into<String>,
        title: Option<String>,
        constraints: Vec<String>,
        completion_criteria: Vec<String>,
        now: &str,
        record: ManagedWorktreeRecord,
    ) -> Result<Self, OrchestratorError> {
        Self::build_with_id(
            id,
            session_id,
            cwd,
            objective,
            title,
            constraints,
            completion_criteria,
            now,
            Some(record),
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Goal identity and managed workspace identity are independent authority bindings."
    )]
    fn build_with_id(
        id: GoalId,
        session_id: impl Into<String>,
        cwd: PathBuf,
        objective: impl Into<String>,
        title: Option<String>,
        constraints: Vec<String>,
        completion_criteria: Vec<String>,
        now: &str,
        managed_worktree: Option<ManagedWorktreeRecord>,
    ) -> Result<Self, OrchestratorError> {
        id.validate()?;
        let session_id = session_id.into();
        config::validate_session_id(&session_id)
            .map_err(|_| OrchestratorError::UnsafeIdentifier("session".to_owned()))?;
        let objective = objective.into();
        if objective.trim().is_empty() {
            return Err(OrchestratorError::CorruptGoal(
                "goal objective must not be empty".to_owned(),
            ));
        }
        let criterion_descriptions = if completion_criteria.is_empty() {
            vec![objective.clone()]
        } else {
            completion_criteria
        };
        let completion_criteria = criterion_descriptions
            .into_iter()
            .map(GoalCompletionCriterion::new)
            .collect::<Result<Vec<_>, _>>()?;
        let goal = Self {
            store_format: GOAL_STORE_FORMAT.to_owned(),
            schema_version: GOAL_SCHEMA_VERSION,
            revision: 1,
            id,
            session_id,
            cwd,
            objective,
            title,
            constraints,
            completion_criteria,
            final_verification_spec: None,
            status: GoalStatus::Planning,
            plan_revision: 0,
            tasks: BTreeMap::new(),
            blockers: Vec::new(),
            final_verifications: Vec::new(),
            legacy_completion_criteria: Vec::new(),
            legacy_final_verification: None,
            pre_execution_plan_rejections: Vec::new(),
            pristine_plan_supersessions: Vec::new(),
            failed_task_replan_requests: Vec::new(),
            failed_task_replacements: Vec::new(),
            workspace_mode: if managed_worktree.is_some() {
                WorkspaceMode::ManagedWorktree
            } else {
                WorkspaceMode::default()
            },
            managed_worktree,
            managed_worktree_creation_intent: None,
            checkpoints: Vec::new(),
            created_at: now.to_owned(),
            updated_at: now.to_owned(),
            completed_at: None,
        };
        goal.validate()?;
        Ok(goal)
    }

    pub(crate) fn id(&self) -> &GoalId {
        &self.id
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    /// Workspace selection. `PRIMARY` is the default and the exact behavior of
    /// every Goal that does not explicitly opt in.
    pub(crate) fn workspace_mode(&self) -> WorkspaceMode {
        self.workspace_mode
    }

    /// Durable managed-worktree ownership, if this Goal has any.
    ///
    /// Presence of this value is host-owned durable state. It is never inferred
    /// from a path name, a branch name, or a Git convention.
    pub(crate) fn managed_worktree(&self) -> Option<&ManagedWorktreeRecord> {
        self.managed_worktree.as_ref()
    }

    /// Outstanding creation intent, if managed-worktree creation is in flight.
    pub(crate) fn managed_worktree_creation_intent(
        &self,
    ) -> Option<&ManagedWorktreeCreationIntent> {
        self.managed_worktree_creation_intent.as_ref()
    }

    /// Host-derived effective execution root (design section 13).
    ///
    /// ```text
    /// PRIMARY          -> Goal.cwd
    /// MANAGED_ACTIVE   -> managed worktree_root
    /// ```
    ///
    /// This is a pure function of durable state. It returns `None` while a
    /// managed workspace is only requested, preparing, blocked, or already
    /// removed, which is how design section 5's "Planner MUST NOT materialize a
    /// managed plan while the workspace is only requested/preparing/ambiguous"
    /// is enforced without touching Git or the filesystem.
    ///
    /// Phase 1 computes this only. No Planner, writer, verifier, or session
    /// authority path is routed through it yet; that is Phase 4 execution-root
    /// plumbing and is deliberately not authorized here.
    ///
    /// This is a pure accessor over durable state. It does not re-validate, so it
    /// must only be reached from an already-validated `Goal` — every constructor
    /// and durable load path calls `validate()` first.
    pub(crate) fn execution_root(&self) -> Option<&std::path::Path> {
        use crate::managed_worktree::ManagedWorktreeLifecycle;
        match self.workspace_mode {
            WorkspaceMode::Primary => Some(self.cwd.as_path()),
            WorkspaceMode::ManagedWorktree => {
                let record = self.managed_worktree.as_ref()?;
                if record.lifecycle() != ManagedWorktreeLifecycle::Active {
                    return None;
                }
                Some(record.worktree_root())
            }
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// Whether the foreground runner performs no work at all for this state.
    ///
    /// Delegates to [`GoalStatus::blocks_foreground_run`], which is also what
    /// `goal_runner`'s stop gate uses, so the runner and managed workspace
    /// preparation cannot disagree about which states do no work.
    pub(crate) fn blocks_foreground_run(&self) -> bool {
        self.status.blocks_foreground_run()
    }

    /// Persist the durable `PREPARED` creation intent.
    ///
    /// Design section 11 requires this to be durable **before** `git worktree
    /// add` is invoked, so a crash after this point is mechanically recoverable.
    /// The caller must use `TaskStore::mutate_goal_snapshot`, which advances
    /// `Goal.revision` and leaves `plan_revision` untouched (design section 21).
    pub(crate) fn set_managed_creation_intent(
        &mut self,
        intent: ManagedWorktreeCreationIntent,
    ) -> Result<(), OrchestratorError> {
        if self.managed_worktree_creation_intent.is_some() {
            return Err(OrchestratorError::CorruptGoal(
                "managed-worktree creation intent is already outstanding".to_owned(),
            ));
        }
        let record = self.managed_worktree.as_ref().ok_or_else(|| {
            OrchestratorError::CorruptGoal(
                "MANAGED_WORKTREE workspace requires a durable managed-worktree record".to_owned(),
            )
        })?;
        let prepared = record.to_prepared()?;
        intent.validate_against_record(&prepared)?;
        self.managed_worktree = Some(prepared);
        self.managed_worktree_creation_intent = Some(intent);
        Ok(())
    }

    /// Replace the outstanding intent with a fresh operation identity.
    ///
    /// Design section 11's bounded retry is permitted only after a mechanically
    /// proven no-side-effect result. The record stays `PREPARED`; only the
    /// operation identity changes, so the two attempts stay distinguishable in
    /// durable evidence.
    pub(crate) fn renew_managed_creation_intent(
        &mut self,
        intent: ManagedWorktreeCreationIntent,
    ) -> Result<(), OrchestratorError> {
        if self.managed_worktree_creation_intent.is_none() {
            return Err(OrchestratorError::CorruptGoal(
                "managed-worktree creation retry requires an outstanding PREPARED intent"
                    .to_owned(),
            ));
        }
        let record = self.managed_worktree.as_ref().ok_or_else(|| {
            OrchestratorError::CorruptGoal(
                "MANAGED_WORKTREE workspace requires a durable managed-worktree record".to_owned(),
            )
        })?;
        if !record.permits_creation_intent() {
            return Err(OrchestratorError::CorruptGoal(
                "managed-worktree lifecycle cannot carry a prepared creation intent".to_owned(),
            ));
        }
        intent.validate_against_record(record)?;
        self.managed_worktree_creation_intent = Some(intent);
        Ok(())
    }

    /// Commit the reconciled `ACTIVE` binding and clear the outstanding intent.
    ///
    /// The caller must already hold a mechanical read-only observation proving
    /// exact durable ownership. `plan_revision` is deliberately not touched.
    pub(crate) fn activate_managed_worktree(
        &mut self,
        head: &str,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        let record = self.managed_worktree.as_ref().ok_or_else(|| {
            OrchestratorError::CorruptGoal(
                "MANAGED_WORKTREE workspace requires a durable managed-worktree record".to_owned(),
            )
        })?;
        let active = record.to_active(head, now)?;
        self.managed_worktree = Some(active);
        self.managed_worktree_creation_intent = None;
        Ok(())
    }

    /// Durably consume one authorized host Git creation attempt.
    ///
    /// The caller must persist the result **before** invoking Git, so a crash
    /// after this point costs the attempt instead of refunding it. It is
    /// monotonic and is never reset by a restart, a resume, a proven
    /// no-side-effect reconciliation, or reaching `ACTIVE`/`BLOCKED`.
    pub(crate) fn consume_managed_creation_attempt(&mut self) -> Result<(), OrchestratorError> {
        let record = self.managed_worktree.as_ref().ok_or_else(|| {
            OrchestratorError::CorruptGoal(
                "MANAGED_WORKTREE workspace requires a durable managed-worktree record".to_owned(),
            )
        })?;
        let consumed = record.consume_creation_attempt()?;
        self.managed_worktree = Some(consumed);
        Ok(())
    }

    /// Record a `BLOCKED` managed workspace with the evidence that stopped it.
    ///
    /// The outstanding intent is retained so explicit host recovery can still
    /// reconcile the exact intended target. `BLOCKED` is never a retry
    /// permission: only `Reconciliation::NoSideEffect` is.
    pub(crate) fn block_managed_worktree(&mut self) -> Result<(), OrchestratorError> {
        let record = self.managed_worktree.as_ref().ok_or_else(|| {
            OrchestratorError::CorruptGoal(
                "MANAGED_WORKTREE workspace requires a durable managed-worktree record".to_owned(),
            )
        })?;
        let blocked = record.to_blocked()?;
        self.managed_worktree = Some(blocked);
        Ok(())
    }

    pub(crate) fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub(crate) fn status(&self) -> GoalStatus {
        self.status
    }

    pub(crate) fn plan_revision(&self) -> u32 {
        self.plan_revision
    }

    pub(crate) fn objective(&self) -> &str {
        &self.objective
    }

    pub(crate) fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub(crate) fn constraints(&self) -> &[String] {
        &self.constraints
    }

    pub(crate) fn completion_criteria(&self) -> &[GoalCompletionCriterion] {
        &self.completion_criteria
    }

    pub(crate) fn completion_criterion_descriptions(&self) -> Vec<String> {
        if self.schema_version == 1 {
            return self.legacy_completion_criteria.clone();
        }
        self.completion_criteria
            .iter()
            .map(|criterion| criterion.description.clone())
            .collect()
    }

    pub(crate) fn final_verification_spec(&self) -> Option<&GoalFinalVerificationSpec> {
        self.final_verification_spec.as_ref()
    }

    pub(crate) fn tasks(&self) -> &BTreeMap<TaskId, Task> {
        &self.tasks
    }

    pub(crate) fn blockers(&self) -> &[GoalBlocker] {
        &self.blockers
    }

    pub(crate) fn final_verifications(&self) -> &[GoalFinalVerificationRecord] {
        &self.final_verifications
    }

    pub(crate) fn legacy_final_verification(&self) -> Option<&VerificationResult> {
        self.legacy_final_verification.as_ref()
    }

    pub(crate) fn latest_applicable_final_verification(
        &self,
    ) -> Option<&GoalFinalVerificationRecord> {
        if self.schema_version != GOAL_SCHEMA_VERSION {
            return None;
        }
        let digest = self.final_verification_spec.as_ref()?.canonical_digest();
        self.final_verifications.iter().rev().find(|record| {
            let revision_applicable = record.committed_goal_revision == self.revision
                || (self.status == GoalStatus::Completed
                    && record.committed_goal_revision.checked_add(1) == Some(self.revision));
            record.source == VerificationOrigin::HostDeterministicGoalVerifier
                && record.goal_id == self.id
                && record.plan_revision == self.plan_revision
                && record.contract_digest == digest
                && revision_applicable
                && record_task_identities_current(self, record)
        })
    }

    pub(crate) fn checkpoints(&self) -> &[GoalCheckpoint] {
        &self.checkpoints
    }

    pub(crate) fn pre_execution_plan_rejections(&self) -> &[PreExecutionPlanRejection] {
        &self.pre_execution_plan_rejections
    }

    pub(crate) fn find_pre_execution_plan_rejection(
        &self,
        request_id: &str,
    ) -> Option<&PreExecutionPlanRejection> {
        self.pre_execution_plan_rejections
            .iter()
            .find(|record| record.request_id() == request_id)
    }

    pub(crate) fn pristine_plan_supersessions(&self) -> &[PristinePlanSupersessionRecord] {
        &self.pristine_plan_supersessions
    }

    pub(crate) fn find_pristine_plan_supersession(
        &self,
        request_id: &str,
    ) -> Option<&PristinePlanSupersessionRecord> {
        self.pristine_plan_supersessions
            .iter()
            .find(|record| record.rejection_request_id() == request_id)
    }

    pub(crate) fn has_pre_execution_plan_rejection_for_plan(&self, plan_revision: u32) -> bool {
        self.pre_execution_plan_rejections
            .iter()
            .any(|record| record.rejected_plan_revision() == plan_revision)
    }

    pub(crate) fn failed_task_replan_requests(&self) -> &[FailedTaskReplanRequest] {
        &self.failed_task_replan_requests
    }

    pub(crate) fn find_failed_task_replan_request(
        &self,
        request_id: &str,
    ) -> Option<&FailedTaskReplanRequest> {
        self.failed_task_replan_requests
            .iter()
            .find(|request| request.request_id() == request_id)
    }

    pub(crate) fn failed_task_replacements(&self) -> &[FailedTaskReplacementRecord] {
        &self.failed_task_replacements
    }

    pub(crate) fn find_failed_task_replacement(
        &self,
        replan_request_id: &str,
    ) -> Option<&FailedTaskReplacementRecord> {
        self.failed_task_replacements
            .iter()
            .find(|record| record.replan_request_id() == replan_request_id)
    }

    pub(crate) fn created_at(&self) -> &str {
        &self.created_at
    }

    pub(crate) fn updated_at(&self) -> &str {
        &self.updated_at
    }

    pub(crate) fn completed_at(&self) -> Option<&str> {
        self.completed_at.as_deref()
    }

    pub(crate) fn has_active_tasks(&self) -> bool {
        self.tasks
            .values()
            .any(|task| matches!(task.status(), TaskStatus::Running | TaskStatus::Verifying))
    }

    pub(crate) fn has_unknown_side_effects(&self) -> bool {
        self.tasks.values().any(Task::has_unknown_side_effect)
    }

    pub(crate) fn has_blocking_state(&self) -> bool {
        !self.blockers.is_empty()
            || self
                .tasks
                .values()
                .any(|task| task.status() == TaskStatus::Blocked || !task.blockers().is_empty())
            || self.has_unknown_side_effects()
    }

    pub(crate) fn has_task_status(&self, status: TaskStatus) -> bool {
        self.tasks.values().any(|task| task.status() == status)
    }

    pub(crate) fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    pub(crate) fn set_committed_revision(&mut self, revision: u64, now: &str) {
        self.revision = revision;
        self.updated_at = now.to_owned();
    }

    pub(crate) fn materialize_initial_plan_with_contract(
        &mut self,
        tasks: Vec<Task>,
        contract: GoalFinalVerificationSpec,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        if self.status != GoalStatus::Planning || self.plan_revision != 0 || !self.tasks.is_empty()
        {
            return Err(OrchestratorError::InvalidDag(
                "initial plan materialization requires a PLANNING Goal with plan_revision 0 and no Tasks"
                    .to_owned(),
            ));
        }
        if tasks.is_empty() {
            return Err(OrchestratorError::InvalidDag(
                "initial plan must contain at least one Task".to_owned(),
            ));
        }
        if contract.plan_revision != 1 {
            return Err(OrchestratorError::InvalidDag(
                "initial Goal final-verification contract must bind plan_revision 1".to_owned(),
            ));
        }

        let mut candidate = self.clone();
        for task in tasks {
            if task.created_plan_revision() != 1 {
                return Err(OrchestratorError::InvalidDag(
                    "initial Task created_plan_revision must be 1".to_owned(),
                ));
            }
            let id = task.id().clone();
            if candidate.tasks.insert(id, task).is_some() {
                return Err(OrchestratorError::InvalidDag(
                    "duplicate Task ID in initial plan".to_owned(),
                ));
            }
        }
        candidate.plan_revision = 1;
        candidate.final_verification_spec = Some(contract);
        candidate.validate_dag()?;
        if !candidate.tasks.values().any(Task::mandatory) {
            return Err(OrchestratorError::InvalidDag(
                "initial plan requires at least one mandatory Task".to_owned(),
            ));
        }
        candidate.validate_final_verification_contract()?;

        let roots = candidate
            .tasks
            .iter()
            .filter_map(|(id, task)| task.dependencies().is_empty().then_some(id.clone()))
            .collect::<Vec<_>>();
        for task_id in roots {
            candidate.transition_task(
                &task_id,
                TaskStatus::Ready,
                TaskTransitionContext::default(),
                now,
            )?;
        }
        candidate.transition_to(GoalStatus::Running, now)?;
        candidate.add_checkpoint(CheckpointReason::PlanCommitted, now)?;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn materialize_initial_plan(
        &mut self,
        tasks: Vec<Task>,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        let mandatory_ids = tasks
            .iter()
            .filter(|task| task.mandatory())
            .map(|task| task.id().clone())
            .collect::<Vec<_>>();
        let bindings = self
            .completion_criteria
            .iter()
            .map(|criterion| {
                GoalCriterionBinding::new(
                    criterion.id.clone(),
                    mandatory_ids
                        .iter()
                        .cloned()
                        .map(|task_id| GoalVerificationRequirement::TaskVerified { task_id })
                        .collect(),
                )
            })
            .collect();
        self.materialize_initial_plan_with_contract(
            tasks,
            GoalFinalVerificationSpec::new(1, bindings),
            now,
        )
    }

    pub(crate) fn apply_replan_mutation(
        &mut self,
        mutation: ReplanMutation,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        if self.status.is_terminal() {
            return Err(OrchestratorError::InvalidDag(
                "terminal Goal plan is immutable".to_owned(),
            ));
        }
        if !matches!(self.status, GoalStatus::Running | GoalStatus::Replanning) {
            return Err(OrchestratorError::InvalidDag(
                "replanning requires a RUNNING or REPLANNING Goal".to_owned(),
            ));
        }

        let next_plan_revision = self
            .plan_revision
            .checked_add(1)
            .ok_or_else(|| OrchestratorError::InvalidDag("plan revision overflow".to_owned()))?;
        let criterion_additions = mutation.add_criterion_requirements;
        let before = self.clone();
        let dependency_targets = mutation
            .add_dependencies
            .iter()
            .map(|(task_id, _)| task_id.clone())
            .collect::<BTreeSet<_>>();
        let mut candidate = self.clone();
        candidate.plan_revision = next_plan_revision;

        let mut new_task_ids = BTreeSet::new();
        for task in mutation.new_tasks {
            if task.created_plan_revision() != next_plan_revision {
                return Err(OrchestratorError::InvalidDag(
                    "replanned Task created_plan_revision must equal the committed plan revision"
                        .to_owned(),
                ));
            }
            if task.status() != TaskStatus::Pending
                || !task.attempts().is_empty()
                || task.evidence_count() != 0
                || !task.verification_results().is_empty()
            {
                return Err(OrchestratorError::InvalidDag(
                    "new replanned Tasks must begin as pristine PENDING authority".to_owned(),
                ));
            }
            let id = task.id().clone();
            if candidate.tasks.insert(id.clone(), task).is_some() {
                return Err(OrchestratorError::InvalidDag(
                    "replan attempted to overwrite an existing Task ID".to_owned(),
                ));
            }
            if !new_task_ids.insert(id) {
                return Err(OrchestratorError::InvalidDag(
                    "replan materialized a duplicate new Task ID".to_owned(),
                ));
            }
        }

        let completed_dependencies = candidate
            .tasks
            .iter()
            .filter_map(|(id, task)| (task.status() == TaskStatus::Completed).then_some(id.clone()))
            .collect::<BTreeSet<_>>();

        let mut seen_dependency_targets = BTreeSet::new();
        for (task_id, additions) in mutation.add_dependencies {
            if additions.is_empty() || !seen_dependency_targets.insert(task_id.clone()) {
                return Err(OrchestratorError::InvalidDag(
                    "replan dependency additions must be non-empty and grouped once per Task"
                        .to_owned(),
                ));
            }
            let task = candidate.tasks.get(&task_id).ok_or_else(|| {
                OrchestratorError::InvalidDag("target task is missing".to_owned())
            })?;
            let mut dependencies = task.dependencies().to_vec();
            let mut seen = dependencies
                .iter()
                .map(|dependency| dependency.task_id().clone())
                .collect::<BTreeSet<_>>();
            for dependency_id in additions {
                if !seen.insert(dependency_id.clone()) {
                    return Err(OrchestratorError::InvalidDag(
                        "replan cannot duplicate an existing hard dependency".to_owned(),
                    ));
                }
                dependencies.push(TaskDependency::completed(dependency_id));
            }
            candidate
                .tasks
                .get_mut(&task_id)
                .expect("target checked above")
                .strengthen_dependencies(dependencies, &completed_dependencies)?;
        }

        let mut seen_verification_targets = BTreeSet::new();
        for (task_id, additions) in mutation.add_verification {
            if additions.is_empty() || !seen_verification_targets.insert(task_id.clone()) {
                return Err(OrchestratorError::InvalidDag(
                    "replan verification additions must be non-empty and grouped once per Task"
                        .to_owned(),
                ));
            }
            let task = candidate.tasks.get(&task_id).ok_or_else(|| {
                OrchestratorError::InvalidDag("target task is missing".to_owned())
            })?;
            let mut verification = task.verification_specs().to_vec();
            for spec in additions {
                if verification.contains(&spec) {
                    return Err(OrchestratorError::InvalidDag(
                        "replan cannot duplicate an existing verification requirement".to_owned(),
                    ));
                }
                verification.push(spec);
            }
            candidate
                .tasks
                .get_mut(&task_id)
                .expect("target checked above")
                .strengthen_verification(verification)?;
        }

        let mut seen_mandatory = BTreeSet::new();
        for task_id in mutation.strengthen_mandatory {
            if !seen_mandatory.insert(task_id.clone()) {
                return Err(OrchestratorError::InvalidDag(
                    "duplicate mandatory strengthening".to_owned(),
                ));
            }
            let task = candidate.tasks.get_mut(&task_id).ok_or_else(|| {
                OrchestratorError::InvalidDag("target task is missing".to_owned())
            })?;
            if task.mandatory() {
                return Err(OrchestratorError::InvalidDag(
                    "mandatory strengthening must change an optional Task".to_owned(),
                ));
            }
            task.strengthen_mandatory()?;
        }

        let mut seen_resolution = BTreeSet::new();
        for task_id in mutation.resolve_needs_replan {
            if !seen_resolution.insert(task_id.clone()) {
                return Err(OrchestratorError::InvalidDag(
                    "duplicate NEEDS_REPLAN resolution".to_owned(),
                ));
            }
            if !dependency_targets.contains(&task_id) {
                return Err(OrchestratorError::InvalidDag(
                    "NEEDS_REPLAN may resolve only after a new prerequisite is committed"
                        .to_owned(),
                ));
            }
            let task = candidate.tasks.get(&task_id).ok_or_else(|| {
                OrchestratorError::InvalidDag("target task is missing".to_owned())
            })?;
            if task.status() != TaskStatus::NeedsReplan {
                return Err(OrchestratorError::InvalidDag(
                    "only NEEDS_REPLAN Tasks may be resolved by the Replanner".to_owned(),
                ));
            }
            if task.has_unknown_side_effect() {
                return Err(OrchestratorError::InvalidDag(
                    "UNKNOWN side effects require reconciliation and cannot be cleared by replanning"
                        .to_owned(),
                ));
            }
            let dependencies_satisfied = candidate.dependencies_satisfied(&task_id)?;
            candidate
                .tasks
                .get_mut(&task_id)
                .expect("target checked above")
                .transition_to(
                    TaskStatus::Pending,
                    dependencies_satisfied,
                    TaskTransitionContext::default(),
                    now,
                )?;
        }

        for task_id in new_task_ids.iter().cloned().collect::<Vec<_>>() {
            let ready = candidate.dependencies_satisfied(&task_id)?;
            if ready {
                candidate
                    .tasks
                    .get_mut(&task_id)
                    .expect("new Task exists")
                    .transition_to(
                        TaskStatus::Ready,
                        true,
                        TaskTransitionContext::default(),
                        now,
                    )?;
            }
        }

        {
            let spec = candidate.final_verification_spec.as_mut().ok_or_else(|| {
                OrchestratorError::InvalidDag(
                    "replan requires an existing structured Goal final-verification contract"
                        .to_owned(),
                )
            })?;
            spec.plan_revision = next_plan_revision;
            let criterion_ids = candidate
                .completion_criteria
                .iter()
                .map(|criterion| criterion.id.clone())
                .collect::<BTreeSet<_>>();
            let mut seen_criteria = BTreeSet::new();
            for (criterion_id, additions) in criterion_additions {
                if !criterion_ids.contains(&criterion_id) {
                    return Err(OrchestratorError::InvalidDag(
                        "replan criterion binding references an unknown completion criterion"
                            .to_owned(),
                    ));
                }
                if additions.is_empty() || !seen_criteria.insert(criterion_id.clone()) {
                    return Err(OrchestratorError::InvalidDag("criterion binding strengthening must be non-empty and grouped once per criterion".to_owned()));
                }
                let binding = spec
                    .criterion_bindings
                    .iter_mut()
                    .find(|binding| binding.criterion_id == criterion_id)
                    .ok_or_else(|| {
                        OrchestratorError::InvalidDag(
                            "replan cannot create missing initial criterion coverage".to_owned(),
                        )
                    })?;
                for requirement in additions {
                    if binding.requirements.contains(&requirement) {
                        return Err(OrchestratorError::InvalidDag(
                            "replan cannot duplicate an existing Goal verification requirement"
                                .to_owned(),
                        ));
                    }
                    let task = candidate.tasks.get(requirement.task_id()).ok_or_else(|| {
                        OrchestratorError::InvalidDag(
                            "criterion strengthening references a missing Task".to_owned(),
                        )
                    })?;
                    if !task.is_active_plan_authority() || !task.mandatory() {
                        return Err(OrchestratorError::InvalidDag(
                            "TaskVerified may bind only a mandatory Task".to_owned(),
                        ));
                    }
                    binding.requirements.push(requirement);
                }
            }
        }

        validate_replan_history_immutability(&before, &candidate)?;
        candidate.validate_dag()?;
        candidate.validate_final_verification_contract()?;
        if candidate.status == GoalStatus::Replanning
            && !candidate.has_task_status(TaskStatus::NeedsReplan)
        {
            candidate.transition_to(GoalStatus::Running, now)?;
        }
        candidate.add_checkpoint(CheckpointReason::ReplanCommitted, now)?;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub(crate) fn apply_pristine_plan_supersession(
        &mut self,
        mutation: PristinePlanSupersessionMutation,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        if self.status.is_terminal()
            || !matches!(self.status, GoalStatus::Running | GoalStatus::Replanning)
        {
            return Err(OrchestratorError::InvalidDag(
                "pristine-plan supersession requires a non-terminal RUNNING or REPLANNING Goal"
                    .to_owned(),
            ));
        }
        if self.plan_revision != mutation.rejected_plan_revision {
            return Err(OrchestratorError::InvalidDag(
                "pristine-plan supersession must target the current rejected plan revision"
                    .to_owned(),
            ));
        }
        let rejection = self
            .find_pre_execution_plan_rejection(&mutation.rejection_request_id)
            .ok_or_else(|| {
                OrchestratorError::InvalidDag(
                    "supersession rejection authority is missing".to_owned(),
                )
            })?;
        if rejection.rejected_plan_revision() != mutation.rejected_plan_revision {
            return Err(OrchestratorError::InvalidDag(
                "supersession rejection authority targets another plan".to_owned(),
            ));
        }
        let next_plan_revision = self
            .plan_revision
            .checked_add(1)
            .ok_or_else(|| OrchestratorError::InvalidDag("plan revision overflow".to_owned()))?;
        if mutation.committed_plan_revision != next_plan_revision {
            return Err(OrchestratorError::InvalidDag(
                "supersession committed plan revision is not the next revision".to_owned(),
            ));
        }

        let derived_affected = self
            .tasks
            .iter()
            .filter_map(|(id, task)| {
                (task.created_plan_revision() == mutation.rejected_plan_revision)
                    .then_some(id.clone())
            })
            .collect::<BTreeSet<_>>();
        let supplied_affected = mutation
            .affected_task_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if derived_affected.is_empty() || supplied_affected != derived_affected {
            return Err(OrchestratorError::InvalidDag(
                "supersession affected Task set is not the host-derived rejected plan".to_owned(),
            ));
        }

        let mut candidate = self.clone();
        for task_id in &derived_affected {
            let task = candidate.tasks.get(task_id).expect("derived Task exists");
            let pristine = task.attempts().is_empty()
                && task.evidence_count() == 0
                && task.verification_results().is_empty()
                && task.blockers().is_empty()
                && !task.has_unknown_side_effect()
                && (!matches!(task.status(), TaskStatus::NeedsReplan)
                    || task_id == rejection.trigger_task_id());
            if !pristine
                || matches!(
                    task.status(),
                    TaskStatus::Running
                        | TaskStatus::Verifying
                        | TaskStatus::Blocked
                        | TaskStatus::Retryable
                        | TaskStatus::Completed
                        | TaskStatus::Failed
                        | TaskStatus::Cancelled
                        | TaskStatus::Superseded
                )
            {
                return Err(OrchestratorError::InvalidDag(
                    "supersession requires every affected Task to be pristine and pre-execution"
                        .to_owned(),
                ));
            }
        }

        let mut new_task_ids = BTreeSet::new();
        for task in mutation.new_tasks {
            if task.created_plan_revision() != next_plan_revision
                || task.status() != TaskStatus::Pending
                || !task.attempts().is_empty()
                || task.evidence_count() != 0
                || !task.verification_results().is_empty()
            {
                return Err(OrchestratorError::InvalidDag(
                    "supersession replacement Tasks must begin as pristine PENDING authority"
                        .to_owned(),
                ));
            }
            let id = task.id().clone();
            if candidate.tasks.insert(id.clone(), task).is_some() || !new_task_ids.insert(id) {
                return Err(OrchestratorError::InvalidDag(
                    "supersession replacement Task identity is duplicated".to_owned(),
                ));
            }
        }

        for task_id in &derived_affected {
            candidate
                .tasks
                .get_mut(task_id)
                .expect("derived Task exists")
                .supersede_for_host(now)?;
        }

        let old_spec = candidate.final_verification_spec.clone().ok_or_else(|| {
            OrchestratorError::InvalidDag(
                "supersession requires a structured Goal final-verification contract".to_owned(),
            )
        })?;
        let mut requested = mutation
            .criterion_rebindings
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let mut replacement_ids = BTreeSet::new();
        let mut new_bindings = Vec::new();
        for binding in old_spec.criterion_bindings() {
            let affected_count = binding
                .requirements()
                .iter()
                .filter(|requirement| derived_affected.contains(requirement.task_id()))
                .count();
            let replacements = if affected_count > 0 {
                requested.remove(binding.criterion_id()).ok_or_else(|| {
                    OrchestratorError::InvalidDag(
                        "every affected criterion binding requires a replacement".to_owned(),
                    )
                })?
            } else {
                if requested.contains_key(binding.criterion_id()) {
                    return Err(OrchestratorError::InvalidDag(
                        "criterion rebindings may only replace affected proof Tasks".to_owned(),
                    ));
                }
                Vec::new()
            };
            if affected_count > 0 {
                if replacements.len() < affected_count
                    || replacements
                        .iter()
                        .any(|id| !new_task_ids.contains(id) || !replacement_ids.insert(id.clone()))
                {
                    return Err(OrchestratorError::InvalidDag(
                        "criterion replacement coverage is not structurally equivalent".to_owned(),
                    ));
                }
                for id in &replacements {
                    let task = candidate.tasks.get(id).expect("replacement Task exists");
                    if !task.is_active_plan_authority()
                        || !task.mandatory()
                        || task.verification_specs().is_empty()
                    {
                        return Err(OrchestratorError::InvalidDag("criterion replacement requires a new mandatory mechanically verified Task".to_owned()));
                    }
                }
            }
            let requirements = binding
                .requirements()
                .iter()
                .filter(|requirement| !derived_affected.contains(requirement.task_id()))
                .cloned()
                .chain(
                    replacements
                        .into_iter()
                        .map(|task_id| GoalVerificationRequirement::TaskVerified { task_id }),
                )
                .collect::<Vec<_>>();
            new_bindings.push(GoalCriterionBinding::new(
                binding.criterion_id().clone(),
                requirements,
            ));
        }
        if !requested.is_empty() || replacement_ids.len() != new_task_ids.len() {
            return Err(OrchestratorError::InvalidDag(
                "supersession replacement Tasks must be structurally bound exactly once".to_owned(),
            ));
        }
        let mut spec = old_spec;
        spec.plan_revision = next_plan_revision;
        spec.criterion_bindings = new_bindings;
        candidate.final_verification_spec = Some(spec);
        candidate.plan_revision = next_plan_revision;
        let mut affected_task_ids = derived_affected.into_iter().collect::<Vec<_>>();
        affected_task_ids.sort();
        let mut replacement_task_ids = new_task_ids.into_iter().collect::<Vec<_>>();
        replacement_task_ids.sort();
        let mut rebound_criterion_ids = candidate
            .final_verification_spec
            .as_ref()
            .unwrap()
            .criterion_bindings()
            .iter()
            .map(|binding| binding.criterion_id().clone())
            .collect::<Vec<_>>();
        rebound_criterion_ids.sort();
        candidate
            .pristine_plan_supersessions
            .push(PristinePlanSupersessionRecord {
                rejection_request_id: mutation.rejection_request_id,
                rejected_plan_revision: mutation.rejected_plan_revision,
                committed_plan_revision: mutation.committed_plan_revision,
                canonical_proposal_digest: mutation.canonical_proposal_digest,
                affected_task_ids,
                replacement_task_ids,
                rebound_criterion_ids,
            });
        for task_id in candidate.tasks.keys().cloned().collect::<Vec<_>>() {
            if candidate.tasks[&task_id].status() != TaskStatus::Pending
                || !candidate.dependencies_satisfied(&task_id)?
            {
                continue;
            }
            // A Task whose structured failure forbids replaying the identical
            // shape must be replaced, never re-armed.
            if !candidate.tasks[&task_id].unchanged_retry_permitted() {
                continue;
            }
            candidate.tasks.get_mut(&task_id).unwrap().transition_to(
                TaskStatus::Ready,
                true,
                TaskTransitionContext::default(),
                now,
            )?;
        }
        if candidate.status == GoalStatus::Replanning {
            candidate.transition_to(GoalStatus::Running, now)?;
        }
        candidate.add_checkpoint(CheckpointReason::ReplanCommitted, now)?;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    /// Commit one generic failed-task replacement as a single durable mutation.
    ///
    /// Everything below happens on a private candidate and is published only
    /// after `validate()` passes, so no committed snapshot can ever observe a
    /// superseded Task whose downstream still depends on it, a replacement
    /// closure whose predecessor is still runnable, or a completion criterion
    /// bound to a permanently unsatisfiable Task. Any validation failure
    /// commits nothing.
    pub(crate) fn apply_task_replacements(
        &mut self,
        mutations: Vec<FailedTaskReplacementMutation>,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        if self.status.is_terminal()
            || !matches!(self.status, GoalStatus::Running | GoalStatus::Replanning)
        {
            return Err(OrchestratorError::InvalidDag(
                "failed-task replacement requires a non-terminal RUNNING or REPLANNING Goal"
                    .to_owned(),
            ));
        }
        let next_plan_revision = self
            .plan_revision
            .checked_add(1)
            .ok_or_else(|| OrchestratorError::InvalidDag("plan revision overflow".to_owned()))?;

        let mut request_ids = BTreeSet::new();
        let mut replaced_ids = BTreeSet::new();
        for mutation in &mutations {
            if mutation.replaced_plan_revision != self.plan_revision {
                return Err(OrchestratorError::InvalidDag(
                    "failed-task replacement must target the current plan revision".to_owned(),
                ));
            }
            if mutation.committed_plan_revision != next_plan_revision {
                return Err(OrchestratorError::InvalidDag(
                    "failed-task replacement committed plan revision is not the next revision"
                        .to_owned(),
                ));
            }
            if !request_ids.insert(mutation.replan_request_id.clone()) {
                return Err(OrchestratorError::InvalidDag(
                    "failed-task replacement replan request IDs must be unique within a transaction"
                        .to_owned(),
                ));
            }
            if self
                .find_failed_task_replacement(&mutation.replan_request_id)
                .is_some()
            {
                return Err(OrchestratorError::InvalidDag(
                    "failed-task replacement replan request was already consumed".to_owned(),
                ));
            }
            if !replaced_ids.insert(mutation.replaced_task_id.clone()) {
                return Err(OrchestratorError::InvalidDag(
                    "a Task may only be superseded once per replacement transaction".to_owned(),
                ));
            }
        }

        let mut candidate = self.clone();
        candidate.plan_revision = next_plan_revision;

        // 1. Materialize every replacement Task as pristine PENDING authority.
        let mut new_task_ids = BTreeSet::new();
        for mutation in &mutations {
            for task in mutation.new_tasks.clone() {
                if task.created_plan_revision() != next_plan_revision
                    || task.status() != TaskStatus::Pending
                    || !task.attempts().is_empty()
                    || task.evidence_count() != 0
                    || !task.verification_results().is_empty()
                    || !task.blockers().is_empty()
                    || task.has_unknown_side_effect()
                    || !task.mandatory()
                    || task.verification_specs().is_empty()
                {
                    return Err(OrchestratorError::InvalidDag(
                        "replacement Tasks must begin as pristine mandatory mechanically verified PENDING authority"
                            .to_owned(),
                    ));
                }
                let id = task.id().clone();
                if candidate.tasks.insert(id.clone(), task).is_some() || !new_task_ids.insert(id) {
                    return Err(OrchestratorError::InvalidDag(
                        "replacement Task identity is duplicated".to_owned(),
                    ));
                }
            }
        }

        // 2. Permanently supersede the replaced Tasks, preserving their full
        //    attempt, evidence, budget, and verification history untouched.
        for mutation in &mutations {
            let task = candidate
                .tasks
                .get_mut(&mutation.replaced_task_id)
                .ok_or_else(|| {
                    OrchestratorError::InvalidDag("replaced Task is missing".to_owned())
                })?;
            if !matches!(
                task.status(),
                TaskStatus::Pending
                    | TaskStatus::Ready
                    | TaskStatus::Retryable
                    | TaskStatus::Blocked
                    | TaskStatus::NeedsReplan
            ) {
                return Err(OrchestratorError::InvalidDag(
                    "only a non-terminal, non-active Task may be superseded by replacement"
                        .to_owned(),
                ));
            }
            if task.has_unknown_side_effect() {
                return Err(OrchestratorError::InvalidDag(
                    "UNKNOWN side-effect state must be reconciled before replacement".to_owned(),
                ));
            }
            task.supersede_for_host(now)?;
        }

        // 3. Rewire every surviving dependent off the superseded Tasks and onto
        //    the union of their replacement completion closures.
        //
        //    The plan is derived from the ENTIRE transaction before any
        //    dependency list is touched. Rewiring one superseded Task at a time
        //    makes the result order-dependent: when two superseded Tasks share
        //    one replacement completion Task, the first rewrite inserts that
        //    edge and the second then sees a duplicate. Deriving the removed and
        //    added sets per dependent from the original transaction first turns
        //    the rewrite into a single set union per dependent, so the final DAG
        //    is identical for any ordering of the same transaction.
        let mut closure_by_replaced = BTreeMap::<TaskId, BTreeSet<TaskId>>::new();
        for mutation in &mutations {
            let closure = mutation.completion_closure_task_ids.clone();
            if closure.is_empty()
                || closure
                    .iter()
                    .any(|id| !new_task_ids.contains(id) || replaced_ids.contains(id))
            {
                return Err(OrchestratorError::InvalidDag(
                    "replacement completion closure must name new, non-superseded Tasks".to_owned(),
                ));
            }
            closure_by_replaced.insert(
                mutation.replaced_task_id.clone(),
                closure.into_iter().collect(),
            );
        }
        // dependent -> (superseded Task IDs removed, union of replacement closures)
        let mut rewiring = BTreeMap::<TaskId, (BTreeSet<TaskId>, BTreeSet<TaskId>)>::new();
        for (replaced_id, closure) in &closure_by_replaced {
            for (id, task) in &candidate.tasks {
                if task.is_active_plan_authority()
                    && task
                        .dependencies()
                        .iter()
                        .any(|edge| edge.task_id() == replaced_id)
                {
                    let entry = rewiring
                        .entry(id.clone())
                        .or_insert_with(|| (BTreeSet::new(), BTreeSet::new()));
                    entry.0.insert(replaced_id.clone());
                    entry.1.extend(closure.iter().cloned());
                }
            }
        }
        // BTreeMap keys are already unique and ordered, so the recorded
        // rewiring list is independent of the replacement ordering.
        for (dependent_id, (removed, added)) in &rewiring {
            candidate
                .tasks
                .get_mut(dependent_id)
                .expect("dependent exists")
                .rewire_dependencies_for_supersessions(removed, added)?;
        }
        let rewired_dependent_ids = rewiring.keys().cloned().collect::<Vec<_>>();

        // These edges are host-derived, so they are budgeted against the same
        // global edge limit the proposal validators enforce. The host-derived
        // union can remove more edges than it adds, so the graph that actually
        // results is counted exactly rather than a running total that would
        // double-count an overlapping closure edge.
        //
        // Counted over the active execution graph only, on the same
        // `is_active_plan_authority()` predicate the validator uses. Counting
        // superseded Tasks here as well would make this backstop disagree with
        // `replanner::enforce_plan_budgets`, and a Goal with a long replacement
        // history would have every legal replacement refused for edges that no
        // longer belong to the plan.
        let final_edge_count = candidate
            .tasks
            .values()
            .filter(|task| task.is_active_plan_authority())
            .map(|task| task.dependencies().len())
            .sum::<usize>();
        if final_edge_count > crate::planner::MAX_PLAN_DEPENDENCY_EDGES {
            return Err(OrchestratorError::InvalidDag(
                "replacement rewiring would exceed the host dependency-edge limit".to_owned(),
            ));
        }

        // 4. Rebind every completion criterion that required a replaced Task.
        //    A superseded Task can never satisfy TASK_VERIFIED, so leaving the
        //    requirement in place would orphan the criterion.
        let old_spec = candidate.final_verification_spec.clone().ok_or_else(|| {
            OrchestratorError::InvalidDag(
                "failed-task replacement requires a structured Goal final-verification contract"
                    .to_owned(),
            )
        })?;
        // A criterion may require several Tasks superseded in the same
        // transaction, so their rebindings must UNION here. Overwriting would
        // silently drop one replaced proof from the contract.
        let mut requested = BTreeMap::<CompletionCriterionId, Vec<TaskId>>::new();
        for mutation in &mutations {
            for (criterion_id, ids) in &mutation.criterion_rebindings {
                requested
                    .entry(criterion_id.clone())
                    .or_default()
                    .extend(ids.iter().cloned());
            }
        }
        let mut rebound_criterion_ids = BTreeSet::new();
        let mut new_bindings = Vec::new();
        for binding in old_spec.criterion_bindings() {
            let replaced_requirements = binding
                .requirements()
                .iter()
                .filter(|requirement| replaced_ids.contains(requirement.task_id()))
                .count();
            let replacements = if replaced_requirements > 0 {
                let mut entries = requested.remove(binding.criterion_id()).ok_or_else(|| {
                    OrchestratorError::InvalidDag(
                        "every criterion bound to a replaced Task requires a replacement"
                            .to_owned(),
                    )
                })?;
                // A criterion may require several Tasks superseded in the same
                // transaction. Their rebindings union here so one transaction
                // cannot silently drop a replaced proof.
                entries.sort();
                entries.dedup();
                if entries.is_empty() {
                    return Err(OrchestratorError::InvalidDag(
                        "criterion replacement coverage is lower than the replaced proof coverage"
                            .to_owned(),
                    ));
                }
                let mut seen = BTreeSet::new();
                for id in &entries {
                    if !new_task_ids.contains(id) || !seen.insert(id.clone()) {
                        return Err(OrchestratorError::InvalidDag(
                            "criterion replacement must name distinct new replacement Tasks"
                                .to_owned(),
                        ));
                    }
                    let task = candidate.tasks.get(id).expect("replacement Task exists");
                    if !task.is_active_plan_authority()
                        || !task.mandatory()
                        || task.verification_specs().is_empty()
                    {
                        return Err(OrchestratorError::InvalidDag(
                            "criterion replacement requires a new mandatory mechanically verified Task"
                                .to_owned(),
                        ));
                    }
                }
                rebound_criterion_ids.insert(binding.criterion_id().clone());
                entries
            } else {
                if requested.contains_key(binding.criterion_id()) {
                    return Err(OrchestratorError::InvalidDag(
                        "criterion rebindings may only replace requirements bound to a replaced Task"
                            .to_owned(),
                    ));
                }
                Vec::new()
            };
            let requirements = binding
                .requirements()
                .iter()
                .filter(|requirement| !replaced_ids.contains(requirement.task_id()))
                .cloned()
                .chain(
                    replacements
                        .into_iter()
                        .map(|task_id| GoalVerificationRequirement::TaskVerified { task_id }),
                )
                .collect::<Vec<_>>();
            if requirements.is_empty() {
                return Err(OrchestratorError::InvalidDag(
                    "a required completion criterion may not be left without proof authority"
                        .to_owned(),
                ));
            }
            new_bindings.push(GoalCriterionBinding::new(
                binding.criterion_id().clone(),
                requirements,
            ));
        }
        if !requested.is_empty() {
            return Err(OrchestratorError::InvalidDag(
                "criterion rebindings must cover exactly the criteria bound to replaced Tasks"
                    .to_owned(),
            ));
        }
        let mut spec = old_spec;
        spec.plan_revision = next_plan_revision;
        spec.criterion_bindings = new_bindings;
        candidate.final_verification_spec = Some(spec);

        // 5. Record durable replacement history: preserved budgets, attempts,
        //    closure, rewiring, and rebound criteria.
        for mutation in mutations {
            let replaced = &candidate.tasks[&mutation.replaced_task_id];
            let mut replaced_attempt_ids = replaced
                .attempts()
                .iter()
                .map(|attempt| attempt.id().clone())
                .collect::<Vec<_>>();
            replaced_attempt_ids.sort();
            let mut replacement_task_ids = mutation
                .new_tasks
                .iter()
                .map(|task| task.id().clone())
                .collect::<Vec<_>>();
            replacement_task_ids.sort();
            let mut completion_closure_task_ids = mutation.completion_closure_task_ids.clone();
            completion_closure_task_ids.sort();
            let mut rebound = rebound_criterion_ids
                .iter()
                .filter(|criterion_id| {
                    mutation
                        .criterion_rebindings
                        .iter()
                        .any(|(id, _)| id == *criterion_id)
                })
                .cloned()
                .collect::<Vec<_>>();
            rebound.sort();
            candidate
                .failed_task_replacements
                .push(FailedTaskReplacementRecord {
                    replan_request_id: mutation.replan_request_id,
                    replaced_task_id: mutation.replaced_task_id,
                    replaced_plan_revision: mutation.replaced_plan_revision,
                    committed_plan_revision: mutation.committed_plan_revision,
                    canonical_proposal_digest: mutation.canonical_proposal_digest,
                    replaced_attempt_ids,
                    preserved_max_attempts: replaced.max_attempts(),
                    preserved_consumed_attempts: replaced.semantic_attempts_consumed(),
                    replacement_task_ids,
                    completion_closure_task_ids,
                    rewired_dependent_task_ids: rewired_dependent_ids.clone(),
                    rebound_criterion_ids: rebound,
                });
        }

        // 6. Propagate readiness and close the replanning window. A Task whose
        //    structured failure forbids replaying the identical shape stays
        //    PENDING: it must be replaced, not re-armed, and its presence must
        //    not fail an unrelated replacement transaction.
        for task_id in candidate.tasks.keys().cloned().collect::<Vec<_>>() {
            if candidate.tasks[&task_id].status() != TaskStatus::Pending
                || !candidate.dependencies_satisfied(&task_id)?
            {
                continue;
            }
            let replay_forbidden = !candidate.tasks[&task_id].unchanged_retry_permitted();
            if replay_forbidden {
                continue;
            }
            candidate.tasks.get_mut(&task_id).unwrap().transition_to(
                TaskStatus::Ready,
                true,
                TaskTransitionContext::default(),
                now,
            )?;
        }
        if candidate.status == GoalStatus::Replanning {
            candidate.transition_to(GoalStatus::Running, now)?;
        }
        candidate.add_checkpoint(CheckpointReason::ReplanCommitted, now)?;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    /// Record a durable request to replace a structurally invalid Task.
    ///
    /// This is a distinct mechanism from `reject_pre_execution_plan`: it is
    /// driven by structured failure evidence on a consumed attempt (or by an
    /// existing pre-execution rejection used purely as its authority), and it
    /// never consumes the trigger's remaining retry budget.
    pub(crate) fn request_failed_task_replan(
        &mut self,
        request_id: String,
        expected_goal_revision: u64,
        expected_plan_revision: u32,
        trigger_task_id: TaskId,
        reason: String,
        policy: FailedTaskReplanPolicy,
        trigger_kind: FailedTaskReplanTriggerKind,
        authority_request_id: Option<String>,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        if self.revision != expected_goal_revision {
            return Err(OrchestratorError::RevisionConflict {
                expected: expected_goal_revision,
                actual: self.revision,
            });
        }
        if self.plan_revision != expected_plan_revision {
            return Err(OrchestratorError::InvalidDag(
                "failed-task replan request plan revision is stale".to_owned(),
            ));
        }
        if self.status.is_terminal()
            || !matches!(self.status, GoalStatus::Running | GoalStatus::Replanning)
        {
            return Err(OrchestratorError::InvalidDag(
                "failed-task replan request requires a non-terminal RUNNING or REPLANNING Goal"
                    .to_owned(),
            ));
        }
        if self.has_active_tasks() || self.has_unknown_side_effects() {
            return Err(OrchestratorError::InvalidDag(
                "failed-task replan request requires no active attempt or UNKNOWN side effect"
                    .to_owned(),
            ));
        }
        if let Some(rejection) = &authority_request_id
            && self.find_pre_execution_plan_rejection(rejection).is_none()
        {
            return Err(OrchestratorError::InvalidDag(
                "failed-task replan request references a missing pre-execution rejection"
                    .to_owned(),
            ));
        }
        if authority_request_id.is_some()
            && trigger_kind != FailedTaskReplanTriggerKind::PreExecutionRejection
        {
            return Err(OrchestratorError::InvalidDag(
                "a pre-execution rejection may only authorize a pre-execution trigger kind"
                    .to_owned(),
            ));
        }
        if let (FailedTaskReplanTriggerKind::PreExecutionRejection, Some(rejection)) =
            (trigger_kind, &authority_request_id)
        {
            let rejection = self
                .find_pre_execution_plan_rejection(rejection)
                .expect("rejection existence was checked above");
            if rejection.trigger_task_id() != &trigger_task_id
                || rejection.rejected_plan_revision() != expected_plan_revision
            {
                return Err(OrchestratorError::InvalidDag(
                    "a pre-execution rejection may only authorize its own trigger Task at its own rejected plan revision"
                        .to_owned(),
                ));
            }
        }

        let task = self.tasks.get(&trigger_task_id).ok_or_else(|| {
            OrchestratorError::InvalidDag(
                "failed-task replan request trigger Task is missing".to_owned(),
            )
        })?;
        let attempt = task.latest_attempt();
        let failure_class = match trigger_kind {
            FailedTaskReplanTriggerKind::PostAttemptFailure => {
                let attempt = attempt.ok_or_else(|| {
                    OrchestratorError::InvalidDag(
                        "post-attempt replan request requires a consumed durable attempt"
                            .to_owned(),
                    )
                })?;
                let class = attempt.effective_failure_class();
                if !class.allows_replan_request() {
                    return Err(OrchestratorError::InvalidDag(format!(
                        "structured failure class {class:?} does not admit a failed-task replan request"
                    )));
                }
                class
            }
            FailedTaskReplanTriggerKind::PreExecutionRejection => {
                if !task.attempts().is_empty()
                    || task.evidence_count() != 0
                    || !task.verification_results().is_empty()
                    || !task.blockers().is_empty()
                {
                    return Err(OrchestratorError::InvalidDag(
                        "pre-execution failed-task replan request requires a pristine trigger Task"
                            .to_owned(),
                    ));
                }
                FailureClass::TaskScopeTooBroad
            }
        };
        if task.has_unknown_side_effect() {
            return Err(OrchestratorError::InvalidDag(
                "UNKNOWN side-effect state must be reconciled before replacement".to_owned(),
            ));
        }

        let mut candidate = self.clone();
        candidate
            .tasks
            .get_mut(&trigger_task_id)
            .expect("trigger Task was checked above")
            .request_failed_task_replan(now)?;
        candidate
            .failed_task_replan_requests
            .push(FailedTaskReplanRequest {
                request_id,
                expected_goal_revision,
                observed_goal_revision: expected_goal_revision,
                trigger_plan_revision: expected_plan_revision,
                observed_plan_revision: expected_plan_revision,
                trigger_task_id,
                reason,
                policy,
                trigger_kind,
                authority_request_id,
                trigger_failure_class: failure_class,
                trigger_side_effect_state: attempt.and_then(|attempt| attempt.side_effect_state()),
                observed_attempt_id: attempt.map(|attempt| attempt.id().clone()),
                observed_consumed_attempts: task.semantic_attempts_consumed(),
                observed_max_attempts: task.max_attempts(),
                authority: FailedTaskReplanAuthorityKind::GoalResume,
                requested_at: now.to_owned(),
            });
        if candidate.status == GoalStatus::Running {
            candidate.transition_to(GoalStatus::Replanning, now)?;
        }
        candidate.add_checkpoint(CheckpointReason::FailedTaskReplanRequested, now)?;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    #[cfg(test)]
    fn rebuild_test_final_verification_contract(&mut self) {
        if self.plan_revision == 0 {
            self.final_verification_spec = None;
            return;
        }
        let mandatory_ids = self
            .tasks
            .iter()
            .filter_map(|(task_id, task)| task.mandatory().then_some(task_id.clone()))
            .collect::<Vec<_>>();
        if mandatory_ids.is_empty() {
            self.final_verification_spec = None;
            return;
        }
        let bindings = self
            .completion_criteria
            .iter()
            .map(|criterion| {
                GoalCriterionBinding::new(
                    criterion.id.clone(),
                    mandatory_ids
                        .iter()
                        .cloned()
                        .map(|task_id| GoalVerificationRequirement::TaskVerified { task_id })
                        .collect(),
                )
            })
            .collect();
        self.final_verification_spec =
            Some(GoalFinalVerificationSpec::new(self.plan_revision, bindings));
    }

    #[cfg(test)]
    pub(crate) fn add_task(
        &mut self,
        title: impl Into<String>,
        objective: impl Into<String>,
        mandatory: bool,
        worker: WorkerKind,
        scope: TaskScope,
        verification: Vec<VerificationSpec>,
        max_attempts: u32,
        now: &str,
    ) -> Result<TaskId, OrchestratorError> {
        if self.status.is_terminal() {
            return Err(OrchestratorError::InvalidDag(
                "terminal goal task graph is immutable".to_owned(),
            ));
        }
        let next_plan_revision = self
            .plan_revision
            .checked_add(1)
            .ok_or_else(|| OrchestratorError::InvalidDag("plan revision overflow".to_owned()))?;
        let task = Task::new(
            title,
            objective,
            mandatory,
            worker,
            scope,
            verification,
            max_attempts,
            next_plan_revision,
            now,
        )?;
        let id = task.id().clone();
        let mut candidate = self.clone();
        if candidate.tasks.insert(id.clone(), task).is_some() {
            return Err(OrchestratorError::InvalidDag(
                "duplicate task ID".to_owned(),
            ));
        }
        candidate.plan_revision = next_plan_revision;
        candidate.rebuild_test_final_verification_contract();
        if candidate.final_verification_spec.is_some() {
            candidate.validate()?;
        } else {
            candidate.validate_dag()?;
        }
        *self = candidate;
        Ok(id)
    }

    #[cfg(test)]
    pub(crate) fn strengthen_task_dependencies(
        &mut self,
        task_id: &TaskId,
        dependency_ids: Vec<TaskId>,
    ) -> Result<(), OrchestratorError> {
        let mut candidate = self.clone();
        let target_mandatory = candidate
            .tasks
            .get(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("target task is missing".to_owned()))?
            .mandatory();
        let completed = candidate
            .tasks
            .iter()
            .filter_map(|(id, task)| (task.status() == TaskStatus::Completed).then_some(id.clone()))
            .collect::<BTreeSet<_>>();
        for dependency_id in &dependency_ids {
            let dependency = candidate.tasks.get(dependency_id).ok_or_else(|| {
                OrchestratorError::InvalidDag("dependency target is missing".to_owned())
            })?;
            if target_mandatory && !dependency.mandatory() {
                return Err(OrchestratorError::InvalidDag(
                    "mandatory task cannot rely on an optional hard dependency".to_owned(),
                ));
            }
        }
        let dependencies = dependency_ids
            .into_iter()
            .map(TaskDependency::completed)
            .collect::<Vec<_>>();
        candidate
            .tasks
            .get_mut(task_id)
            .expect("target checked above")
            .strengthen_dependencies(dependencies, &completed)?;
        candidate.plan_revision = candidate
            .plan_revision
            .checked_add(1)
            .ok_or_else(|| OrchestratorError::InvalidDag("plan revision overflow".to_owned()))?;
        candidate.rebuild_test_final_verification_contract();
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn strengthen_task_verification(
        &mut self,
        task_id: &TaskId,
        verification: Vec<VerificationSpec>,
    ) -> Result<(), OrchestratorError> {
        let mut candidate = self.clone();
        candidate
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("target task is missing".to_owned()))?
            .strengthen_verification(verification)?;
        candidate.plan_revision = candidate
            .plan_revision
            .checked_add(1)
            .ok_or_else(|| OrchestratorError::InvalidDag("plan revision overflow".to_owned()))?;
        candidate.rebuild_test_final_verification_contract();
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn strengthen_task_mandatory(
        &mut self,
        task_id: &TaskId,
    ) -> Result<(), OrchestratorError> {
        let mut candidate = self.clone();
        let dependencies = candidate
            .tasks
            .get(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("target task is missing".to_owned()))?
            .dependencies()
            .iter()
            .map(|dependency| dependency.task_id().clone())
            .collect::<Vec<_>>();
        if dependencies.iter().any(|id| {
            candidate
                .tasks
                .get(id)
                .is_some_and(|dependency| !dependency.mandatory())
        }) {
            return Err(OrchestratorError::InvalidDag(
                "mandatory task cannot rely on an optional hard dependency".to_owned(),
            ));
        }
        if candidate
            .tasks
            .get_mut(task_id)
            .expect("target checked above")
            .strengthen_mandatory()?
        {
            candidate.plan_revision = candidate.plan_revision.checked_add(1).ok_or_else(|| {
                OrchestratorError::InvalidDag("plan revision overflow".to_owned())
            })?;
            candidate.rebuild_test_final_verification_contract();
        }
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub(crate) fn transition_task(
        &mut self,
        task_id: &TaskId,
        next: TaskStatus,
        context: TaskTransitionContext,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        let mut candidate = self.clone();
        let dependencies_satisfied = candidate.dependencies_satisfied(task_id)?;
        candidate
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .transition_to(next, dependencies_satisfied, context, now)?;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub(crate) fn reject_pre_execution_plan(
        &mut self,
        request_id: String,
        expected_goal_revision: u64,
        expected_plan_revision: u32,
        trigger_task_id: TaskId,
        reason: String,
        replan_policy: PreExecutionPlanReplanPolicy,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        if self.revision != expected_goal_revision {
            return Err(OrchestratorError::RevisionConflict {
                expected: expected_goal_revision,
                actual: self.revision,
            });
        }
        if self.plan_revision != expected_plan_revision {
            return Err(OrchestratorError::InvalidDag(
                "pre-execution plan rejection plan revision is stale".to_owned(),
            ));
        }
        if !matches!(self.status, GoalStatus::Running) || self.status.is_terminal() {
            return Err(OrchestratorError::InvalidDag(
                "pre-execution plan rejection requires a non-terminal RUNNING Goal".to_owned(),
            ));
        }
        if !self.blockers.is_empty()
            || self.has_active_tasks()
            || self.has_unknown_side_effects()
            || self.tasks.values().any(|task| {
                task.needs_reviewer_recovery()
                    || task.attempts().iter().any(|attempt| {
                        attempt.mutation_intent().is_some_and(|intent| {
                            !matches!(
                                intent.state(),
                                MutationIntentState::ReconciledNotPerformed
                                    | MutationIntentState::ReconciledPerformed
                            )
                        })
                    })
                    || (task.scope().operation_kind() != TaskOperationKind::ReadOnly
                        && task.latest_attempt().is_some_and(|attempt| {
                            attempt.side_effect_state()
                                == Some(crate::fallback::SideEffectState::ConfirmedPerformed)
                        }))
                    || crate::writer::holds_workspace_mutation_lease(task)
            })
        {
            return Err(OrchestratorError::InvalidDag(
                "pre-execution plan rejection requires no active recovery, mutation, lease, blocker, or side effect state".to_owned(),
            ));
        }

        let task = self.tasks.get(&trigger_task_id).ok_or_else(|| {
            OrchestratorError::InvalidDag(
                "pre-execution plan rejection trigger Task is missing".to_owned(),
            )
        })?;
        if task.created_plan_revision() != expected_plan_revision {
            return Err(OrchestratorError::InvalidDag(
                "pre-execution plan rejection trigger Task is not in the rejected plan".to_owned(),
            ));
        }

        let mut candidate = self.clone();
        candidate
            .tasks
            .get_mut(&trigger_task_id)
            .expect("trigger Task was checked above")
            .reject_pre_execution_plan(now)?;
        candidate
            .pre_execution_plan_rejections
            .push(PreExecutionPlanRejection {
                request_id,
                expected_goal_revision,
                observed_goal_revision: expected_goal_revision,
                rejected_plan_revision: expected_plan_revision,
                observed_plan_revision: expected_plan_revision,
                trigger_task_id,
                reason,
                replan_policy,
                authority: PreExecutionPlanRejectionAuthorityKind::GoalResume,
                rejected_at: now.to_owned(),
            });
        candidate.transition_to(GoalStatus::Replanning, now)?;
        candidate.add_checkpoint(CheckpointReason::PreExecutionPlanRejected, now)?;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub(crate) fn task_prepare_latest_mutation_intent(
        &mut self,
        task_id: &TaskId,
        intent: MutationIntent,
    ) -> Result<(), OrchestratorError> {
        let mut candidate = self.clone();
        candidate
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .prepare_latest_mutation_intent(intent)?;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub(crate) fn task_advance_latest_mutation_intent(
        &mut self,
        task_id: &TaskId,
        expected_operation_id: &str,
        update: MutationIntentUpdate,
    ) -> Result<(), OrchestratorError> {
        let mut candidate = self.clone();
        candidate
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .advance_latest_mutation_intent(expected_operation_id, update)?;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub(crate) fn task_reconcile_latest_mutation_intent(
        &mut self,
        task_id: &TaskId,
        expected_operation_id: &str,
        state: crate::mutation::MutationIntentState,
        summary: String,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        let mut candidate = self.clone();
        candidate
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .reconcile_latest_mutation_intent(expected_operation_id, state, summary, now)?;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub(crate) fn reconcile_legacy_writer_pre_mutation_task(
        &mut self,
        task_id: &TaskId,
        authority: &crate::goal_api::LegacyWriterPreMutationReconciliationAuthority,
        now: &str,
    ) -> Result<bool, OrchestratorError> {
        let mut candidate = self.clone();
        let reconciled = candidate
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .reconcile_legacy_writer_pre_mutation(authority, now)?;
        if !reconciled {
            return Ok(false);
        }
        candidate.validate()?;
        *self = candidate;
        Ok(true)
    }

    pub(crate) fn recover_legacy_readonly_timeout_task(
        &mut self,
        task_id: &TaskId,
        authority: &crate::goal_api::ReadonlyTransportRecoveryAuthority,
        now: &str,
    ) -> Result<bool, OrchestratorError> {
        let mut candidate = self.clone();
        let recovered = candidate
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .recover_legacy_readonly_timeout(authority, now)?;
        if !recovered {
            return Ok(false);
        }
        candidate.validate()?;
        *self = candidate;
        Ok(true)
    }

    pub(crate) fn reconcile_exhausted_readonly_replan_task(
        &mut self,
        task_id: &TaskId,
        authority: &crate::replanner::ReadonlyReplanRecoveryAuthority,
        next_plan_revision: u32,
        now: &str,
    ) -> Result<bool, OrchestratorError> {
        let mut candidate = self.clone();
        let plan_revision_before = candidate.plan_revision;
        let reconciled = candidate
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .reconcile_exhausted_readonly_replan(
                authority,
                plan_revision_before,
                next_plan_revision,
                now,
            )?;
        if !reconciled {
            return Ok(false);
        }
        candidate.validate()?;
        *self = candidate;
        Ok(true)
    }

    pub(crate) fn complete_task_from_verifier(
        &mut self,
        task_id: &TaskId,
        authority: &crate::verifier::VerifierCompletionAuthority,
        context: TaskTransitionContext,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        let mut candidate = self.clone();
        let dependencies_satisfied = candidate.dependencies_satisfied(task_id)?;
        candidate
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .complete_from_verifier(authority, dependencies_satisfied, context, now)?;
        candidate.validate()?;
        *self = candidate;
        Ok(())
    }

    pub(crate) fn task_add_blocker(
        &mut self,
        task_id: &TaskId,
        blocker: crate::task::TaskBlocker,
    ) -> Result<(), OrchestratorError> {
        self.tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .add_blocker(blocker)
    }

    pub(crate) fn task_clear_blockers(
        &mut self,
        task_id: &TaskId,
    ) -> Result<(), OrchestratorError> {
        self.tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .clear_blockers()
    }

    pub(crate) fn task_bind_latest_attempt_execution(
        &mut self,
        task_id: &TaskId,
        operation_id: Option<String>,
        scope_identity: Option<String>,
        request_id: Option<String>,
        side_effect_class: Option<crate::fallback::SideEffectClass>,
        side_effect_state: Option<crate::fallback::SideEffectState>,
        remaining_attempt_budget: Option<u32>,
        remaining_side_effect_budget: Option<u32>,
    ) -> Result<(), OrchestratorError> {
        self.tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .bind_latest_attempt_execution(
                operation_id,
                scope_identity,
                request_id,
                side_effect_class,
                side_effect_state,
                remaining_attempt_budget,
                remaining_side_effect_budget,
            )
    }

    pub(crate) fn task_mark_latest_attempt_interrupted(
        &mut self,
        task_id: &TaskId,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        self.tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .mark_latest_attempt_interrupted(now)
    }

    pub(crate) fn task_record_latest_worker_report(
        &mut self,
        task_id: &TaskId,
        report: WorkerReport,
    ) -> Result<(), OrchestratorError> {
        self.tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .record_latest_worker_report(report)
    }

    pub(crate) fn task_record_latest_attempt_failure_class(
        &mut self,
        task_id: &TaskId,
        class: FailureClass,
    ) -> Result<(), OrchestratorError> {
        self.tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .record_latest_attempt_failure_class(class)
    }

    pub(crate) fn task_add_evidence(
        &mut self,
        task_id: &TaskId,
        evidence: crate::task::TaskEvidence,
    ) -> Result<(), OrchestratorError> {
        self.tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .add_evidence(evidence)
    }

    pub(crate) fn task_record_verification_result(
        &mut self,
        task_id: &TaskId,
        result: VerificationResult,
    ) -> Result<(), OrchestratorError> {
        self.tasks
            .get_mut(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?
            .record_verification_result(result)
    }

    pub(crate) fn add_blocker(&mut self, blocker: GoalBlocker) -> Result<(), OrchestratorError> {
        if self.status.is_terminal() {
            return Err(OrchestratorError::invalid_transition(
                "goal",
                format!("{:?}", self.status).to_ascii_uppercase(),
                format!("{:?}", self.status).to_ascii_uppercase(),
                "terminal goal blocker history is immutable",
            ));
        }
        self.blockers.push(blocker);
        Ok(())
    }

    pub(crate) fn clear_blockers(&mut self) -> Result<(), OrchestratorError> {
        if self.status.is_terminal() {
            return Err(OrchestratorError::invalid_transition(
                "goal",
                format!("{:?}", self.status).to_ascii_uppercase(),
                format!("{:?}", self.status).to_ascii_uppercase(),
                "terminal goal blocker history is immutable",
            ));
        }
        self.blockers.clear();
        Ok(())
    }

    pub(crate) fn append_final_verification_from_goal_verifier(
        &mut self,
        _authority: &crate::goal_verifier::GoalFinalVerificationWriteAuthority,
        record: GoalFinalVerificationRecord,
    ) -> Result<(), OrchestratorError> {
        if self.status != GoalStatus::Verifying {
            return Err(invalid_goal_transition(
                self.status,
                self.status,
                "authoritative Goal final verification may only be appended in VERIFYING",
            ));
        }
        if record.source != VerificationOrigin::HostDeterministicGoalVerifier
            || record.goal_id != self.id
            || record.plan_revision != self.plan_revision
        {
            return Err(OrchestratorError::CorruptGoal(
                "Goal final-verification record authority binding is invalid".to_owned(),
            ));
        }
        let expected_digest = self
            .final_verification_spec
            .as_ref()
            .ok_or_else(|| {
                OrchestratorError::CorruptGoal(
                    "Goal final-verification contract is missing".to_owned(),
                )
            })?
            .canonical_digest();
        if record.contract_digest != expected_digest {
            return Err(OrchestratorError::CorruptGoal(
                "Goal final-verification record contract digest is stale".to_owned(),
            ));
        }
        if record.committed_goal_revision != self.revision.saturating_add(1) {
            return Err(OrchestratorError::CorruptGoal(
                "Goal final-verification committed revision binding is invalid".to_owned(),
            ));
        }
        if self
            .final_verifications
            .iter()
            .any(|existing| existing.id == record.id)
        {
            return Err(OrchestratorError::CorruptGoal(
                "Goal final-verification identity is duplicated".to_owned(),
            ));
        }
        self.final_verifications.push(record);
        Ok(())
    }

    pub(crate) fn enter_verifying_from_goal_verifier(
        &mut self,
        _authority: &crate::goal_verifier::GoalVerificationEntryAuthority,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        let from = self.status;
        if from != GoalStatus::Running {
            return Err(invalid_goal_transition(
                from,
                GoalStatus::Verifying,
                "Goal Verifier entry requires RUNNING",
            ));
        }
        // Final truth evaluation belongs to the Goal Verifier. The sealed entry
        // itself revalidates only the durable structural authority boundary so
        // FAILED/INDETERMINATE results can still be recorded authoritatively.
        self.validate_dag()?;
        self.validate_final_verification_contract()?;
        self.status = GoalStatus::Verifying;
        self.updated_at = now.to_owned();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn enter_verifying_for_test(&mut self, now: &str) -> Result<(), OrchestratorError> {
        let from = self.status;
        if from != GoalStatus::Running {
            return Err(invalid_goal_transition(
                from,
                GoalStatus::Verifying,
                "test helper requires RUNNING",
            ));
        }
        self.require_goal_verification_entry_ready(from, GoalStatus::Verifying)?;
        self.status = GoalStatus::Verifying;
        self.updated_at = now.to_owned();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn record_final_verification(
        &mut self,
        result: VerificationResult,
    ) -> Result<(), OrchestratorError> {
        if self.status != GoalStatus::Verifying {
            return Err(invalid_goal_transition(
                self.status,
                self.status,
                "test final verification helper requires VERIFYING",
            ));
        }
        let spec = self.final_verification_spec.as_ref().ok_or_else(|| {
            OrchestratorError::InvalidDag(
                "test final verification helper requires structured contract".to_owned(),
            )
        })?;
        let mut criterion_results = Vec::new();
        for binding in &spec.criterion_bindings {
            let observations = binding
                .requirements
                .iter()
                .map(|requirement| {
                    let task_id = requirement.task_id().clone();
                    let task = self.tasks.get(&task_id);
                    let latest = task.and_then(|task| task.verification_results().last());
                    GoalRequirementObservation::TaskVerified {
                        task_id,
                        task_status: task
                            .map(|task| task.status())
                            .unwrap_or(TaskStatus::Blocked),
                        verification_id: latest.map(|verification| verification.id().clone()),
                        verification_outcome: latest.map(|verification| verification.outcome()),
                        outcome: latest
                            .map(|verification| verification.outcome())
                            .unwrap_or(VerificationOutcome::Indeterminate),
                    }
                })
                .collect();
            criterion_results.push(GoalCriterionVerificationResult::new(
                binding.criterion_id.clone(),
                result.outcome(),
                observations,
            ));
        }
        self.final_verifications
            .push(GoalFinalVerificationRecord::new(
                result.outcome(),
                self.id.clone(),
                self.revision,
                self.revision,
                self.plan_revision,
                spec.canonical_digest(),
                criterion_results,
                result.started_at().to_owned(),
                result.finished_at().to_owned(),
            ));
        Ok(())
    }

    pub(crate) fn add_checkpoint(
        &mut self,
        reason: CheckpointReason,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        if self.status.is_terminal() {
            return Err(invalid_goal_transition(
                self.status,
                self.status,
                "terminal checkpoint history is immutable",
            ));
        }
        let active_task_ids = self
            .tasks
            .iter()
            .filter_map(|(id, task)| {
                matches!(task.status(), TaskStatus::Running | TaskStatus::Verifying)
                    .then_some(id.clone())
            })
            .collect();
        self.checkpoints.push(GoalCheckpoint {
            id: Uuid::new_v4().to_string(),
            goal_revision: self.revision,
            plan_revision: self.plan_revision,
            at: now.to_owned(),
            reason,
            goal_status: self.status,
            active_task_ids,
        });
        Ok(())
    }

    pub(crate) fn transition_to(
        &mut self,
        next: GoalStatus,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        let from = self.status;
        if from.is_terminal() {
            return Err(invalid_goal_transition(
                from,
                next,
                "terminal goal state is immutable",
            ));
        }
        if next == GoalStatus::Verifying {
            return Err(invalid_goal_transition(
                from,
                next,
                "RUNNING -> VERIFYING is reserved to the sealed Goal Verifier authority",
            ));
        }
        if next == GoalStatus::Completed {
            return Err(invalid_goal_transition(
                from,
                next,
                "Goal completion is reserved to the Phase 9 Finalizer authority",
            ));
        }
        let allowed = matches!(
            (from, next),
            (
                GoalStatus::Planning,
                GoalStatus::Running
                    | GoalStatus::Blocked
                    | GoalStatus::Cancelling
                    | GoalStatus::Failed
            ) | (
                GoalStatus::Running,
                GoalStatus::Replanning
                    | GoalStatus::Pausing
                    | GoalStatus::Blocked
                    | GoalStatus::Cancelling
                    | GoalStatus::Failed
            ) | (
                GoalStatus::Replanning,
                GoalStatus::Running
                    | GoalStatus::Blocked
                    | GoalStatus::Pausing
                    | GoalStatus::Cancelling
                    | GoalStatus::Failed
            ) | (
                GoalStatus::Pausing,
                GoalStatus::Paused | GoalStatus::Blocked | GoalStatus::Cancelling
            ) | (
                GoalStatus::Paused,
                GoalStatus::Running
                    | GoalStatus::Replanning
                    | GoalStatus::Blocked
                    | GoalStatus::Cancelling
            ) | (
                GoalStatus::Blocked,
                GoalStatus::Running
                    | GoalStatus::Replanning
                    | GoalStatus::Paused
                    | GoalStatus::Cancelling
                    | GoalStatus::Failed
            ) | (
                GoalStatus::Verifying,
                GoalStatus::Replanning
                    | GoalStatus::Blocked
                    | GoalStatus::Pausing
                    | GoalStatus::Cancelling
                    | GoalStatus::Failed
            ) | (
                GoalStatus::Cancelling,
                GoalStatus::Cancelled | GoalStatus::Blocked
            )
        );
        if !allowed {
            return Err(invalid_goal_transition(
                from,
                next,
                "transition is not in the V1 state table",
            ));
        }
        if from == GoalStatus::Planning && next == GoalStatus::Running {
            self.validate_dag()?;
            self.validate_final_verification_contract()?;
            if !self.tasks.values().any(Task::mandatory) {
                return Err(invalid_goal_transition(
                    from,
                    next,
                    "V1 runnable goal requires at least one mandatory task",
                ));
            }
        }
        if from == GoalStatus::Cancelling
            && next == GoalStatus::Cancelled
            && self.tasks.values().any(|task| {
                matches!(task.status(), TaskStatus::Running | TaskStatus::Verifying)
                    || task.has_unknown_side_effect()
            })
        {
            return Err(invalid_goal_transition(
                from,
                next,
                "active work or unresolved side effects remain",
            ));
        }
        self.status = next;
        self.updated_at = now.to_owned();
        if next.is_terminal() {
            self.completed_at = Some(now.to_owned());
        }
        Ok(())
    }

    pub(crate) fn complete_from_finalizer(
        &mut self,
        _authority: &crate::goal_finalizer::GoalFinalizationAuthority,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        let from = self.status;
        if from.is_terminal() {
            return Err(invalid_goal_transition(
                from,
                GoalStatus::Completed,
                "terminal goal state is immutable",
            ));
        }
        if from != GoalStatus::Verifying {
            return Err(invalid_goal_transition(
                from,
                GoalStatus::Completed,
                "Finalizer completion requires VERIFYING",
            ));
        }
        self.require_goal_completion_ready(from, GoalStatus::Completed)?;
        self.status = GoalStatus::Completed;
        self.updated_at = now.to_owned();
        self.completed_at = Some(now.to_owned());
        Ok(())
    }

    pub(crate) fn recover_stale_running(&mut self, now: &str) -> Result<bool, OrchestratorError> {
        let mut candidate = self.clone();
        let mut changed = false;
        for task in candidate.tasks.values_mut() {
            if task.status() == TaskStatus::Running {
                let before = task.clone();
                task.recover_stale_running(now)?;
                changed |= *task != before;
            }
        }
        if changed
            && candidate.tasks.values().any(|task| {
                task.is_active_plan_authority()
                    && task.mandatory()
                    && task.status() == TaskStatus::Blocked
            })
            && !candidate.status.is_terminal()
            && candidate.status != GoalStatus::Blocked
        {
            candidate.transition_to(GoalStatus::Blocked, now)?;
        }
        if changed {
            candidate.add_checkpoint(CheckpointReason::Recovery, now)?;
            candidate.validate()?;
            *self = candidate;
        }
        Ok(changed)
    }

    /// Pure validation of the Managed Worktrees V1 durable state bound to this
    /// Goal (design sections 3, 4, 5, 8, 11, 21).
    ///
    /// This validates only durable data. It performs no filesystem access, no Git
    /// access, and derives nothing from repository observation; reconciliation
    /// against real Git state is a separate, later phase.
    fn validate_managed_workspace_state(&self) -> Result<(), OrchestratorError> {
        let record = self.managed_worktree.as_ref();
        let intent = self.managed_worktree_creation_intent.as_ref();

        if self.workspace_mode.is_primary() {
            if record.is_some() || intent.is_some() {
                return Err(OrchestratorError::CorruptGoal(
                    "PRIMARY workspace must not carry managed-worktree state".to_owned(),
                ));
            }
            return Ok(());
        }

        let record = record.ok_or_else(|| {
            OrchestratorError::CorruptGoal(
                "MANAGED_WORKTREE workspace requires a durable managed-worktree record".to_owned(),
            )
        })?;
        record.validate()?;

        // One non-terminal Goal owns at most one managed linked worktree
        // (design section 4), which is structural here: the record is singular.
        if record.goal_id() != &self.id {
            return Err(OrchestratorError::CorruptGoal(
                "managed-worktree record ownership does not match the durable Goal".to_owned(),
            ));
        }
        // Design section 13 freezes `Goal.cwd` as the durable identity root, so
        // a managed record can never relocate the primary root.
        if record.primary_root() != self.cwd.as_path() {
            return Err(OrchestratorError::CorruptGoal(
                "managed-worktree primary_root must remain the durable Goal.cwd".to_owned(),
            ));
        }
        // Workspace lifecycle mutations advance Goal revision but never the plan
        // revision (design section 21), so a creation-time binding ahead of the
        // current Goal revision is impossible.
        if record.created_goal_revision() > self.revision {
            return Err(OrchestratorError::CorruptGoal(
                "managed-worktree created_goal_revision cannot exceed the current Goal revision"
                    .to_owned(),
            ));
        }

        // An outstanding intent means creation has not been reconciled yet, and a
        // reconciled worktree must not keep a stale PREPARED intent around. A
        // blocked creation may retain its intent so explicit host recovery can
        // still reconcile the exact intended target.
        match (
            intent,
            record.requires_creation_intent(),
            record.permits_creation_intent(),
        ) {
            (None, true, _) => Err(OrchestratorError::CorruptGoal(
                "managed-worktree creation is outstanding without a durable PREPARED intent"
                    .to_owned(),
            )),
            (Some(_), _, false) => Err(OrchestratorError::CorruptGoal(
                "reconciled managed worktree must not retain a PREPARED creation intent".to_owned(),
            )),
            (Some(intent), _, true) => intent.validate_against_record(record),
            (None, false, _) => Ok(()),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), OrchestratorError> {
        if self.store_format != GOAL_STORE_FORMAT {
            return Err(OrchestratorError::CorruptGoal(
                "unexpected store_format".to_owned(),
            ));
        }
        if self.schema_version == 1 {
            return self.validate_legacy_schema1_terminal();
        }
        if self.schema_version != GOAL_SCHEMA_VERSION {
            return Err(OrchestratorError::UnsupportedSchema(
                self.schema_version as u64,
            ));
        }
        if self.revision == 0 {
            return Err(OrchestratorError::CorruptGoal(
                "goal revision must start at 1".to_owned(),
            ));
        }
        self.id.validate()?;
        config::validate_session_id(&self.session_id)
            .map_err(|_| OrchestratorError::CorruptGoal("invalid session_id".to_owned()))?;
        if self.objective.trim().is_empty() {
            return Err(OrchestratorError::CorruptGoal(
                "goal objective must not be empty".to_owned(),
            ));
        }
        if self.completion_criteria.is_empty() {
            return Err(OrchestratorError::CorruptGoal(
                "current-schema Goal must have at least one host-owned completion criterion"
                    .to_owned(),
            ));
        }
        self.validate_managed_workspace_state()?;
        let mut criterion_ids = BTreeSet::new();
        for criterion in &self.completion_criteria {
            criterion.id.validate()?;
            if criterion.description.trim().is_empty() || !criterion.required {
                return Err(OrchestratorError::CorruptGoal(
                    "current-schema completion criteria must be non-empty and required in V1"
                        .to_owned(),
                ));
            }
            if !criterion_ids.insert(criterion.id.clone()) {
                return Err(OrchestratorError::CorruptGoal(
                    "completion criterion IDs must be unique within a Goal".to_owned(),
                ));
            }
        }
        if self.plan_revision == 0 {
            if self.final_verification_spec.is_some() {
                return Err(OrchestratorError::CorruptGoal(
                    "unmaterialized plan cannot carry a final-verification contract".to_owned(),
                ));
            }
        } else {
            self.validate_final_verification_contract()?;
        }
        for record in &self.final_verifications {
            VerificationId::parse(record.id.as_str())?;
            if record.source != VerificationOrigin::HostDeterministicGoalVerifier
                || record.goal_id != self.id
                || record.evaluated_goal_revision == 0
                || record.evaluated_goal_revision > record.committed_goal_revision
                || record.committed_goal_revision > self.revision
                || record.plan_revision == 0
                || record.plan_revision > self.plan_revision
                || record.contract_digest.len() != 64
            {
                return Err(OrchestratorError::CorruptGoal(
                    "Goal final-verification history contains an invalid authority binding"
                        .to_owned(),
                ));
            }
        }
        let mut rejection_request_ids = BTreeSet::new();
        for rejection in &self.pre_execution_plan_rejections {
            if rejection.request_id.trim().is_empty()
                || rejection.request_id.chars().count() > 128
                || rejection.expected_goal_revision == 0
                || rejection.expected_goal_revision > self.revision
                || rejection.observed_goal_revision != rejection.expected_goal_revision
                || rejection.rejected_plan_revision == 0
                || rejection.rejected_plan_revision > self.plan_revision
                || rejection.observed_plan_revision != rejection.rejected_plan_revision
                || rejection.reason.trim().is_empty()
                || rejection.reason.chars().count() > 8_192
                || rejection.authority != PreExecutionPlanRejectionAuthorityKind::GoalResume
                || !rejection_request_ids.insert(rejection.request_id.clone())
            {
                return Err(OrchestratorError::CorruptGoal(
                    "Goal pre-execution plan rejection history contains an invalid or duplicate record".to_owned(),
                ));
            }
            let task = self.tasks.get(&rejection.trigger_task_id).ok_or_else(|| {
                OrchestratorError::CorruptGoal(
                    "pre-execution plan rejection references a missing trigger Task".to_owned(),
                )
            })?;
            if task.created_plan_revision() != rejection.rejected_plan_revision {
                return Err(OrchestratorError::CorruptGoal(
                    "pre-execution plan rejection trigger Task is not bound to its rejected plan"
                        .to_owned(),
                ));
            }
        }
        let mut supersession_request_ids = BTreeSet::new();
        for record in &self.pristine_plan_supersessions {
            if record.rejection_request_id.trim().is_empty()
                || record.rejected_plan_revision == 0
                || record.rejected_plan_revision >= record.committed_plan_revision
                || record.committed_plan_revision > self.plan_revision
                || record.canonical_proposal_digest.len() != 64
                || !record
                    .canonical_proposal_digest
                    .chars()
                    .all(|c| c.is_ascii_hexdigit())
                || record.affected_task_ids.is_empty()
                || record.replacement_task_ids.is_empty()
                || !supersession_request_ids.insert(record.rejection_request_id.clone())
            {
                return Err(OrchestratorError::CorruptGoal(
                    "Goal pristine-plan supersession history contains an invalid or duplicate record".to_owned(),
                ));
            }
            let rejection = self
                .find_pre_execution_plan_rejection(&record.rejection_request_id)
                .ok_or_else(|| {
                    OrchestratorError::CorruptGoal(
                        "pristine-plan supersession references a missing pre-execution rejection"
                            .to_owned(),
                    )
                })?;
            if rejection.rejected_plan_revision() != record.rejected_plan_revision
                || record
                    .affected_task_ids
                    .iter()
                    .collect::<BTreeSet<_>>()
                    .len()
                    != record.affected_task_ids.len()
                || record
                    .replacement_task_ids
                    .iter()
                    .collect::<BTreeSet<_>>()
                    .len()
                    != record.replacement_task_ids.len()
            {
                return Err(OrchestratorError::CorruptGoal(
                    "pristine-plan supersession history contains duplicate or mismatched Task identities".to_owned(),
                ));
            }
            let derived_affected = self
                .tasks
                .iter()
                .filter_map(|(id, task)| {
                    (task.created_plan_revision() == record.rejected_plan_revision)
                        .then_some(id.clone())
                })
                .collect::<BTreeSet<_>>();
            let recorded_affected = record
                .affected_task_ids
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            let derived_replacements = self
                .tasks
                .iter()
                .filter_map(|(id, task)| {
                    (task.created_plan_revision() == record.committed_plan_revision)
                        .then_some(id.clone())
                })
                .collect::<BTreeSet<_>>();
            let recorded_replacements = record
                .replacement_task_ids
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            if derived_affected != recorded_affected
                || derived_replacements != recorded_replacements
                || recorded_affected
                    .intersection(&recorded_replacements)
                    .next()
                    .is_some()
                || record
                    .affected_task_ids
                    .iter()
                    .any(|id| self.tasks[id].status() != TaskStatus::Superseded)
                || record.replacement_task_ids.iter().any(|id| {
                    let task = &self.tasks[id];
                    task.status() == TaskStatus::Superseded
                        || !task.mandatory()
                        || task.verification_specs().is_empty()
                })
            {
                return Err(OrchestratorError::CorruptGoal(
                    "pristine-plan supersession history is not bound to exact superseded and replacement Task authority".to_owned(),
                ));
            }
            let criteria = self
                .completion_criteria
                .iter()
                .map(|criterion| criterion.id.clone())
                .collect::<BTreeSet<_>>();
            let rebound_criteria = record
                .rebound_criterion_ids
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            if rebound_criteria.len() != record.rebound_criterion_ids.len()
                || rebound_criteria != criteria
            {
                return Err(OrchestratorError::CorruptGoal(
                    "pristine-plan supersession history has incomplete or duplicate criterion coverage".to_owned(),
                ));
            }
            if let Some(spec) = self.final_verification_spec.as_ref() {
                for binding in spec.criterion_bindings() {
                    if binding
                        .requirements()
                        .iter()
                        .any(|requirement| recorded_affected.contains(requirement.task_id()))
                    {
                        return Err(OrchestratorError::CorruptGoal(
                            "final-verification authority remains bound to a superseded Task"
                                .to_owned(),
                        ));
                    }
                }
            }
        }
        for checkpoint in &self.checkpoints {
            checkpoint.validate()?;
            if checkpoint.goal_revision > self.revision
                || checkpoint.plan_revision > self.plan_revision
            {
                return Err(OrchestratorError::CorruptGoal(
                    "checkpoint refers to a future goal/plan revision".to_owned(),
                ));
            }
        }
        self.validate_failed_task_replacement_history()?;
        self.validate_dag()?;
        if self.status.is_terminal() != self.completed_at.is_some() {
            return Err(OrchestratorError::CorruptGoal(
                "terminal/completed_at invariant is inconsistent".to_owned(),
            ));
        }
        if self.status.is_terminal()
            && self
                .tasks
                .values()
                .any(|task| matches!(task.status(), TaskStatus::Running | TaskStatus::Verifying))
        {
            return Err(OrchestratorError::CorruptGoal(
                "terminal goal contains active task state".to_owned(),
            ));
        }
        if self.status == GoalStatus::Completed {
            self.require_goal_completion_ready(GoalStatus::Verifying, GoalStatus::Completed)?;
        }
        Ok(())
    }

    /// Validate the durable failed-task replan/replacement history.
    ///
    /// These invariants are the durable half of the replacement contract: a
    /// superseded Task must remain permanently non-runnable, its attempt and
    /// retry accounting must be preserved exactly, no active Task may still
    /// depend on it, and no completion criterion may still require it.
    fn validate_failed_task_replacement_history(&self) -> Result<(), OrchestratorError> {
        let mut request_ids = BTreeSet::new();
        for request in &self.failed_task_replan_requests {
            if request.request_id.trim().is_empty()
                || request.request_id.chars().count() > 128
                || !request_ids.insert(request.request_id.clone())
                || request.expected_goal_revision == 0
                || request.expected_goal_revision > self.revision
                || request.observed_goal_revision != request.expected_goal_revision
                || request.trigger_plan_revision == 0
                || request.trigger_plan_revision > self.plan_revision
                || request.observed_plan_revision != request.trigger_plan_revision
                || request.reason.trim().is_empty()
                || request.reason.chars().count() > 8_192
                || request.authority != FailedTaskReplanAuthorityKind::GoalResume
                || request.observed_consumed_attempts > request.observed_max_attempts
                || request.requested_at.trim().is_empty()
            {
                return Err(OrchestratorError::CorruptGoal(
                    "Goal failed-task replan request history contains an invalid or duplicate record"
                        .to_owned(),
                ));
            }
            match (
                request.trigger_kind,
                request.authority_request_id.as_deref(),
            ) {
                (FailedTaskReplanTriggerKind::PostAttemptFailure, None) => {}
                (FailedTaskReplanTriggerKind::PreExecutionRejection, Some(authority)) => {
                    let rejection = self.find_pre_execution_plan_rejection(authority).ok_or_else(
                        || {
                            OrchestratorError::CorruptGoal(
                                "pre-execution failed-task replan request references a missing rejection"
                                    .to_owned(),
                            )
                        },
                    )?;
                    if rejection.trigger_task_id() != &request.trigger_task_id {
                        return Err(OrchestratorError::CorruptGoal(
                            "pre-execution failed-task replan request trigger does not match its rejection"
                                .to_owned(),
                        ));
                    }
                }
                _ => {
                    return Err(OrchestratorError::CorruptGoal(
                        "failed-task replan request trigger kind and authority are inconsistent"
                            .to_owned(),
                    ));
                }
            }
            let task = self.tasks.get(&request.trigger_task_id).ok_or_else(|| {
                OrchestratorError::CorruptGoal(
                    "failed-task replan request references a missing trigger Task".to_owned(),
                )
            })?;
            if matches!(
                task.status(),
                TaskStatus::Completed
                    | TaskStatus::Cancelled
                    | TaskStatus::Running
                    | TaskStatus::Verifying
            ) {
                return Err(OrchestratorError::CorruptGoal(
                    "failed-task replan request trigger Task is not a replaceable state".to_owned(),
                ));
            }
        }

        let mut consumed_request_ids = BTreeSet::new();
        let mut replaced_task_ids = BTreeSet::new();
        for record in &self.failed_task_replacements {
            if record.canonical_proposal_digest.len() != 64
                || !record
                    .canonical_proposal_digest
                    .chars()
                    .all(|c| c.is_ascii_hexdigit())
                || record.replaced_plan_revision == 0
                || record.replaced_plan_revision >= record.committed_plan_revision
                || record.committed_plan_revision > self.plan_revision
                || !consumed_request_ids.insert(record.replan_request_id.clone())
                || !replaced_task_ids.insert(record.replaced_task_id.clone())
                || self
                    .find_failed_task_replan_request(&record.replan_request_id)
                    .is_none()
            {
                return Err(OrchestratorError::CorruptGoal(
                    "Goal failed-task replacement history contains an invalid or duplicate record"
                        .to_owned(),
                ));
            }
            let replaced = self.tasks.get(&record.replaced_task_id).ok_or_else(|| {
                OrchestratorError::CorruptGoal(
                    "failed-task replacement references a missing replaced Task".to_owned(),
                )
            })?;
            let attempt_ids = replaced
                .attempts()
                .iter()
                .map(|attempt| attempt.id().clone())
                .collect::<BTreeSet<_>>();
            let recorded_attempt_ids = record
                .replaced_attempt_ids
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            if replaced.status() != TaskStatus::Superseded
                || replaced.max_attempts() != record.preserved_max_attempts
                || replaced.semantic_attempts_consumed() != record.preserved_consumed_attempts
                || attempt_ids != recorded_attempt_ids
            {
                return Err(OrchestratorError::CorruptGoal(
                    "failed-task replacement did not preserve the replaced Task's durable history"
                        .to_owned(),
                ));
            }
            // The closure is transaction-scoped: one transaction may supersede
            // several Tasks against a single shared committed closure.
            let committed_tasks = self
                .tasks
                .values()
                .filter(|task| task.created_plan_revision() == record.committed_plan_revision)
                .map(|task| task.id().clone())
                .collect::<BTreeSet<_>>();
            if record.completion_closure_task_ids.is_empty()
                || record
                    .completion_closure_task_ids
                    .iter()
                    .collect::<BTreeSet<_>>()
                    .len()
                    != record.completion_closure_task_ids.len()
                || record
                    .completion_closure_task_ids
                    .iter()
                    .any(|id| !committed_tasks.contains(id))
                || record
                    .replacement_task_ids
                    .iter()
                    .collect::<BTreeSet<_>>()
                    .len()
                    != record.replacement_task_ids.len()
                || record
                    .replacement_task_ids
                    .iter()
                    .any(|id| !committed_tasks.contains(id))
            {
                return Err(OrchestratorError::CorruptGoal(
                    "failed-task replacement closure is not bound to its committed replacement Tasks"
                        .to_owned(),
                ));
            }
        }

        if !replaced_task_ids.is_empty() {
            for task in self.tasks.values() {
                if !task.is_active_plan_authority() {
                    continue;
                }
                if task
                    .dependencies()
                    .iter()
                    .any(|edge| replaced_task_ids.contains(edge.task_id()))
                {
                    return Err(OrchestratorError::CorruptGoal(
                        "an active Task still depends on a superseded replacement target"
                            .to_owned(),
                    ));
                }
            }
            if let Some(spec) = self.final_verification_spec.as_ref() {
                for binding in spec.criterion_bindings() {
                    if binding
                        .requirements()
                        .iter()
                        .any(|requirement| replaced_task_ids.contains(requirement.task_id()))
                    {
                        return Err(OrchestratorError::CorruptGoal(
                            "completion-criterion authority remains bound to a superseded Task"
                                .to_owned(),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_legacy_schema1_terminal(&self) -> Result<(), OrchestratorError> {
        if !self.status.is_terminal() {
            return Err(OrchestratorError::SchemaUpgradeRequired(1));
        }
        if self.revision == 0 {
            return Err(OrchestratorError::CorruptGoal(
                "goal revision must start at 1".to_owned(),
            ));
        }
        self.id.validate()?;
        config::validate_session_id(&self.session_id)
            .map_err(|_| OrchestratorError::CorruptGoal("invalid legacy session_id".to_owned()))?;
        if self.objective.trim().is_empty() || self.completed_at.is_none() {
            return Err(OrchestratorError::CorruptGoal(
                "legacy terminal Goal shape is invalid".to_owned(),
            ));
        }
        self.validate_dag()?;
        // A legacy terminal Goal has no managed-workspace history to preserve, so
        // durable managed state on one is corruption rather than compatibility.
        self.validate_managed_workspace_state()?;
        Ok(())
    }

    pub(crate) fn validate_final_verification_contract(&self) -> Result<(), OrchestratorError> {
        if self.schema_version != GOAL_SCHEMA_VERSION {
            return Err(OrchestratorError::SchemaUpgradeRequired(
                self.schema_version as u64,
            ));
        }
        let spec = self.final_verification_spec.as_ref().ok_or_else(|| {
            OrchestratorError::InvalidDag(
                "structured Goal final-verification contract is missing".to_owned(),
            )
        })?;
        if spec.plan_revision != self.plan_revision {
            return Err(OrchestratorError::InvalidDag(
                "Goal final-verification contract plan_revision is stale".to_owned(),
            ));
        }
        let criterion_ids = self
            .completion_criteria
            .iter()
            .map(|criterion| criterion.id.clone())
            .collect::<BTreeSet<_>>();
        let mut bound = BTreeSet::new();
        for binding in &spec.criterion_bindings {
            binding.criterion_id.validate()?;
            if !criterion_ids.contains(&binding.criterion_id) {
                return Err(OrchestratorError::InvalidDag(
                    "Goal verification binding references an unknown criterion".to_owned(),
                ));
            }
            if !bound.insert(binding.criterion_id.clone()) {
                return Err(OrchestratorError::InvalidDag(
                    "Goal verification criterion binding is duplicated".to_owned(),
                ));
            }
            if binding.requirements.is_empty() {
                return Err(OrchestratorError::InvalidDag(
                    "required completion criterion has no structured proof requirement".to_owned(),
                ));
            }
            let mut seen_requirements = BTreeSet::new();
            for requirement in &binding.requirements {
                if !seen_requirements.insert(requirement.clone()) {
                    return Err(OrchestratorError::InvalidDag(
                        "Goal verification requirement is duplicated".to_owned(),
                    ));
                }
                let task = self.tasks.get(requirement.task_id()).ok_or_else(|| {
                    OrchestratorError::InvalidDag(
                        "TaskVerified binding references a missing Task".to_owned(),
                    )
                })?;
                if !task.is_active_plan_authority() || !task.mandatory() {
                    return Err(OrchestratorError::InvalidDag(
                        "TaskVerified binding references a non-mandatory Task".to_owned(),
                    ));
                }
            }
        }
        for criterion in &self.completion_criteria {
            if criterion.required && !bound.contains(&criterion.id) {
                return Err(OrchestratorError::InvalidDag(
                    "required completion criterion is not structurally mapped".to_owned(),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn validate_dag(&self) -> Result<(), OrchestratorError> {
        for (id, task) in &self.tasks {
            if id != task.id() {
                return Err(OrchestratorError::InvalidDag(
                    "task map key does not match task ID".to_owned(),
                ));
            }
            if task.created_plan_revision() == 0
                || task.created_plan_revision() > self.plan_revision
            {
                return Err(OrchestratorError::InvalidDag(
                    "task created_plan_revision is outside the Goal plan history".to_owned(),
                ));
            }
            task.validate_local()?;
            let mut seen_dependencies = BTreeSet::new();
            for dependency in task.dependencies() {
                let dependency_id = dependency.task_id();
                if dependency_id == id {
                    return Err(OrchestratorError::InvalidDag(
                        "task cannot depend on itself".to_owned(),
                    ));
                }
                if !seen_dependencies.insert(dependency_id.clone()) {
                    return Err(OrchestratorError::InvalidDag(
                        "duplicate dependency edge".to_owned(),
                    ));
                }
                let target = self.tasks.get(dependency_id).ok_or_else(|| {
                    OrchestratorError::InvalidDag("dependency target is missing".to_owned())
                })?;
                if task.is_active_plan_authority() && !target.is_active_plan_authority() {
                    return Err(OrchestratorError::InvalidDag(
                        "active Task cannot depend on a superseded Task".to_owned(),
                    ));
                }
                if task.mandatory() && !target.mandatory() {
                    return Err(OrchestratorError::InvalidDag(
                        "mandatory task cannot rely on an optional hard dependency".to_owned(),
                    ));
                }
            }
            if matches!(task.status(), TaskStatus::Ready | TaskStatus::Completed)
                && !self.dependencies_satisfied(id)?
            {
                return Err(OrchestratorError::InvalidDag(
                    "READY/COMPLETED task has incomplete hard dependency".to_owned(),
                ));
            }
        }
        self.validate_acyclic()?;
        let active_writers = self
            .tasks
            .values()
            .filter(|task| {
                matches!(task.status(), TaskStatus::Running | TaskStatus::Verifying)
                    && task.scope().operation_kind() != TaskOperationKind::ReadOnly
            })
            .count();
        if active_writers > 1 {
            return Err(OrchestratorError::InvalidDag(
                "more than one workspace mutation task is active".to_owned(),
            ));
        }
        Ok(())
    }

    fn validate_acyclic(&self) -> Result<(), OrchestratorError> {
        let mut indegree = self
            .tasks
            .keys()
            .cloned()
            .map(|id| (id, 0usize))
            .collect::<BTreeMap<_, _>>();
        let mut dependents: BTreeMap<TaskId, Vec<TaskId>> = BTreeMap::new();
        for (task_id, task) in &self.tasks {
            for dependency in task.dependencies() {
                *indegree.get_mut(task_id).expect("task exists") += 1;
                dependents
                    .entry(dependency.task_id().clone())
                    .or_default()
                    .push(task_id.clone());
            }
        }
        let mut queue = indegree
            .iter()
            .filter_map(|(id, count)| (*count == 0).then_some(id.clone()))
            .collect::<VecDeque<_>>();
        let mut visited = 0usize;
        while let Some(id) = queue.pop_front() {
            visited += 1;
            if let Some(children) = dependents.get(&id) {
                for child in children {
                    let count = indegree.get_mut(child).expect("dependent exists");
                    *count -= 1;
                    if *count == 0 {
                        queue.push_back(child.clone());
                    }
                }
            }
        }
        if visited != self.tasks.len() {
            return Err(OrchestratorError::InvalidDag(
                "task graph contains a cycle".to_owned(),
            ));
        }
        Ok(())
    }

    fn dependencies_satisfied(&self, task_id: &TaskId) -> Result<bool, OrchestratorError> {
        let task = self
            .tasks
            .get(task_id)
            .ok_or_else(|| OrchestratorError::InvalidDag("task is missing".to_owned()))?;
        Ok(task.dependencies().iter().all(|dependency| {
            self.tasks
                .get(dependency.task_id())
                .is_some_and(|dependency| {
                    dependency.is_active_plan_authority()
                        && dependency.status() == TaskStatus::Completed
                })
        }))
    }

    fn require_goal_verification_entry_ready(
        &self,
        from: GoalStatus,
        to: GoalStatus,
    ) -> Result<(), OrchestratorError> {
        if self
            .tasks
            .values()
            .filter(|task| task.is_active_plan_authority() && task.mandatory())
            .any(|task| task.status() != TaskStatus::Completed)
            || self.tasks.values().any(|task| {
                matches!(
                    task.status(),
                    TaskStatus::Running | TaskStatus::Verifying | TaskStatus::NeedsReplan
                )
            })
            || self.has_unresolved_mandatory_blocker()
            || self.tasks.values().any(Task::has_unknown_side_effect)
        {
            return Err(invalid_goal_transition(
                from,
                to,
                "mandatory work/blockers/side effects are not fully resolved",
            ));
        }
        if self
            .tasks
            .values()
            .filter(|task| task.is_active_plan_authority() && task.mandatory())
            .any(|task| {
                task.verification_results()
                    .last()
                    .map(VerificationResult::outcome)
                    != Some(VerificationOutcome::Passed)
            })
        {
            return Err(invalid_goal_transition(
                from,
                to,
                "latest mandatory Task verification is not PASSED",
            ));
        }
        self.validate_dag()?;
        self.validate_final_verification_contract()
    }

    fn require_goal_completion_ready(
        &self,
        from: GoalStatus,
        to: GoalStatus,
    ) -> Result<(), OrchestratorError> {
        if self
            .tasks
            .values()
            .filter(|task| task.is_active_plan_authority() && task.mandatory())
            .any(|task| task.status() != TaskStatus::Completed)
        {
            return Err(invalid_goal_transition(
                from,
                to,
                "not every mandatory task is COMPLETED",
            ));
        }
        if self
            .tasks
            .values()
            .filter(|task| task.is_active_plan_authority() && task.mandatory())
            .any(|task| {
                matches!(
                    task.status(),
                    TaskStatus::Pending
                        | TaskStatus::Ready
                        | TaskStatus::Running
                        | TaskStatus::Blocked
                        | TaskStatus::Retryable
                        | TaskStatus::NeedsReplan
                        | TaskStatus::Verifying
                )
            })
        {
            return Err(invalid_goal_transition(
                from,
                to,
                "mandatory task state still requires work",
            ));
        }
        if self.has_unresolved_mandatory_blocker()
            || self.tasks.values().any(Task::has_unknown_side_effect)
        {
            return Err(invalid_goal_transition(
                from,
                to,
                "unresolved blocker or unknown side effect remains",
            ));
        }
        if self
            .latest_applicable_final_verification()
            .map(GoalFinalVerificationRecord::outcome)
            != Some(VerificationOutcome::Passed)
        {
            return Err(invalid_goal_transition(
                from,
                to,
                "latest applicable authoritative Goal final verification has not passed",
            ));
        }
        if !self.checkpoints.iter().any(|checkpoint| {
            checkpoint.reason == CheckpointReason::FinalVerification
                && checkpoint.goal_status == GoalStatus::Verifying
                && checkpoint.goal_revision <= self.revision
        }) {
            return Err(invalid_goal_transition(
                from,
                to,
                "final verification checkpoint has not been recorded",
            ));
        }
        self.validate_dag()
    }

    fn has_unresolved_mandatory_blocker(&self) -> bool {
        self.blockers.iter().any(|blocker| blocker.mandatory)
            || self
                .tasks
                .values()
                .filter(|task| task.is_active_plan_authority() && task.mandatory())
                .any(|task| !task.blockers().is_empty())
    }
}

fn record_task_identities_current(goal: &Goal, record: &GoalFinalVerificationRecord) -> bool {
    record
        .criterion_results
        .iter()
        .flat_map(|criterion| criterion.observations.iter())
        .all(|observation| match observation {
            GoalRequirementObservation::TaskVerified {
                task_id,
                task_status,
                verification_id,
                verification_outcome,
                ..
            } => {
                let Some(task) = goal.tasks.get(task_id) else {
                    return false;
                };
                if task.status() != *task_status {
                    return false;
                }
                match (
                    verification_id,
                    verification_outcome,
                    task.verification_results().last(),
                ) {
                    (Some(expected_id), Some(expected_outcome), Some(current)) => {
                        current.id() == expected_id && current.outcome() == *expected_outcome
                    }
                    (None, None, None) => true,
                    _ => false,
                }
            }
        })
}

fn validate_replan_history_immutability(
    before: &Goal,
    after: &Goal,
) -> Result<(), OrchestratorError> {
    for (task_id, old) in &before.tasks {
        let new = after.tasks.get(task_id).ok_or_else(|| {
            OrchestratorError::InvalidDag(
                "replanning cannot delete existing Task history".to_owned(),
            )
        })?;
        if old.is_terminal() {
            if old != new {
                return Err(OrchestratorError::InvalidDag(
                    "terminal Task history is immutable during replanning".to_owned(),
                ));
            }
            continue;
        }
        if old.title() != new.title()
            || old.objective() != new.objective()
            || old.worker() != new.worker()
            || old.scope() != new.scope()
            || old.attempts() != new.attempts()
            || old.evidence() != new.evidence()
            || old.blockers() != new.blockers()
            || old.verification_results() != new.verification_results()
            || old.max_attempts() != new.max_attempts()
            || old.created_plan_revision() != new.created_plan_revision()
        {
            return Err(OrchestratorError::InvalidDag(
                "replanning attempted to rewrite immutable Task authority/history".to_owned(),
            ));
        }
        if old.mandatory() && !new.mandatory() {
            return Err(OrchestratorError::InvalidDag(
                "replanning cannot make a mandatory Task optional".to_owned(),
            ));
        }
        let old_dependencies = old
            .dependencies()
            .iter()
            .map(|dependency| dependency.task_id().clone())
            .collect::<BTreeSet<_>>();
        let new_dependencies = new
            .dependencies()
            .iter()
            .map(|dependency| dependency.task_id().clone())
            .collect::<BTreeSet<_>>();
        if !old_dependencies.is_subset(&new_dependencies) {
            return Err(OrchestratorError::InvalidDag(
                "replanning cannot remove hard dependencies".to_owned(),
            ));
        }
        if old
            .verification_specs()
            .iter()
            .any(|spec| !new.verification_specs().contains(spec))
        {
            return Err(OrchestratorError::InvalidDag(
                "replanning cannot weaken verification requirements".to_owned(),
            ));
        }
        if old.status() != new.status()
            && !(old.status() == TaskStatus::NeedsReplan && new.status() == TaskStatus::Pending)
        {
            return Err(OrchestratorError::InvalidDag(
                "replanning changed an existing Task state outside NEEDS_REPLAN -> PENDING"
                    .to_owned(),
            ));
        }
    }
    Ok(())
}

fn invalid_goal_transition(
    from: GoalStatus,
    to: GoalStatus,
    reason: impl Into<String>,
) -> OrchestratorError {
    OrchestratorError::invalid_transition(
        "goal",
        format!("{from:?}").to_ascii_uppercase(),
        format!("{to:?}").to_ascii_uppercase(),
        reason,
    )
}

fn serialize_tasks<S>(tasks: &BTreeMap<TaskId, Task>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut sequence = serializer.serialize_seq(Some(tasks.len()))?;
    for task in tasks.values() {
        sequence.serialize_element(task)?;
    }
    sequence.end()
}

fn deserialize_tasks<'de, D>(deserializer: D) -> Result<BTreeMap<TaskId, Task>, D::Error>
where
    D: Deserializer<'de>,
{
    let tasks = Vec::<Task>::deserialize(deserializer)?;
    let mut map = BTreeMap::new();
    for task in tasks {
        let id = task.id().clone();
        if map.insert(id, task).is_some() {
            return Err(D::Error::custom("duplicate task ID"));
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskEvidence;

    const NOW: &str = "2026-01-01T00:00:00Z";

    fn scope() -> TaskScope {
        TaskScope::new(
            vec![PathBuf::from("src")],
            vec![],
            TaskOperationKind::ReadOnly,
            ReplaySafety::SafeReadOnly,
        )
    }

    fn goal() -> Goal {
        Goal::new(
            "phase2-test",
            PathBuf::from("/tmp/project"),
            "test objective",
            Some("test".into()),
            vec![],
            vec!["all checks pass".into()],
            NOW,
        )
        .unwrap()
    }

    fn add_task(goal: &mut Goal, mandatory: bool) -> TaskId {
        goal.add_task(
            "task",
            "task objective",
            mandatory,
            WorkerKind::CodexReadonly,
            scope(),
            vec![],
            3,
            NOW,
        )
        .unwrap()
    }

    fn complete_task(goal: &mut Goal, id: &TaskId) {
        let safe = TaskTransitionContext {
            active_worker_stopped: true,
            side_effect_reconciled: true,
        };
        goal.transition_task(id, TaskStatus::Ready, safe, NOW)
            .unwrap();
        goal.transition_task(id, TaskStatus::Running, safe, NOW)
            .unwrap();
        goal.transition_task(id, TaskStatus::Verifying, safe, NOW)
            .unwrap();
        goal.task_record_verification_result(
            id,
            VerificationResult::new(VerificationOutcome::Passed, vec![], NOW, NOW),
        )
        .unwrap();
        goal.transition_task(id, TaskStatus::Completed, safe, NOW)
            .unwrap();
    }

    #[test]
    fn goal_serialization_roundtrip_preserves_schema_and_tasks() {
        let mut goal = goal();
        add_task(&mut goal, true);
        let encoded = serde_json::to_vec_pretty(&goal).unwrap();
        let decoded: Goal = serde_json::from_slice(&encoded).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded.schema_version(), GOAL_SCHEMA_VERSION);
        assert_eq!(decoded.revision(), 1);
        assert_eq!(decoded.tasks().len(), 1);
    }

    #[test]
    fn goal_status_roundtrip_and_terminal_set_are_exact() {
        for status in [
            GoalStatus::Planning,
            GoalStatus::Running,
            GoalStatus::Replanning,
            GoalStatus::Pausing,
            GoalStatus::Paused,
            GoalStatus::Blocked,
            GoalStatus::Verifying,
            GoalStatus::Cancelling,
            GoalStatus::Completed,
            GoalStatus::Failed,
            GoalStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(serde_json::from_str::<GoalStatus>(&json).unwrap(), status);
        }
        assert!(GoalStatus::Completed.is_terminal());
        assert!(GoalStatus::Failed.is_terminal());
        assert!(GoalStatus::Cancelled.is_terminal());
        assert!(!GoalStatus::Blocked.is_terminal());
    }

    #[test]
    fn valid_linear_branching_and_join_dags_are_accepted() {
        let mut goal = goal();
        let a = add_task(&mut goal, true);
        let b = add_task(&mut goal, true);
        let c = add_task(&mut goal, true);
        let d = add_task(&mut goal, true);
        goal.strengthen_task_dependencies(&b, vec![a.clone()])
            .unwrap();
        goal.strengthen_task_dependencies(&c, vec![a.clone()])
            .unwrap();
        goal.strengthen_task_dependencies(&d, vec![b, c]).unwrap();
        goal.validate_dag().unwrap();
    }

    #[test]
    fn self_dependency_missing_dependency_and_cycle_are_rejected() {
        let mut goal = goal();
        let a = add_task(&mut goal, true);
        let b = add_task(&mut goal, true);
        assert!(
            goal.strengthen_task_dependencies(&a, vec![a.clone()])
                .is_err()
        );
        assert!(
            goal.strengthen_task_dependencies(&a, vec![TaskId::new()])
                .is_err()
        );
        goal.strengthen_task_dependencies(&b, vec![a.clone()])
            .unwrap();
        assert!(goal.strengthen_task_dependencies(&a, vec![b]).is_err());
    }

    #[test]
    fn duplicate_dependency_edge_is_rejected() {
        let mut goal = goal();
        let a = add_task(&mut goal, true);
        let b = add_task(&mut goal, true);
        assert!(
            goal.strengthen_task_dependencies(&b, vec![a.clone(), a])
                .is_err()
        );
    }

    #[test]
    fn mandatory_task_cannot_depend_on_optional_task() {
        let mut goal = goal();
        let optional = add_task(&mut goal, false);
        let mandatory = add_task(&mut goal, true);
        assert!(
            goal.strengthen_task_dependencies(&mandatory, vec![optional])
                .is_err()
        );
    }

    #[test]
    fn ready_gating_follows_hard_dependency_completion() {
        let mut goal = goal();
        let a = add_task(&mut goal, true);
        let b = add_task(&mut goal, true);
        goal.strengthen_task_dependencies(&b, vec![a.clone()])
            .unwrap();
        assert!(
            goal.transition_task(&b, TaskStatus::Ready, TaskTransitionContext::default(), NOW)
                .is_err()
        );
        complete_task(&mut goal, &a);
        goal.transition_task(&b, TaskStatus::Ready, TaskTransitionContext::default(), NOW)
            .unwrap();
    }

    #[test]
    fn goal_completion_is_gated_by_tasks_final_verification_and_checkpoint() {
        let mut goal = goal();
        let task = add_task(&mut goal, true);
        goal.transition_to(GoalStatus::Running, NOW).unwrap();
        assert!(goal.transition_to(GoalStatus::Verifying, NOW).is_err());
        complete_task(&mut goal, &task);
        goal.enter_verifying_for_test(NOW).unwrap();
        assert!(goal.transition_to(GoalStatus::Completed, NOW).is_err());
        goal.record_final_verification(VerificationResult::new(
            VerificationOutcome::Passed,
            vec![],
            NOW,
            NOW,
        ))
        .unwrap();
        assert!(goal.transition_to(GoalStatus::Completed, NOW).is_err());
        goal.add_checkpoint(CheckpointReason::FinalVerification, NOW)
            .unwrap();
        assert!(goal.transition_to(GoalStatus::Completed, NOW).is_err());
        crate::goal_finalizer::complete_goal_for_test(&mut goal, NOW).unwrap();
        assert!(goal.is_terminal());
    }

    #[test]
    fn goal_cannot_skip_verifying_or_leave_terminal_state() {
        let mut goal = goal();
        add_task(&mut goal, true);
        goal.transition_to(GoalStatus::Running, NOW).unwrap();
        assert!(goal.transition_to(GoalStatus::Completed, NOW).is_err());
        goal.transition_to(GoalStatus::Failed, NOW).unwrap();
        assert!(goal.transition_to(GoalStatus::Running, NOW).is_err());
    }

    #[test]
    fn dynamic_insertion_and_strengthening_are_monotonic() {
        let mut goal = goal();
        let a = add_task(&mut goal, true);
        let b = add_task(&mut goal, true);
        let before = goal.plan_revision();
        goal.strengthen_task_dependencies(&b, vec![a]).unwrap();
        assert!(goal.plan_revision() > before);
        let verification = VerificationSpec::StructuredEvidence {
            requirement_id: "evidence-1".into(),
        };
        goal.strengthen_task_verification(&b, vec![verification.clone()])
            .unwrap();
        assert!(goal.strengthen_task_verification(&b, vec![]).is_err());
    }

    #[test]
    fn mandatory_strengthening_is_one_way() {
        let mut goal = goal();
        let task = add_task(&mut goal, false);
        goal.strengthen_task_mandatory(&task).unwrap();
        assert!(goal.tasks().get(&task).unwrap().mandatory());
    }

    #[test]
    fn duplicate_task_ids_in_persisted_array_are_rejected() {
        let mut goal = goal();
        let id = add_task(&mut goal, true);
        let mut value = serde_json::to_value(&goal).unwrap();
        let first = value["tasks"][0].clone();
        value["tasks"].as_array_mut().unwrap().push(first);
        assert!(serde_json::from_value::<Goal>(value).is_err());
        assert!(goal.tasks().contains_key(&id));
    }

    #[test]
    fn completed_task_history_rejects_new_evidence() {
        let mut goal = goal();
        let id = add_task(&mut goal, true);
        complete_task(&mut goal, &id);
        assert!(
            goal.task_add_evidence(
                &id,
                TaskEvidence::ReviewResult {
                    summary: "late rewrite".into(),
                    blocking_findings: 0,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn rejected_cycle_insertion_leaves_goal_exactly_unchanged() {
        let mut goal = goal();
        let a = add_task(&mut goal, true);
        let b = add_task(&mut goal, true);
        goal.strengthen_task_dependencies(&b, vec![a.clone()])
            .unwrap();
        let before = goal.clone();
        assert!(goal.strengthen_task_dependencies(&a, vec![b]).is_err());
        assert_eq!(goal, before);
    }

    #[test]
    fn rejected_missing_dependency_leaves_goal_exactly_unchanged() {
        let mut goal = goal();
        let task = add_task(&mut goal, true);
        let before = goal.clone();
        assert!(
            goal.strengthen_task_dependencies(&task, vec![TaskId::new()])
                .is_err()
        );
        assert_eq!(goal, before);
    }

    #[test]
    fn rejected_dependency_removal_leaves_goal_exactly_unchanged() {
        let mut goal = goal();
        let dependency = add_task(&mut goal, true);
        let task = add_task(&mut goal, true);
        goal.strengthen_task_dependencies(&task, vec![dependency])
            .unwrap();
        let before = goal.clone();
        assert!(goal.strengthen_task_dependencies(&task, vec![]).is_err());
        assert_eq!(goal, before);
    }

    #[test]
    fn rejected_verification_weakening_leaves_goal_exactly_unchanged() {
        let mut goal = goal();
        let task = add_task(&mut goal, true);
        goal.strengthen_task_verification(
            &task,
            vec![VerificationSpec::StructuredEvidence {
                requirement_id: "required-evidence".into(),
            }],
        )
        .unwrap();
        let before = goal.clone();
        assert!(goal.strengthen_task_verification(&task, vec![]).is_err());
        assert_eq!(goal, before);
    }

    #[test]
    fn rejected_terminal_history_mutation_leaves_goal_exactly_unchanged() {
        let mut goal = goal();
        let task = add_task(&mut goal, true);
        complete_task(&mut goal, &task);
        let before = goal.clone();
        assert!(
            goal.task_add_evidence(
                &task,
                TaskEvidence::ReviewResult {
                    summary: "late rewrite".into(),
                    blocking_findings: 0,
                },
            )
            .is_err()
        );
        assert_eq!(goal, before);
    }

    #[test]
    fn final_verification_indeterminate_never_completes_goal() {
        let mut goal = goal();
        let id = add_task(&mut goal, true);
        goal.transition_to(GoalStatus::Running, NOW).unwrap();
        complete_task(&mut goal, &id);
        goal.enter_verifying_for_test(NOW).unwrap();
        goal.record_final_verification(VerificationResult::new(
            VerificationOutcome::Indeterminate,
            vec![],
            NOW,
            NOW,
        ))
        .unwrap();
        goal.add_checkpoint(CheckpointReason::FinalVerification, NOW)
            .unwrap();
        assert!(goal.transition_to(GoalStatus::Completed, NOW).is_err());
    }
}
