use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent::AgentError;
use crate::config;
use crate::failure_class::FailureClass;
use crate::goal::{
    CheckpointReason, CompletionCriterionId, FailedTaskReplacementMutation, FailedTaskReplanPolicy,
    FailedTaskReplanTriggerKind, Goal, GoalId, GoalStatus, GoalVerificationRequirement,
    PreExecutionPlanReplanPolicy, ReplanMutation,
};
use crate::orchestrator_error::OrchestratorError;
use crate::planner::{self, PlannerError};
use crate::task::{
    Task, TaskId, TaskOperationKind, TaskScope, TaskStatus, VerificationResult, VerificationSpec,
    WorkerKind,
};
use crate::task_store::TaskStore;
use crate::worker_capability;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ReplannerRequest {
    goal_id: String,
    goal_revision: u64,
    plan_revision: u32,
    objective: String,
    title: Option<String>,
    constraints: Vec<String>,
    completion_criteria: Vec<ReplannerCriterion>,
    criterion_bindings: Vec<ReplannerCriterionBindingSnapshot>,
    cwd: PathBuf,
    tasks: Vec<ReplannerTaskSnapshot>,
    pre_execution_plan_rejections: Vec<ReplannerPreExecutionPlanRejectionSnapshot>,
    consecutive_pre_execution_plan_rejection_count: usize,
    goal_blockers: Vec<ReplannerBlocker>,
    eligible_needs_replan_task_ids: Vec<String>,
    /// Durable failed-task replacement authorities the host will accept a
    /// `replace_tasks` proposal against. Typed, not prose.
    failed_task_replan_requests: Vec<ReplannerFailedTaskReplanRequest>,
    /// Host-derived, deterministic Task-sizing risk. The model must size work
    /// against these numbers instead of guessing token budgets.
    sizing: ReplannerSizingProfile,
    /// Host-visible compact read-only output contract for decomposition work.
    readonly_output_contract: ReadonlyOutputContract,
    allowed_worker_kinds: Vec<WorkerKind>,
    allowed_operation_kinds: Vec<TaskOperationKind>,
    prohibited_operations: Vec<&'static str>,
}

/// A durable host authority to replace one structurally invalid Task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ReplannerFailedTaskReplanRequest {
    replan_request_id: String,
    trigger_task_id: String,
    trigger_plan_revision: u32,
    policy: FailedTaskReplanPolicy,
    trigger_kind: FailedTaskReplanTriggerKind,
    trigger_failure_class: FailureClass,
    trigger_side_effect_state: Option<crate::fallback::SideEffectState>,
    remaining_attempt_budget: Option<u32>,
    reason: String,
}

/// Deterministic, host-derived sizing risk. These are conservative structural
/// bounds, never token predictions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct TaskSizingProfile {
    pub(crate) independent_entity_count: usize,
    pub(crate) evidence_dimension_count: usize,
    pub(crate) evidence_shape_estimate: usize,
    pub(crate) max_single_readonly_task_records: usize,
    pub(crate) over_budget: bool,
    pub(crate) guidance: Vec<&'static str>,
}

impl TaskSizingProfile {
    /// Derive a sizing profile from bounded structural counts only. No token
    /// estimation and no model involvement: the same inputs always yield the
    /// same profile.
    pub(crate) fn derive(entity_count: usize, dimension_count: usize) -> Self {
        let evidence_shape_estimate = entity_count.saturating_mul(dimension_count);
        let over_budget = evidence_shape_estimate > planner::MAX_SINGLE_READONLY_TASK_RECORDS;
        let mut guidance = vec![
            "size one read-only Task per bounded evidence dimension, not per cross-product",
            "prefer compact structured evidence, artifact references, and short synthesis",
            "state explicit unknowns instead of repeating large source excerpts",
        ];
        if over_budget {
            guidance.push(
                "a single read-only Task covering the full cross-product is structurally too broad: split it",
            );
            guidance.push(
                "add a compact join/synthesis Task that depends on the bounded read-only Tasks",
            );
        }
        Self {
            independent_entity_count: entity_count,
            evidence_dimension_count: dimension_count,
            evidence_shape_estimate,
            max_single_readonly_task_records: planner::MAX_SINGLE_READONLY_TASK_RECORDS,
            over_budget,
            guidance,
        }
    }
}

type ReplannerSizingProfile = TaskSizingProfile;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ReadonlyOutputContract {
    max_evidence_items: usize,
    max_summary_bytes: usize,
    max_evidence_field_bytes: usize,
    max_evidence_total_bytes: usize,
    guidance: &'static str,
}

