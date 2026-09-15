use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::AgentError;
use crate::config;
use crate::goal::{
    CompletionCriterionId, Goal, GoalId, GoalStatus, GoalVerificationRequirement, ReplanMutation,
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
    goal_blockers: Vec<ReplannerBlocker>,
    eligible_needs_replan_task_ids: Vec<String>,
    allowed_worker_kinds: Vec<WorkerKind>,
    allowed_operation_kinds: Vec<TaskOperationKind>,
    prohibited_operations: Vec<&'static str>,
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
        goal_blockers: Vec::new(),
        eligible_needs_replan_task_ids: Vec::new(),
        allowed_worker_kinds: Vec::new(),
        allowed_operation_kinds: Vec::new(),
        prohibited_operations: Vec::new(),
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
    let goal_root = planner::validate_session_goal_binding(goal, session)
        .map_err(map_planner_validation_error)?;

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
    let goal_root = planner::validate_session_goal_binding(&current, session)
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
    let proposal: ReplanProposal = serde_json::from_value(raw).map_err(|_| {
        ReplannerError::ReplannerSchemaViolation(
            "proposal shape does not match the strict Phase 7 schema".to_owned(),
        )
    })?;

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

    let change_count = proposal.add_tasks.len()
        + proposal.add_dependencies.len()
        + proposal.strengthen_verification.len()
        + proposal.strengthen_mandatory.len()
        + proposal.strengthen_criterion_bindings.len()
        + proposal.resolve_needs_replan.len();
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
            "schema-2 replan requires an existing Goal final-verification contract".to_owned(),
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
        validated_resolution.push(task_id);
    }

    Ok(ValidatedReplan {
        new_tasks: validated_new_tasks,
        add_dependencies: validated_add_dependencies,
        add_verification: validated_verification,
        strengthen_mandatory: validated_mandatory,
        add_criterion_requirements: validated_criterion_requirements,
        resolve_needs_replan: validated_resolution,
    })
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
    use crate::goal::{CheckpointReason, GoalBlocker};
    use crate::task::{
        ReplaySafety, TaskDependency, TaskEvidence, TaskScope, TaskTransitionContext,
        VerificationCheckResult, VerificationOutcome,
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
