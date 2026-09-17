use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde::de::Error as _;
use serde::ser::SerializeSeq;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config;
use crate::mutation::{MutationIntent, MutationIntentState, MutationIntentUpdate};
use crate::orchestrator_error::OrchestratorError;
use crate::task::{
    ReplaySafety, Task, TaskDependency, TaskId, TaskOperationKind, TaskScope, TaskStatus,
    TaskTransitionContext, VerificationId, VerificationOutcome, VerificationResult, VerificationSpec, WorkerKind,
    WorkerReport,
};

pub(crate) const GOAL_STORE_FORMAT: &str = "local-mcp-goal";
pub(crate) const GOAL_SCHEMA_VERSION: u32 = 2;

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

    fn validate(&self) -> Result<(), OrchestratorError> {
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
    pub(crate) fn new() -> Self { Self(Uuid::new_v4().to_string()) }
    pub(crate) fn parse(value: &str) -> Result<Self, OrchestratorError> {
        let uuid = Uuid::parse_str(value)
            .map_err(|_| OrchestratorError::UnsafeIdentifier("CompletionCriterionId".to_owned()))?;
        Ok(Self(uuid.to_string()))
    }
    pub(crate) fn as_str(&self) -> &str { &self.0 }
    fn validate(&self) -> Result<(), OrchestratorError> {
        if Self::parse(&self.0)?.0 != self.0 {
            return Err(OrchestratorError::UnsafeIdentifier("CompletionCriterionId".to_owned()));
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
            return Err(OrchestratorError::CorruptGoal("completion criterion description must not be empty".to_owned()));
        }
        Ok(Self { id: CompletionCriterionId::new(), description, required: true })
    }
    pub(crate) fn id(&self) -> &CompletionCriterionId { &self.id }
    pub(crate) fn description(&self) -> &str { &self.description }
    pub(crate) fn required(&self) -> bool { self.required }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub(crate) enum GoalVerificationRequirement {
    TaskVerified { task_id: TaskId },
}

impl GoalVerificationRequirement {
    pub(crate) fn task_id(&self) -> &TaskId { match self { Self::TaskVerified { task_id } => task_id } }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoalCriterionBinding {
    criterion_id: CompletionCriterionId,
    requirements: Vec<GoalVerificationRequirement>,
}

impl GoalCriterionBinding {
    pub(crate) fn new(criterion_id: CompletionCriterionId, requirements: Vec<GoalVerificationRequirement>) -> Self { Self { criterion_id, requirements } }
    pub(crate) fn criterion_id(&self) -> &CompletionCriterionId { &self.criterion_id }
    pub(crate) fn requirements(&self) -> &[GoalVerificationRequirement] { &self.requirements }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoalFinalVerificationSpec {
    plan_revision: u32,
    criterion_bindings: Vec<GoalCriterionBinding>,
}

impl GoalFinalVerificationSpec {
    pub(crate) fn new(plan_revision: u32, criterion_bindings: Vec<GoalCriterionBinding>) -> Self { Self { plan_revision, criterion_bindings } }
    pub(crate) fn plan_revision(&self) -> u32 { self.plan_revision }
    pub(crate) fn criterion_bindings(&self) -> &[GoalCriterionBinding] { &self.criterion_bindings }
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
pub(crate) enum VerificationOrigin { HostDeterministicGoalVerifier }

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

impl GoalRequirementObservation {
    pub(crate) fn task_verification_identity(&self) -> (&TaskId, Option<&VerificationId>) {
        match self { Self::TaskVerified { task_id, verification_id, .. } => (task_id, verification_id.as_ref()) }
    }
    pub(crate) fn outcome(&self) -> VerificationOutcome { match self { Self::TaskVerified { outcome, .. } => *outcome } }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoalCriterionVerificationResult {
    criterion_id: CompletionCriterionId,
    outcome: VerificationOutcome,
    observations: Vec<GoalRequirementObservation>,
}

impl GoalCriterionVerificationResult {
    pub(crate) fn new(criterion_id: CompletionCriterionId, outcome: VerificationOutcome, observations: Vec<GoalRequirementObservation>) -> Self { Self { criterion_id, outcome, observations } }
    pub(crate) fn criterion_id(&self) -> &CompletionCriterionId { &self.criterion_id }
    pub(crate) fn outcome(&self) -> VerificationOutcome { self.outcome }
    pub(crate) fn observations(&self) -> &[GoalRequirementObservation] { &self.observations }
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

impl GoalFinalVerificationRecord {
    pub(crate) fn new(outcome: VerificationOutcome, goal_id: GoalId, evaluated_goal_revision: u64, committed_goal_revision: u64, plan_revision: u32, contract_digest: String, criterion_results: Vec<GoalCriterionVerificationResult>, started_at: impl Into<String>, finished_at: impl Into<String>) -> Self {
        Self { id: VerificationId::new(), outcome, source: VerificationOrigin::HostDeterministicGoalVerifier, goal_id, evaluated_goal_revision, committed_goal_revision, plan_revision, contract_digest, criterion_results, started_at: started_at.into(), finished_at: finished_at.into() }
    }
    pub(crate) fn id(&self) -> &VerificationId { &self.id }
    pub(crate) fn outcome(&self) -> VerificationOutcome { self.outcome }
    pub(crate) fn source(&self) -> VerificationOrigin { self.source }
    pub(crate) fn goal_id(&self) -> &GoalId { &self.goal_id }
    pub(crate) fn evaluated_goal_revision(&self) -> u64 { self.evaluated_goal_revision }
    pub(crate) fn committed_goal_revision(&self) -> u64 { self.committed_goal_revision }
    pub(crate) fn plan_revision(&self) -> u32 { self.plan_revision }
    pub(crate) fn contract_digest(&self) -> &str { &self.contract_digest }
    pub(crate) fn criterion_results(&self) -> &[GoalCriterionVerificationResult] { &self.criterion_results }
    pub(crate) fn started_at(&self) -> &str { &self.started_at }
    pub(crate) fn finished_at(&self) -> &str { &self.finished_at }
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
    Pause,
    Recovery,
    FinalVerification,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum PreExecutionPlanRejectionAuthorityKind {
    GoalResume,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreExecutionPlanRejection {
    request_id: String,
    expected_goal_revision: u64,
    rejected_plan_revision: u32,
    trigger_task_id: TaskId,
    reason: String,
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
    ) -> bool {
        self.request_id == request_id
            && self.expected_goal_revision == expected_goal_revision
            && self.rejected_plan_revision == expected_plan_revision
            && self.trigger_task_id == *trigger_task_id
            && self.reason == reason
    }

    pub(crate) fn request_id(&self) -> &str { &self.request_id }
    pub(crate) fn rejected_plan_revision(&self) -> u32 { self.rejected_plan_revision }
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
    pub(crate) add_criterion_requirements: Vec<(CompletionCriterionId, Vec<GoalVerificationRequirement>)>,
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
    checkpoints: Vec<GoalCheckpoint>,
    created_at: String,
    updated_at: String,
    completed_at: Option<String>,
}

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

    pub(crate) fn revision(&self) -> u64 {
        self.revision
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
        self.completion_criteria.iter().map(|criterion| criterion.description.clone()).collect()
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

    pub(crate) fn latest_applicable_final_verification(&self) -> Option<&GoalFinalVerificationRecord> {
        if self.schema_version != GOAL_SCHEMA_VERSION { return None; }
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

    pub(crate) fn has_pre_execution_plan_rejection_for_plan(
        &self,
        plan_revision: u32,
    ) -> bool {
        self.pre_execution_plan_rejections
            .iter()
            .any(|record| record.rejected_plan_revision() == plan_revision)
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
        self.tasks.values().any(|task| {
            matches!(task.status(), TaskStatus::Running | TaskStatus::Verifying)
        })
    }

    pub(crate) fn has_unknown_side_effects(&self) -> bool {
        self.tasks.values().any(Task::has_unknown_side_effect)
    }

    pub(crate) fn has_blocking_state(&self) -> bool {
        !self.blockers.is_empty()
            || self.tasks.values().any(|task| {
                task.status() == TaskStatus::Blocked || !task.blockers().is_empty()
            })
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
        if self.status != GoalStatus::Planning || self.plan_revision != 0 || !self.tasks.is_empty() {
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
            .map(|criterion| GoalCriterionBinding::new(
                criterion.id.clone(),
                mandatory_ids.iter().cloned().map(|task_id| GoalVerificationRequirement::TaskVerified { task_id }).collect(),
            ))
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

        let next_plan_revision = self.plan_revision.checked_add(1).ok_or_else(|| {
            OrchestratorError::InvalidDag("plan revision overflow".to_owned())
        })?;
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
            let task = candidate
                .tasks
                .get(&task_id)
                .ok_or_else(|| OrchestratorError::InvalidDag("target task is missing".to_owned()))?;
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
            let task = candidate
                .tasks
                .get(&task_id)
                .ok_or_else(|| OrchestratorError::InvalidDag("target task is missing".to_owned()))?;
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
            let task = candidate
                .tasks
                .get_mut(&task_id)
                .ok_or_else(|| OrchestratorError::InvalidDag("target task is missing".to_owned()))?;
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
                    "NEEDS_REPLAN may resolve only after a new prerequisite is committed".to_owned(),
                ));
            }
            let task = candidate
                .tasks
                .get(&task_id)
                .ok_or_else(|| OrchestratorError::InvalidDag("target task is missing".to_owned()))?;
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
                OrchestratorError::InvalidDag("replan requires an existing structured Goal final-verification contract".to_owned())
            })?;
            spec.plan_revision = next_plan_revision;
            let criterion_ids = candidate.completion_criteria.iter().map(|criterion| criterion.id.clone()).collect::<BTreeSet<_>>();
            let mut seen_criteria = BTreeSet::new();
            for (criterion_id, additions) in criterion_additions {
                if !criterion_ids.contains(&criterion_id) {
                    return Err(OrchestratorError::InvalidDag("replan criterion binding references an unknown completion criterion".to_owned()));
                }
                if additions.is_empty() || !seen_criteria.insert(criterion_id.clone()) {
                    return Err(OrchestratorError::InvalidDag("criterion binding strengthening must be non-empty and grouped once per criterion".to_owned()));
                }
                let binding = spec.criterion_bindings.iter_mut().find(|binding| binding.criterion_id == criterion_id).ok_or_else(|| {
                    OrchestratorError::InvalidDag("replan cannot create missing initial criterion coverage".to_owned())
                })?;
                for requirement in additions {
                    if binding.requirements.contains(&requirement) {
                        return Err(OrchestratorError::InvalidDag("replan cannot duplicate an existing Goal verification requirement".to_owned()));
                    }
                    let task = candidate.tasks.get(requirement.task_id()).ok_or_else(|| {
                        OrchestratorError::InvalidDag("criterion strengthening references a missing Task".to_owned())
                    })?;
                    if !task.mandatory() {
                        return Err(OrchestratorError::InvalidDag("TaskVerified may bind only a mandatory Task".to_owned()));
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

    #[cfg(test)]
    fn rebuild_test_final_verification_contract(&mut self) {
        if self.plan_revision == 0 {
            self.final_verification_spec = None;
            return;
        }
        let mandatory_ids = self.tasks.iter()
            .filter_map(|(task_id, task)| task.mandatory().then_some(task_id.clone()))
            .collect::<Vec<_>>();
        if mandatory_ids.is_empty() {
            self.final_verification_spec = None;
            return;
        }
        let bindings = self.completion_criteria.iter().map(|criterion| {
            GoalCriterionBinding::new(
                criterion.id.clone(),
                mandatory_ids.iter().cloned().map(|task_id| GoalVerificationRequirement::TaskVerified { task_id }).collect(),
            )
        }).collect();
        self.final_verification_spec = Some(GoalFinalVerificationSpec::new(self.plan_revision, bindings));
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
        let next_plan_revision = self.plan_revision.checked_add(1).ok_or_else(|| {
            OrchestratorError::InvalidDag("plan revision overflow".to_owned())
        })?;
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
        if candidate.final_verification_spec.is_some() { candidate.validate()?; } else { candidate.validate_dag()?; }
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
        candidate.plan_revision = candidate.plan_revision.checked_add(1).ok_or_else(|| {
            OrchestratorError::InvalidDag("plan revision overflow".to_owned())
        })?;
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
        candidate.plan_revision = candidate.plan_revision.checked_add(1).ok_or_else(|| {
            OrchestratorError::InvalidDag("plan revision overflow".to_owned())
        })?;
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
                rejected_plan_revision: expected_plan_revision,
                trigger_task_id,
                reason,
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
        let expected_digest = self.final_verification_spec.as_ref().ok_or_else(|| {
            OrchestratorError::CorruptGoal("Goal final-verification contract is missing".to_owned())
        })?.canonical_digest();
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
        if self.final_verifications.iter().any(|existing| existing.id == record.id) {
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
            return Err(invalid_goal_transition(from, GoalStatus::Verifying, "test helper requires RUNNING"));
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
            return Err(invalid_goal_transition(self.status, self.status, "test final verification helper requires VERIFYING"));
        }
        let spec = self.final_verification_spec.as_ref().ok_or_else(|| {
            OrchestratorError::InvalidDag("test final verification helper requires structured contract".to_owned())
        })?;
        let mut criterion_results = Vec::new();
        for binding in &spec.criterion_bindings {
            let observations = binding.requirements.iter().map(|requirement| {
                let task_id = requirement.task_id().clone();
                let task = self.tasks.get(&task_id);
                let latest = task.and_then(|task| task.verification_results().last());
                GoalRequirementObservation::TaskVerified {
                    task_id,
                    task_status: task.map(|task| task.status()).unwrap_or(TaskStatus::Blocked),
                    verification_id: latest.map(|verification| verification.id().clone()),
                    verification_outcome: latest.map(|verification| verification.outcome()),
                    outcome: latest.map(|verification| verification.outcome()).unwrap_or(VerificationOutcome::Indeterminate),
                }
            }).collect();
            criterion_results.push(GoalCriterionVerificationResult::new(
                binding.criterion_id.clone(),
                result.outcome(),
                observations,
            ));
        }
        self.final_verifications.push(GoalFinalVerificationRecord::new(
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
                GoalStatus::Running | GoalStatus::Blocked | GoalStatus::Cancelling | GoalStatus::Failed
            )
                | (
                    GoalStatus::Running,
                    GoalStatus::Replanning
                        | GoalStatus::Pausing
                        | GoalStatus::Blocked
                        | GoalStatus::Cancelling
                        | GoalStatus::Failed
                )
                | (
                    GoalStatus::Replanning,
                    GoalStatus::Running
                        | GoalStatus::Blocked
                        | GoalStatus::Pausing
                        | GoalStatus::Cancelling
                        | GoalStatus::Failed
                )
                | (
                    GoalStatus::Pausing,
                    GoalStatus::Paused | GoalStatus::Blocked | GoalStatus::Cancelling
                )
                | (
                    GoalStatus::Paused,
                    GoalStatus::Running | GoalStatus::Replanning | GoalStatus::Blocked | GoalStatus::Cancelling
                )
                | (
                    GoalStatus::Blocked,
                    GoalStatus::Running
                        | GoalStatus::Replanning
                        | GoalStatus::Paused
                        | GoalStatus::Cancelling
                        | GoalStatus::Failed
                )
                | (
                    GoalStatus::Verifying,
                    GoalStatus::Replanning
                        | GoalStatus::Blocked
                        | GoalStatus::Pausing
                        | GoalStatus::Cancelling
                        | GoalStatus::Failed
                )
                | (GoalStatus::Cancelling, GoalStatus::Cancelled | GoalStatus::Blocked)
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
        if from == GoalStatus::Cancelling && next == GoalStatus::Cancelled {
            if self.tasks.values().any(|task| {
                matches!(task.status(), TaskStatus::Running | TaskStatus::Verifying)
                    || task.has_unknown_side_effect()
            }) {
                return Err(invalid_goal_transition(
                    from,
                    next,
                    "active work or unresolved side effects remain",
                ));
            }
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
            && candidate
                .tasks
                .values()
                .any(|task| task.mandatory() && task.status() == TaskStatus::Blocked)
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
            return Err(OrchestratorError::UnsupportedSchema(self.schema_version as u64));
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
                "schema-2 Goal must have at least one host-owned completion criterion".to_owned(),
            ));
        }
        let mut criterion_ids = BTreeSet::new();
        for criterion in &self.completion_criteria {
            criterion.id.validate()?;
            if criterion.description.trim().is_empty() || !criterion.required {
                return Err(OrchestratorError::CorruptGoal(
                    "schema-2 completion criteria must be non-empty and required in V1".to_owned(),
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
                    "Goal final-verification history contains an invalid authority binding".to_owned(),
                ));
            }
        }
        let mut rejection_request_ids = BTreeSet::new();
        for rejection in &self.pre_execution_plan_rejections {
            if rejection.request_id.trim().is_empty()
                || rejection.request_id.chars().count() > 128
                || rejection.expected_goal_revision == 0
                || rejection.expected_goal_revision > self.revision
                || rejection.rejected_plan_revision == 0
                || rejection.rejected_plan_revision > self.plan_revision
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
                    "pre-execution plan rejection trigger Task is not bound to its rejected plan".to_owned(),
                ));
            }
        }
        for checkpoint in &self.checkpoints {
            checkpoint.validate()?;
            if checkpoint.goal_revision > self.revision || checkpoint.plan_revision > self.plan_revision {
                return Err(OrchestratorError::CorruptGoal(
                    "checkpoint refers to a future goal/plan revision".to_owned(),
                ));
            }
        }
        self.validate_dag()?;
        if self.status.is_terminal() != self.completed_at.is_some() {
            return Err(OrchestratorError::CorruptGoal(
                "terminal/completed_at invariant is inconsistent".to_owned(),
            ));
        }
        if self.status.is_terminal()
            && self.tasks.values().any(|task| {
                matches!(task.status(), TaskStatus::Running | TaskStatus::Verifying)
            })
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

    fn validate_legacy_schema1_terminal(&self) -> Result<(), OrchestratorError> {
        if !self.status.is_terminal() {
            return Err(OrchestratorError::SchemaUpgradeRequired(1));
        }
        if self.revision == 0 {
            return Err(OrchestratorError::CorruptGoal("legacy Goal revision must start at 1".to_owned()));
        }
        self.id.validate()?;
        config::validate_session_id(&self.session_id)
            .map_err(|_| OrchestratorError::CorruptGoal("invalid legacy session_id".to_owned()))?;
        if self.objective.trim().is_empty() || self.completed_at.is_none() {
            return Err(OrchestratorError::CorruptGoal("legacy terminal Goal shape is invalid".to_owned()));
        }
        self.validate_dag()?;
        Ok(())
    }

    pub(crate) fn validate_final_verification_contract(&self) -> Result<(), OrchestratorError> {
        if self.schema_version != GOAL_SCHEMA_VERSION {
            return Err(OrchestratorError::SchemaUpgradeRequired(self.schema_version as u64));
        }
        let spec = self.final_verification_spec.as_ref().ok_or_else(|| {
            OrchestratorError::InvalidDag("structured Goal final-verification contract is missing".to_owned())
        })?;
        if spec.plan_revision != self.plan_revision {
            return Err(OrchestratorError::InvalidDag(
                "Goal final-verification contract plan_revision is stale".to_owned(),
            ));
        }
        let criterion_ids = self.completion_criteria.iter().map(|criterion| criterion.id.clone()).collect::<BTreeSet<_>>();
        let mut bound = BTreeSet::new();
        for binding in &spec.criterion_bindings {
            binding.criterion_id.validate()?;
            if !criterion_ids.contains(&binding.criterion_id) {
                return Err(OrchestratorError::InvalidDag("Goal verification binding references an unknown criterion".to_owned()));
            }
            if !bound.insert(binding.criterion_id.clone()) {
                return Err(OrchestratorError::InvalidDag("Goal verification criterion binding is duplicated".to_owned()));
            }
            if binding.requirements.is_empty() {
                return Err(OrchestratorError::InvalidDag("required completion criterion has no structured proof requirement".to_owned()));
            }
            let mut seen_requirements = BTreeSet::new();
            for requirement in &binding.requirements {
                if !seen_requirements.insert(requirement.clone()) {
                    return Err(OrchestratorError::InvalidDag("Goal verification requirement is duplicated".to_owned()));
                }
                let task = self.tasks.get(requirement.task_id()).ok_or_else(|| {
                    OrchestratorError::InvalidDag("TaskVerified binding references a missing Task".to_owned())
                })?;
                if !task.mandatory() {
                    return Err(OrchestratorError::InvalidDag("TaskVerified binding references a non-mandatory Task".to_owned()));
                }
            }
        }
        for criterion in &self.completion_criteria {
            if criterion.required && !bound.contains(&criterion.id) {
                return Err(OrchestratorError::InvalidDag("required completion criterion is not structurally mapped".to_owned()));
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
            if task.created_plan_revision() == 0 || task.created_plan_revision() > self.plan_revision {
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
                .is_some_and(|dependency| dependency.status() == TaskStatus::Completed)
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
            .filter(|task| task.mandatory())
            .any(|task| task.status() != TaskStatus::Completed)
            || self.tasks.values().any(|task| {
                matches!(task.status(), TaskStatus::Running | TaskStatus::Verifying | TaskStatus::NeedsReplan)
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
        if self.tasks.values().filter(|task| task.mandatory()).any(|task| {
            task.verification_results().last().map(VerificationResult::outcome) != Some(VerificationOutcome::Passed)
        }) {
            return Err(invalid_goal_transition(from, to, "latest mandatory Task verification is not PASSED"));
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
            .filter(|task| task.mandatory())
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
            .filter(|task| task.mandatory())
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
        if self.latest_applicable_final_verification().map(GoalFinalVerificationRecord::outcome)
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
                .filter(|task| task.mandatory())
                .any(|task| !task.blockers().is_empty())
    }
}

fn record_task_identities_current(goal: &Goal, record: &GoalFinalVerificationRecord) -> bool {
    record.criterion_results.iter().flat_map(|criterion| criterion.observations.iter()).all(|observation| {
        match observation {
            GoalRequirementObservation::TaskVerified {
                task_id,
                task_status,
                verification_id,
                verification_outcome,
                ..
            } => {
                let Some(task) = goal.tasks.get(task_id) else { return false; };
                if task.status() != *task_status { return false; }
                match (verification_id, verification_outcome, task.verification_results().last()) {
                    (Some(expected_id), Some(expected_outcome), Some(current)) => {
                        current.id() == expected_id && current.outcome() == *expected_outcome
                    }
                    (None, None, None) => true,
                    _ => false,
                }
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
            OrchestratorError::InvalidDag("replanning cannot delete existing Task history".to_owned())
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

fn serialize_tasks<S>(
    tasks: &BTreeMap<TaskId, Task>,
    serializer: S,
) -> Result<S::Ok, S::Error>
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
        goal.transition_task(id, TaskStatus::Ready, safe, NOW).unwrap();
        goal.transition_task(id, TaskStatus::Running, safe, NOW).unwrap();
        goal.transition_task(id, TaskStatus::Verifying, safe, NOW).unwrap();
        goal.task_record_verification_result(
            id,
            VerificationResult::new(VerificationOutcome::Passed, vec![], NOW, NOW),
        )
        .unwrap();
        goal.transition_task(id, TaskStatus::Completed, safe, NOW).unwrap();
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
        goal.strengthen_task_dependencies(&b, vec![a.clone()]).unwrap();
        goal.strengthen_task_dependencies(&c, vec![a.clone()]).unwrap();
        goal.strengthen_task_dependencies(&d, vec![b, c]).unwrap();
        goal.validate_dag().unwrap();
    }

    #[test]
    fn self_dependency_missing_dependency_and_cycle_are_rejected() {
        let mut goal = goal();
        let a = add_task(&mut goal, true);
        let b = add_task(&mut goal, true);
        assert!(goal.strengthen_task_dependencies(&a, vec![a.clone()]).is_err());
        assert!(goal.strengthen_task_dependencies(&a, vec![TaskId::new()]).is_err());
        goal.strengthen_task_dependencies(&b, vec![a.clone()]).unwrap();
        assert!(goal.strengthen_task_dependencies(&a, vec![b]).is_err());
    }

    #[test]
    fn duplicate_dependency_edge_is_rejected() {
        let mut goal = goal();
        let a = add_task(&mut goal, true);
        let b = add_task(&mut goal, true);
        assert!(goal
            .strengthen_task_dependencies(&b, vec![a.clone(), a])
            .is_err());
    }

    #[test]
    fn mandatory_task_cannot_depend_on_optional_task() {
        let mut goal = goal();
        let optional = add_task(&mut goal, false);
        let mandatory = add_task(&mut goal, true);
        assert!(goal
            .strengthen_task_dependencies(&mandatory, vec![optional])
            .is_err());
    }

    #[test]
    fn ready_gating_follows_hard_dependency_completion() {
        let mut goal = goal();
        let a = add_task(&mut goal, true);
        let b = add_task(&mut goal, true);
        goal.strengthen_task_dependencies(&b, vec![a.clone()]).unwrap();
        assert!(goal
            .transition_task(&b, TaskStatus::Ready, TaskTransitionContext::default(), NOW)
            .is_err());
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
        goal.add_checkpoint(CheckpointReason::FinalVerification, NOW).unwrap();
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
        assert!(goal
            .task_add_evidence(
                &id,
                TaskEvidence::ReviewResult {
                    summary: "late rewrite".into(),
                    blocking_findings: 0,
                },
            )
            .is_err());
    }

    #[test]
    fn rejected_cycle_insertion_leaves_goal_exactly_unchanged() {
        let mut goal = goal();
        let a = add_task(&mut goal, true);
        let b = add_task(&mut goal, true);
        goal.strengthen_task_dependencies(&b, vec![a.clone()]).unwrap();
        let before = goal.clone();
        assert!(goal.strengthen_task_dependencies(&a, vec![b]).is_err());
        assert_eq!(goal, before);
    }

    #[test]
    fn rejected_missing_dependency_leaves_goal_exactly_unchanged() {
        let mut goal = goal();
        let task = add_task(&mut goal, true);
        let before = goal.clone();
        assert!(goal
            .strengthen_task_dependencies(&task, vec![TaskId::new()])
            .is_err());
        assert_eq!(goal, before);
    }

    #[test]
    fn rejected_dependency_removal_leaves_goal_exactly_unchanged() {
        let mut goal = goal();
        let dependency = add_task(&mut goal, true);
        let task = add_task(&mut goal, true);
        goal.strengthen_task_dependencies(&task, vec![dependency]).unwrap();
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
        assert!(goal
            .task_add_evidence(
                &task,
                TaskEvidence::ReviewResult {
                    summary: "late rewrite".into(),
                    blocking_findings: 0,
                },
            )
            .is_err());
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
        goal.add_checkpoint(CheckpointReason::FinalVerification, NOW).unwrap();
        assert!(goal.transition_to(GoalStatus::Completed, NOW).is_err());
    }
}