const REPLANNER_PRE_EXECUTION_REJECTION_HISTORY_LIMIT: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ReplannerPreExecutionPlanRejectionSnapshot {
    request_id: String,
    rejected_plan_revision: u32,
    trigger_task_id: String,
    reason: String,
    observed_goal_revision: u64,
    at: String,
    replan_policy: PreExecutionPlanReplanPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ReplannerCriterion {
    criterion_id: String,
    description: String,
    required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ReplannerCriterionBindingSnapshot {
    criterion_id: String,
    task_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ReplannerTaskSnapshot {
    task_id: String,
    title: String,
    objective: String,
    mandatory: bool,
    status: TaskStatus,
    dependencies: Vec<String>,
    worker: WorkerKind,
    scope: TaskScope,
    verification: Vec<VerificationSpec>,
    verification_results: Vec<VerificationResult>,
    evidence: Vec<crate::task::TaskEvidence>,
    blockers: Vec<ReplannerBlocker>,
    attempts: Vec<ReplannerAttemptSummary>,
    max_attempts: u32,
    created_plan_revision: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ReplannerAttemptSummary {
    attempt_id: String,
    worker: WorkerKind,
    outcome: Option<crate::task::AttemptOutcome>,
    /// Host-owned structured failure classification. Recovery policy reads
    /// this, never the human-readable report prose.
    failure_class: Option<FailureClass>,
    /// Effective classification including the legacy-report fallback, so the
    /// model never has to infer a class from prose.
    effective_failure_class: Option<FailureClass>,
    operation_id: Option<String>,
    scope_identity: Option<String>,
    side_effect_state: Option<crate::fallback::SideEffectState>,
    remaining_attempt_budget: Option<u32>,
    remaining_side_effect_budget: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct ReplannerBlocker {
    code: String,
    detail: String,
    mandatory: bool,
}

pub(crate) trait ReplannerBackend {
    fn propose_replan(&self, request: &ReplannerRequest) -> Result<Vec<u8>, ReplannerError>;
}

impl ReplannerRequest {
    pub(crate) fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }
}

#[cfg(test)]
pub(crate) fn replanner_request_for_model_backend_test(cwd: PathBuf) -> ReplannerRequest {
    ReplannerRequest {
        goal_id: "00000000-0000-4000-8000-000000000001".to_owned(),
        goal_revision: 9,
        plan_revision: 4,
        objective: "repair the plan monotonically".to_owned(),
        title: None,
        constraints: Vec::new(),
        completion_criteria: Vec::new(),
        criterion_bindings: Vec::new(),
        cwd,
        tasks: Vec::new(),
        pre_execution_plan_rejections: Vec::new(),
        consecutive_pre_execution_plan_rejection_count: 0,
        goal_blockers: Vec::new(),
        eligible_needs_replan_task_ids: Vec::new(),
        failed_task_replan_requests: Vec::new(),
        sizing: TaskSizingProfile::derive(0, 0),
        readonly_output_contract: ReadonlyOutputContract {
            max_evidence_items: planner::MAX_READONLY_EVIDENCE_ITEMS,
            max_summary_bytes: planner::MAX_READONLY_SUMMARY_BYTES,
            max_evidence_field_bytes: planner::MAX_READONLY_EVIDENCE_FIELD_BYTES,
            max_evidence_total_bytes: planner::MAX_READONLY_EVIDENCE_TOTAL_BYTES,
            guidance: planner::READONLY_OUTPUT_CONTRACT_GUIDANCE,
        },
        allowed_worker_kinds: Vec::new(),
        allowed_operation_kinds: Vec::new(),
        prohibited_operations: Vec::new(),
    }
}

pub(crate) struct ReadonlyReplanRecoveryAuthority {
    _private: (),
}

impl ReadonlyReplanRecoveryAuthority {
    fn for_replanner() -> Self {
        Self { _private: () }
    }
}

#[derive(Debug)]
pub(crate) enum ReplannerError {
    ReplannerUnavailable,
    Model(AgentError),
    ReplannerOutputInvalid(String),
    ReplannerSchemaViolation(String),
    RevisionConflict { expected: u64, actual: u64 },
    PlanConflict { expected: u32, actual: u32 },
    ReplanNotApplicable(String),
    ReplanAuthorityViolation(String),
    NoSafeReplan(String),
    Store(OrchestratorError),
}

impl fmt::Display for ReplannerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReplannerUnavailable => write!(f, "replanner backend is unavailable"),
            Self::Model(error) => write!(f, "replanner model invocation failed: {error}"),
            Self::ReplannerOutputInvalid(reason) => {
                write!(f, "replanner output is invalid: {reason}")
            }
            Self::ReplannerSchemaViolation(reason) => write!(
                f,
                "replanner proposal violates the Phase 7 schema: {reason}"
            ),
            Self::RevisionConflict { expected, actual } => write!(
                f,
                "replan conflict: proposal was based on Goal revision {expected}, current revision is {actual}"
            ),
            Self::PlanConflict { expected, actual } => write!(
                f,
                "replan conflict: proposal was based on plan revision {expected}, current plan revision is {actual}"
            ),
            Self::ReplanNotApplicable(reason) => write!(f, "replan is not applicable: {reason}"),
            Self::ReplanAuthorityViolation(reason) => {
                write!(f, "replan exceeds Goal/session authority: {reason}")
            }
            Self::NoSafeReplan(reason) => write!(f, "no safe replan is available: {reason}"),
            Self::Store(error) => write!(f, "durable Goal operation failed: {error}"),
        }
    }
}

impl std::error::Error for ReplannerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplanProposal {
    goal_id: String,
    base_goal_revision: u64,
    base_plan_revision: u32,
    summary: String,
    #[serde(default)]
    add_tasks: Vec<ReplanTaskProposal>,
    #[serde(default)]
    add_dependencies: Vec<DependencyAdditionProposal>,
    #[serde(default)]
    strengthen_verification: Vec<VerificationStrengtheningProposal>,
    #[serde(default)]
    strengthen_mandatory: Vec<MandatoryStrengtheningProposal>,
    #[serde(default)]
    strengthen_criterion_bindings: Vec<CriterionBindingStrengtheningProposal>,
    #[serde(default)]
    resolve_needs_replan: Vec<String>,
    #[serde(default)]
    pristine_plan_supersession: Option<PristinePlanSupersessionProposal>,
    #[serde(default)]
    replace_tasks: Vec<TaskReplacementProposal>,
}

/// A generic, host-validated failed-task replacement.
///
/// This is a *different durable concept* from `pristine_plan_supersession`,
/// which supersedes an entire pre-execution-rejected plan revision. Here the
/// model names one structurally invalid Task, the replacement closure it must be
/// replaced by, and the criteria that must follow the closure. The host
/// independently derives the downstream rewiring and every invariant check.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskReplacementProposal {
    replan_request_id: String,
    old_task_id: String,
    completion_closure_task_refs: Vec<TaskRefProposal>,
    criterion_rebindings: Vec<CriterionReplacementProposal>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PristinePlanSupersessionProposal {
    rejection_request_id: String,
    criterion_rebindings: Vec<CriterionReplacementProposal>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CriterionReplacementProposal {
    criterion_id: String,
    replacement_task_refs: Vec<TaskRefProposal>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplanTaskProposal {
    proposal_id: String,
    title: String,
    objective: String,
    mandatory: bool,
    worker: WorkerKind,
    #[serde(default)]
    dependencies: Vec<TaskRefProposal>,
    scope: planner::TaskScopeProposal,
    verification: Vec<VerificationSpec>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(
    tag = "ref_kind",
    rename_all = "SCREAMING_SNAKE_CASE",
    deny_unknown_fields
)]
enum TaskRefProposal {
    Existing { task_id: String },
    New { proposal_id: String },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DependencyAdditionProposal {
    task: TaskRefProposal,
    dependency: TaskRefProposal,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerificationStrengtheningProposal {
    task_id: String,
    add: Vec<VerificationSpec>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MandatoryStrengtheningProposal {
    task_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CriterionBindingStrengtheningProposal {
    criterion_id: String,
    add_task_refs: Vec<TaskRefProposal>,
}

#[derive(Clone, Debug)]
struct ValidatedReplan {
    new_tasks: Vec<ValidatedNewTask>,
    add_dependencies: Vec<(ValidatedTaskRef, ValidatedTaskRef)>,
    add_verification: Vec<(TaskId, Vec<VerificationSpec>)>,
    strengthen_mandatory: Vec<TaskId>,
    add_criterion_requirements: Vec<(CompletionCriterionId, Vec<ValidatedTaskRef>)>,
    resolve_needs_replan: Vec<TaskId>,
    reconcile_exhausted_readonly: Vec<TaskId>,
    pristine_plan_supersession: Option<ValidatedPristinePlanSupersession>,
    task_replacements: Vec<ValidatedTaskReplacement>,
}

#[derive(Clone, Debug)]
struct ValidatedTaskReplacement {
    replan_request_id: String,
    old_task_id: TaskId,
    committed_plan_revision: u32,
    canonical_proposal_digest: String,
    completion_closure_refs: Vec<ValidatedTaskRef>,
    criterion_rebindings: Vec<(CompletionCriterionId, Vec<ValidatedTaskRef>)>,
}

#[derive(Clone, Debug)]
struct ValidatedPristinePlanSupersession {
    rejection_request_id: String,
    rejected_plan_revision: u32,
    canonical_proposal_digest: String,
    affected_task_ids: Vec<TaskId>,
    criterion_rebindings: Vec<(CompletionCriterionId, Vec<ValidatedTaskRef>)>,
}

#[derive(Clone, Debug)]
struct ValidatedNewTask {
    proposal_id: String,
    title: String,
    objective: String,
    mandatory: bool,
    worker: WorkerKind,
    dependencies: Vec<ValidatedTaskRef>,
    scope: TaskScope,
    verification: Vec<VerificationSpec>,
    max_attempts: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ValidatedTaskRef {
    Existing(TaskId),
    New(String),
}

pub(crate) fn replanner_request_for_goal(
    goal: &Goal,
    session: &config::Session,
) -> Result<ReplannerRequest, ReplannerError> {
    ensure_replan_eligible(goal)?;
    planner::validate_session_goal_binding(goal, session).map_err(map_planner_validation_error)?;
    let goal_root =
        planner::execution_root_for_goal(goal, session).map_err(map_planner_validation_error)?;

    let eligible_needs_replan_task_ids = goal
        .tasks()
        .iter()
        .filter_map(|(id, task)| {
            (task.status() == TaskStatus::NeedsReplan && !task.has_unknown_side_effect())
                .then_some(id.as_str().to_owned())
        })
        .collect::<Vec<_>>();
    if eligible_needs_replan_task_ids.is_empty() {
        return Err(ReplannerError::NoSafeReplan(
            "all NEEDS_REPLAN candidates have unresolved UNKNOWN side effects".to_owned(),
        ));
    }

    let tasks = goal
        .tasks()
        .values()
        .map(|task| ReplannerTaskSnapshot {
            task_id: task.id().as_str().to_owned(),
            title: task.title().to_owned(),
            objective: task.objective().to_owned(),
            mandatory: task.mandatory(),
            status: task.status(),
            dependencies: task
                .dependencies()
                .iter()
                .map(|dependency| dependency.task_id().as_str().to_owned())
                .collect(),
            worker: task.worker(),
            scope: task.scope().clone(),
            verification: task.verification_specs().to_vec(),
            verification_results: task.verification_results().to_vec(),
            evidence: task.evidence().to_vec(),
            blockers: task
                .blockers()
                .iter()
                .map(|blocker| ReplannerBlocker {
                    code: blocker.code().to_owned(),
                    detail: blocker.detail().to_owned(),
                    mandatory: blocker.mandatory(),
                })
                .collect(),
            attempts: task
                .attempts()
                .iter()
                .map(|attempt| ReplannerAttemptSummary {
                    attempt_id: attempt.id().as_str().to_owned(),
                    worker: attempt.worker(),
                    outcome: attempt.outcome(),
                    failure_class: attempt.failure_class(),
                    effective_failure_class: Some(attempt.effective_failure_class()),
                    operation_id: attempt.operation_id().map(str::to_owned),
                    scope_identity: attempt.scope_identity().map(str::to_owned),
                    side_effect_state: attempt.side_effect_state(),
                    remaining_attempt_budget: attempt.remaining_attempt_budget(),
                    remaining_side_effect_budget: attempt.remaining_side_effect_budget(),
                })
                .collect(),
            max_attempts: task.max_attempts(),
            created_plan_revision: task.created_plan_revision(),
        })
        .collect();

    let mut pre_execution_plan_rejections = goal
        .pre_execution_plan_rejections()
        .iter()
        .rev()
        .take(REPLANNER_PRE_EXECUTION_REJECTION_HISTORY_LIMIT)
        .map(|rejection| ReplannerPreExecutionPlanRejectionSnapshot {
            request_id: rejection.request_id().to_owned(),
            rejected_plan_revision: rejection.rejected_plan_revision(),
            trigger_task_id: rejection.trigger_task_id().as_str().to_owned(),
            reason: rejection.reason().to_owned(),
            observed_goal_revision: rejection.observed_goal_revision(),
            at: rejection.rejected_at().to_owned(),
            replan_policy: rejection.replan_policy(),
        })
        .collect::<Vec<_>>();
    pre_execution_plan_rejections.reverse();
    let latest_committed_replan_revision = goal
        .checkpoints()
        .iter()
        .rev()
        .find(|checkpoint| checkpoint.reason() == CheckpointReason::ReplanCommitted)
        .map(|checkpoint| checkpoint.plan_revision());
    let consecutive_pre_execution_plan_rejection_count = goal
        .pre_execution_plan_rejections()
        .iter()
        .filter(|rejection| {
            latest_committed_replan_revision
                .is_none_or(|revision| rejection.rejected_plan_revision() >= revision)
        })
        .count();

    Ok(ReplannerRequest {
        goal_id: goal.id().as_str().to_owned(),
        goal_revision: goal.revision(),
        plan_revision: goal.plan_revision(),
        objective: goal.objective().to_owned(),
        title: goal.title().map(str::to_owned),
        constraints: goal.constraints().to_vec(),
        completion_criteria: goal
            .completion_criteria()
            .iter()
            .map(|criterion| ReplannerCriterion {
                criterion_id: criterion.id().as_str().to_owned(),
                description: criterion.description().to_owned(),
                required: criterion.required(),
            })
            .collect(),
        criterion_bindings: goal
            .final_verification_spec()
            .map(|spec| {
                spec.criterion_bindings()
                    .iter()
                    .map(|binding| ReplannerCriterionBindingSnapshot {
                        criterion_id: binding.criterion_id().as_str().to_owned(),
                        task_ids: binding
                            .requirements()
                            .iter()
                            .map(|requirement| requirement.task_id().as_str().to_owned())
                            .collect(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        cwd: goal_root,
        tasks,
        pre_execution_plan_rejections,
        consecutive_pre_execution_plan_rejection_count,
        goal_blockers: goal
            .blockers()
            .iter()
            .map(|blocker| ReplannerBlocker {
                code: blocker.code().to_owned(),
                detail: blocker.detail().to_owned(),
                mandatory: blocker.mandatory(),
            })
            .collect(),
        eligible_needs_replan_task_ids,
        failed_task_replan_requests: goal
            .failed_task_replan_requests()
            .iter()
            .filter(|request| {
                goal.find_failed_task_replacement(request.request_id())
                    .is_none()
            })
            .map(|request| ReplannerFailedTaskReplanRequest {
                replan_request_id: request.request_id().to_owned(),
                trigger_task_id: request.trigger_task_id().as_str().to_owned(),
                trigger_plan_revision: request.trigger_plan_revision(),
                policy: request.policy(),
                trigger_kind: request.trigger_kind(),
                trigger_failure_class: request.trigger_failure_class(),
                trigger_side_effect_state: request.trigger_side_effect_state(),
                remaining_attempt_budget: goal
                    .tasks()
                    .get(request.trigger_task_id())
                    .map(|task| task.semantic_attempts_remaining()),
                reason: request.reason().to_owned(),
            })
            .collect(),
        sizing: planner::task_sizing_profile_for_goal(goal),
        readonly_output_contract: ReadonlyOutputContract {
            max_evidence_items: planner::MAX_READONLY_EVIDENCE_ITEMS,
            max_summary_bytes: planner::MAX_READONLY_SUMMARY_BYTES,
            max_evidence_field_bytes: planner::MAX_READONLY_EVIDENCE_FIELD_BYTES,
            max_evidence_total_bytes: planner::MAX_READONLY_EVIDENCE_TOTAL_BYTES,
            guidance: planner::READONLY_OUTPUT_CONTRACT_GUIDANCE,
        },
        allowed_worker_kinds: worker_capability::production_plannable_worker_kinds().to_vec(),
        allowed_operation_kinds: vec![
            TaskOperationKind::ReadOnly,
            TaskOperationKind::LocalMutation,
            TaskOperationKind::HostNativeApproved,
        ],
        prohibited_operations: vec![
            "task execution during replanning",
            "Task or Goal completion",
            "attempt/evidence/history rewriting",
            "budget replenishment",
            "scope or cwd authority widening",
            "UNKNOWN side-effect erasure or replay",
            "approval or sandbox bypass",
            "automatic commit, push, merge, release, or publication",
            "scheduler or worker invocation",
        ],
    })
}

#[allow(
    dead_code,
    reason = "Frozen replanner entrypoint is retained for staged Goal Orchestrator integration."
)]
pub(crate) fn replan_goal<B: ReplannerBackend>(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    replanner: &B,
) -> Result<Goal, ReplannerError> {
    let goal = store
        .load_goal(&session.id, goal_id)
        .map_err(map_store_error)?;
    let request = replanner_request_for_goal(&goal, session)?;
    let output = replanner.propose_replan(&request)?;
    materialize_replan_output(
        store,
        session,
        goal_id,
        goal.revision(),
        goal.plan_revision(),
        &output,
    )
}

pub(crate) fn materialize_replan_output(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    expected_goal_revision: u64,
    expected_plan_revision: u32,
    output: &[u8],
) -> Result<Goal, ReplannerError> {
    if output.len() > planner::MAX_PLAN_PROPOSAL_BYTES {
        return Err(ReplannerError::ReplannerSchemaViolation(
            "proposal exceeds the 256 KiB host limit".to_owned(),
        ));
    }
    let current = store
        .load_goal(&session.id, goal_id)
        .map_err(map_store_error)?;
    let raw: serde_json::Value = serde_json::from_slice(output).map_err(|error| {
        ReplannerError::ReplannerOutputInvalid(format!(
            "malformed JSON at line {}, column {}",
            error.line(),
            error.column()
        ))
    })?;
    let proposal: ReplanProposal = serde_json::from_value(raw.clone()).map_err(|_| {
        ReplannerError::ReplannerSchemaViolation(
            "proposal shape does not match the strict Phase 7 schema".to_owned(),
        )
    })?;
    if let Some(supersession) = proposal.pristine_plan_supersession.as_ref() {
        let digest = canonical_proposal_digest(&raw);
        if let Some(record) =
            current.find_pristine_plan_supersession(&supersession.rejection_request_id)
        {
            if record.canonical_proposal_digest() == digest {
                return Ok(current);
            }
            return Err(ReplannerError::ReplanAuthorityViolation(
                "replayed supersession rejection ID has a different effective proposal digest"
                    .to_owned(),
            ));
        }
    }
    // Replaying the same accepted replacement identity with the same effective
    // proposal returns the already-materialized result. The same identity with
    // a different effective proposal is rejected.
    for replacement in &proposal.replace_tasks {
        if let Some(record) = current.find_failed_task_replacement(&replacement.replan_request_id) {
            if record.canonical_proposal_digest() == canonical_proposal_digest(&raw) {
                return Ok(current);
            }
            return Err(ReplannerError::ReplanAuthorityViolation(
                "replayed failed-task replacement ID has a different effective proposal digest"
                    .to_owned(),
            ));
        }
    }
    if current.revision() != expected_goal_revision {
        return Err(ReplannerError::RevisionConflict {
            expected: expected_goal_revision,
            actual: current.revision(),
        });
    }
    if current.plan_revision() != expected_plan_revision {
        return Err(ReplannerError::PlanConflict {
            expected: expected_plan_revision,
            actual: current.plan_revision(),
        });
    }
    ensure_replan_eligible(&current)?;
    planner::validate_session_goal_binding(&current, session)
        .map_err(map_planner_validation_error)?;
    let goal_root = planner::execution_root_for_goal(&current, session)
        .map_err(map_planner_validation_error)?;
    let validated = parse_and_validate_proposal(output, &current, &goal_root)?;

    store
        .mutate_goal_snapshot(
            &session.id,
            goal_id,
            expected_goal_revision,
            move |goal, now| {
                if goal.plan_revision() != expected_plan_revision {
                    return Err(OrchestratorError::InvalidDag(
                        "stale replan plan revision".to_owned(),
                    ));
                }
                materialize_validated_replan(goal, validated, now)?;
                validate_writer_serialization(goal)?;
                Ok(())
            },
        )
        .map_err(map_store_error)
}

fn canonical_proposal_digest(value: &serde_json::Value) -> String {
    let mut canonical = canonicalize_json(value);
    if let serde_json::Value::Object(object) = &mut canonical {
        object.remove("summary");
    }
    let bytes = serde_json::to_vec(&canonical).expect("canonical JSON is serializable");
    format!("{:x}", Sha256::digest(bytes))
}

fn canonicalize_json(value: &serde_json::Value) -> serde_json::Value {
    canonicalize_json_with_key(value, None)
}

fn canonicalize_json_with_key(value: &serde_json::Value, key: Option<&str>) -> serde_json::Value {
    match value {
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .map(|(child_key, value)| {
                    (
                        child_key.clone(),
                        canonicalize_json_with_key(value, Some(child_key)),
                    )
                })
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            let mut values = values
                .iter()
                .map(|value| canonicalize_json_with_key(value, None))
                .collect::<Vec<_>>();
            if matches!(
                key,
                Some(
                    "add_tasks"
                        | "add_dependencies"
                        | "strengthen_verification"
                        | "strengthen_mandatory"
                        | "strengthen_criterion_bindings"
                        | "resolve_needs_replan"
                        | "dependencies"
                        | "verification"
                        | "allowed_paths"
                        | "forbidden_paths"
                        | "criterion_rebindings"
                        | "replacement_task_refs"
                )
            ) {
                values.sort_by_key(|value| serde_json::to_string(value).expect("canonical JSON"));
            }
            serde_json::Value::Array(values)
        }
        other => other.clone(),
    }
}

fn ensure_replan_eligible(goal: &Goal) -> Result<(), ReplannerError> {
    if goal.is_terminal() {
        return Err(ReplannerError::ReplanNotApplicable(
            "terminal Goal history is immutable".to_owned(),
        ));
    }
    if !matches!(goal.status(), GoalStatus::Running | GoalStatus::Replanning) {
        return Err(ReplannerError::ReplanNotApplicable(
            "Phase 7 replanning requires a RUNNING or REPLANNING Goal".to_owned(),
        ));
    }
    if !goal.has_task_status(TaskStatus::NeedsReplan) {
        return Err(ReplannerError::ReplanNotApplicable(
            "Phase 7 requires at least one NEEDS_REPLAN Task; BLOCKED alone is not replannable"
                .to_owned(),
        ));
    }
    Ok(())
}

fn parse_and_validate_proposal(
    output: &[u8],
    goal: &Goal,
    goal_root: &std::path::Path,
) -> Result<ValidatedReplan, ReplannerError> {
    let raw: serde_json::Value = serde_json::from_slice(output).map_err(|error| {
        ReplannerError::ReplannerOutputInvalid(format!(
            "malformed JSON at line {}, column {}",
            error.line(),
            error.column()
        ))
    })?;
    let proposal: ReplanProposal = serde_json::from_value(raw.clone()).map_err(|_| {
        ReplannerError::ReplannerSchemaViolation(
            "proposal shape does not match the strict Phase 7 schema".to_owned(),
        )
    })?;
    let proposal_digest = canonical_proposal_digest(&raw);

    if proposal.goal_id != goal.id().as_str() {
        return Err(ReplannerError::ReplannerSchemaViolation(
            "proposal goal_id does not match the authoritative Goal".to_owned(),
        ));
    }
    if proposal.base_goal_revision != goal.revision() {
        return Err(ReplannerError::RevisionConflict {
            expected: proposal.base_goal_revision,
            actual: goal.revision(),
        });
    }
    if proposal.base_plan_revision != goal.plan_revision() {
        return Err(ReplannerError::PlanConflict {
            expected: proposal.base_plan_revision,
            actual: goal.plan_revision(),
        });
    }
    planner::validate_text(
        &proposal.summary,
        planner::MAX_PLAN_SUMMARY_BYTES,
        "Replan summary",
    )
    .map_err(map_planner_validation_error)?;

    let pristine_plan_supersession = proposal.pristine_plan_supersession.clone();
    let has_monotonic_changes = !proposal.add_dependencies.is_empty()
        || !proposal.strengthen_verification.is_empty()
        || !proposal.strengthen_mandatory.is_empty()
        || !proposal.strengthen_criterion_bindings.is_empty()
        || !proposal.resolve_needs_replan.is_empty();
    // Captured before the validation loops consume the proposal fields.
    let replacement_has_other_changes = has_monotonic_changes
        || !proposal.add_dependencies.is_empty()
        || !proposal.strengthen_verification.is_empty()
        || !proposal.strengthen_mandatory.is_empty()
        || !proposal.strengthen_criterion_bindings.is_empty()
        || !proposal.resolve_needs_replan.is_empty();

    let change_count = proposal.add_tasks.len()
        + proposal.add_dependencies.len()
        + proposal.strengthen_verification.len()
        + proposal.strengthen_mandatory.len()
        + proposal.strengthen_criterion_bindings.len()
        + proposal.resolve_needs_replan.len()
        + usize::from(pristine_plan_supersession.is_some())
        + proposal.replace_tasks.len();
    if change_count == 0 {
        return Err(ReplannerError::ReplannerSchemaViolation(
            "replan must contain at least one monotonic change".to_owned(),
        ));
    }
    if goal.tasks().len().saturating_add(proposal.add_tasks.len()) > planner::MAX_PLAN_TASKS {
        return Err(ReplannerError::ReplannerSchemaViolation(
            "candidate plan exceeds the 128 Task host limit".to_owned(),
        ));
    }

    let existing_dependency_edges = goal
        .tasks()
        .values()
        .map(|task| task.dependencies().len())
        .sum::<usize>();
    let proposed_dependency_edges = proposal
        .add_tasks
        .iter()
        .map(|task| task.dependencies.len())
        .sum::<usize>()
        .saturating_add(proposal.add_dependencies.len());
    if existing_dependency_edges.saturating_add(proposed_dependency_edges)
        > planner::MAX_PLAN_DEPENDENCY_EDGES
    {
        return Err(ReplannerError::ReplannerSchemaViolation(
            "candidate plan exceeds the 1024 dependency-edge limit".to_owned(),
        ));
    }

    let existing_scope_paths = goal
        .tasks()
        .values()
        .map(|task| task.scope().allowed_paths().len() + task.scope().forbidden_paths().len())
        .sum::<usize>();
    let new_scope_paths = proposal
        .add_tasks
        .iter()
        .map(|task| task.scope.allowed_paths.len() + task.scope.forbidden_paths.len())
        .sum::<usize>();
    if existing_scope_paths.saturating_add(new_scope_paths) > planner::MAX_SCOPE_PATHS_TOTAL {
        return Err(ReplannerError::ReplannerSchemaViolation(
            "candidate plan exceeds the 1024 scope-path limit".to_owned(),
        ));
    }

    let existing_verification = goal
        .tasks()
        .values()
        .map(|task| task.verification_specs().len())
        .sum::<usize>();
    let new_verification = proposal
        .add_tasks
        .iter()
        .map(|task| task.verification.len())
        .sum::<usize>()
        .saturating_add(
            proposal
                .strengthen_verification
                .iter()
                .map(|entry| entry.add.len())
                .sum::<usize>(),
        );
    if existing_verification.saturating_add(new_verification) > planner::MAX_VERIFICATION_TOTAL {
        return Err(ReplannerError::ReplannerSchemaViolation(
            "candidate plan exceeds the 1024 verification-entry limit".to_owned(),
        ));
    }

    let mut local_ids = BTreeSet::new();
    for task in &proposal.add_tasks {
        planner::validate_proposal_id(&task.proposal_id).map_err(map_planner_validation_error)?;
        if !local_ids.insert(task.proposal_id.clone()) {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "duplicate proposal-local Task identity".to_owned(),
            ));
        }
        if goal
            .tasks()
            .keys()
            .any(|existing| existing.as_str() == task.proposal_id)
        {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "proposal-local Task identity collides with an existing Task ID".to_owned(),
            ));
        }
    }
    let mut validated_new_tasks = Vec::with_capacity(proposal.add_tasks.len());
    for task in proposal.add_tasks {
        planner::validate_text(&task.title, planner::MAX_PLAN_TITLE_BYTES, "Task title")
            .map_err(map_planner_validation_error)?;
        planner::validate_text(
            &task.objective,
            planner::MAX_PLAN_OBJECTIVE_BYTES,
            "Task objective",
        )
        .map_err(map_planner_validation_error)?;
        if !worker_capability::is_production_plannable(task.worker) {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "new Task worker is not production-plannable".to_owned(),
            ));
        }
        if task.dependencies.len() > planner::MAX_DEPENDENCIES_PER_TASK {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "a new Task has too many dependencies".to_owned(),
            ));
        }
        let mut seen_dependencies = BTreeSet::new();
        let mut dependencies = Vec::with_capacity(task.dependencies.len());
        for dependency in task.dependencies {
            let validated = validate_task_ref(dependency, goal, &local_ids)?;
            if validated == ValidatedTaskRef::New(task.proposal_id.clone()) {
                return Err(ReplannerError::ReplannerSchemaViolation(
                    "new Task cannot depend on itself".to_owned(),
                ));
            }
            if !seen_dependencies.insert(validated.clone()) {
                return Err(ReplannerError::ReplannerSchemaViolation(
                    "duplicate dependency edge".to_owned(),
                ));
            }
            dependencies.push(validated);
        }
        let scope = planner::validate_and_normalize_scope(&task.scope, task.worker, goal_root)
            .map_err(map_planner_validation_error)?;
        if task.verification.is_empty()
            || task.verification.len() > planner::MAX_VERIFICATION_PER_TASK
        {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "each new Task requires 1..=32 verification specifications".to_owned(),
            ));
        }
        let mut verification = Vec::with_capacity(task.verification.len());
        for spec in task.verification {
            let normalized = planner::validate_and_normalize_verification(spec, goal_root)
                .map_err(map_planner_validation_error)?;
            if verification.contains(&normalized) {
                return Err(ReplannerError::ReplannerSchemaViolation(
                    "new Task contains duplicate verification requirements".to_owned(),
                ));
            }
            verification.push(normalized);
        }
        let max_attempts = match scope.operation_kind() {
            TaskOperationKind::ReadOnly => planner::READ_ONLY_MAX_ATTEMPTS,
            TaskOperationKind::LocalMutation | TaskOperationKind::HostNativeApproved => {
                planner::EFFECTFUL_MAX_ATTEMPTS
            }
        };
        validated_new_tasks.push(ValidatedNewTask {
            proposal_id: task.proposal_id,
            title: task.title,
            objective: task.objective,
            mandatory: task.mandatory,
            worker: task.worker,
            dependencies,
            scope,
            verification,
            max_attempts,
        });
    }

    let new_by_id = validated_new_tasks
        .iter()
        .map(|task| (task.proposal_id.clone(), task))
        .collect::<BTreeMap<_, _>>();
    for task in &validated_new_tasks {
        for dependency in &task.dependencies {
            if task.mandatory && !task_ref_is_mandatory(dependency, goal, &new_by_id)? {
                return Err(ReplannerError::ReplannerSchemaViolation(
                    "mandatory Task cannot depend on an optional Task".to_owned(),
                ));
            }
        }
    }

    let mut validated_add_dependencies = Vec::with_capacity(proposal.add_dependencies.len());
    let mut seen_edges = BTreeSet::new();
    let mut added_count_by_target = BTreeMap::<ValidatedTaskRef, usize>::new();
    for edge in proposal.add_dependencies {
        let target = validate_task_ref(edge.task, goal, &local_ids)?;
        let dependency = validate_task_ref(edge.dependency, goal, &local_ids)?;
        if target == dependency {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "Task cannot depend on itself".to_owned(),
            ));
        }
        if !seen_edges.insert((target.clone(), dependency.clone())) {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "duplicate dependency addition".to_owned(),
            ));
        }
        if dependency_already_present(&target, &dependency, goal, &new_by_id)? {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "replan cannot re-add an existing dependency edge".to_owned(),
            ));
        }
        if let ValidatedTaskRef::Existing(task_id) = &target {
            let task = &goal.tasks()[task_id];
            if !matches!(
                task.status(),
                TaskStatus::Pending | TaskStatus::Ready | TaskStatus::NeedsReplan
            ) {
                return Err(ReplannerError::ReplanAuthorityViolation(
                    "dependencies cannot change for active or terminal Tasks".to_owned(),
                ));
            }
        }
        *added_count_by_target.entry(target.clone()).or_default() += 1;
        validated_add_dependencies.push((target, dependency));
    }
    for (target, added) in &added_count_by_target {
        let base = match target {
            ValidatedTaskRef::Existing(id) => goal.tasks()[id].dependencies().len(),
            ValidatedTaskRef::New(id) => new_by_id[id].dependencies.len(),
        };
        if base.saturating_add(*added) > planner::MAX_DEPENDENCIES_PER_TASK {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "a Task exceeds the 64 dependency limit after replan".to_owned(),
            ));
        }
    }

    let mut validated_verification = Vec::with_capacity(proposal.strengthen_verification.len());
    let mut seen_verification_targets = BTreeSet::new();
    for strengthening in proposal.strengthen_verification {
        let task_id = parse_existing_task_id(&strengthening.task_id, goal)?;
        if !seen_verification_targets.insert(task_id.clone()) {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "verification strengthening must be grouped once per Task".to_owned(),
            ));
        }
        let task = &goal.tasks()[&task_id];
        if !matches!(
            task.status(),
            TaskStatus::Pending | TaskStatus::Ready | TaskStatus::NeedsReplan
        ) {
            return Err(ReplannerError::ReplanAuthorityViolation(
                "verification cannot change for active or terminal Tasks".to_owned(),
            ));
        }
        if strengthening.add.is_empty()
            || task
                .verification_specs()
                .len()
                .saturating_add(strengthening.add.len())
                > planner::MAX_VERIFICATION_PER_TASK
        {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "verification strengthening must add 1..=32 bounded requirements".to_owned(),
            ));
        }
        let mut additions = Vec::with_capacity(strengthening.add.len());
        for spec in strengthening.add {
            let normalized = planner::validate_and_normalize_verification(spec, goal_root)
                .map_err(map_planner_validation_error)?;
            if task.verification_specs().contains(&normalized) || additions.contains(&normalized) {
                return Err(ReplannerError::ReplannerSchemaViolation(
                    "verification strengthening cannot duplicate an existing requirement"
                        .to_owned(),
                ));
            }
            additions.push(normalized);
        }
        validated_verification.push((task_id, additions));
    }

    let mut validated_mandatory = Vec::with_capacity(proposal.strengthen_mandatory.len());
    let mut seen_mandatory = BTreeSet::new();
    for strengthening in proposal.strengthen_mandatory {
        let task_id = parse_existing_task_id(&strengthening.task_id, goal)?;
        if !seen_mandatory.insert(task_id.clone()) {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "duplicate mandatory strengthening".to_owned(),
            ));
        }
        let task = &goal.tasks()[&task_id];
        if task.is_terminal() {
            return Err(ReplannerError::ReplanAuthorityViolation(
                "terminal Task mandatory authority is immutable".to_owned(),
            ));
        }
        if task.mandatory() {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "strengthen_mandatory may target only optional Tasks".to_owned(),
            ));
        }
        validated_mandatory.push(task_id);
    }

    let criterion_by_id = goal
        .completion_criteria()
        .iter()
        .map(|criterion| (criterion.id().as_str(), criterion))
        .collect::<BTreeMap<_, _>>();
    let current_spec = goal.final_verification_spec().ok_or_else(|| {
        ReplannerError::ReplanAuthorityViolation(
            "replan requires an existing Goal final-verification contract".to_owned(),
        )
    })?;
    let mut seen_criterion_strengthenings = BTreeSet::new();
    let mut validated_criterion_requirements =
        Vec::with_capacity(proposal.strengthen_criterion_bindings.len());
    for strengthening in proposal.strengthen_criterion_bindings {
        let criterion = criterion_by_id
            .get(strengthening.criterion_id.as_str())
            .ok_or_else(|| {
                ReplannerError::ReplannerSchemaViolation(
                    "criterion strengthening references an unknown CompletionCriterionId"
                        .to_owned(),
                )
            })?;
        if !seen_criterion_strengthenings.insert(strengthening.criterion_id.clone())
            || strengthening.add_task_refs.is_empty()
        {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "criterion strengthening must be non-empty and grouped once per criterion"
                    .to_owned(),
            ));
        }
        let current_binding = current_spec
            .criterion_bindings()
            .iter()
            .find(|binding| binding.criterion_id() == criterion.id())
            .ok_or_else(|| {
                ReplannerError::ReplanAuthorityViolation(
                    "replan cannot repair missing initial criterion coverage by replacement"
                        .to_owned(),
                )
            })?;
        let current_task_ids = current_binding
            .requirements()
            .iter()
            .map(|requirement| requirement.task_id().clone())
            .collect::<BTreeSet<_>>();
        let mut additions = Vec::new();
        let mut seen_refs = BTreeSet::new();
        for reference in strengthening.add_task_refs {
            let validated_ref = validate_task_ref(reference, goal, &local_ids)?;
            if !seen_refs.insert(validated_ref.clone()) {
                return Err(ReplannerError::ReplannerSchemaViolation(
                    "criterion strengthening contains duplicate Task reference".to_owned(),
                ));
            }
            match &validated_ref {
                ValidatedTaskRef::Existing(task_id) => {
                    if current_task_ids.contains(task_id) {
                        return Err(ReplannerError::ReplannerSchemaViolation("criterion strengthening cannot duplicate an existing TaskVerified proof".to_owned()));
                    }
                    let task = &goal.tasks()[task_id];
                    if !task.mandatory() || task.verification_specs().is_empty() {
                        return Err(ReplannerError::ReplanAuthorityViolation("TaskVerified strengthening requires a mandatory Task with mechanical Task verification".to_owned()));
                    }
                }
                ValidatedTaskRef::New(proposal_id) => {
                    let task = new_by_id.get(proposal_id).ok_or_else(|| {
                        ReplannerError::ReplannerSchemaViolation(
                            "criterion strengthening new Task disappeared".to_owned(),
                        )
                    })?;
                    if !task.mandatory || task.verification.is_empty() {
                        return Err(ReplannerError::ReplanAuthorityViolation("TaskVerified strengthening requires a mandatory new Task with mechanical Task verification".to_owned()));
                    }
                }
            }
            additions.push(validated_ref);
        }
        validated_criterion_requirements.push((criterion.id().clone(), additions));
    }

    let dependency_targets = validated_add_dependencies
        .iter()
        .map(|(target, _)| target.clone())
        .collect::<BTreeSet<_>>();
    let mut validated_resolution = Vec::with_capacity(proposal.resolve_needs_replan.len());
    let mut reconcile_exhausted_readonly = Vec::new();
    let mut seen_resolution = BTreeSet::new();
    for task_id in proposal.resolve_needs_replan {
        let task_id = parse_existing_task_id(&task_id, goal)?;
        if !seen_resolution.insert(task_id.clone()) {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "duplicate NEEDS_REPLAN resolution".to_owned(),
            ));
        }
        let task = &goal.tasks()[&task_id];
        if task.status() != TaskStatus::NeedsReplan {
            return Err(ReplannerError::ReplanAuthorityViolation(
                "only NEEDS_REPLAN Tasks may be resolved".to_owned(),
            ));
        }
        if task.has_unknown_side_effect() {
            return Err(ReplannerError::NoSafeReplan(
                "UNKNOWN side-effect state must remain blocked for reconciliation".to_owned(),
            ));
        }
        if !dependency_targets.contains(&ValidatedTaskRef::Existing(task_id.clone())) {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "resolving NEEDS_REPLAN requires a committed new hard dependency".to_owned(),
            ));
        }
        if task.semantic_attempts_remaining() == 0 {
            if !task.can_reconcile_exhausted_readonly_replan() {
                return Err(ReplannerError::NoSafeReplan(
                    "NEEDS_REPLAN Task exhausted semantic attempts and is not eligible for bounded non-mutating readonly replan reconciliation".to_owned(),
                ));
            }
            reconcile_exhausted_readonly.push(task_id.clone());
        }
        validated_resolution.push(task_id);
    }

    validate_readonly_reassessment_barrier(
        goal,
        &validated_new_tasks,
        &validated_add_dependencies,
        &validated_resolution,
    )?;

    let pristine_plan_supersession = if let Some(supersession) = pristine_plan_supersession {
        if has_monotonic_changes || !proposal.replace_tasks.is_empty() {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "pristine-plan supersession cannot be mixed with ordinary monotonic replan changes or failed-task replacement"
                    .to_owned(),
            ));
        }
        Some(validate_pristine_plan_supersession(
            supersession,
            goal,
            &validated_new_tasks,
            &new_by_id,
            &validated_add_dependencies,
            &validated_resolution,
            proposal_digest.clone(),
        )?)
    } else {
        None
    };

    let task_replacements = validate_task_replacements(
        proposal.replace_tasks.clone(),
        goal,
        &validated_new_tasks,
        &new_by_id,
        replacement_has_other_changes || pristine_plan_supersession.is_some(),
        proposal_digest,
    )?;

    Ok(ValidatedReplan {
        new_tasks: validated_new_tasks,
        add_dependencies: validated_add_dependencies,
        add_verification: validated_verification,
        strengthen_mandatory: validated_mandatory,
        add_criterion_requirements: validated_criterion_requirements,
        resolve_needs_replan: validated_resolution,
        reconcile_exhausted_readonly,
        pristine_plan_supersession,
        task_replacements,
    })
}

fn validate_pristine_plan_supersession(
    proposal: PristinePlanSupersessionProposal,
    goal: &Goal,
    new_tasks: &[ValidatedNewTask],
    new_by_id: &BTreeMap<String, &ValidatedNewTask>,
    added_dependencies: &[(ValidatedTaskRef, ValidatedTaskRef)],
    resolved_task_ids: &[TaskId],
    digest: String,
) -> Result<ValidatedPristinePlanSupersession, ReplannerError> {
    if !added_dependencies.is_empty() || !resolved_task_ids.is_empty() {
        return Err(ReplannerError::ReplannerSchemaViolation(
            "pristine-plan supersession cannot add ordinary dependency or resolution mutations"
                .to_owned(),
        ));
    }
    let rejection = goal
        .find_pre_execution_plan_rejection(&proposal.rejection_request_id)
        .ok_or_else(|| {
            ReplannerError::ReplanAuthorityViolation(
                "pristine-plan supersession requires an existing pre-execution rejection authority"
                    .to_owned(),
            )
        })?;
    if rejection.rejected_plan_revision() != goal.plan_revision() {
        return Err(ReplannerError::ReplanNotApplicable(
            "the rejection does not target the current plan revision".to_owned(),
        ));
    }
    let affected_task_ids = goal
        .tasks()
        .iter()
        .filter_map(|(id, task)| {
            (task.created_plan_revision() == goal.plan_revision()).then_some(id.clone())
        })
        .collect::<Vec<_>>();
    if affected_task_ids.is_empty()
        || !affected_task_ids
            .iter()
            .any(|id| id == rejection.trigger_task_id())
    {
        return Err(ReplannerError::ReplanAuthorityViolation(
            "host-derived rejected plan set is empty or lacks the exact rejection trigger"
                .to_owned(),
        ));
    }
    for task_id in &affected_task_ids {
        let task = &goal.tasks()[task_id];
        if !task.attempts().is_empty()
            || task.evidence_count() != 0
            || !task.verification_results().is_empty()
            || !task.blockers().is_empty()
            || task.has_unknown_side_effect()
            || (task.status() == TaskStatus::NeedsReplan && task_id != rejection.trigger_task_id())
            || !matches!(
                task.status(),
                TaskStatus::Pending | TaskStatus::Ready | TaskStatus::NeedsReplan
            )
        {
            return Err(ReplannerError::ReplanAuthorityViolation(
                "affected rejected-plan Tasks are not strictly pristine".to_owned(),
            ));
        }
    }
    let affected = affected_task_ids.iter().cloned().collect::<BTreeSet<_>>();
    let current_spec = goal.final_verification_spec().ok_or_else(|| {
        ReplannerError::ReplanAuthorityViolation(
            "supersession requires a structured final-verification contract".to_owned(),
        )
    })?;
    let mut rebindings = BTreeMap::new();
    for binding in current_spec.criterion_bindings() {
        if binding
            .requirements()
            .iter()
            .any(|requirement| affected.contains(requirement.task_id()))
        {
            let entry = proposal
                .criterion_rebindings
                .iter()
                .find(|entry| entry.criterion_id == binding.criterion_id().as_str())
                .ok_or_else(|| {
                    ReplannerError::ReplanAuthorityViolation(
                        "every affected criterion requires an explicit replacement binding"
                            .to_owned(),
                    )
                })?;
            let criterion_id = CompletionCriterionId::parse(&entry.criterion_id).map_err(|_| {
                ReplannerError::ReplannerSchemaViolation("criterion ID is malformed".to_owned())
            })?;
            if rebindings
                .insert(criterion_id.clone(), entry.replacement_task_refs.clone())
                .is_some()
            {
                return Err(ReplannerError::ReplannerSchemaViolation(
                    "criterion replacement binding is duplicated".to_owned(),
                ));
            }
        }
    }
    if rebindings.len() != proposal.criterion_rebindings.len() {
        return Err(ReplannerError::ReplannerSchemaViolation(
            "criterion replacement bindings must cover exactly the affected criteria".to_owned(),
        ));
    }
    let mut replacement_ids = BTreeSet::new();
    let mut validated_rebindings = Vec::new();
    for (criterion_id, refs) in rebindings {
        let affected_count = current_spec
            .criterion_bindings()
            .iter()
            .find(|binding| binding.criterion_id() == &criterion_id)
            .unwrap()
            .requirements()
            .iter()
            .filter(|requirement| affected.contains(requirement.task_id()))
            .count();
        if refs.len() < affected_count || refs.is_empty() {
            return Err(ReplannerError::ReplanAuthorityViolation(
                "criterion replacement cardinality is lower than the affected proof coverage"
                    .to_owned(),
            ));
        }
        let mut validated_refs = Vec::new();
        for reference in refs {
            let validated = validate_task_ref(
                reference,
                goal,
                &new_tasks
                    .iter()
                    .map(|task| task.proposal_id.clone())
                    .collect::<BTreeSet<_>>(),
            )?;
            let ValidatedTaskRef::New(proposal_id) = &validated else {
                return Err(ReplannerError::ReplanAuthorityViolation(
                    "criterion replacement references must be new proposal Tasks".to_owned(),
                ));
            };
            let task = new_by_id.get(proposal_id).ok_or_else(|| {
                ReplannerError::ReplannerSchemaViolation(
                    "criterion replacement Task is missing".to_owned(),
                )
            })?;
            if !task.mandatory
                || task.verification.is_empty()
                || !replacement_ids.insert(proposal_id.clone())
            {
                return Err(ReplannerError::ReplanAuthorityViolation("criterion replacement requires distinct new mandatory mechanically verified Tasks".to_owned()));
            }
            validated_refs.push(validated);
        }
        validated_rebindings.push((criterion_id, validated_refs));
    }
    if replacement_ids.len() != new_tasks.len() {
        return Err(ReplannerError::ReplanAuthorityViolation(
            "every replacement Task must be bound to a required criterion exactly once".to_owned(),
        ));
    }
    let readonly_refs = new_tasks
        .iter()
        .filter(|task| {
            task.worker == WorkerKind::CodexReadonly
                && task.scope.operation_kind() == TaskOperationKind::ReadOnly
                && task.scope.replay_safety() == crate::task::ReplaySafety::SafeReadOnly
        })
        .map(|task| ValidatedTaskRef::New(task.proposal_id.clone()))
        .collect::<BTreeSet<_>>();
    if rejection.replan_policy() == PreExecutionPlanReplanPolicy::RequireReadonlyReassessment
        && (readonly_refs.is_empty()
            || new_tasks
                .iter()
                .filter(|task| task.scope.operation_kind() != TaskOperationKind::ReadOnly)
                .any(|task| {
                    !task
                        .dependencies
                        .iter()
                        .any(|dependency| readonly_refs.contains(dependency))
                }))
    {
        return Err(ReplannerError::ReplanAuthorityViolation("REQUIRE_READONLY_REASSESSMENT requires replacement mutation Tasks to depend on a new safe READ_ONLY reassessment".to_owned()));
    }
    if new_tasks.iter().flat_map(|task| task.dependencies.iter()).any(|dependency| matches!(dependency, ValidatedTaskRef::Existing(id) if affected.contains(id))) {
        return Err(ReplannerError::ReplanAuthorityViolation("replacement Tasks cannot depend on superseded rejected-plan Tasks".to_owned()));
    }
    Ok(ValidatedPristinePlanSupersession {
        rejection_request_id: proposal.rejection_request_id,
        rejected_plan_revision: goal.plan_revision(),
        canonical_proposal_digest: digest,
        affected_task_ids,
        criterion_rebindings: validated_rebindings,
    })
}

/// Independently validate a proposed generic failed-task replacement.
///
/// The model's proposal is advisory. This function re-derives every invariant
/// from durable host state: the replacement authority, the trigger's
/// eligibility and structured failure class, side-effect safety, the
/// replacement closure's shape, downstream rewiring feasibility, criterion
/// coverage, decomposition materiality, and host limits.
#[expect(
    clippy::too_many_arguments,
    reason = "Replacement validation keeps every host authority channel explicit."
)]
fn validate_task_replacements(
    proposals: Vec<TaskReplacementProposal>,
    goal: &Goal,
    new_tasks: &[ValidatedNewTask],
    new_by_id: &BTreeMap<String, &ValidatedNewTask>,
    mixed_with_other_changes: bool,
    digest: String,
) -> Result<Vec<ValidatedTaskReplacement>, ReplannerError> {
    if proposals.is_empty() {
        return Ok(Vec::new());
    }
    if mixed_with_other_changes {
        return Err(ReplannerError::ReplannerSchemaViolation(
            "failed-task replacement cannot be mixed with ordinary monotonic replan changes"
                .to_owned(),
        ));
    }
    if new_tasks.is_empty() {
        return Err(ReplannerError::ReplannerSchemaViolation(
            "failed-task replacement requires new replacement Tasks in add_tasks".to_owned(),
        ));
    }
    let local_ids = new_tasks
        .iter()
        .map(|task| task.proposal_id.clone())
        .collect::<BTreeSet<_>>();
    let next_plan_revision = goal.plan_revision().saturating_add(1);

    let mut seen_requests = BTreeSet::new();
    let mut replaced_ids = BTreeSet::new();
    let mut validated = Vec::with_capacity(proposals.len());
    for proposal in proposals {
        if !seen_requests.insert(proposal.replan_request_id.clone()) {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "each failed-task replacement must use a distinct replan_request_id".to_owned(),
            ));
        }
        let request = goal
            .find_failed_task_replan_request(&proposal.replan_request_id)
            .ok_or_else(|| {
                ReplannerError::ReplanAuthorityViolation(
                    "failed-task replacement requires an existing durable replan request authority"
                        .to_owned(),
                )
            })?;
        if goal
            .find_failed_task_replacement(&proposal.replan_request_id)
            .is_some()
        {
            return Err(ReplannerError::ReplanAuthorityViolation(
                "failed-task replacement replan request was already consumed".to_owned(),
            ));
        }
        if request.trigger_plan_revision() != goal.plan_revision() {
            return Err(ReplannerError::ReplanNotApplicable(
                "the failed-task replan request does not target the current plan revision"
                    .to_owned(),
            ));
        }

        // 1. The old Task exists and is host-eligible for replacement.
        let old_task_id = parse_existing_task_id(&proposal.old_task_id, goal)?;
        if request.trigger_task_id() != &old_task_id {
            return Err(ReplannerError::ReplanAuthorityViolation(
                "failed-task replacement must target the replan request's exact trigger Task"
                    .to_owned(),
            ));
        }
        if !replaced_ids.insert(old_task_id.clone()) {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "a Task may only be superseded once per replan proposal".to_owned(),
            ));
        }
        let old_task = &goal.tasks()[&old_task_id];
        if !matches!(
            old_task.status(),
            TaskStatus::Pending
                | TaskStatus::Ready
                | TaskStatus::Retryable
                | TaskStatus::Blocked
                | TaskStatus::NeedsReplan
        ) {
            return Err(ReplannerError::ReplanAuthorityViolation(
                "only a non-terminal, non-active Task may be superseded by replacement".to_owned(),
            ));
        }
        if old_task.has_unknown_side_effect() {
            return Err(ReplannerError::NoSafeReplan(
                "UNKNOWN side-effect state must be reconciled before replacement".to_owned(),
            ));
        }

        // 2. The trigger kind and structured failure class are host-derived and
        //    must both admit replacement. Authority, platform-safety, and
        //    unclassified failures can never be repaired by decomposition.
        let failure_class = match request.trigger_kind() {
            FailedTaskReplanTriggerKind::PostAttemptFailure => {
                let attempt = old_task.latest_attempt().ok_or_else(|| {
                    ReplannerError::ReplanAuthorityViolation(
                        "post-attempt replacement requires a consumed durable attempt".to_owned(),
                    )
                })?;
                let effective = attempt.effective_failure_class();
                if effective != request.trigger_failure_class() {
                    return Err(ReplannerError::ReplanAuthorityViolation(
                        "durable failure evidence no longer matches the recorded replan request"
                            .to_owned(),
                    ));
                }
                if !effective.allows_task_replacement() {
                    return Err(ReplannerError::NoSafeReplan(format!(
                        "structured failure class {effective:?} does not admit task replacement"
                    )));
                }
                effective
            }
            FailedTaskReplanTriggerKind::PreExecutionRejection => {
                let rejection_id = request.authority_request_id().ok_or_else(|| {
                    ReplannerError::ReplanAuthorityViolation(
                        "pre-execution replacement requires a rejection authority".to_owned(),
                    )
                })?;
                let rejection = goal
                    .find_pre_execution_plan_rejection(rejection_id)
                    .ok_or_else(|| {
                        ReplannerError::ReplanAuthorityViolation(
                            "pre-execution replacement references a missing rejection".to_owned(),
                        )
                    })?;
                if rejection.trigger_task_id() != &old_task_id
                    || !old_task.attempts().is_empty()
                    || old_task.evidence_count() != 0
                    || !old_task.verification_results().is_empty()
                    || !old_task.blockers().is_empty()
                {
                    return Err(ReplannerError::ReplanAuthorityViolation(
                        "pre-execution replacement requires a pristine, exactly-named trigger Task"
                            .to_owned(),
                    ));
                }
                request.trigger_failure_class()
            }
        };

        // 3. The replacement closure must not widen the superseded Task's
        //    authority. Scope containment is checked per replacement cone
        //    below, once each replacement owns its Tasks.

        // 4. The completion closure must name distinct, new, mandatory,
        //    mechanically verified Tasks, and must be exactly the maximal nodes
        //    of the replacement sub-DAG so dependency semantics stay explicit.
        if proposal.completion_closure_task_refs.is_empty() {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "replacement completion closure must name at least one Task".to_owned(),
            ));
        }
        let mut closure_refs = Vec::new();
        for reference in &proposal.completion_closure_task_refs {
            let reference = reference.clone();
            let validated = validate_task_ref(reference, goal, &local_ids)?;
            let ValidatedTaskRef::New(proposal_id) = &validated else {
                return Err(ReplannerError::ReplanAuthorityViolation(
                    "replacement completion closure must reference new proposal Tasks".to_owned(),
                ));
            };
            // `validate_task_ref` already guarantees the reference is a
            // proposal-local id, and proposal-local ids are rejected if they
            // collide with an existing durable Task ID, so the closure can
            // never name the replaced Task.
            let task = new_by_id.get(proposal_id).ok_or_else(|| {
                ReplannerError::ReplannerSchemaViolation(
                    "replacement completion closure references an unknown Task".to_owned(),
                )
            })?;
            if !task.mandatory || task.verification.is_empty() {
                return Err(ReplannerError::ReplanAuthorityViolation(
                    "replacement completion closure requires mandatory mechanically verified Tasks"
                        .to_owned(),
                ));
            }
            if closure_refs.contains(&validated) {
                return Err(ReplannerError::ReplannerSchemaViolation(
                    "replacement completion closure contains a duplicate Task".to_owned(),
                ));
            }
            closure_refs.push(validated);
        }
        let closure_ids = closure_refs
            .iter()
            .filter_map(|reference| match reference {
                ValidatedTaskRef::New(id) => Some(id.clone()),
                ValidatedTaskRef::Existing(_) => None,
            })
            .collect::<BTreeSet<_>>();
        let replaced_refs = replaced_ids
            .iter()
            .map(|id| ValidatedTaskRef::Existing(id.clone()))
            .collect::<BTreeSet<_>>();
        for task in new_tasks {
            if task
                .dependencies
                .iter()
                .any(|dependency| replaced_refs.contains(dependency))
            {
                return Err(ReplannerError::ReplanAuthorityViolation(
                    "replacement Tasks cannot depend on a superseded Task".to_owned(),
                ));
            }
        }

        // Budget-aware decomposition: a size/budget failure may only be
        //    repaired by materially smaller work units, never by prepending
        //    another broad reassessment Task.
        let requires_decomposition = failure_class.requires_decomposition()
            || request.policy() == FailedTaskReplanPolicy::RequireDecomposition;
        if requires_decomposition
            && (new_tasks.len() < planner::MIN_DECOMPOSED_REPLACEMENT_TASKS
                || closure_refs.len() >= new_tasks.len())
        {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "a size or budget failure requires a materially decomposed replacement closure with a bounded join Task"
                    .to_owned(),
            ));
        }
        for task in new_tasks {
            if task.objective.len() > planner::MAX_REPLACEMENT_OBJECTIVE_BYTES {
                return Err(ReplannerError::ReplannerSchemaViolation(
                    "replacement Task objective exceeds the compact replacement bound".to_owned(),
                ));
            }
        }

        // 5. Criterion rebinding must cover exactly the criteria that required
        //    the replaced Task. A superseded Task can never satisfy
        //    TASK_VERIFIED, so leaving the requirement would orphan it.
        let current_spec = goal.final_verification_spec().ok_or_else(|| {
            ReplannerError::ReplanAuthorityViolation(
                "failed-task replacement requires a Goal final-verification contract".to_owned(),
            )
        })?;
        let mut requested = proposal
            .criterion_rebindings
            .iter()
            .map(|entry| {
                (
                    entry.criterion_id.clone(),
                    entry.replacement_task_refs.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        if requested.len() != proposal.criterion_rebindings.len() {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "criterion replacement bindings must be unique per criterion".to_owned(),
            ));
        }
        let mut validated_rebindings = Vec::new();
        for binding in current_spec.criterion_bindings() {
            let replaced_count = binding
                .requirements()
                .iter()
                .filter(|requirement| requirement.task_id() == &old_task_id)
                .count();
            let replacements = if replaced_count > 0 {
                let mut entries = requested
                    .remove(binding.criterion_id().as_str())
                    .ok_or_else(|| {
                        ReplannerError::ReplanAuthorityViolation(
                            "every criterion bound to the replaced Task requires a replacement"
                                .to_owned(),
                        )
                    })?;
                // A criterion may require several Tasks superseded by the same
                // transaction. Their rebindings union so the coverage check
                // sees every replaced proof, and unrelated requirements are
                // never dropped.
                entries.sort();
                entries.dedup();
                if entries.is_empty() {
                    return Err(ReplannerError::ReplanAuthorityViolation(
                        "criterion replacement coverage is lower than the replaced proof coverage"
                            .to_owned(),
                    ));
                }
                let mut refs = Vec::new();
                for reference in entries {
                    let reference = reference.clone();
                    let validated = validate_task_ref(reference, goal, &local_ids)?;
                    let ValidatedTaskRef::New(proposal_id) = &validated else {
                        return Err(ReplannerError::ReplanAuthorityViolation(
                            "criterion replacement must reference new proposal Tasks".to_owned(),
                        ));
                    };
                    if !closure_ids.contains(proposal_id) {
                        return Err(ReplannerError::ReplanAuthorityViolation(
                            "criterion replacement must name the declared completion closure"
                                .to_owned(),
                        ));
                    }
                    if refs.contains(&validated) {
                        return Err(ReplannerError::ReplannerSchemaViolation(
                            "criterion replacement contains a duplicate Task".to_owned(),
                        ));
                    }
                    refs.push(validated);
                }
                refs
            } else {
                if requested.contains_key(binding.criterion_id().as_str()) {
                    return Err(ReplannerError::ReplanAuthorityViolation(
                        "criterion rebindings may only replace requirements bound to the replaced Task"
                            .to_owned(),
                    ));
                }
                Vec::new()
            };
            if replaced_count > 0 {
                validated_rebindings.push((binding.criterion_id().clone(), replacements));
            }
        }
        if !requested.is_empty() {
            return Err(ReplannerError::ReplannerSchemaViolation(
                "criterion rebindings must cover exactly the criteria bound to the replaced Task"
                    .to_owned(),
            ));
        }

        // A criterion requiring two Tasks that this transaction supersedes must
        // still receive proof. The union above already covers both, so the only
        // remaining requirement is that the replacement really is the declared
        // completion authority.

        validated.push(ValidatedTaskReplacement {
            replan_request_id: proposal.replan_request_id,
            old_task_id,
            committed_plan_revision: next_plan_revision,
            canonical_proposal_digest: digest.clone(),
            completion_closure_refs: closure_refs,
            criterion_rebindings: validated_rebindings,
        });
    }

    // Closure shape is a property of the whole transaction, not of a single
    // entry: the declared closure of each replacement must be exactly the
    // maximal nodes of its own replacement cone, and the union of the cones
    // must cover every new Task so nothing is left dangling or unowned.
    let mut covered = BTreeSet::new();
    for replacement in &validated {
        let cone = ancestor_cone(new_tasks, &replacement.completion_closure_refs);
        // Maximality: nothing else inside the cone may be upstream of a
        // declared closure member, otherwise that member is not the closure.
        for member in &replacement.completion_closure_refs {
            for other in &cone {
                if other != member && dependency_graph_reaches_between(new_tasks, other, member) {
                    return Err(ReplannerError::ReplanAuthorityViolation(
                        "the declared completion closure must be the maximal nodes of its replacement cone"
                            .to_owned(),
                    ));
                }
            }
        }
        // Authority containment: the model may narrow the superseded Task's
        // scope but never widen it. Allowed paths may only shrink, forbidden
        // paths may only grow, and the operation kind and replay safety are
        // fixed. Checked per cone so a transaction replacing disjointly scoped
        // Tasks is not over-constrained by the unrelated one.
        let old_scope = &goal.tasks()[&replacement.old_task_id].scope();
        for reference in &cone {
            let ValidatedTaskRef::New(proposal_id) = reference else {
                continue;
            };
            let task = new_tasks
                .iter()
                .find(|task| task.proposal_id == *proposal_id)
                .expect("cone members are proposal-local");
            let scope = &task.scope;
            if scope.operation_kind() != old_scope.operation_kind()
                || scope.replay_safety() != old_scope.replay_safety()
            {
                return Err(ReplannerError::ReplanAuthorityViolation(
                    "replacement Tasks may not change the superseded Task's operation kind or replay safety"
                        .to_owned(),
                ));
            }
            if !is_path_subset(scope.allowed_paths(), old_scope.allowed_paths()) {
                return Err(ReplannerError::ReplanAuthorityViolation(
                    "replacement Task allowed paths may only narrow within the superseded Task's scope"
                        .to_owned(),
                ));
            }
            if !is_path_superset(scope.forbidden_paths(), old_scope.forbidden_paths()) {
                return Err(ReplannerError::ReplanAuthorityViolation(
                    "replacement Task forbidden paths may only grow within the superseded Task's scope"
                        .to_owned(),
                ));
            }
        }
        covered.extend(cone);
    }
    for task in new_tasks {
        let reference = ValidatedTaskRef::New(task.proposal_id.clone());
        if !covered.contains(&reference) {
            return Err(ReplannerError::ReplanAuthorityViolation(
                "every replacement Task must be upstream of a declared completion closure"
                    .to_owned(),
            ));
        }
    }

    // A replacement Task may not simultaneously serve as a downstream
    // dependent of another replaced Task in the same transaction: that would
    // make the rewiring order-dependent. Every pair is compared, including the
    // final replacement, which a `skip` window would otherwise omit.
    for left in &validated {
        for right in &validated {
            if std::ptr::eq(left, right) {
                continue;
            }
            if left
                .completion_closure_refs
                .iter()
                .any(|reference| {
                    let ValidatedTaskRef::New(id) = reference else {
                        return false;
                    };
                    new_tasks
                        .iter()
                        .find(|task| task.proposal_id == *id)
                        .is_some_and(|task| {
                            task.dependencies.iter().any(|dependency| {
                                matches!(dependency, ValidatedTaskRef::Existing(dep) if *dep == right.old_task_id)
                            })
                        })
                })
            {
                return Err(ReplannerError::ReplanAuthorityViolation(
                    "a replacement closure cannot also be a replacement trigger".to_owned(),
                ));
            }
        }
    }
    Ok(validated)
}

/// Every new Task that the given completion closure transitively depends on,
/// including the closure members themselves.
fn ancestor_cone(
    new_tasks: &[ValidatedNewTask],
    closure: &[ValidatedTaskRef],
) -> BTreeSet<ValidatedTaskRef> {
    let mut cone = closure.iter().cloned().collect::<BTreeSet<_>>();
    loop {
        let mut extended = cone.clone();
        for task in new_tasks {
            let reference = ValidatedTaskRef::New(task.proposal_id.clone());
            if !cone.contains(&reference) {
                continue;
            }
            for dependency in &task.dependencies {
                if let ValidatedTaskRef::New(_) = dependency {
                    extended.insert(dependency.clone());
                }
            }
        }
        if extended == cone {
            return cone;
        }
        cone = extended;
    }
}

/// Whether every path in `narrower` is contained in `wider`.
fn is_path_subset(narrower: &[PathBuf], wider: &[PathBuf]) -> bool {
    narrower.iter().all(|path| {
        wider
            .iter()
            .any(|candidate| path == candidate || path.starts_with(candidate))
    })
}

/// Whether every path in `smaller` is covered by some path in `larger`.
fn is_path_superset(larger: &[PathBuf], smaller: &[PathBuf]) -> bool {
    is_path_subset(smaller, larger)
}

/// Whether `target` is reachable from `start` by walking hard dependencies
/// (i.e. `start` transitively depends on `target`).
fn dependency_graph_reaches_between(
    new_tasks: &[ValidatedNewTask],
    start: &ValidatedTaskRef,
    target: &ValidatedTaskRef,
) -> bool {
    let graph = new_tasks
        .iter()
        .map(|task| {
            (
                ValidatedTaskRef::New(task.proposal_id.clone()),
                task.dependencies.iter().cloned().collect::<BTreeSet<_>>(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    dependency_graph_reaches(&graph, start, |node| node == target)
}

fn validate_readonly_reassessment_barrier(
    goal: &Goal,
    new_tasks: &[ValidatedNewTask],
    added_dependencies: &[(ValidatedTaskRef, ValidatedTaskRef)],
    resolved_task_ids: &[TaskId],
) -> Result<(), ReplannerError> {
    let mut dependency_graph = BTreeMap::<ValidatedTaskRef, BTreeSet<ValidatedTaskRef>>::new();
    for (task_id, task) in goal.tasks() {
        dependency_graph.insert(
            ValidatedTaskRef::Existing(task_id.clone()),
            task.dependencies()
                .iter()
                .map(|dependency| ValidatedTaskRef::Existing(dependency.task_id().clone()))
                .collect(),
        );
    }
    for task in new_tasks {
        dependency_graph.insert(
            ValidatedTaskRef::New(task.proposal_id.clone()),
            task.dependencies.iter().cloned().collect(),
        );
    }
    for (target, dependency) in added_dependencies {
        dependency_graph
            .entry(target.clone())
            .or_default()
            .insert(dependency.clone());
    }

    let new_readonly_refs = new_tasks
        .iter()
        .filter(|task| {
            task.worker == WorkerKind::CodexReadonly
                && task.scope.operation_kind() == TaskOperationKind::ReadOnly
                && task.scope.replay_safety() == crate::task::ReplaySafety::SafeReadOnly
        })
        .map(|task| ValidatedTaskRef::New(task.proposal_id.clone()))
        .collect::<Vec<_>>();

    for rejection in goal.pre_execution_plan_rejections() {
        if rejection.replan_policy() != PreExecutionPlanReplanPolicy::RequireReadonlyReassessment
            || !resolved_task_ids.contains(rejection.trigger_task_id())
        {
            continue;
        }
        let trigger_ref = ValidatedTaskRef::Existing(rejection.trigger_task_id().clone());
        let current_or_proposed_completed_readonly = dependency_graph_reaches(
            &dependency_graph,
            &trigger_ref,
            |candidate| match candidate {
                ValidatedTaskRef::Existing(task_id) => {
                    goal.tasks().get(task_id).is_some_and(|task| {
                        task.created_plan_revision() > rejection.rejected_plan_revision()
                            && task.worker() == WorkerKind::CodexReadonly
                            && task.scope().operation_kind() == TaskOperationKind::ReadOnly
                            && task.status() == TaskStatus::Completed
                            && task.blockers().is_empty()
                            && task.evidence_count() > 0
                            && !task.has_unknown_side_effect()
                            && task.verification_results().last().is_some_and(|result| {
                                result.outcome() == crate::task::VerificationOutcome::Passed
                            })
                    })
                }
                ValidatedTaskRef::New(_) => false,
            },
        );
        if current_or_proposed_completed_readonly {
            continue;
        }

        let qualifying_new_readonly = new_readonly_refs
            .iter()
            .filter(|candidate| {
                dependency_graph_reaches(&dependency_graph, &trigger_ref, |node| node == *candidate)
            })
            .cloned()
            .collect::<Vec<_>>();
        if qualifying_new_readonly.is_empty() {
            return Err(ReplannerError::ReplanAuthorityViolation(
                "REQUIRE_READONLY_REASSESSMENT requires a new READ_ONLY prerequisite in the rejected trigger dependency chain".to_owned(),
            ));
        }

        for task in new_tasks
            .iter()
            .filter(|task| task.scope.operation_kind() != TaskOperationKind::ReadOnly)
        {
            let task_ref = ValidatedTaskRef::New(task.proposal_id.clone());
            if !qualifying_new_readonly.iter().any(|readonly| {
                dependency_graph_reaches(&dependency_graph, &task_ref, |node| node == readonly)
            }) {
                return Err(ReplannerError::ReplanAuthorityViolation(
                    "REQUIRE_READONLY_REASSESSMENT requires every new mutation Task to depend on the new READ_ONLY prerequisite".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn dependency_graph_reaches<F>(
    graph: &BTreeMap<ValidatedTaskRef, BTreeSet<ValidatedTaskRef>>,
    start: &ValidatedTaskRef,
    mut predicate: F,
) -> bool
where
    F: FnMut(&ValidatedTaskRef) -> bool,
{
    let mut stack = graph
        .get(start)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect::<Vec<_>>();
    let mut visited = BTreeSet::new();
    while let Some(node) = stack.pop() {
        if !visited.insert(node.clone()) {
            continue;
        }
        if predicate(&node) {
            return true;
        }
        if let Some(dependencies) = graph.get(&node) {
            stack.extend(dependencies.iter().cloned());
        }
    }
    false
}

fn validate_task_ref(
    reference: TaskRefProposal,
    goal: &Goal,
    local_ids: &BTreeSet<String>,
) -> Result<ValidatedTaskRef, ReplannerError> {
    match reference {
        TaskRefProposal::Existing { task_id } => Ok(ValidatedTaskRef::Existing(
            parse_existing_task_id(&task_id, goal)?,
        )),
        TaskRefProposal::New { proposal_id } => {
            planner::validate_proposal_id(&proposal_id).map_err(map_planner_validation_error)?;
            if !local_ids.contains(&proposal_id) {
                return Err(ReplannerError::ReplannerSchemaViolation(
                    "dependency references an unknown proposal-local Task".to_owned(),
                ));
            }
            Ok(ValidatedTaskRef::New(proposal_id))
        }
    }
}

fn parse_existing_task_id(value: &str, goal: &Goal) -> Result<TaskId, ReplannerError> {
    let task_id = TaskId::parse(value).map_err(|_| {
        ReplannerError::ReplannerSchemaViolation("existing Task reference is malformed".to_owned())
    })?;
    if !goal.tasks().contains_key(&task_id) {
        return Err(ReplannerError::ReplannerSchemaViolation(
            "existing Task reference is missing".to_owned(),
        ));
    }
    Ok(task_id)
}

fn task_ref_is_mandatory(
    reference: &ValidatedTaskRef,
    goal: &Goal,
    new_by_id: &BTreeMap<String, &ValidatedNewTask>,
) -> Result<bool, ReplannerError> {
    match reference {
        ValidatedTaskRef::Existing(id) => Ok(goal.tasks()[id].mandatory()),
        ValidatedTaskRef::New(id) => {
            new_by_id.get(id).map(|task| task.mandatory).ok_or_else(|| {
                ReplannerError::ReplannerSchemaViolation(
                    "proposal-local dependency disappeared during validation".to_owned(),
                )
            })
        }
    }
}

fn dependency_already_present(
    target: &ValidatedTaskRef,
    dependency: &ValidatedTaskRef,
    goal: &Goal,
    new_by_id: &BTreeMap<String, &ValidatedNewTask>,
) -> Result<bool, ReplannerError> {
    match target {
        ValidatedTaskRef::Existing(id) => match dependency {
            ValidatedTaskRef::Existing(dependency_id) => Ok(goal.tasks()[id]
                .dependencies()
                .iter()
                .any(|candidate| candidate.task_id() == dependency_id)),
            ValidatedTaskRef::New(_) => Ok(false),
        },
        ValidatedTaskRef::New(id) => new_by_id
            .get(id)
            .map(|task| task.dependencies.contains(dependency))
            .ok_or_else(|| {
                ReplannerError::ReplannerSchemaViolation(
                    "proposal-local Task disappeared during validation".to_owned(),
                )
            }),
    }
}
fn materialize_validated_replan(
    goal: &mut Goal,
    validated: ValidatedReplan,
    now: &str,
) -> Result<(), OrchestratorError> {
    let next_plan_revision = goal
        .plan_revision()
        .checked_add(1)
        .ok_or_else(|| OrchestratorError::InvalidDag("plan revision overflow".to_owned()))?;
    let reconcile_exhausted_readonly = validated.reconcile_exhausted_readonly.clone();
    let pristine_plan_supersession = validated.pristine_plan_supersession.clone();
    let task_replacements = validated.task_replacements.clone();

    let mut materialized = Vec::with_capacity(validated.new_tasks.len());
    let mut local_to_id = BTreeMap::<String, TaskId>::new();
    for proposed in validated.new_tasks {
        let task = Task::new(
            proposed.title,
            proposed.objective,
            proposed.mandatory,
            proposed.worker,
            proposed.scope,
            proposed.verification,
            proposed.max_attempts,
            next_plan_revision,
            now,
        )?;
        let id = task.id().clone();
        local_to_id.insert(proposed.proposal_id.clone(), id);
        materialized.push((proposed.proposal_id, proposed.dependencies, task));
    }

    let resolve_reference = |reference: &ValidatedTaskRef| -> Result<TaskId, OrchestratorError> {
        match reference {
            ValidatedTaskRef::Existing(id) => Ok(id.clone()),
            ValidatedTaskRef::New(local) => local_to_id.get(local).cloned().ok_or_else(|| {
                OrchestratorError::InvalidDag(
                    "validated proposal-local Task identity disappeared".to_owned(),
                )
            }),
        }
    };

    let completed = goal
        .tasks()
        .iter()
        .filter_map(|(id, task)| (task.status() == TaskStatus::Completed).then_some(id.clone()))
        .collect::<BTreeSet<_>>();
    for (_, dependencies, task) in &mut materialized {
        let dependencies = dependencies
            .iter()
            .map(&resolve_reference)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(crate::task::TaskDependency::completed)
            .collect::<Vec<_>>();
        task.strengthen_dependencies(dependencies, &completed)?;
    }

    if let Some(supersession) = pristine_plan_supersession {
        let criterion_rebindings = supersession
            .criterion_rebindings
            .into_iter()
            .map(|(criterion_id, refs)| {
                let ids = refs
                    .iter()
                    .map(&resolve_reference)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((criterion_id, ids))
            })
            .collect::<Result<Vec<_>, OrchestratorError>>()?;
        return goal.apply_pristine_plan_supersession(
            crate::goal::PristinePlanSupersessionMutation {
                rejection_request_id: supersession.rejection_request_id,
                rejected_plan_revision: supersession.rejected_plan_revision,
                committed_plan_revision: next_plan_revision,
                canonical_proposal_digest: supersession.canonical_proposal_digest,
                affected_task_ids: supersession.affected_task_ids,
                new_tasks: materialized.into_iter().map(|(_, _, task)| task).collect(),
                criterion_rebindings,
            },
            now,
        );
    }

    if !task_replacements.is_empty() {
        // Every replacement closure is disjoint and together they cover all
        // new Tasks, so the transaction supersedes each old Task and creates
        // its own bounded closure in one durable mutation.
        let mut claimed = BTreeSet::new();
        let mut mutations = Vec::with_capacity(task_replacements.len());
        for replacement in task_replacements {
            let mut closure_ids = Vec::new();
            for reference in &replacement.completion_closure_refs {
                if let ValidatedTaskRef::New(local) = reference {
                    closure_ids.push(local_to_id.get(local).cloned().ok_or_else(|| {
                        OrchestratorError::InvalidDag(
                            "validated replacement closure identity disappeared".to_owned(),
                        )
                    })?);
                }
            }
            // This replacement owns every new Task in the transitive dependency
            // cone of its closure, so multiple replacements never share a Task
            // and no replacement Task is left dangling.
            let mut owned = replacement
                .completion_closure_refs
                .iter()
                .filter_map(|reference| match reference {
                    ValidatedTaskRef::New(local) => Some(local.clone()),
                    ValidatedTaskRef::Existing(_) => None,
                })
                .collect::<BTreeSet<_>>();
            loop {
                let mut extended = owned.clone();
                for (proposal_id, dependencies, _) in &materialized {
                    if !owned.contains(proposal_id) {
                        continue;
                    }
                    for dependency in dependencies {
                        if let ValidatedTaskRef::New(local) = dependency {
                            extended.insert(local.clone());
                        }
                    }
                }
                if extended == owned {
                    break;
                }
                owned = extended;
            }
            let criterion_rebindings = replacement
                .criterion_rebindings
                .iter()
                .map(|(criterion_id, refs)| {
                    let ids = refs
                        .iter()
                        .map(&resolve_reference)
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok((criterion_id.clone(), ids))
                })
                .collect::<Result<Vec<_>, OrchestratorError>>()?;
            let mut new_tasks = Vec::new();
            for (local, _, task) in &materialized {
                if owned.contains(local) && claimed.insert(local.clone()) {
                    new_tasks.push(task.clone());
                }
            }
            mutations.push(FailedTaskReplacementMutation {
                replan_request_id: replacement.replan_request_id,
                replaced_task_id: replacement.old_task_id,
                replaced_plan_revision: goal.plan_revision(),
                committed_plan_revision: replacement.committed_plan_revision,
                canonical_proposal_digest: replacement.canonical_proposal_digest,
                completion_closure_task_ids: closure_ids,
                criterion_rebindings,
                new_tasks,
            });
        }
        return goal.apply_task_replacements(mutations, now);
    }

    let mut existing_additions = BTreeMap::<TaskId, Vec<TaskId>>::new();
    let mut new_additions = BTreeMap::<String, Vec<TaskId>>::new();
    for (target, dependency) in validated.add_dependencies {
        let dependency_id = resolve_reference(&dependency)?;
        match target {
            ValidatedTaskRef::Existing(id) => existing_additions
                .entry(id)
                .or_default()
                .push(dependency_id),
            ValidatedTaskRef::New(local) => {
                new_additions.entry(local).or_default().push(dependency_id)
            }
        }
    }

    for (local, additions) in new_additions {
        let (_, _, task) = materialized
            .iter_mut()
            .find(|(proposal_id, _, _)| proposal_id == &local)
            .ok_or_else(|| {
                OrchestratorError::InvalidDag("new dependency target disappeared".to_owned())
            })?;
        let mut dependencies = task.dependencies().to_vec();
        dependencies.extend(
            additions
                .into_iter()
                .map(crate::task::TaskDependency::completed),
        );
        task.strengthen_dependencies(dependencies, &completed)?;
    }

    let criterion_additions = validated
        .add_criterion_requirements
        .into_iter()
        .map(|(criterion_id, refs)| {
            let requirements = refs
                .iter()
                .map(&resolve_reference)
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|task_id| GoalVerificationRequirement::TaskVerified { task_id })
                .collect();
            Ok((criterion_id, requirements))
        })
        .collect::<Result<Vec<_>, OrchestratorError>>()?;

    let recovery_authority = ReadonlyReplanRecoveryAuthority::for_replanner();
    for task_id in reconcile_exhausted_readonly {
        if !goal.reconcile_exhausted_readonly_replan_task(
            &task_id,
            &recovery_authority,
            next_plan_revision,
            now,
        )? {
            return Err(OrchestratorError::InvalidDag(
                "validated exhausted readonly replan reconciliation became ineligible before commit"
                    .to_owned(),
            ));
        }
    }

    goal.apply_replan_mutation(
        ReplanMutation {
            new_tasks: materialized.into_iter().map(|(_, _, task)| task).collect(),
            add_dependencies: existing_additions.into_iter().collect(),
            add_verification: validated.add_verification,
            strengthen_mandatory: validated.strengthen_mandatory,
            resolve_needs_replan: validated.resolve_needs_replan,
            add_criterion_requirements: criterion_additions,
        },
        now,
    )
}

fn validate_writer_serialization(goal: &Goal) -> Result<(), OrchestratorError> {
    let writers = goal
        .tasks()
        .iter()
        .filter_map(|(id, task)| {
            (!task.is_terminal() && task.scope().operation_kind() != TaskOperationKind::ReadOnly)
                .then_some(id.clone())
        })
        .collect::<Vec<_>>();
    for left in 0..writers.len() {
        for right in left + 1..writers.len() {
            let a = &writers[left];
            let b = &writers[right];
            if !task_depends_on(goal, a, b) && !task_depends_on(goal, b, a) {
                return Err(OrchestratorError::InvalidDag("non-terminal mutation Tasks must remain totally ordered by hard dependencies in V1".to_owned()));
            }
        }
    }
    Ok(())
}

fn task_depends_on(goal: &Goal, start: &TaskId, target: &TaskId) -> bool {
    let mut stack = vec![start.clone()];
    let mut seen = BTreeSet::new();
    while let Some(task_id) = stack.pop() {
        if !seen.insert(task_id.clone()) {
            continue;
        }
        let Some(task) = goal.tasks().get(&task_id) else {
            continue;
        };
        for dependency in task.dependencies() {
            if dependency.task_id() == target {
                return true;
            }
            stack.push(dependency.task_id().clone());
        }
    }
    false
}

fn map_planner_validation_error(error: PlannerError) -> ReplannerError {
    match error {
        PlannerError::PlannerOutputInvalid(reason) => {
            ReplannerError::ReplannerOutputInvalid(reason)
        }
        PlannerError::PlannerSchemaViolation(reason) => {
            ReplannerError::ReplannerSchemaViolation(reason)
        }
        PlannerError::PlanAuthorityViolation(reason) => {
            ReplannerError::ReplanAuthorityViolation(reason)
        }
        PlannerError::PlanNotApplicable(reason) => ReplannerError::ReplanNotApplicable(reason),
        PlannerError::PlanConflict { expected, actual } => {
            ReplannerError::RevisionConflict { expected, actual }
        }
        PlannerError::PlannerUnavailable => ReplannerError::ReplannerUnavailable,
        PlannerError::Model(error) => ReplannerError::Model(error),
        PlannerError::Store(error) => ReplannerError::Store(error),
    }
}

fn map_store_error(error: OrchestratorError) -> ReplannerError {
    match error {
        OrchestratorError::RevisionConflict { expected, actual } => {
            ReplannerError::RevisionConflict { expected, actual }
        }
        other => ReplannerError::Store(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;

    use serde_json::{Value, json};
    use uuid::Uuid;

    use crate::fallback::{SideEffectClass, SideEffectState};
    use crate::goal::{CheckpointReason, GoalBlocker, PreExecutionPlanReplanPolicy};
    use crate::task::{
        ReadonlyReplanRecoveryAuthorityKind, ReplaySafety, TaskDependency, TaskEvidence, TaskScope,
        TaskTransitionContext, VerificationCheckResult, VerificationOutcome,
    };
    use crate::task_store::FaultPoint;

    const NOW: &str = "2026-09-13T00:00:00Z";

    struct Fixture {
        root: PathBuf,
        state: PathBuf,
        repo: PathBuf,
        session: config::Session,
        store: TaskStore,
        goal_id: GoalId,
        trigger_id: TaskId,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn scope(repo: &std::path::Path, operation_kind: TaskOperationKind) -> TaskScope {
        let replay_safety = match operation_kind {
            TaskOperationKind::ReadOnly => ReplaySafety::SafeReadOnly,
            TaskOperationKind::LocalMutation | TaskOperationKind::HostNativeApproved => {
                ReplaySafety::VerifyBeforeRetry
            }
        };
        TaskScope::new(
            vec![repo.to_path_buf()],
            vec![repo.join("frozen-forbidden")],
            operation_kind,
            replay_safety,
        )
    }

    fn verification(id: &str) -> Vec<VerificationSpec> {
        vec![VerificationSpec::StructuredEvidence {
            requirement_id: id.to_owned(),
        }]
    }

    fn base_fixture(unknown_side_effect: bool) -> Fixture {
        let root = std::env::temp_dir().join(format!("local-mcp-phase7-{}", Uuid::new_v4()));
        let repo = root.join("repo");
        let state = root.join("state");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("sentinel.txt"), b"unchanged\n").unwrap();
        let session = config::Session {
            id: format!("phase7-{}", Uuid::new_v4()),
            cwd: repo.clone(),
            permitted_directories: vec![repo.clone()],
        };
        let store = TaskStore::with_state_root(state.clone());
        let mut goal = Goal::new(
            session.id.clone(),
            repo.clone(),
            "repair a verified task without weakening authority",
            Some("Phase 7 fixture".to_owned()),
            vec!["stay inside repository authority".to_owned()],
            vec!["repair prerequisite is durably verified".to_owned()],
            NOW,
        )
        .unwrap();
        let trigger = Task::new(
            "trigger",
            "original immutable task objective",
            true,
            WorkerKind::CodexReadonly,
            scope(&repo, TaskOperationKind::ReadOnly),
            verification("trigger.base"),
            2,
            1,
            NOW,
        )
        .unwrap();
        let trigger_id = trigger.id().clone();
        goal.materialize_initial_plan(vec![trigger], NOW).unwrap();
        goal.transition_task(
            &trigger_id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.task_bind_latest_attempt_execution(
            &trigger_id,
            Some("op-phase7".to_owned()),
            Some("scope-phase7".to_owned()),
            Some("request-phase7".to_owned()),
            Some(SideEffectClass::None),
            Some(if unknown_side_effect {
                SideEffectState::Unknown
            } else {
                SideEffectState::ConfirmedNotPerformed
            }),
            Some(1),
            Some(0),
        )
        .unwrap();
        goal.task_add_evidence(
            &trigger_id,
            TaskEvidence::StructuredObservation {
                requirement_id: "trigger.failure".to_owned(),
                source: "phase7-fixture".to_owned(),
                passed: false,
                detail: "missing prerequisite".to_owned(),
            },
        )
        .unwrap();
        goal.transition_task(
            &trigger_id,
            TaskStatus::NeedsReplan,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        Fixture {
            root,
            state,
            repo,
            session,
            store,
            goal_id,
            trigger_id,
        }
    }

    fn fixture() -> Fixture {
        base_fixture(false)
    }

    fn pre_execution_rejection_fixture_with_policy(replan_policy: &str) -> Fixture {
        let root =
            std::env::temp_dir().join(format!("local-mcp-phase7-pre-rejection-{}", Uuid::new_v4()));
        let repo = root.join("repo");
        let state = root.join("state");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("sentinel.txt"), b"unchanged\n").unwrap();
        let session = config::Session {
            id: format!("phase7-pre-rejection-{}", Uuid::new_v4()),
            cwd: repo.clone(),
            permitted_directories: vec![repo.clone()],
        };
        let store = TaskStore::with_state_root(state.clone());
        let mut goal = Goal::new(
            session.id.clone(),
            repo.clone(),
            "replan a rejected accepted plan",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        let trigger = Task::new(
            "trigger",
            "the accepted plan needs reassessment",
            true,
            WorkerKind::CodexWriter,
            scope(&repo, TaskOperationKind::LocalMutation),
            verification("trigger.rejection"),
            2,
            1,
            NOW,
        )
        .unwrap();
        let trigger_id = trigger.id().clone();
        goal.materialize_initial_plan(vec![trigger], NOW).unwrap();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let rejection_args = json!({
            "session_id": session.id,
            "goal_id": goal_id.as_str(),
            "pre_execution_plan_rejection": {
                "request_id": "rejection-feedback-1",
                "expected_goal_revision": 1,
                "expected_plan_revision": 1,
                "trigger_task_id": trigger_id.as_str(),
                "reason": "the repository contract is not established",
                "replan_policy": replan_policy
            }
        });
        crate::goal_api::goal_resume(&rejection_args, &session, &store).unwrap();
        Fixture {
            root,
            state,
            repo,
            session,
            store,
            goal_id,
            trigger_id,
        }
    }

    fn pre_execution_rejection_fixture() -> Fixture {
        pre_execution_rejection_fixture_with_policy("REQUIRE_READONLY_REASSESSMENT")
    }

    fn exhaust_trigger_with_second_needs_replan(
        fixture: &Fixture,
        safe_latest: bool,
    ) -> (crate::task::AttemptId, crate::task::AttemptId) {
        let current = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let updated = fixture
            .store
            .mutate_goal_snapshot(
                &fixture.session.id,
                &fixture.goal_id,
                current.revision(),
                |goal, now| {
                    goal.transition_task(
                        &fixture.trigger_id,
                        TaskStatus::Pending,
                        TaskTransitionContext::default(),
                        now,
                    )?;
                    goal.transition_task(
                        &fixture.trigger_id,
                        TaskStatus::Ready,
                        TaskTransitionContext::default(),
                        now,
                    )?;
                    goal.transition_task(
                        &fixture.trigger_id,
                        TaskStatus::Running,
                        TaskTransitionContext::default(),
                        now,
                    )?;
                    goal.task_bind_latest_attempt_execution(
                        &fixture.trigger_id,
                        (!safe_latest).then(|| "unsafe-op-2".to_owned()),
                        (!safe_latest).then(|| "unsafe-scope-2".to_owned()),
                        None,
                        Some(SideEffectClass::None),
                        Some(SideEffectState::ConfirmedNotPerformed),
                        Some(0),
                        Some(0),
                    )?;
                    goal.transition_task(
                        &fixture.trigger_id,
                        TaskStatus::NeedsReplan,
                        TaskTransitionContext::default(),
                        now,
                    )
                },
            )
            .unwrap();
        let task = &updated.tasks()[&fixture.trigger_id];
        assert_eq!(task.attempts().len(), 2);
        assert_eq!(task.semantic_attempts_consumed(), 2);
        assert_eq!(task.semantic_attempts_remaining(), 0);
        (
            task.attempts()[0].id().clone(),
            task.attempts()[1].id().clone(),
        )
    }

    fn existing_ref(id: &TaskId) -> Value {
        json!({"ref_kind": "EXISTING", "task_id": id.as_str()})
    }

    fn new_ref(id: &str) -> Value {
        json!({"ref_kind": "NEW", "proposal_id": id})
    }

    fn new_task_value(
        id: &str,
        worker: &str,
        operation_kind: &str,
        replay_safety: &str,
        dependencies: Vec<Value>,
    ) -> Value {
        json!({
            "proposal_id": id,
            "title": format!("Task {id}"),
            "objective": format!("Complete {id} without executing during replanning"),
            "mandatory": true,
            "worker": worker,
            "dependencies": dependencies,
            "scope": {
                "allowed_paths": ["."],
                "forbidden_paths": [],
                "operation_kind": operation_kind,
                "replay_safety": replay_safety
            },
            "verification": [{
                "kind": "STRUCTURED_EVIDENCE",
                "requirement_id": format!("evidence.{id}")
            }]
        })
    }

    fn read_only_task(id: &str, dependencies: Vec<Value>) -> Value {
        new_task_value(
            id,
            "CODEX_READONLY",
            "READ_ONLY",
            "SAFE_READ_ONLY",
            dependencies,
        )
    }

    fn writer_task(id: &str, dependencies: Vec<Value>) -> Value {
        new_task_value(
            id,
            "CODEX_WRITER",
            "LOCAL_MUTATION",
            "VERIFY_BEFORE_RETRY",
            dependencies,
        )
    }

    fn proposal_value(fixture: &Fixture) -> Value {
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        json!({
            "goal_id": fixture.goal_id.as_str(),
            "base_goal_revision": goal.revision(),
            "base_plan_revision": goal.plan_revision(),
            "summary": "add a bounded repair prerequisite",
            "add_tasks": [read_only_task("repair", vec![])],
            "add_dependencies": [{
                "task": existing_ref(&fixture.trigger_id),
                "dependency": new_ref("repair")
            }],
            "strengthen_verification": [],
            "strengthen_mandatory": [],
            "resolve_needs_replan": [fixture.trigger_id.as_str()]
        })
    }

    fn proposal_bytes(fixture: &Fixture) -> Vec<u8> {
        serde_json::to_vec(&proposal_value(fixture)).unwrap()
    }

    fn goal_path(fixture: &Fixture) -> PathBuf {
        fixture
            .state
            .join("goals")
            .join(&fixture.session.id)
            .join(format!("{}.json", fixture.goal_id.as_str()))
    }

    fn bytes(fixture: &Fixture) -> Vec<u8> {
        fs::read(goal_path(fixture)).unwrap()
    }

    fn repo_bytes(fixture: &Fixture) -> Vec<u8> {
        fs::read(fixture.repo.join("sentinel.txt")).unwrap()
    }

    fn apply(fixture: &Fixture, proposal: &[u8]) -> Result<Goal, ReplannerError> {
        let current = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        materialize_replan_output(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            current.revision(),
            current.plan_revision(),
            proposal,
        )
    }

    #[test]
    fn pristine_plan_supersession_replaces_a_pre_execution_rejected_plan() {
        let fixture = pre_execution_rejection_fixture();
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let criterion_id = goal.completion_criteria()[0].id().as_str().to_owned();
        let mut proposal = proposal_value(&fixture);
        proposal["summary"] = json!("replace the rejected pristine plan");
        proposal["add_tasks"] = json!([read_only_task("replacement", vec![])]);
        proposal["add_dependencies"] = json!([]);
        proposal["resolve_needs_replan"] = json!([]);
        proposal["pristine_plan_supersession"] = json!({
            "rejection_request_id": "rejection-feedback-1",
            "criterion_rebindings": [{
                "criterion_id": criterion_id,
                "replacement_task_refs": [new_ref("replacement")]
            }]
        });

        let result = apply(&fixture, &serde_json::to_vec(&proposal).unwrap()).unwrap();
        assert_eq!(result.plan_revision(), 2);
        assert_eq!(result.tasks().len(), 2);
        assert_eq!(
            result.tasks()[&fixture.trigger_id].status(),
            TaskStatus::Superseded
        );
        let replacement = result
            .tasks()
            .values()
            .find(|task| task.title() == "Task replacement")
            .unwrap();
        assert_eq!(replacement.status(), TaskStatus::Ready);
        let durable = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        assert_eq!(durable, result);

        let replay = materialize_replan_output(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            goal.revision(),
            goal.plan_revision(),
            &serde_json::to_vec(&proposal).unwrap(),
        )
        .unwrap();
        assert_eq!(replay, result);
    }

    #[test]
    fn ordinary_task_transition_cannot_enter_superseded() {
        let fixture = pre_execution_rejection_fixture();
        let before = bytes(&fixture);
        let current = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let result = fixture.store.mutate_goal_snapshot(
            &fixture.session.id,
            &fixture.goal_id,
            current.revision(),
            |goal, now| {
                goal.transition_task(
                    &fixture.trigger_id,
                    TaskStatus::Superseded,
                    TaskTransitionContext::default(),
                    now,
                )
                .map(|_| ())
            },
        );
        assert!(matches!(
            result,
            Err(OrchestratorError::InvalidTransition { .. })
        ));
        assert_eq!(bytes(&fixture), before);
    }

    #[test]
    fn replanner_rejects_command_exit_without_mutating_durable_goal() {
        let fixture = pre_execution_rejection_fixture();
        let mut proposal = proposal_value(&fixture);
        proposal["add_tasks"] = json!([read_only_task("replacement", vec![])]);
        proposal["add_tasks"][0]["verification"] = json!([{
            "kind": "COMMAND_EXIT",
            "command": ["git", "rev-parse", "--show-toplevel"],
            "cwd": null,
            "accepted_exit_codes": [0]
        }]);
        proposal["add_dependencies"] = json!([]);
        proposal["resolve_needs_replan"] = json!([]);
        let before = bytes(&fixture);
        let result = apply(&fixture, &serde_json::to_vec(&proposal).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplannerSchemaViolation(reason))
                if reason.contains("COMMAND_EXIT verification is currently unsupported")
        ));
        assert_eq!(bytes(&fixture), before);
    }

    #[test]
    fn pristine_supersession_persistence_failure_preserves_history() {
        let fixture = pre_execution_rejection_fixture();
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let criterion_id = goal.completion_criteria()[0].id().as_str().to_owned();
        let mut proposal = proposal_value(&fixture);
        proposal["add_tasks"] = json!([read_only_task("replacement", vec![])]);
        proposal["add_dependencies"] = json!([]);
        proposal["resolve_needs_replan"] = json!([]);
        proposal["pristine_plan_supersession"] = json!({
            "rejection_request_id": "rejection-feedback-1",
            "criterion_rebindings": [{"criterion_id": criterion_id, "replacement_task_refs": [new_ref("replacement")]}]
        });
        let before = bytes(&fixture);
        let fault_store = TaskStore::with_fault(fixture.state.clone(), FaultPoint::BeforeReplace);
        let result = materialize_replan_output(
            &fault_store,
            &fixture.session,
            &fixture.goal_id,
            goal.revision(),
            goal.plan_revision(),
            &serde_json::to_vec(&proposal).unwrap(),
        );
        assert!(matches!(
            result,
            Err(ReplannerError::Store(OrchestratorError::PersistenceIo(_)))
        ));
        assert_eq!(bytes(&fixture), before);
        let reloaded = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        assert_eq!(reloaded.plan_revision(), goal.plan_revision());
        assert!(
            reloaded
                .find_pristine_plan_supersession("rejection-feedback-1")
                .is_none()
        );
    }

    #[test]
    fn tampered_supersession_history_is_rejected_on_reload() {
        let fixture = pre_execution_rejection_fixture();
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let criterion_id = goal.completion_criteria()[0].id().as_str().to_owned();
        let mut proposal = proposal_value(&fixture);
        proposal["add_tasks"] = json!([read_only_task("replacement", vec![])]);
        proposal["add_dependencies"] = json!([]);
        proposal["resolve_needs_replan"] = json!([]);
        proposal["pristine_plan_supersession"] = json!({
            "rejection_request_id": "rejection-feedback-1",
            "criterion_rebindings": [{"criterion_id": criterion_id, "replacement_task_refs": [new_ref("replacement")]}]
        });
        apply(&fixture, &serde_json::to_vec(&proposal).unwrap()).unwrap();
        let path = goal_path(&fixture);
        let mut durable: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        durable["pristine_plan_supersessions"][0]["affected_task_ids"] = json!([]);
        fs::write(&path, serde_json::to_vec_pretty(&durable).unwrap()).unwrap();
        assert!(matches!(
            fixture
                .store
                .load_goal(&fixture.session.id, &fixture.goal_id),
            Err(OrchestratorError::CorruptGoal(_))
        ));
    }

    #[test]
    fn supersession_with_unknown_rejection_request_id_is_rejected_nonmutating() {
        let fixture = pre_execution_rejection_fixture();
        let before = bytes(&fixture);
        let before_repo = repo_bytes(&fixture);
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let criterion_id = goal.completion_criteria()[0].id().as_str().to_owned();
        let mut proposal = proposal_value(&fixture);
        proposal["summary"] = json!("replace the rejected pristine plan");
        proposal["add_tasks"] = json!([read_only_task("replacement", vec![])]);
        proposal["add_dependencies"] = json!([]);
        proposal["resolve_needs_replan"] = json!([]);
        proposal["pristine_plan_supersession"] = json!({
            "rejection_request_id": "rejection-feedback-NOT-REAL",
            "criterion_rebindings": [{
                "criterion_id": criterion_id,
                "replacement_task_refs": [new_ref("replacement")]
            }]
        });
        let result = apply(&fixture, &serde_json::to_vec(&proposal).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplanAuthorityViolation(_))
        ));
        assert_eq!(bytes(&fixture), before);
        assert_eq!(repo_bytes(&fixture), before_repo);
        let reloaded = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        assert_eq!(reloaded.plan_revision(), 1);
        assert_eq!(reloaded.pristine_plan_supersessions().len(), 0);
    }

    #[test]
    fn supersession_rejects_when_affected_task_has_prior_execution_evidence() {
        let fixture = pre_execution_rejection_fixture();
        // Simulate the rejected-plan Task having been executed before the
        // rejection, making it non-pristine. The host must refuse supersession.
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        fixture
            .store
            .mutate_goal_snapshot(
                &fixture.session.id,
                &fixture.goal_id,
                goal.revision(),
                |goal, _| {
                    goal.task_add_evidence(
                        &fixture.trigger_id,
                        TaskEvidence::StructuredObservation {
                            requirement_id: "executed.before.rejection".to_owned(),
                            source: "phase7-fixture".to_owned(),
                            passed: true,
                            detail: "prior execution evidence that violates pristineness"
                                .to_owned(),
                        },
                    )?;
                    Ok(())
                },
            )
            .unwrap();
        let before = bytes(&fixture);
        let before_repo = repo_bytes(&fixture);
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let criterion_id = goal.completion_criteria()[0].id().as_str().to_owned();
        let mut proposal = proposal_value(&fixture);
        proposal["summary"] = json!("replace the rejected pristine plan");
        proposal["add_tasks"] = json!([read_only_task("replacement", vec![])]);
        proposal["add_dependencies"] = json!([]);
        proposal["resolve_needs_replan"] = json!([]);
        proposal["pristine_plan_supersession"] = json!({
            "rejection_request_id": "rejection-feedback-1",
            "criterion_rebindings": [{
                "criterion_id": criterion_id,
                "replacement_task_refs": [new_ref("replacement")]
            }]
        });
        let result = apply(&fixture, &serde_json::to_vec(&proposal).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplanAuthorityViolation(_))
        ));
        assert_eq!(bytes(&fixture), before);
        assert_eq!(repo_bytes(&fixture), before_repo);
        let reloaded = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        assert_eq!(reloaded.plan_revision(), 1);
        assert_eq!(reloaded.pristine_plan_supersessions().len(), 0);
        // Pristineness violation is preserved — the Task keeps its evidence.
        assert_eq!(reloaded.tasks()[&fixture.trigger_id].evidence_count(), 1);
    }

    #[test]
    fn supersession_with_existing_replacement_ref_is_rejected_nonmutating() {
        let fixture = pre_execution_rejection_fixture();
        let before = bytes(&fixture);
        let before_repo = repo_bytes(&fixture);
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let criterion_id = goal.completion_criteria()[0].id().as_str().to_owned();
        let mut proposal = proposal_value(&fixture);
        proposal["summary"] = json!("replace the rejected pristine plan");
        proposal["add_tasks"] = json!([]);
        proposal["add_dependencies"] = json!([]);
        proposal["resolve_needs_replan"] = json!([]);
        proposal["pristine_plan_supersession"] = json!({
            "rejection_request_id": "rejection-feedback-1",
            "criterion_rebindings": [{
                "criterion_id": criterion_id,
                "replacement_task_refs": [existing_ref(&fixture.trigger_id)]
            }]
        });
        let result = apply(&fixture, &serde_json::to_vec(&proposal).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplanAuthorityViolation(_))
        ));
        assert_eq!(bytes(&fixture), before);
        assert_eq!(repo_bytes(&fixture), before_repo);
        let reloaded = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        assert_eq!(reloaded.plan_revision(), 1);
        assert_eq!(reloaded.pristine_plan_supersessions().len(), 0);
    }

    #[test]
    fn exhausted_readonly_needs_replan_reconciles_append_only_with_new_prerequisite() {
        let fixture = fixture();
        let (attempt_1, attempt_2) = exhaust_trigger_with_second_needs_replan(&fixture, true);
        let before = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let task_before = &before.tasks()[&fixture.trigger_id];
        assert!(task_before.can_reconcile_exhausted_readonly_replan());
        assert_eq!(task_before.max_attempts(), 2);
        let mut proposal = proposal_value(&fixture);
        proposal["add_tasks"][0] = writer_task("repair", vec![]);
        let result = apply(&fixture, &serde_json::to_vec(&proposal).unwrap()).unwrap();
        assert_eq!(result.plan_revision(), 2);
        let task = &result.tasks()[&fixture.trigger_id];
        assert_eq!(task.status(), TaskStatus::Pending);
        assert_eq!(task.max_attempts(), 2);
        assert_eq!(task.attempts().len(), 2);
        assert_eq!(task.attempts()[0].id(), &attempt_1);
        assert_eq!(task.attempts()[1].id(), &attempt_2);
        assert_eq!(task.semantic_attempts_consumed(), 1);
        assert_eq!(task.semantic_attempts_remaining(), 1);
        assert_eq!(task.readonly_replan_reconciliations(), 1);
        assert!(task.evidence().iter().any(|evidence| matches!(
            evidence,
            TaskEvidence::ReadonlyReplanRecovery {
                attempt_id,
                authority: ReadonlyReplanRecoveryAuthorityKind::Replanner,
                plan_revision_before: 1,
                plan_revision_after: 2,
                side_effect_state: SideEffectState::ConfirmedNotPerformed,
            } if attempt_id == &attempt_2
        )));
        let repair = result
            .tasks()
            .values()
            .find(|candidate| candidate.created_plan_revision() == 2)
            .unwrap();
        assert_eq!(repair.worker(), WorkerKind::CodexWriter);
        assert_eq!(repair.status(), TaskStatus::Ready);
        assert!(
            task.dependencies()
                .iter()
                .any(|dependency| dependency.task_id() == repair.id())
        );
    }

    #[test]
    fn exhausted_readonly_needs_replan_reconciliation_rejects_unsafe_latest_attempt_atomically() {
        let fixture = fixture();
        exhaust_trigger_with_second_needs_replan(&fixture, false);
        let before = bytes(&fixture);
        let durable = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        assert!(!durable.tasks()[&fixture.trigger_id].can_reconcile_exhausted_readonly_replan());
        let result = apply(&fixture, &proposal_bytes(&fixture));
        assert!(matches!(result, Err(ReplannerError::NoSafeReplan(_))));
        assert_eq!(bytes(&fixture), before);
    }

    #[test]
    fn request_contains_durable_replan_context_without_mutation_capabilities() {
        let fixture = fixture();
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let request = replanner_request_for_goal(&goal, &fixture.session).unwrap();
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["goal_revision"], 1);
        assert_eq!(value["plan_revision"], 1);
        assert_eq!(
            value["allowed_worker_kinds"],
            json!(["CODEX_READONLY", "CODEX_WRITER"])
        );
        assert_eq!(
            value["eligible_needs_replan_task_ids"][0],
            fixture.trigger_id.as_str()
        );
        assert_eq!(
            value["tasks"][0]["attempts"][0]["operation_id"],
            "op-phase7"
        );
        assert_eq!(value["tasks"][0]["evidence"].as_array().unwrap().len(), 1);
        for forbidden in [
            "state_root",
            "persistence_lock",
            "approval_token",
            "sandbox_bypass",
            "process_handle",
            "credentials",
            "yolo",
        ] {
            assert!(
                value.get(forbidden).is_none(),
                "unexpected authority field {forbidden}"
            );
        }
    }

    #[test]
    fn replanner_request_contains_pre_execution_rejection_feedback() {
        let fixture = pre_execution_rejection_fixture();
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let request = replanner_request_for_goal(&goal, &fixture.session).unwrap();
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(
            value["pre_execution_plan_rejections"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            value["pre_execution_plan_rejections"][0]["request_id"],
            json!("rejection-feedback-1")
        );
        assert_eq!(
            value["pre_execution_plan_rejections"][0]["rejected_plan_revision"],
            json!(1)
        );
        assert_eq!(
            value["pre_execution_plan_rejections"][0]["replan_policy"],
            json!("REQUIRE_READONLY_REASSESSMENT")
        );
        assert_eq!(
            value["consecutive_pre_execution_plan_rejection_count"],
            json!(1)
        );
    }

    #[test]
    fn replanner_request_bounds_pre_execution_rejection_history() {
        let fixture = pre_execution_rejection_fixture();
        let mut goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let mut trigger_id = fixture.trigger_id.clone();

        for index in 1..9 {
            let expected_plan_revision = goal.plan_revision();

            let next_trigger = Task::new(
                format!("next-trigger-{index}"),
                "synthetic next trigger",
                true,
                WorkerKind::CodexReadonly,
                scope(&fixture.repo, TaskOperationKind::ReadOnly),
                verification(&format!("next-trigger-{index}")),
                1,
                expected_plan_revision + 1,
                NOW,
            )
            .unwrap();
            let next_trigger_id = next_trigger.id().clone();
            goal.apply_replan_mutation(
                ReplanMutation {
                    new_tasks: vec![next_trigger],
                    add_dependencies: vec![(trigger_id.clone(), vec![next_trigger_id.clone()])],
                    add_verification: vec![],
                    strengthen_mandatory: vec![],
                    resolve_needs_replan: vec![trigger_id.clone()],
                    add_criterion_requirements: vec![],
                },
                NOW,
            )
            .unwrap();
            trigger_id = next_trigger_id;

            let expected_goal_revision = goal.revision();
            let expected_plan_revision = goal.plan_revision();
            goal.reject_pre_execution_plan(
                format!("bounded-rejection-{index}"),
                expected_goal_revision,
                expected_plan_revision,
                trigger_id.clone(),
                format!("synthetic rejection {index}"),
                PreExecutionPlanReplanPolicy::Normal,
                NOW,
            )
            .unwrap();
        }

        let request = replanner_request_for_goal(&goal, &fixture.session).unwrap();
        let value = serde_json::to_value(request).unwrap();
        let history = value["pre_execution_plan_rejections"].as_array().unwrap();
        assert_eq!(history.len(), 8);
        assert_eq!(history[0]["request_id"], json!("bounded-rejection-1"));
        assert_eq!(history[7]["request_id"], json!("bounded-rejection-8"));
        assert_eq!(
            value["consecutive_pre_execution_plan_rejection_count"],
            json!(1)
        );
    }

    fn writer_first_proposal_value(fixture: &Fixture) -> Value {
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        json!({
            "goal_id": fixture.goal_id.as_str(),
            "base_goal_revision": goal.revision(),
            "base_plan_revision": goal.plan_revision(),
            "summary": "add a mutation before establishing the repository contract",
            "add_tasks": [writer_task("repair", vec![])],
            "add_dependencies": [{
                "task": existing_ref(&fixture.trigger_id),
                "dependency": new_ref("repair")
            }],
            "strengthen_verification": [],
            "strengthen_mandatory": [],
            "strengthen_criterion_bindings": [],
            "resolve_needs_replan": [fixture.trigger_id.as_str()]
        })
    }

    #[test]
    fn require_readonly_reassessment_rejects_writer_first_before_materialization() {
        let fixture = pre_execution_rejection_fixture();
        let before = bytes(&fixture);
        let result = apply(
            &fixture,
            &serde_json::to_vec(&writer_first_proposal_value(&fixture)).unwrap(),
        );
        assert!(matches!(
            result,
            Err(ReplannerError::ReplanAuthorityViolation(_))
        ));
        let after = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        assert_eq!(after.plan_revision(), 1);
        assert_eq!(bytes(&fixture), before);
        assert_eq!(
            fs::read(fixture.repo.join("sentinel.txt")).unwrap(),
            b"unchanged\n"
        );
    }

    #[test]
    fn require_readonly_reassessment_accepts_new_readonly_prerequisite() {
        let fixture = pre_execution_rejection_fixture();
        let mut proposal = writer_first_proposal_value(&fixture);
        proposal["summary"] = json!("establish the repository contract before mutation");
        proposal["add_tasks"] = json!([read_only_task("inspect", vec![])]);
        proposal["add_dependencies"] = json!([{
            "task": existing_ref(&fixture.trigger_id),
            "dependency": new_ref("inspect")
        }]);
        let result = apply(&fixture, &serde_json::to_vec(&proposal).unwrap()).unwrap();
        assert_eq!(result.plan_revision(), 2);
        assert_eq!(
            result.tasks()[&fixture.trigger_id].status(),
            TaskStatus::Pending
        );
        let inspection = result
            .tasks()
            .values()
            .find(|task| task.title() == "Task inspect")
            .unwrap();
        assert_eq!(inspection.worker(), WorkerKind::CodexReadonly);
        assert_eq!(
            inspection.scope().operation_kind(),
            TaskOperationKind::ReadOnly
        );
        assert_eq!(inspection.status(), TaskStatus::Ready);
        assert!(inspection.attempts().is_empty());
        assert_eq!(
            fs::read(fixture.repo.join("sentinel.txt")).unwrap(),
            b"unchanged\n"
        );
    }

    #[test]
    fn completed_readonly_reassessment_allows_later_scoped_writer_remediation() {
        let fixture = pre_execution_rejection_fixture();
        let mut first = writer_first_proposal_value(&fixture);
        first["summary"] = json!("establish the repository contract before remediation");
        first["add_tasks"] = json!([read_only_task("inspect", vec![])]);
        first["add_dependencies"] = json!([{
            "task": existing_ref(&fixture.trigger_id),
            "dependency": new_ref("inspect")
        }]);
        apply(&fixture, &serde_json::to_vec(&first).unwrap()).unwrap();

        let after_first = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let inspection_id = after_first
            .tasks()
            .values()
            .find(|task| task.title() == "Task inspect")
            .unwrap()
            .id()
            .clone();
        let trigger_id = fixture.trigger_id.clone();
        fixture
            .store
            .mutate_goal_snapshot(
                &fixture.session.id,
                &fixture.goal_id,
                after_first.revision(),
                |goal, now| {
                    goal.transition_task(
                        &inspection_id,
                        TaskStatus::Running,
                        TaskTransitionContext::default(),
                        now,
                    )?;
                    goal.task_add_evidence(
                        &inspection_id,
                        TaskEvidence::StructuredObservation {
                            requirement_id: "evidence.inspect".to_owned(),
                            source: "synthetic-readonly".to_owned(),
                            passed: true,
                            detail: "repository contract established".to_owned(),
                        },
                    )?;
                    goal.transition_task(
                        &inspection_id,
                        TaskStatus::Verifying,
                        TaskTransitionContext::default(),
                        now,
                    )?;
                    goal.task_record_verification_result(
                        &inspection_id,
                        VerificationResult::new(
                            VerificationOutcome::Passed,
                            vec![VerificationCheckResult::new(0, true, Some("ok".to_owned()))],
                            now,
                            now,
                        ),
                    )?;
                    goal.transition_task(
                        &inspection_id,
                        TaskStatus::Completed,
                        TaskTransitionContext::default(),
                        now,
                    )?;
                    goal.transition_task(
                        &trigger_id,
                        TaskStatus::Ready,
                        TaskTransitionContext::default(),
                        now,
                    )?;
                    goal.transition_task(
                        &trigger_id,
                        TaskStatus::Running,
                        TaskTransitionContext::default(),
                        now,
                    )?;
                    goal.transition_task(
                        &trigger_id,
                        TaskStatus::NeedsReplan,
                        TaskTransitionContext::default(),
                        now,
                    )
                },
            )
            .unwrap();

        let before_writer = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let mut writer = writer_first_proposal_value(&fixture);
        writer["summary"] = json!("apply only the scoped remediation established by inspection");
        writer["add_tasks"] = json!([writer_task("repair", vec![existing_ref(&inspection_id)])]);
        let result = apply(&fixture, &serde_json::to_vec(&writer).unwrap()).unwrap();
        assert_eq!(result.plan_revision(), before_writer.plan_revision() + 1);
        let repair = result
            .tasks()
            .values()
            .find(|task| task.title() == "Task repair")
            .unwrap();
        assert_eq!(repair.status(), TaskStatus::Ready);
        assert!(
            repair
                .dependencies()
                .iter()
                .any(|dependency| dependency.task_id() == &inspection_id)
        );
    }

    #[test]
    fn normal_policy_does_not_infer_reassessment_from_rejection_reason() {
        let fixture = pre_execution_rejection_fixture_with_policy("NORMAL");
        let mut proposal = writer_first_proposal_value(&fixture);
        proposal["summary"] =
            json!("the rejection reason mentions a readonly reassessment, but policy is normal");
        let result = apply(&fixture, &serde_json::to_vec(&proposal).unwrap()).unwrap();
        assert_eq!(result.plan_revision(), 2);
        assert_eq!(
            result.tasks()[&fixture.trigger_id].status(),
            TaskStatus::Pending
        );
        assert!(result.tasks().values().any(|task| {
            task.title() == "Task repair" && task.worker() == WorkerKind::CodexWriter
        }));
    }

    #[test]
    fn readonly_barrier_rejects_indirect_writer_before_readonly_bypass() {
        let fixture = pre_execution_rejection_fixture();
        let mut proposal = writer_first_proposal_value(&fixture);
        proposal["add_tasks"] = json!([
            writer_task("repair", vec![]),
            read_only_task("inspect", vec![new_ref("repair")])
        ]);
        proposal["add_dependencies"] = json!([{
            "task": existing_ref(&fixture.trigger_id),
            "dependency": new_ref("inspect")
        }]);
        let before = bytes(&fixture);
        let result = apply(&fixture, &serde_json::to_vec(&proposal).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplanAuthorityViolation(_))
        ));
        assert_eq!(bytes(&fixture), before);
    }

    #[test]
    fn readonly_barrier_allows_scoped_writer_downstream_of_new_readonly() {
        let fixture = pre_execution_rejection_fixture();
        let mut proposal = writer_first_proposal_value(&fixture);
        proposal["summary"] = json!("reassess first, then apply the established scoped repair");
        proposal["add_tasks"] = json!([
            read_only_task("inspect", vec![]),
            writer_task("repair", vec![new_ref("inspect")])
        ]);
        proposal["add_dependencies"] = json!([{
            "task": existing_ref(&fixture.trigger_id),
            "dependency": new_ref("repair")
        }]);
        let result = apply(&fixture, &serde_json::to_vec(&proposal).unwrap()).unwrap();
        assert_eq!(result.plan_revision(), 2);
        let inspection = result
            .tasks()
            .values()
            .find(|task| task.title() == "Task inspect")
            .unwrap();
        let repair = result
            .tasks()
            .values()
            .find(|task| task.title() == "Task repair")
            .unwrap();
        assert_eq!(inspection.status(), TaskStatus::Ready);
        assert_eq!(repair.status(), TaskStatus::Pending);
        assert!(
            repair
                .dependencies()
                .iter()
                .any(|dependency| { dependency.task_id() == inspection.id() })
        );
    }

    #[test]
    fn successful_replan_from_replanning_returns_to_running_when_trigger_is_resolved() {
        let fixture = fixture();
        let current = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let replanning = fixture
            .store
            .mutate_goal_snapshot(
                &fixture.session.id,
                &fixture.goal_id,
                current.revision(),
                |goal, now| goal.transition_to(GoalStatus::Replanning, now),
            )
            .unwrap();
        assert_eq!(replanning.status(), GoalStatus::Replanning);
        assert!(replanning.has_task_status(TaskStatus::NeedsReplan));

        let result = apply(&fixture, &proposal_bytes(&fixture)).unwrap();
        assert_eq!(result.status(), GoalStatus::Running);
        assert!(!result.has_task_status(TaskStatus::NeedsReplan));
        assert_eq!(
            result.checkpoints().last().unwrap().reason(),
            CheckpointReason::ReplanCommitted
        );
        assert_eq!(
            result.checkpoints().last().unwrap().goal_status(),
            GoalStatus::Running
        );
        assert!(
            result
                .tasks()
                .values()
                .any(|task| task.status() == TaskStatus::Ready)
        );
    }

    #[test]
    fn successful_replan_from_replanning_stays_replanning_while_trigger_remains() {
        let fixture = fixture();
        let current = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        fixture
            .store
            .mutate_goal_snapshot(
                &fixture.session.id,
                &fixture.goal_id,
                current.revision(),
                |goal, now| goal.transition_to(GoalStatus::Replanning, now),
            )
            .unwrap();
        let mut value = proposal_value(&fixture);
        value["resolve_needs_replan"] = json!([]);
        let result = apply(&fixture, &serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(result.status(), GoalStatus::Replanning);
        assert!(result.has_task_status(TaskStatus::NeedsReplan));
        assert_eq!(
            result.checkpoints().last().unwrap().goal_status(),
            GoalStatus::Replanning
        );
    }

    #[test]
    fn valid_replan_commits_once_and_preserves_existing_history_and_budgets() {
        let fixture = fixture();
        let before = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let old_task = before.tasks()[&fixture.trigger_id].clone();
        let sentinel = fs::read(fixture.repo.join("sentinel.txt")).unwrap();
        let result = apply(&fixture, &proposal_bytes(&fixture)).unwrap();

        assert_eq!(result.revision(), before.revision() + 1);
        assert_eq!(result.plan_revision(), before.plan_revision() + 1);
        assert_eq!(result.status(), GoalStatus::Running);
        assert_ne!(result.status(), GoalStatus::Completed);
        assert_eq!(result.tasks().len(), 2);
        let trigger = &result.tasks()[&fixture.trigger_id];
        assert_eq!(trigger.status(), TaskStatus::Pending);
        assert_eq!(trigger.attempts(), old_task.attempts());
        assert_eq!(trigger.evidence(), old_task.evidence());
        assert_eq!(
            trigger.verification_results(),
            old_task.verification_results()
        );
        assert_eq!(trigger.scope(), old_task.scope());
        assert_eq!(trigger.max_attempts(), old_task.max_attempts());
        assert_eq!(
            trigger.latest_attempt().unwrap().remaining_attempt_budget(),
            Some(1)
        );
        assert_eq!(
            trigger
                .latest_attempt()
                .unwrap()
                .remaining_side_effect_budget(),
            Some(0)
        );
        let repair = result
            .tasks()
            .values()
            .find(|task| task.title() == "Task repair")
            .unwrap();
        assert_eq!(repair.status(), TaskStatus::Ready);
        assert!(repair.attempts().is_empty());
        assert_eq!(repair.created_plan_revision(), 2);
        assert_eq!(
            result.checkpoints().last().unwrap().reason(),
            CheckpointReason::ReplanCommitted
        );
        assert_eq!(
            fs::read(fixture.repo.join("sentinel.txt")).unwrap(),
            sentinel
        );
    }

    struct StaticBackend {
        output: Vec<u8>,
        calls: Cell<usize>,
    }

    impl ReplannerBackend for StaticBackend {
        fn propose_replan(&self, _request: &ReplannerRequest) -> Result<Vec<u8>, ReplannerError> {
            self.calls.set(self.calls.get() + 1);
            Ok(self.output.clone())
        }
    }

    #[test]
    fn backend_is_proposal_only_and_is_invoked_once() {
        let fixture = fixture();
        let sentinel = fs::read(fixture.repo.join("sentinel.txt")).unwrap();
        let backend = StaticBackend {
            output: proposal_bytes(&fixture),
            calls: Cell::new(0),
        };
        let result =
            replan_goal(&fixture.store, &fixture.session, &fixture.goal_id, &backend).unwrap();
        assert_eq!(backend.calls.get(), 1);
        assert_eq!(result.revision(), 2);
        assert_eq!(
            fs::read(fixture.repo.join("sentinel.txt")).unwrap(),
            sentinel
        );
        assert!(
            result
                .tasks()
                .values()
                .all(|task| task.status() != TaskStatus::Running)
        );
    }

    #[test]
    fn forbidden_rewrite_surfaces_are_rejected_without_durable_mutation() {
        let forbidden_fields = [
            "delete_tasks",
            "remove_dependencies",
            "replace_verification",
            "mandatory_to_optional",
            "modify_completed_tasks",
            "rewrite_attempts",
            "delete_evidence",
            "budget_overrides",
            "scope_updates",
            "remove_forbidden_paths",
            "operation_identity_reset",
            "side_effect_state_reset",
        ];
        for field in forbidden_fields {
            let fixture = fixture();
            let before = bytes(&fixture);
            let mut value = proposal_value(&fixture);
            value
                .as_object_mut()
                .unwrap()
                .insert(field.to_owned(), json!([]));
            let result = apply(&fixture, &serde_json::to_vec(&value).unwrap());
            assert!(
                matches!(result, Err(ReplannerError::ReplannerSchemaViolation(_))),
                "field {field}: {result:?}"
            );
            assert_eq!(
                bytes(&fixture),
                before,
                "field {field} changed durable bytes"
            );
        }
    }

    #[test]
    fn task_state_attempt_budget_and_verification_result_injection_are_rejected() {
        for field in [
            "status",
            "attempts",
            "max_attempts",
            "verification_results",
            "evidence",
        ] {
            let fixture = fixture();
            let before = bytes(&fixture);
            let mut value = proposal_value(&fixture);
            let task = value["add_tasks"][0].as_object_mut().unwrap();
            task.insert(
                field.to_owned(),
                json!(if field == "status" {
                    "COMPLETED"
                } else {
                    "injected"
                }),
            );
            let result = apply(&fixture, &serde_json::to_vec(&value).unwrap());
            assert!(
                matches!(result, Err(ReplannerError::ReplannerSchemaViolation(_))),
                "field {field}: {result:?}"
            );
            assert_eq!(bytes(&fixture), before);
        }
    }
    #[test]
    fn scope_widening_and_malformed_paths_are_rejected_nonmutating() {
        let fixture = fixture();
        let before = bytes(&fixture);
        let mut value = proposal_value(&fixture);
        value["add_tasks"][0]["scope"]["allowed_paths"] = json!(["../outside"]);
        let result = apply(&fixture, &serde_json::to_vec(&value).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplanAuthorityViolation(_))
        ));
        assert_eq!(bytes(&fixture), before);
    }

    #[test]
    fn unknown_worker_kind_is_strictly_rejected() {
        let fixture = fixture();
        let before = bytes(&fixture);
        let mut value = proposal_value(&fixture);
        value["add_tasks"][0]["worker"] = json!("ROOT_SHELL");
        let result = apply(&fixture, &serde_json::to_vec(&value).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplannerSchemaViolation(_))
        ));
        assert_eq!(bytes(&fixture), before);
    }

    #[test]
    fn known_but_non_plannable_workers_cannot_be_introduced_by_replanner() {
        for worker in ["LOCAL_OPERATION", "CODEX_REVIEWER", "VERIFIER"] {
            let fixture = fixture();
            let before = bytes(&fixture);
            let mut value = proposal_value(&fixture);
            value["add_tasks"][0]["worker"] = json!(worker);
            let result = apply(&fixture, &serde_json::to_vec(&value).unwrap());
            assert!(matches!(
                result,
                Err(ReplannerError::ReplannerSchemaViolation(_))
            ));
            assert_eq!(bytes(&fixture), before);
        }
    }

    #[test]
    fn missing_dependency_reference_is_rejected_nonmutating() {
        let fixture = fixture();
        let before = bytes(&fixture);
        let missing = TaskId::new();
        let mut value = proposal_value(&fixture);
        value["add_dependencies"][0]["dependency"] = existing_ref(&missing);
        let result = apply(&fixture, &serde_json::to_vec(&value).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplannerSchemaViolation(_))
        ));
        assert_eq!(bytes(&fixture), before);
    }

    #[test]
    fn duplicate_dependency_edge_is_rejected_nonmutating() {
        let fixture = fixture();
        let before = bytes(&fixture);
        let mut value = proposal_value(&fixture);
        let duplicate = value["add_dependencies"][0].clone();
        value["add_dependencies"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        let result = apply(&fixture, &serde_json::to_vec(&value).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplannerSchemaViolation(_))
        ));
        assert_eq!(bytes(&fixture), before);
    }

    #[test]
    fn proposal_local_id_collision_with_existing_task_id_is_rejected() {
        let fixture = fixture();
        let before = bytes(&fixture);
        let mut value = proposal_value(&fixture);
        value["add_tasks"][0]["proposal_id"] = json!(fixture.trigger_id.as_str());
        value["add_dependencies"][0]["dependency"] = new_ref(fixture.trigger_id.as_str());
        let result = apply(&fixture, &serde_json::to_vec(&value).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplannerSchemaViolation(_))
        ));
        assert_eq!(bytes(&fixture), before);
    }

    fn cycle_fixture() -> (Fixture, TaskId) {
        let root = std::env::temp_dir().join(format!("local-mcp-phase7-cycle-{}", Uuid::new_v4()));
        let repo = root.join("repo");
        let state = root.join("state");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("sentinel.txt"), b"unchanged\n").unwrap();
        let session = config::Session {
            id: format!("phase7-cycle-{}", Uuid::new_v4()),
            cwd: repo.clone(),
            permitted_directories: vec![repo.clone()],
        };
        let store = TaskStore::with_state_root(state.clone());
        let mut goal = Goal::new(
            session.id.clone(),
            repo.clone(),
            "cycle fixture",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        let trigger = Task::new(
            "trigger",
            "trigger",
            true,
            WorkerKind::CodexReadonly,
            scope(&repo, TaskOperationKind::ReadOnly),
            verification("cycle.trigger"),
            2,
            1,
            NOW,
        )
        .unwrap();
        let trigger_id = trigger.id().clone();
        let mut follower = Task::new(
            "follower",
            "follower",
            true,
            WorkerKind::CodexReadonly,
            scope(&repo, TaskOperationKind::ReadOnly),
            verification("cycle.follower"),
            2,
            1,
            NOW,
        )
        .unwrap();
        let follower_id = follower.id().clone();
        follower
            .strengthen_dependencies(
                vec![TaskDependency::completed(trigger_id.clone())],
                &BTreeSet::new(),
            )
            .unwrap();
        goal.materialize_initial_plan(vec![trigger, follower], NOW)
            .unwrap();
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
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        (
            Fixture {
                root,
                state,
                repo,
                session,
                store,
                goal_id,
                trigger_id,
            },
            follower_id,
        )
    }

    #[test]
    fn cycle_introduced_by_replan_is_rejected_atomically() {
        let (fixture, follower_id) = cycle_fixture();
        let before = bytes(&fixture);
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let value = json!({
            "goal_id": fixture.goal_id.as_str(),
            "base_goal_revision": goal.revision(),
            "base_plan_revision": goal.plan_revision(),
            "summary": "invalid cycle",
            "add_tasks": [],
            "add_dependencies": [{
                "task": existing_ref(&fixture.trigger_id),
                "dependency": existing_ref(&follower_id)
            }],
            "strengthen_verification": [],
            "strengthen_mandatory": [],
            "resolve_needs_replan": [fixture.trigger_id.as_str()]
        });
        let result = apply(&fixture, &serde_json::to_vec(&value).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::Store(OrchestratorError::InvalidDag(_)))
        ));
        assert_eq!(bytes(&fixture), before);
    }

    #[test]
    fn writer_concurrency_violation_is_rejected_atomically() {
        let fixture = fixture();
        let before = bytes(&fixture);
        let mut value = proposal_value(&fixture);
        value["add_tasks"] = json!([
            read_only_task("repair", vec![]),
            writer_task("writer-a", vec![]),
            writer_task("writer-b", vec![])
        ]);
        let result = apply(&fixture, &serde_json::to_vec(&value).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::Store(OrchestratorError::InvalidDag(_)))
        ));
        assert_eq!(bytes(&fixture), before);
    }

    #[test]
    fn stale_goal_revision_replan_is_rejected_without_rebase() {
        let fixture = fixture();
        let stale = proposal_bytes(&fixture);
        let original = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let advanced = fixture
            .store
            .mutate_goal_snapshot(
                &fixture.session.id,
                &fixture.goal_id,
                original.revision(),
                |goal, _| goal.add_blocker(GoalBlocker::new("RACE", "concurrent mutation", false)),
            )
            .unwrap();
        let authoritative = bytes(&fixture);
        let result = materialize_replan_output(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            original.revision(),
            original.plan_revision(),
            &stale,
        );
        assert!(matches!(
            result,
            Err(ReplannerError::RevisionConflict { .. })
        ));
        assert_eq!(bytes(&fixture), authoritative);
        assert_eq!(
            fixture
                .store
                .load_goal(&fixture.session.id, &fixture.goal_id)
                .unwrap(),
            advanced
        );
    }

    #[test]
    fn stale_plan_revision_is_rejected_nonmutating() {
        let fixture = fixture();
        let before = bytes(&fixture);
        let current = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let result = materialize_replan_output(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            current.revision(),
            current.plan_revision() - 1,
            &proposal_bytes(&fixture),
        );
        assert!(matches!(result, Err(ReplannerError::PlanConflict { .. })));
        assert_eq!(bytes(&fixture), before);
    }

    #[test]
    fn duplicate_application_is_stale_and_does_not_duplicate_repair_task() {
        let fixture = fixture();
        let proposal = proposal_bytes(&fixture);
        let base = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let first = materialize_replan_output(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            base.revision(),
            base.plan_revision(),
            &proposal,
        )
        .unwrap();
        let bytes_after_first = bytes(&fixture);
        let second = materialize_replan_output(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            base.revision(),
            base.plan_revision(),
            &proposal,
        );
        assert!(matches!(
            second,
            Err(ReplannerError::RevisionConflict { .. })
        ));
        assert_eq!(bytes(&fixture), bytes_after_first);
        assert_eq!(
            first
                .tasks()
                .values()
                .filter(|task| task.title() == "Task repair")
                .count(),
            1
        );
    }

    #[test]
    fn persistence_failure_before_replace_preserves_authoritative_bytes_and_revisions() {
        let fixture = fixture();
        let before = bytes(&fixture);
        let base = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let fault_store = TaskStore::with_fault(fixture.state.clone(), FaultPoint::BeforeReplace);
        let result = materialize_replan_output(
            &fault_store,
            &fixture.session,
            &fixture.goal_id,
            base.revision(),
            base.plan_revision(),
            &proposal_bytes(&fixture),
        );
        assert!(matches!(
            result,
            Err(ReplannerError::Store(OrchestratorError::PersistenceIo(_)))
        ));
        assert_eq!(bytes(&fixture), before);
        let reloaded = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        assert_eq!(reloaded.revision(), base.revision());
        assert_eq!(reloaded.plan_revision(), base.plan_revision());
        assert_eq!(reloaded.tasks().len(), base.tasks().len());
        let dir = goal_path(&fixture).parent().unwrap().to_path_buf();
        assert!(fs::read_dir(dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn unknown_side_effect_cannot_be_erased_by_replanner() {
        let fixture = base_fixture(true);
        let before = bytes(&fixture);
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        assert!(matches!(
            replanner_request_for_goal(&goal, &fixture.session),
            Err(ReplannerError::NoSafeReplan(_))
        ));
        let result = apply(&fixture, &proposal_bytes(&fixture));
        assert!(matches!(result, Err(ReplannerError::NoSafeReplan(_))));
        assert_eq!(bytes(&fixture), before);
        let reloaded = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let task = &reloaded.tasks()[&fixture.trigger_id];
        assert_eq!(task.status(), TaskStatus::NeedsReplan);
        assert_eq!(
            task.latest_attempt().unwrap().side_effect_state(),
            Some(SideEffectState::Unknown)
        );
    }
    fn completed_history_fixture() -> (Fixture, TaskId) {
        let root =
            std::env::temp_dir().join(format!("local-mcp-phase7-completed-{}", Uuid::new_v4()));
        let repo = root.join("repo");
        let state = root.join("state");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("sentinel.txt"), b"unchanged\n").unwrap();
        let session = config::Session {
            id: format!("phase7-completed-{}", Uuid::new_v4()),
            cwd: repo.clone(),
            permitted_directories: vec![repo.clone()],
        };
        let store = TaskStore::with_state_root(state.clone());
        let mut goal = Goal::new(
            session.id.clone(),
            repo.clone(),
            "completed history fixture",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        let completed = Task::new(
            "completed",
            "historical completed task",
            true,
            WorkerKind::CodexReadonly,
            scope(&repo, TaskOperationKind::ReadOnly),
            verification("completed.base"),
            2,
            1,
            NOW,
        )
        .unwrap();
        let completed_id = completed.id().clone();
        let trigger = Task::new(
            "trigger",
            "needs repair",
            true,
            WorkerKind::CodexReadonly,
            scope(&repo, TaskOperationKind::ReadOnly),
            verification("trigger.base"),
            2,
            1,
            NOW,
        )
        .unwrap();
        let trigger_id = trigger.id().clone();
        goal.materialize_initial_plan(vec![completed, trigger], NOW)
            .unwrap();

        goal.transition_task(
            &completed_id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.task_add_evidence(
            &completed_id,
            TaskEvidence::StructuredObservation {
                requirement_id: "completed.evidence".to_owned(),
                source: "fixture".to_owned(),
                passed: true,
                detail: "historical proof".to_owned(),
            },
        )
        .unwrap();
        goal.transition_task(
            &completed_id,
            TaskStatus::Verifying,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.task_record_verification_result(
            &completed_id,
            VerificationResult::new(
                VerificationOutcome::Passed,
                vec![VerificationCheckResult::new(0, true, Some("ok".to_owned()))],
                NOW,
                NOW,
            ),
        )
        .unwrap();
        goal.transition_task(
            &completed_id,
            TaskStatus::Completed,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();

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
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        (
            Fixture {
                root,
                state,
                repo,
                session,
                store,
                goal_id,
                trigger_id,
            },
            completed_id,
        )
    }

    #[test]
    fn completed_task_history_is_byte_semantically_immutable_when_follow_up_is_added() {
        let (fixture, completed_id) = completed_history_fixture();
        let before = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let completed_before = before.tasks()[&completed_id].clone();
        let mut value = proposal_value(&fixture);
        value["add_tasks"][0]["dependencies"] = json!([existing_ref(&completed_id)]);
        let result = apply(&fixture, &serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(result.tasks()[&completed_id], completed_before);
        assert_eq!(
            result.tasks()[&completed_id].status(),
            TaskStatus::Completed
        );
    }

    #[test]
    fn completed_task_modification_is_rejected_nonmutating() {
        let (fixture, completed_id) = completed_history_fixture();
        let before = bytes(&fixture);
        let mut value = proposal_value(&fixture);
        value["add_dependencies"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "task": existing_ref(&completed_id),
                "dependency": new_ref("repair")
            }));
        let result = apply(&fixture, &serde_json::to_vec(&value).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplanAuthorityViolation(_))
        ));
        assert_eq!(bytes(&fixture), before);
    }

    #[test]
    fn verification_can_only_be_strengthened_and_optional_can_become_mandatory() {
        let root =
            std::env::temp_dir().join(format!("local-mcp-phase7-strengthen-{}", Uuid::new_v4()));
        let repo = root.join("repo");
        let state = root.join("state");
        fs::create_dir_all(&repo).unwrap();
        let session = config::Session {
            id: format!("phase7-strengthen-{}", Uuid::new_v4()),
            cwd: repo.clone(),
            permitted_directories: vec![repo.clone()],
        };
        let store = TaskStore::with_state_root(state.clone());
        let mut goal = Goal::new(
            session.id.clone(),
            repo.clone(),
            "strengthen",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        let anchor = Task::new(
            "anchor",
            "mandatory anchor",
            true,
            WorkerKind::CodexReadonly,
            scope(&repo, TaskOperationKind::ReadOnly),
            verification("anchor"),
            2,
            1,
            NOW,
        )
        .unwrap();
        let target = Task::new(
            "optional-target",
            "optional target",
            false,
            WorkerKind::CodexReadonly,
            scope(&repo, TaskOperationKind::ReadOnly),
            verification("target.base"),
            2,
            1,
            NOW,
        )
        .unwrap();
        let target_id = target.id().clone();
        goal.materialize_initial_plan(vec![anchor, target], NOW)
            .unwrap();
        goal.transition_task(
            &target_id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.transition_task(
            &target_id,
            TaskStatus::NeedsReplan,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let fixture = Fixture {
            root,
            state,
            repo,
            session,
            store,
            goal_id,
            trigger_id: target_id.clone(),
        };
        let current = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let value = json!({
            "goal_id": fixture.goal_id.as_str(),
            "base_goal_revision": current.revision(),
            "base_plan_revision": current.plan_revision(),
            "summary": "strengthen only",
            "add_tasks": [read_only_task("repair", vec![])],
            "add_dependencies": [{"task": existing_ref(&target_id), "dependency": new_ref("repair")}],
            "strengthen_verification": [{
                "task_id": target_id.as_str(),
                "add": [{"kind": "STRUCTURED_EVIDENCE", "requirement_id": "target.extra"}]
            }],
            "strengthen_mandatory": [{"task_id": target_id.as_str()}],
            "resolve_needs_replan": [target_id.as_str()]
        });
        let result = apply(&fixture, &serde_json::to_vec(&value).unwrap()).unwrap();
        let target = &result.tasks()[&target_id];
        assert!(target.mandatory());
        assert_eq!(target.verification_specs().len(), 2);
        assert!(target.verification_specs().iter().any(|spec| matches!(spec, VerificationSpec::StructuredEvidence { requirement_id } if requirement_id == "target.base")));
        assert!(target.verification_specs().iter().any(|spec| matches!(spec, VerificationSpec::StructuredEvidence { requirement_id } if requirement_id == "target.extra")));
    }

    #[test]
    fn ready_writer_is_materialized_but_never_executed_or_scheduled() {
        let fixture = fixture();
        let sentinel = fs::read(fixture.repo.join("sentinel.txt")).unwrap();
        let mut value = proposal_value(&fixture);
        value["add_tasks"] = json!([
            read_only_task("repair", vec![]),
            writer_task("future-writer", vec![])
        ]);
        let result = apply(&fixture, &serde_json::to_vec(&value).unwrap()).unwrap();
        let writer = result
            .tasks()
            .values()
            .find(|task| task.title() == "Task future-writer")
            .unwrap();
        assert_eq!(writer.status(), TaskStatus::Ready);
        assert!(writer.attempts().is_empty());
        assert_eq!(writer.evidence_count(), 0);
        assert!(
            result
                .tasks()
                .values()
                .all(|task| task.status() != TaskStatus::Running)
        );
        assert_eq!(result.status(), GoalStatus::Running);
        assert_ne!(result.status(), GoalStatus::Completed);
        assert_eq!(
            fs::read(fixture.repo.join("sentinel.txt")).unwrap(),
            sentinel
        );
    }

    #[test]
    fn blocked_task_without_needs_replan_is_not_replanner_eligible() {
        let root =
            std::env::temp_dir().join(format!("local-mcp-phase7-blocked-{}", Uuid::new_v4()));
        let repo = root.join("repo");
        let state = root.join("state");
        fs::create_dir_all(&repo).unwrap();
        let session = config::Session {
            id: format!("phase7-blocked-{}", Uuid::new_v4()),
            cwd: repo.clone(),
            permitted_directories: vec![repo.clone()],
        };
        let _store = TaskStore::with_state_root(state);
        let mut goal = Goal::new(
            session.id.clone(),
            repo.clone(),
            "blocked",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap();
        let task = Task::new(
            "blocked",
            "blocked",
            true,
            WorkerKind::CodexReadonly,
            scope(&repo, TaskOperationKind::ReadOnly),
            verification("blocked"),
            2,
            1,
            NOW,
        )
        .unwrap();
        let id = task.id().clone();
        goal.materialize_initial_plan(vec![task], NOW).unwrap();
        goal.transition_task(
            &id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.transition_task(
            &id,
            TaskStatus::Blocked,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        assert!(matches!(
            replanner_request_for_goal(&goal, &session),
            Err(ReplannerError::ReplanNotApplicable(_))
        ));
    }

    #[test]
    fn no_verifier_bypass_or_goal_completion_surface_exists_in_strict_schema() {
        for (field, injected) in [
            (
                "task_state_resolution_hints",
                json!([{"task_id":"x","status":"COMPLETED"}]),
            ),
            ("verification_results", json!([{"outcome":"PASSED"}])),
            ("goal_status", json!("COMPLETED")),
        ] {
            let fixture = fixture();
            let before = bytes(&fixture);
            let mut value = proposal_value(&fixture);
            value
                .as_object_mut()
                .unwrap()
                .insert(field.to_owned(), injected);
            let result = apply(&fixture, &serde_json::to_vec(&value).unwrap());
            assert!(matches!(
                result,
                Err(ReplannerError::ReplannerSchemaViolation(_))
            ));
            assert_eq!(bytes(&fixture), before);
        }
    }

    #[test]
    fn proposal_size_and_dependency_limits_match_phase4_safety_bounds() {
        let first = fixture();
        let before = bytes(&first);
        let oversized = vec![b' '; planner::MAX_PLAN_PROPOSAL_BYTES + 1];
        let result = apply(&first, &oversized);
        assert!(matches!(
            result,
            Err(ReplannerError::ReplannerSchemaViolation(_))
        ));
        assert_eq!(bytes(&first), before);

        let task_limit = fixture();
        let before = bytes(&task_limit);
        let mut value = proposal_value(&task_limit);
        value["add_tasks"] = Value::Array(
            (0..planner::MAX_PLAN_TASKS)
                .map(|index| read_only_task(&format!("extra-{index}"), vec![]))
                .collect(),
        );
        let result = apply(&task_limit, &serde_json::to_vec(&value).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplannerSchemaViolation(_))
        ));
        assert_eq!(bytes(&task_limit), before);

        let second = fixture();
        let before = bytes(&second);
        let mut value = proposal_value(&second);
        value["add_tasks"][0]["dependencies"] = Value::Array(
            (0..=planner::MAX_DEPENDENCIES_PER_TASK)
                .map(|_| existing_ref(&second.trigger_id))
                .collect(),
        );
        let result = apply(&second, &serde_json::to_vec(&value).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplannerSchemaViolation(_))
        ));
        assert_eq!(bytes(&second), before);
    }

    #[test]
    fn malformed_existing_task_identifier_is_rejected() {
        let malformed = fixture();
        let before = bytes(&malformed);
        let mut value = proposal_value(&malformed);
        value["add_dependencies"][0]["task"] =
            json!({"ref_kind":"EXISTING", "task_id":"not-a-uuid"});
        let result = apply(&malformed, &serde_json::to_vec(&value).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplannerSchemaViolation(_))
        ));
        assert_eq!(bytes(&malformed), before);

        let nested = fixture();
        let before = bytes(&nested);
        let mut value = proposal_value(&nested);
        value["add_dependencies"][0]["task"]["authority"] = json!("injected");
        let result = apply(&nested, &serde_json::to_vec(&value).unwrap());
        assert!(matches!(
            result,
            Err(ReplannerError::ReplannerSchemaViolation(_))
        ));
        assert_eq!(bytes(&nested), before);
    }
}
