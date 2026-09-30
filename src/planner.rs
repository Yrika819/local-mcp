use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::AgentError;
use crate::config;
use crate::goal::{
    CompletionCriterionId, Goal, GoalCriterionBinding, GoalFinalVerificationSpec, GoalId,
    GoalStatus, GoalVerificationRequirement,
};
use crate::orchestrator_error::OrchestratorError;
use crate::task::{
    ReplaySafety, Task, TaskDependency, TaskId, TaskOperationKind, TaskScope, VerificationSpec,
    WorkerKind,
};
use crate::task_store::TaskStore;
use crate::worker_capability;

pub(crate) const MAX_PLAN_PROPOSAL_BYTES: usize = 256 * 1024;
pub(crate) const MAX_PLAN_TASKS: usize = 128;
pub(crate) const MAX_PLAN_DEPENDENCY_EDGES: usize = 1024;
pub(crate) const MAX_PLAN_TITLE_BYTES: usize = 256;
pub(crate) const MAX_PLAN_OBJECTIVE_BYTES: usize = 16 * 1024;
pub(crate) const MAX_PLAN_SUMMARY_BYTES: usize = 8 * 1024;
pub(crate) const MAX_PROPOSAL_ID_BYTES: usize = 64;
pub(crate) const MAX_DEPENDENCIES_PER_TASK: usize = 64;
pub(crate) const MAX_SCOPE_PATHS_PER_KIND: usize = 64;
pub(crate) const MAX_SCOPE_PATHS_TOTAL: usize = 1024;
pub(crate) const MAX_VERIFICATION_PER_TASK: usize = 32;
pub(crate) const MAX_VERIFICATION_TOTAL: usize = 1024;
pub(crate) const MAX_PATH_BYTES: usize = 4096;
pub(crate) const MAX_COMMAND_ARGS: usize = 64;
pub(crate) const MAX_COMMAND_ARG_BYTES: usize = 8192;
pub(crate) const MAX_EVIDENCE_REQUIREMENT_ID_BYTES: usize = 128;

/// Host-visible compact read-only output contract. Reused verbatim by the
/// read-only worker so the declared contract and the enforced bounds cannot
/// drift apart.
pub(crate) const MAX_READONLY_EVIDENCE_ITEMS: usize = 32;
pub(crate) const MAX_READONLY_SUMMARY_BYTES: usize = 16 * 1024;
pub(crate) const MAX_READONLY_EVIDENCE_FIELD_BYTES: usize = 8 * 1024;
pub(crate) const MAX_READONLY_EVIDENCE_TOTAL_BYTES: usize = 64 * 1024;
pub(crate) const READONLY_OUTPUT_CONTRACT_GUIDANCE: &str = "return at most 32 compact structured evidence records, each at most 8 KiB and 64 KiB in total, with a summary of at most 16 KiB; prefer artifact/file references and short synthesis over repeated large source excerpts";

/// Deterministic conservative bound on the structured records one bounded
/// read-only Task may be expected to return. Above this, a single Task is
/// treated as structurally too broad and must be decomposed.
pub(crate) const MAX_SINGLE_READONLY_TASK_RECORDS: usize = 24;

/// Minimum number of Tasks in a size/budget-triggered replacement closure.
pub(crate) const MIN_DECOMPOSED_REPLACEMENT_TASKS: usize = 3;

/// Tighter objective bound for replacement Tasks, so a decomposition cannot
/// reintroduce one giant broad objective.
pub(crate) const MAX_REPLACEMENT_OBJECTIVE_BYTES: usize = 4 * 1024;

const INITIAL_PLAN_REVISION: u32 = 1;
pub(crate) const READ_ONLY_MAX_ATTEMPTS: u32 = 2;
pub(crate) const EFFECTFUL_MAX_ATTEMPTS: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct PlannerRequest {
    goal_id: String,
    goal_revision: u64,
    objective: String,
    title: Option<String>,
    constraints: Vec<String>,
    completion_criteria: Vec<PlannerCriterion>,
    cwd: PathBuf,
    permitted_roots: Vec<PathBuf>,
    existing_blockers: Vec<PlannerBlocker>,
    sizing: crate::replanner::TaskSizingProfile,
    readonly_output_contract: PlannerReadonlyOutputContract,
    allowed_worker_kinds: Vec<WorkerKind>,
    allowed_operation_kinds: Vec<TaskOperationKind>,
    prohibited_operations: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct PlannerReadonlyOutputContract {
    max_evidence_items: usize,
    max_summary_bytes: usize,
    max_evidence_field_bytes: usize,
    max_evidence_total_bytes: usize,
    guidance: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct PlannerCriterion {
    criterion_id: String,
    description: String,
    required: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct PlannerBlocker {
    code: String,
    detail: String,
    mandatory: bool,
}

pub(crate) trait PlannerBackend {
    fn propose_initial_plan(&self, request: &PlannerRequest) -> Result<Vec<u8>, PlannerError>;
}

impl PlannerRequest {
    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }
}

#[derive(Debug)]
pub(crate) enum PlannerError {
    PlannerUnavailable,
    Model(AgentError),
    PlannerOutputInvalid(String),
    PlannerSchemaViolation(String),
    PlanConflict { expected: u64, actual: u64 },
    PlanNotApplicable(String),
    PlanAuthorityViolation(String),
    Store(OrchestratorError),
}

impl fmt::Display for PlannerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlannerUnavailable => write!(f, "planner backend is unavailable"),
            Self::Model(error) => write!(f, "planner model invocation failed: {error}"),
            Self::PlannerOutputInvalid(reason) => write!(f, "planner output is invalid: {reason}"),
            Self::PlannerSchemaViolation(reason) => {
                write!(f, "planner proposal violates the Phase 4 schema: {reason}")
            }
            Self::PlanConflict { expected, actual } => write!(
                f,
                "plan conflict: proposal was based on Goal revision {expected}, current revision is {actual}"
            ),
            Self::PlanNotApplicable(reason) => write!(f, "plan is not applicable: {reason}"),
            Self::PlanAuthorityViolation(reason) => {
                write!(f, "plan exceeds Goal/session authority: {reason}")
            }
            Self::Store(error) => write!(f, "durable Goal operation failed: {error}"),
        }
    }
}

impl std::error::Error for PlannerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanProposal {
    goal_id: String,
    goal_revision: u64,
    summary: String,
    tasks: Vec<PlanTaskProposal>,
    criterion_bindings: Vec<PlanCriterionBindingProposal>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanCriterionBindingProposal {
    criterion_id: String,
    task_refs: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanTaskProposal {
    proposal_id: String,
    title: String,
    objective: String,
    mandatory: bool,
    worker: WorkerKind,
    #[serde(default)]
    dependencies: Vec<String>,
    scope: TaskScopeProposal,
    verification: Vec<VerificationSpec>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskScopeProposal {
    pub(crate) allowed_paths: Vec<PathBuf>,
    #[serde(default)]
    pub(crate) forbidden_paths: Vec<PathBuf>,
    pub(crate) operation_kind: TaskOperationKind,
    pub(crate) replay_safety: ReplaySafety,
}

#[derive(Clone, Debug)]
struct ValidatedPlan {
    tasks: Vec<ValidatedTask>,
    criterion_bindings: Vec<(CompletionCriterionId, Vec<String>)>,
}

#[derive(Clone, Debug)]
struct ValidatedTask {
    proposal_id: String,
    title: String,
    objective: String,
    mandatory: bool,
    worker: WorkerKind,
    dependencies: Vec<String>,
    scope: TaskScope,
    verification: Vec<VerificationSpec>,
    max_attempts: u32,
}

pub(crate) fn planner_request_for_goal(
    goal: &Goal,
    session: &config::Session,
) -> Result<PlannerRequest, PlannerError> {
    ensure_initial_planning_state(goal)?;
    validate_session_goal_binding(goal, session)?;
    let goal_root = execution_root_for_goal(goal, session)?;

    let mut permitted_roots = Vec::new();
    if goal.workspace_mode().is_managed() {
        permitted_roots.push(goal_root.clone());
    } else {
        for root in std::iter::once(&session.cwd).chain(session.permitted_directories.iter()) {
            if let Ok(canonical) = fs::canonicalize(root)
                && !permitted_roots.contains(&canonical)
            {
                permitted_roots.push(canonical);
            }
        }
    }
    if !permitted_roots
        .iter()
        .any(|root| goal_root.starts_with(root))
    {
        return Err(PlannerError::PlanAuthorityViolation(
            "Goal cwd is not inside the current session authority".to_owned(),
        ));
    }

    Ok(PlannerRequest {
        goal_id: goal.id().as_str().to_owned(),
        goal_revision: goal.revision(),
        objective: goal.objective().to_owned(),
        title: goal.title().map(str::to_owned),
        constraints: goal.constraints().to_vec(),
        completion_criteria: goal
            .completion_criteria()
            .iter()
            .map(|criterion| PlannerCriterion {
                criterion_id: criterion.id().as_str().to_owned(),
                description: criterion.description().to_owned(),
                required: criterion.required(),
            })
            .collect(),
        cwd: goal_root,
        permitted_roots,
        existing_blockers: goal
            .blockers()
            .iter()
            .map(|blocker| PlannerBlocker {
                code: blocker.code().to_owned(),
                detail: blocker.detail().to_owned(),
                mandatory: blocker.mandatory(),
            })
            .collect(),
        allowed_worker_kinds: worker_capability::production_plannable_worker_kinds().to_vec(),
        allowed_operation_kinds: vec![
            TaskOperationKind::ReadOnly,
            TaskOperationKind::LocalMutation,
            TaskOperationKind::HostNativeApproved,
        ],
        prohibited_operations: vec![
            "task execution during planning",
            "approval or sandbox bypass",
            "automatic commit, push, merge, release, or publication",
            "planner-authored durable IDs, revisions, statuses, attempts, evidence, or verification results",
        ],
        sizing: task_sizing_profile_for_goal(goal),
        readonly_output_contract: PlannerReadonlyOutputContract {
            max_evidence_items: MAX_READONLY_EVIDENCE_ITEMS,
            max_summary_bytes: MAX_READONLY_SUMMARY_BYTES,
            max_evidence_field_bytes: MAX_READONLY_EVIDENCE_FIELD_BYTES,
            max_evidence_total_bytes: MAX_READONLY_EVIDENCE_TOTAL_BYTES,
            guidance: READONLY_OUTPUT_CONTRACT_GUIDANCE,
        },
    })
}

/// Derive the deterministic, host-owned Task-sizing profile for a Goal.
///
/// The counts are purely structural: independent entities come from the
/// distinct requested-evidence requirements across the Goal's criteria, and
/// evidence dimensions come from the distinct host-owned evidence/verification
/// requirements. Nothing here predicts tokens, so the result is reproducible
/// and auditable. A Goal that has not been planned yet falls back to its
/// criteria, which is the only sizing signal available at planning time.
pub(crate) fn task_sizing_profile_for_goal(goal: &Goal) -> crate::replanner::TaskSizingProfile {
    let entity_count = goal
        .completion_criteria()
        .iter()
        .map(|criterion| criterion.description().trim())
        .filter(|description| !description.is_empty())
        .collect::<BTreeSet<_>>()
        .len()
        .max(1);
    let dimension_count = goal
        .tasks()
        .values()
        .flat_map(|task| task.verification_specs().iter())
        .filter_map(|spec| match spec {
            VerificationSpec::StructuredEvidence { requirement_id } => {
                Some(requirement_id.as_str())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>()
        .len()
        .max(1);
    crate::replanner::TaskSizingProfile::derive(entity_count, dimension_count)
}

#[allow(
    dead_code,
    reason = "Frozen planner entrypoint is retained for staged Goal Orchestrator integration."
)]
pub(crate) fn plan_initial_goal<B: PlannerBackend>(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    planner: &B,
) -> Result<Goal, PlannerError> {
    let goal = store
        .load_goal(&session.id, goal_id)
        .map_err(map_store_error)?;
    let request = planner_request_for_goal(&goal, session)?;
    let output = planner.propose_initial_plan(&request)?;
    materialize_initial_plan_output(store, session, goal_id, goal.revision(), &output)
}

pub(crate) fn materialize_initial_plan_output(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    expected_goal_revision: u64,
    output: &[u8],
) -> Result<Goal, PlannerError> {
    if output.len() > MAX_PLAN_PROPOSAL_BYTES {
        return Err(PlannerError::PlannerSchemaViolation(
            "proposal exceeds the 256 KiB host limit".to_owned(),
        ));
    }

    let current = store
        .load_goal(&session.id, goal_id)
        .map_err(map_store_error)?;
    if current.revision() != expected_goal_revision {
        return Err(PlannerError::PlanConflict {
            expected: expected_goal_revision,
            actual: current.revision(),
        });
    }
    ensure_initial_planning_state(&current)?;
    let goal_root = execution_root_for_goal(&current, session)?;
    let validated = parse_and_validate_proposal(output, &current, &goal_root)?;

    store
        .mutate_goal_snapshot(
            &session.id,
            goal_id,
            expected_goal_revision,
            move |goal, now| materialize_validated_plan(goal, validated, now),
        )
        .map_err(map_store_error)
}

fn ensure_initial_planning_state(goal: &Goal) -> Result<(), PlannerError> {
    if goal.status() != GoalStatus::Planning {
        return Err(PlannerError::PlanNotApplicable(
            "initial planning requires a PLANNING Goal".to_owned(),
        ));
    }
    if goal.plan_revision() != 0 || !goal.tasks().is_empty() {
        return Err(PlannerError::PlanNotApplicable(
            "initial planning requires plan_revision 0 and an empty Task DAG".to_owned(),
        ));
    }
    // The caller immediately derives and validates the execution root before
    // constructing a request or materializing any model-proposed paths.
    Ok(())
}

/// Managed execution-root derivation is separate from primary Session identity.
pub(crate) fn execution_root_for_goal(
    goal: &Goal,
    session: &config::Session,
) -> Result<PathBuf, PlannerError> {
    crate::managed_worktree_prepare::validated_execution_root(goal, session).map_err(|detail| {
        PlannerError::PlanAuthorityViolation(format!("execution-root gate refused: {detail}"))
    })
}

pub(crate) fn validate_session_goal_binding(
    goal: &Goal,
    session: &config::Session,
) -> Result<PathBuf, PlannerError> {
    if goal.session_id() != session.id {
        return Err(PlannerError::PlanAuthorityViolation(
            "Goal does not belong to this session".to_owned(),
        ));
    }
    let goal_root = fs::canonicalize(goal.cwd()).map_err(|_| {
        PlannerError::PlanAuthorityViolation("Goal cwd cannot be canonicalized".to_owned())
    })?;
    let session_cwd = fs::canonicalize(&session.cwd).map_err(|_| {
        PlannerError::PlanAuthorityViolation("session cwd cannot be canonicalized".to_owned())
    })?;
    if goal_root != session_cwd {
        return Err(PlannerError::PlanAuthorityViolation(
            "Goal cwd no longer matches the bound session cwd".to_owned(),
        ));
    }
    let mut within_session = false;
    for root in std::iter::once(&session.cwd).chain(session.permitted_directories.iter()) {
        if let Ok(canonical) = fs::canonicalize(root)
            && goal_root.starts_with(canonical)
        {
            within_session = true;
            break;
        }
    }
    if !within_session {
        return Err(PlannerError::PlanAuthorityViolation(
            "Goal cwd is outside the current session authority".to_owned(),
        ));
    }
    Ok(goal_root)
}

fn parse_and_validate_proposal(
    output: &[u8],
    goal: &Goal,
    goal_root: &Path,
) -> Result<ValidatedPlan, PlannerError> {
    let raw: serde_json::Value = serde_json::from_slice(output).map_err(|error| {
        PlannerError::PlannerOutputInvalid(format!(
            "malformed JSON at line {}, column {}",
            error.line(),
            error.column()
        ))
    })?;
    let proposal: PlanProposal = serde_json::from_value(raw).map_err(|_| {
        PlannerError::PlannerSchemaViolation(
            "proposal shape does not match the strict Phase 4 schema".to_owned(),
        )
    })?;

    if proposal.goal_id != goal.id().as_str() {
        return Err(PlannerError::PlanConflict {
            expected: goal.revision(),
            actual: goal.revision(),
        });
    }
    if proposal.goal_revision != goal.revision() {
        return Err(PlannerError::PlanConflict {
            expected: proposal.goal_revision,
            actual: goal.revision(),
        });
    }
    if proposal.summary.trim().is_empty() || proposal.summary.len() > MAX_PLAN_SUMMARY_BYTES {
        return Err(PlannerError::PlannerSchemaViolation(
            "summary must be non-empty and at most 8192 bytes".to_owned(),
        ));
    }
    if proposal.tasks.is_empty() || proposal.tasks.len() > MAX_PLAN_TASKS {
        return Err(PlannerError::PlannerSchemaViolation(
            "initial plan must contain 1..=128 Tasks".to_owned(),
        ));
    }

    let mut ids = BTreeSet::new();
    let mut dependency_edges = 0usize;
    let mut scope_paths = 0usize;
    let mut verification_entries = 0usize;
    let mut validated = Vec::with_capacity(proposal.tasks.len());

    for task in proposal.tasks {
        validate_proposal_id(&task.proposal_id)?;
        if !ids.insert(task.proposal_id.clone()) {
            return Err(PlannerError::PlannerSchemaViolation(
                "duplicate proposal_id".to_owned(),
            ));
        }
        validate_text(&task.title, MAX_PLAN_TITLE_BYTES, "Task title")?;
        validate_text(&task.objective, MAX_PLAN_OBJECTIVE_BYTES, "Task objective")?;
        if !worker_capability::is_production_plannable(task.worker) {
            return Err(PlannerError::PlannerSchemaViolation(
                "Task worker is not production-plannable".to_owned(),
            ));
        }
        if task.dependencies.len() > MAX_DEPENDENCIES_PER_TASK {
            return Err(PlannerError::PlannerSchemaViolation(
                "a Task has too many dependencies".to_owned(),
            ));
        }
        let mut local_dependencies = BTreeSet::new();
        for dependency in &task.dependencies {
            validate_proposal_id(dependency)?;
            if dependency == &task.proposal_id {
                return Err(PlannerError::PlannerSchemaViolation(
                    "Task cannot depend on itself".to_owned(),
                ));
            }
            if !local_dependencies.insert(dependency.clone()) {
                return Err(PlannerError::PlannerSchemaViolation(
                    "duplicate dependency edge".to_owned(),
                ));
            }
        }
        dependency_edges = dependency_edges
            .checked_add(task.dependencies.len())
            .ok_or_else(|| {
                PlannerError::PlannerSchemaViolation("dependency count overflow".to_owned())
            })?;
        if dependency_edges > MAX_PLAN_DEPENDENCY_EDGES {
            return Err(PlannerError::PlannerSchemaViolation(
                "plan exceeds the 1024 dependency-edge limit".to_owned(),
            ));
        }

        scope_paths = scope_paths
            .checked_add(task.scope.allowed_paths.len() + task.scope.forbidden_paths.len())
            .ok_or_else(|| {
                PlannerError::PlannerSchemaViolation("scope count overflow".to_owned())
            })?;
        if scope_paths > MAX_SCOPE_PATHS_TOTAL {
            return Err(PlannerError::PlannerSchemaViolation(
                "plan exceeds the 1024 scope-path limit".to_owned(),
            ));
        }
        verification_entries = verification_entries
            .checked_add(task.verification.len())
            .ok_or_else(|| {
                PlannerError::PlannerSchemaViolation("verification count overflow".to_owned())
            })?;
        if verification_entries > MAX_VERIFICATION_TOTAL {
            return Err(PlannerError::PlannerSchemaViolation(
                "plan exceeds the 1024 verification-entry limit".to_owned(),
            ));
        }

        let scope = validate_and_normalize_scope(&task.scope, task.worker, goal_root)?;
        if task.verification.is_empty() || task.verification.len() > MAX_VERIFICATION_PER_TASK {
            return Err(PlannerError::PlannerSchemaViolation(
                "each Task requires 1..=32 verification specifications".to_owned(),
            ));
        }
        let mut verification = Vec::with_capacity(task.verification.len());
        for spec in task.verification {
            verification.push(validate_and_normalize_verification(spec, goal_root)?);
        }

        let max_attempts = match scope.operation_kind() {
            TaskOperationKind::ReadOnly => READ_ONLY_MAX_ATTEMPTS,
            TaskOperationKind::LocalMutation | TaskOperationKind::HostNativeApproved => {
                EFFECTFUL_MAX_ATTEMPTS
            }
        };
        validated.push(ValidatedTask {
            proposal_id: task.proposal_id,
            title: task.title,
            objective: task.objective,
            mandatory: task.mandatory,
            worker: task.worker,
            dependencies: task.dependencies,
            scope,
            verification,
            max_attempts,
        });
    }

    if !validated.iter().any(|task| task.mandatory) {
        return Err(PlannerError::PlannerSchemaViolation(
            "initial plan requires at least one mandatory Task".to_owned(),
        ));
    }

    let by_id = validated
        .iter()
        .enumerate()
        .map(|(index, task)| (task.proposal_id.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    for task in &validated {
        for dependency in &task.dependencies {
            let dependency_index = by_id.get(dependency.as_str()).ok_or_else(|| {
                PlannerError::PlannerSchemaViolation("dependency target is missing".to_owned())
            })?;
            if task.mandatory && !validated[*dependency_index].mandatory {
                return Err(PlannerError::PlannerSchemaViolation(
                    "mandatory Task cannot depend on an optional Task".to_owned(),
                ));
            }
        }
    }

    validate_acyclic(&validated, &by_id)?;
    validate_writer_ordering(&validated, &by_id)?;

    let authoritative_criteria = goal
        .completion_criteria()
        .iter()
        .map(|criterion| (criterion.id().as_str(), criterion))
        .collect::<BTreeMap<_, _>>();
    let mut seen_criteria = BTreeSet::new();
    let mut criterion_bindings = Vec::with_capacity(proposal.criterion_bindings.len());
    for binding in proposal.criterion_bindings {
        let criterion = authoritative_criteria
            .get(binding.criterion_id.as_str())
            .ok_or_else(|| {
                PlannerError::PlannerSchemaViolation(
                    "criterion binding references an unknown host CompletionCriterionId".to_owned(),
                )
            })?;
        if !seen_criteria.insert(binding.criterion_id.clone()) {
            return Err(PlannerError::PlannerSchemaViolation(
                "duplicate criterion binding".to_owned(),
            ));
        }
        if binding.task_refs.is_empty() {
            return Err(PlannerError::PlannerSchemaViolation(
                "required criterion binding must reference at least one proposal-local Task"
                    .to_owned(),
            ));
        }
        let mut refs = BTreeSet::new();
        for task_ref in &binding.task_refs {
            validate_proposal_id(task_ref)?;
            if !refs.insert(task_ref.clone()) {
                return Err(PlannerError::PlannerSchemaViolation(
                    "criterion binding contains duplicate proposal-local Task reference".to_owned(),
                ));
            }
            let task_index = by_id.get(task_ref.as_str()).ok_or_else(|| {
                PlannerError::PlannerSchemaViolation(
                    "criterion binding references an unknown proposal-local Task".to_owned(),
                )
            })?;
            if !validated[*task_index].mandatory {
                return Err(PlannerError::PlannerSchemaViolation(
                    "TaskVerified criterion binding requires a mandatory Task".to_owned(),
                ));
            }
            if validated[*task_index].verification.is_empty() {
                return Err(PlannerError::PlannerSchemaViolation("TaskVerified criterion binding requires mechanically evaluable Task verification".to_owned()));
            }
        }
        criterion_bindings.push((criterion.id().clone(), binding.task_refs));
    }
    for criterion in goal.completion_criteria() {
        if criterion.required() && !seen_criteria.contains(criterion.id().as_str()) {
            return Err(PlannerError::PlannerSchemaViolation(
                "required completion criterion is missing structured TaskVerified coverage"
                    .to_owned(),
            ));
        }
    }

    Ok(ValidatedPlan {
        tasks: validated,
        criterion_bindings,
    })
}

pub(crate) fn validate_proposal_id(value: &str) -> Result<(), PlannerError> {
    if value.is_empty()
        || value.len() > MAX_PROPOSAL_ID_BYTES
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(PlannerError::PlannerSchemaViolation(
            "proposal_id must be 1..=64 ASCII letters, numbers, '-' or '_'".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_text(
    value: &str,
    max_bytes: usize,
    label: &str,
) -> Result<(), PlannerError> {
    if value.trim().is_empty() || value.len() > max_bytes {
        return Err(PlannerError::PlannerSchemaViolation(format!(
            "{label} must be non-empty and at most {max_bytes} bytes"
        )));
    }
    Ok(())
}

pub(crate) fn validate_and_normalize_scope(
    proposal: &TaskScopeProposal,
    worker: WorkerKind,
    goal_root: &Path,
) -> Result<TaskScope, PlannerError> {
    if proposal.allowed_paths.is_empty()
        || proposal.allowed_paths.len() > MAX_SCOPE_PATHS_PER_KIND
        || proposal.forbidden_paths.len() > MAX_SCOPE_PATHS_PER_KIND
    {
        return Err(PlannerError::PlannerSchemaViolation(
            "scope requires 1..=64 allowed_paths and at most 64 forbidden_paths".to_owned(),
        ));
    }
    match proposal.operation_kind {
        TaskOperationKind::ReadOnly if proposal.replay_safety != ReplaySafety::SafeReadOnly => {
            return Err(PlannerError::PlannerSchemaViolation(
                "READ_ONLY scope must use SAFE_READ_ONLY replay safety".to_owned(),
            ));
        }
        TaskOperationKind::LocalMutation | TaskOperationKind::HostNativeApproved
            if proposal.replay_safety == ReplaySafety::SafeReadOnly =>
        {
            return Err(PlannerError::PlannerSchemaViolation(
                "effectful scope cannot claim SAFE_READ_ONLY replay safety".to_owned(),
            ));
        }
        _ => {}
    }
    if matches!(
        worker,
        WorkerKind::CodexReadonly | WorkerKind::CodexReviewer | WorkerKind::Verifier
    ) && proposal.operation_kind != TaskOperationKind::ReadOnly
    {
        return Err(PlannerError::PlanAuthorityViolation(
            "read-only/reviewer/verifier WorkerKind cannot carry mutation scope in Phase 4"
                .to_owned(),
        ));
    }

    let effectful = proposal.operation_kind != TaskOperationKind::ReadOnly;
    let allowed_paths = normalize_path_set(&proposal.allowed_paths, goal_root, effectful)?;
    let forbidden_paths = normalize_path_set(&proposal.forbidden_paths, goal_root, effectful)?;
    Ok(TaskScope::new(
        allowed_paths,
        forbidden_paths,
        proposal.operation_kind,
        proposal.replay_safety,
    ))
}

fn normalize_path_set(
    paths: &[PathBuf],
    goal_root: &Path,
    effectful: bool,
) -> Result<Vec<PathBuf>, PlannerError> {
    let mut normalized = Vec::with_capacity(paths.len());
    let mut seen = BTreeSet::new();
    for path in paths {
        let resolved = normalize_planner_path(path, goal_root, effectful)?;
        if !seen.insert(resolved.clone()) {
            return Err(PlannerError::PlannerSchemaViolation(
                "duplicate scope path after canonicalization".to_owned(),
            ));
        }
        normalized.push(resolved);
    }
    Ok(normalized)
}

fn normalize_planner_path(
    path: &Path,
    goal_root: &Path,
    effectful: bool,
) -> Result<PathBuf, PlannerError> {
    let display = path.to_string_lossy();
    if display.is_empty() || display.len() > MAX_PATH_BYTES {
        return Err(PlannerError::PlannerSchemaViolation(
            "path must be non-empty and at most 4096 bytes".to_owned(),
        ));
    }
    if display.starts_with(":(") {
        return Err(PlannerError::PlanAuthorityViolation(
            "Git pathspec magic is not allowed in Planner scope".to_owned(),
        ));
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(PlannerError::PlanAuthorityViolation(
            "parent-directory traversal is not allowed in Planner scope".to_owned(),
        ));
    }
    if effectful && contains_git_internal(path) {
        return Err(PlannerError::PlanAuthorityViolation(
            "effectful Planner scope cannot target .git internals".to_owned(),
        ));
    }

    let canonical_goal_root = config::canonical_path(goal_root).map_err(|_| {
        PlannerError::PlanAuthorityViolation(
            "Goal execution root cannot be canonicalized".to_owned(),
        )
    })?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        canonical_goal_root.join(path)
    };
    let resolved = canonicalize_existing_prefix(&absolute)?;
    if !resolved.starts_with(&canonical_goal_root) {
        return Err(PlannerError::PlanAuthorityViolation(
            "Task path escapes the Goal cwd".to_owned(),
        ));
    }
    if effectful && contains_git_internal(&resolved) {
        return Err(PlannerError::PlanAuthorityViolation(
            "effectful Planner scope cannot resolve through .git internals".to_owned(),
        ));
    }
    Ok(resolved)
}

fn canonicalize_existing_prefix(path: &Path) -> Result<PathBuf, PlannerError> {
    let mut existing = path.to_path_buf();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or_else(|| {
            PlannerError::PlanAuthorityViolation("path has no canonicalizable ancestor".to_owned())
        })?;
        suffix.push(name.to_os_string());
        if !existing.pop() {
            return Err(PlannerError::PlanAuthorityViolation(
                "path has no canonicalizable ancestor".to_owned(),
            ));
        }
    }
    let mut resolved = config::canonical_path(&existing).map_err(|_| {
        PlannerError::PlanAuthorityViolation("path ancestor cannot be canonicalized".to_owned())
    })?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn contains_git_internal(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::Normal(value) if value == ".git"))
}

pub(crate) fn validate_and_normalize_verification(
    spec: VerificationSpec,
    goal_root: &Path,
) -> Result<VerificationSpec, PlannerError> {
    match spec {
        VerificationSpec::CommandExit {
            command,
            cwd,
            accepted_exit_codes,
        } => {
            if command.is_empty() || command.len() > MAX_COMMAND_ARGS {
                return Err(PlannerError::PlannerSchemaViolation(
                    "COMMAND_EXIT requires 1..=64 argv entries".to_owned(),
                ));
            }
            if command
                .iter()
                .any(|arg| arg.is_empty() || arg.len() > MAX_COMMAND_ARG_BYTES)
            {
                return Err(PlannerError::PlannerSchemaViolation(
                    "COMMAND_EXIT argv entries must be non-empty and at most 8192 bytes".to_owned(),
                ));
            }
            if accepted_exit_codes.is_empty() || accepted_exit_codes.len() > 32 {
                return Err(PlannerError::PlannerSchemaViolation(
                    "COMMAND_EXIT requires 1..=32 accepted exit codes".to_owned(),
                ));
            }
            let unique = accepted_exit_codes.iter().copied().collect::<BTreeSet<_>>();
            if unique.len() != accepted_exit_codes.len() {
                return Err(PlannerError::PlannerSchemaViolation(
                    "COMMAND_EXIT contains duplicate accepted exit codes".to_owned(),
                ));
            }
            let cwd = cwd
                .map(|path| normalize_planner_path(&path, goal_root, false))
                .transpose()?;
            Ok(VerificationSpec::CommandExit {
                command,
                cwd,
                accepted_exit_codes,
            })
        }
        VerificationSpec::FileExists { path, must_be_file } => Ok(VerificationSpec::FileExists {
            path: normalize_planner_path(&path, goal_root, false)?,
            must_be_file,
        }),
        VerificationSpec::FileDigest {
            path,
            expected_sha256,
        } => {
            if expected_sha256.len() != 64
                || !expected_sha256
                    .chars()
                    .all(|character| character.is_ascii_hexdigit())
            {
                return Err(PlannerError::PlannerSchemaViolation(
                    "FILE_DIGEST expected_sha256 must contain exactly 64 hexadecimal characters"
                        .to_owned(),
                ));
            }
            Ok(VerificationSpec::FileDigest {
                path: normalize_planner_path(&path, goal_root, false)?,
                expected_sha256: expected_sha256.to_ascii_lowercase(),
            })
        }
        VerificationSpec::GitScope {
            allowed_changed_paths,
            require_no_other_changes,
        } => {
            if allowed_changed_paths.len() > MAX_SCOPE_PATHS_PER_KIND {
                return Err(PlannerError::PlannerSchemaViolation(
                    "GIT_SCOPE contains too many paths".to_owned(),
                ));
            }
            Ok(VerificationSpec::GitScope {
                allowed_changed_paths: normalize_path_set(
                    &allowed_changed_paths,
                    goal_root,
                    false,
                )?,
                require_no_other_changes,
            })
        }
        VerificationSpec::NoForbiddenChanges { forbidden_paths } => {
            if forbidden_paths.is_empty() || forbidden_paths.len() > MAX_SCOPE_PATHS_PER_KIND {
                return Err(PlannerError::PlannerSchemaViolation(
                    "NO_FORBIDDEN_CHANGES requires 1..=64 paths".to_owned(),
                ));
            }
            Ok(VerificationSpec::NoForbiddenChanges {
                forbidden_paths: normalize_path_set(&forbidden_paths, goal_root, false)?,
            })
        }
        VerificationSpec::StructuredEvidence { requirement_id } => {
            if requirement_id.is_empty()
                || requirement_id.len() > MAX_EVIDENCE_REQUIREMENT_ID_BYTES
                || !requirement_id.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
                })
            {
                return Err(PlannerError::PlannerSchemaViolation(
                    "STRUCTURED_EVIDENCE requirement_id is invalid".to_owned(),
                ));
            }
            Ok(VerificationSpec::StructuredEvidence { requirement_id })
        }
        VerificationSpec::ReviewGate {
            max_blocking_findings,
        } => {
            if max_blocking_findings > 1024 {
                return Err(PlannerError::PlannerSchemaViolation(
                    "REVIEW_GATE limit is unreasonably large".to_owned(),
                ));
            }
            Ok(VerificationSpec::ReviewGate {
                max_blocking_findings,
            })
        }
    }
}

fn validate_acyclic(
    tasks: &[ValidatedTask],
    by_id: &BTreeMap<&str, usize>,
) -> Result<(), PlannerError> {
    let mut indegree = vec![0usize; tasks.len()];
    let mut dependents = vec![Vec::new(); tasks.len()];
    for (task_index, task) in tasks.iter().enumerate() {
        for dependency in &task.dependencies {
            let dependency_index = *by_id.get(dependency.as_str()).ok_or_else(|| {
                PlannerError::PlannerSchemaViolation("dependency target is missing".to_owned())
            })?;
            indegree[task_index] += 1;
            dependents[dependency_index].push(task_index);
        }
    }
    let mut queue = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, degree)| (*degree == 0).then_some(index))
        .collect::<VecDeque<_>>();
    let mut visited = 0usize;
    while let Some(index) = queue.pop_front() {
        visited += 1;
        for dependent in &dependents[index] {
            indegree[*dependent] -= 1;
            if indegree[*dependent] == 0 {
                queue.push_back(*dependent);
            }
        }
    }
    if visited != tasks.len() {
        return Err(PlannerError::PlannerSchemaViolation(
            "Task proposal contains a dependency cycle".to_owned(),
        ));
    }
    Ok(())
}

fn validate_writer_ordering(
    tasks: &[ValidatedTask],
    by_id: &BTreeMap<&str, usize>,
) -> Result<(), PlannerError> {
    let writers = tasks
        .iter()
        .enumerate()
        .filter_map(|(index, task)| {
            (task.scope.operation_kind() != TaskOperationKind::ReadOnly).then_some(index)
        })
        .collect::<Vec<_>>();
    for left_index in 0..writers.len() {
        for right_index in left_index + 1..writers.len() {
            let left = writers[left_index];
            let right = writers[right_index];
            if !depends_on(left, right, tasks, by_id) && !depends_on(right, left, tasks, by_id) {
                return Err(PlannerError::PlannerSchemaViolation(
                    "mutation Tasks must be totally ordered by hard dependencies in V1".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn depends_on(
    task_index: usize,
    target_index: usize,
    tasks: &[ValidatedTask],
    by_id: &BTreeMap<&str, usize>,
) -> bool {
    let mut stack = vec![task_index];
    let mut seen = BTreeSet::new();
    while let Some(index) = stack.pop() {
        if !seen.insert(index) {
            continue;
        }
        for dependency in &tasks[index].dependencies {
            if let Some(dependency_index) = by_id.get(dependency.as_str()).copied() {
                if dependency_index == target_index {
                    return true;
                }
                stack.push(dependency_index);
            }
        }
    }
    false
}

fn materialize_validated_plan(
    goal: &mut Goal,
    plan: ValidatedPlan,
    now: &str,
) -> Result<(), OrchestratorError> {
    let ValidatedPlan {
        tasks,
        criterion_bindings,
    } = plan;
    let mut id_map = BTreeMap::<String, TaskId>::new();
    let mut materialized = Vec::with_capacity(tasks.len());
    for proposed in tasks {
        let task = Task::new(
            proposed.title,
            proposed.objective,
            proposed.mandatory,
            proposed.worker,
            proposed.scope,
            proposed.verification,
            proposed.max_attempts,
            INITIAL_PLAN_REVISION,
            now,
        )?;
        let id = task.id().clone();
        if id_map.insert(proposed.proposal_id.clone(), id).is_some() {
            return Err(OrchestratorError::InvalidDag(
                "duplicate proposal-local Task identity after validation".to_owned(),
            ));
        }
        materialized.push((proposed.proposal_id, proposed.dependencies, task));
    }

    let completed_dependencies = BTreeSet::new();
    for (_, dependencies, task) in &mut materialized {
        let dependencies = dependencies
            .iter()
            .map(|proposal_id| {
                id_map.get(proposal_id).cloned().ok_or_else(|| {
                    OrchestratorError::InvalidDag(
                        "validated dependency target disappeared".to_owned(),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(TaskDependency::completed)
            .collect::<Vec<_>>();
        task.strengthen_dependencies(dependencies, &completed_dependencies)?;
    }

    let bindings = criterion_bindings.into_iter().map(|(criterion_id, task_refs)| {
        let requirements = task_refs.into_iter().map(|proposal_id| {
            id_map.get(&proposal_id).cloned().map(|task_id| GoalVerificationRequirement::TaskVerified { task_id })
                .ok_or_else(|| OrchestratorError::InvalidDag("validated criterion Task reference disappeared during materialization".to_owned()))
        }).collect::<Result<Vec<_>, _>>()?;
        Ok(GoalCriterionBinding::new(criterion_id, requirements))
    }).collect::<Result<Vec<_>, OrchestratorError>>()?;
    goal.materialize_initial_plan_with_contract(
        materialized.into_iter().map(|(_, _, task)| task).collect(),
        GoalFinalVerificationSpec::new(INITIAL_PLAN_REVISION, bindings),
        now,
    )
}

fn map_store_error(error: OrchestratorError) -> PlannerError {
    match error {
        OrchestratorError::RevisionConflict { expected, actual } => {
            PlannerError::PlanConflict { expected, actual }
        }
        other => PlannerError::Store(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use crate::goal::GoalBlocker;
    use crate::task::TaskStatus;
    use crate::task_store::FaultPoint;

    const NOW: &str = "2026-09-13T00:00:00Z";

    struct Fixture {
        root: PathBuf,
        state: PathBuf,
        repo: PathBuf,
        session: config::Session,
        store: TaskStore,
        goal_id: GoalId,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn fixture() -> Fixture {
        let root = std::env::temp_dir().join(format!("local-mcp-phase4-{}", Uuid::new_v4()));
        let repo = root.join("repo");
        let state = root.join("state");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("sentinel.txt"), b"unchanged\n").unwrap();
        let session = config::Session {
            id: format!("phase4-{}", Uuid::new_v4()),
            cwd: repo.clone(),
            permitted_directories: vec![repo.clone()],
        };
        let store = TaskStore::with_state_root(state.clone());
        let goal = Goal::new(
            session.id.clone(),
            repo.clone(),
            "materialize a safe plan",
            Some("Phase 4 fixture".to_owned()),
            vec!["do not execute work".to_owned()],
            vec!["host validation passes".to_owned()],
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
        }
    }

    fn read_only_task(id: &str, dependencies: &[&str]) -> Value {
        task_value(
            id,
            dependencies,
            "CODEX_READONLY",
            "READ_ONLY",
            "SAFE_READ_ONLY",
        )
    }

    fn mutation_task(id: &str, dependencies: &[&str]) -> Value {
        task_value(
            id,
            dependencies,
            "CODEX_WRITER",
            "LOCAL_MUTATION",
            "VERIFY_BEFORE_RETRY",
        )
    }

    fn task_value(
        id: &str,
        dependencies: &[&str],
        worker: &str,
        operation_kind: &str,
        replay_safety: &str,
    ) -> Value {
        json!({
            "proposal_id": id,
            "title": format!("Task {id}"),
            "objective": format!("Complete {id}"),
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

    fn proposal(fixture: &Fixture, tasks: Vec<Value>) -> Vec<u8> {
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let task_refs = tasks
            .iter()
            .filter_map(|task| task["proposal_id"].as_str().map(str::to_owned))
            .collect::<Vec<_>>();
        let criterion_bindings = goal
            .completion_criteria()
            .iter()
            .map(|criterion| {
                json!({
                    "criterion_id": criterion.id().as_str(),
                    "task_refs": task_refs
                })
            })
            .collect::<Vec<_>>();
        serde_json::to_vec(&json!({
            "goal_id": fixture.goal_id.as_str(),
            "goal_revision": 1,
            "summary": "bounded initial plan",
            "tasks": tasks,
            "criterion_bindings": criterion_bindings
        }))
        .unwrap()
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

    fn apply(fixture: &Fixture, proposal: &[u8]) -> Result<Goal, PlannerError> {
        materialize_initial_plan_output(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            1,
            proposal,
        )
    }

    #[test]
    fn planner_request_contains_planning_context_without_durable_authority_fields() {
        let fixture = fixture();
        let goal = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        let request = planner_request_for_goal(&goal, &fixture.session).unwrap();
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["goal_revision"], 1);
        assert_eq!(value["objective"], "materialize a safe plan");
        assert_eq!(
            value["allowed_worker_kinds"],
            json!(["CODEX_READONLY", "CODEX_WRITER"])
        );
        for forbidden in [
            "schema_version",
            "plan_revision",
            "attempts",
            "evidence",
            "authorized",
            "skip_approval",
            "yolo",
        ] {
            assert!(
                value.get(forbidden).is_none(),
                "unexpected field {forbidden}"
            );
        }
    }

    #[test]
    fn valid_single_task_plan_commits_once_and_does_not_execute() {
        let fixture = fixture();
        let sentinel_before = fs::read(fixture.repo.join("sentinel.txt")).unwrap();
        let durable = apply(
            &fixture,
            &proposal(&fixture, vec![read_only_task("inspect", &[])]),
        )
        .unwrap();
        assert_eq!(durable.revision(), 2);
        assert_eq!(durable.plan_revision(), 1);
        assert_eq!(durable.status(), GoalStatus::Running);
        assert_eq!(durable.tasks().len(), 1);
        let task = durable.tasks().values().next().unwrap();
        assert_eq!(task.status(), TaskStatus::Ready);
        assert!(task.attempts().is_empty());
        assert_eq!(task.evidence_count(), 0);
        assert_eq!(task.verification_results().len(), 0);
        assert_eq!(task.created_plan_revision(), 1);
        assert_eq!(task.max_attempts(), 2);
        assert_eq!(
            fs::read(fixture.repo.join("sentinel.txt")).unwrap(),
            sentinel_before
        );
    }

    #[test]
    fn valid_linear_dag_derives_ready_and_pending() {
        let fixture = fixture();
        let durable = apply(
            &fixture,
            &proposal(
                &fixture,
                vec![
                    read_only_task("inspect", &[]),
                    mutation_task("implement", &["inspect"]),
                ],
            ),
        )
        .unwrap();
        let mut by_title = durable
            .tasks()
            .values()
            .map(|task| (task.title().to_owned(), task.status()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(by_title.remove("Task inspect"), Some(TaskStatus::Ready));
        assert_eq!(by_title.remove("Task implement"), Some(TaskStatus::Pending));
    }

    #[test]
    fn valid_branching_read_only_dag_is_accepted() {
        let fixture = fixture();
        let durable = apply(
            &fixture,
            &proposal(
                &fixture,
                vec![
                    read_only_task("a", &[]),
                    read_only_task("b", &[]),
                    read_only_task("join", &["a", "b"]),
                ],
            ),
        )
        .unwrap();
        assert_eq!(
            durable
                .tasks()
                .values()
                .filter(|task| task.status() == TaskStatus::Ready)
                .count(),
            2
        );
        assert_eq!(
            durable
                .tasks()
                .values()
                .filter(|task| task.status() == TaskStatus::Pending)
                .count(),
            1
        );
    }

    #[test]
    fn valid_join_dag_with_one_ordered_writer_is_accepted() {
        let fixture = fixture();
        let durable = apply(
            &fixture,
            &proposal(
                &fixture,
                vec![
                    read_only_task("a", &[]),
                    read_only_task("b", &[]),
                    mutation_task("write", &["a", "b"]),
                ],
            ),
        )
        .unwrap();
        assert_eq!(durable.tasks().len(), 3);
        assert_eq!(
            durable
                .tasks()
                .values()
                .filter(|task| task.status() == TaskStatus::Ready)
                .count(),
            2
        );
    }

    #[test]
    fn authoritative_task_ids_are_server_generated_uuids() {
        let fixture = fixture();
        let durable = apply(
            &fixture,
            &proposal(&fixture, vec![read_only_task("inspect", &[])]),
        )
        .unwrap();
        let task_id = durable.tasks().keys().next().unwrap();
        assert!(Uuid::parse_str(task_id.as_str()).is_ok());
        assert_ne!(task_id.as_str(), "inspect");
    }

    #[test]
    fn duplicate_proposal_id_is_rejected() {
        let fixture = fixture();
        let result = apply(
            &fixture,
            &proposal(
                &fixture,
                vec![read_only_task("same", &[]), read_only_task("same", &[])],
            ),
        );
        assert!(matches!(
            result,
            Err(PlannerError::PlannerSchemaViolation(_))
        ));
    }

    #[test]
    fn missing_dependency_is_rejected() {
        let fixture = fixture();
        let result = apply(
            &fixture,
            &proposal(&fixture, vec![read_only_task("a", &["missing"])]),
        );
        assert!(matches!(
            result,
            Err(PlannerError::PlannerSchemaViolation(_))
        ));
    }

    #[test]
    fn self_dependency_is_rejected() {
        let fixture = fixture();
        let result = apply(
            &fixture,
            &proposal(&fixture, vec![read_only_task("a", &["a"])]),
        );
        assert!(matches!(
            result,
            Err(PlannerError::PlannerSchemaViolation(_))
        ));
    }

    #[test]
    fn dependency_cycle_is_rejected() {
        let fixture = fixture();
        let result = apply(
            &fixture,
            &proposal(
                &fixture,
                vec![read_only_task("a", &["b"]), read_only_task("b", &["a"])],
            ),
        );
        assert!(matches!(
            result,
            Err(PlannerError::PlannerSchemaViolation(_))
        ));
    }

    #[test]
    fn unknown_worker_kind_is_rejected_strictly() {
        let fixture = fixture();
        let mut task = read_only_task("a", &[]);
        task["worker"] = json!("MAGIC_WORKER");
        let result = apply(&fixture, &proposal(&fixture, vec![task]));
        assert!(matches!(
            result,
            Err(PlannerError::PlannerSchemaViolation(_))
        ));
    }

    #[test]
    fn known_but_non_plannable_worker_kinds_are_rejected_before_commit() {
        let fixture = fixture();
        let before = bytes(&fixture);
        for worker in ["LOCAL_OPERATION", "CODEX_REVIEWER", "VERIFIER"] {
            let mut task = read_only_task("a", &[]);
            task["worker"] = json!(worker);
            let result = apply(&fixture, &proposal(&fixture, vec![task]));
            assert!(matches!(
                result,
                Err(PlannerError::PlannerSchemaViolation(_))
            ));
            assert_eq!(bytes(&fixture), before);
        }
    }

    #[test]
    fn parent_scope_escape_is_rejected() {
        let fixture = fixture();
        let mut task = read_only_task("a", &[]);
        task["scope"]["allowed_paths"] = json!(["../escape"]);
        let result = apply(&fixture, &proposal(&fixture, vec![task]));
        assert!(matches!(
            result,
            Err(PlannerError::PlanAuthorityViolation(_))
        ));
    }

    #[test]
    fn absolute_scope_escape_is_rejected() {
        let fixture = fixture();
        let mut task = read_only_task("a", &[]);
        task["scope"]["allowed_paths"] = json!([fixture.root.join("outside")]);
        let result = apply(&fixture, &proposal(&fixture, vec![task]));
        assert!(matches!(
            result,
            Err(PlannerError::PlanAuthorityViolation(_))
        ));
    }

    #[test]
    fn invalid_verification_schema_is_rejected() {
        let fixture = fixture();
        let mut task = read_only_task("a", &[]);
        task["verification"] = json!([{
            "kind": "FILE_DIGEST",
            "path": "sentinel.txt",
            "expected_sha256": "bad"
        }]);
        let result = apply(&fixture, &proposal(&fixture, vec![task]));
        assert!(matches!(
            result,
            Err(PlannerError::PlannerSchemaViolation(_))
        ));
    }

    #[test]
    fn oversized_plan_is_rejected_before_mutation() {
        let fixture = fixture();
        let large = vec![b'x'; MAX_PLAN_PROPOSAL_BYTES + 1];
        let result = apply(&fixture, &large);
        assert!(matches!(
            result,
            Err(PlannerError::PlannerSchemaViolation(_))
        ));
        assert_eq!(
            fixture
                .store
                .load_goal(&fixture.session.id, &fixture.goal_id)
                .unwrap()
                .revision(),
            1
        );
    }

    #[test]
    fn unexpected_authoritative_task_field_is_rejected() {
        let fixture = fixture();
        let mut task = read_only_task("a", &[]);
        task["status"] = json!("COMPLETED");
        let result = apply(&fixture, &proposal(&fixture, vec![task]));
        assert!(matches!(
            result,
            Err(PlannerError::PlannerSchemaViolation(_))
        ));
    }

    #[test]
    fn invalid_proposals_preserve_exact_durable_bytes() {
        let fixture = fixture();
        let before = bytes(&fixture);
        let invalid = [
            proposal(&fixture, vec![read_only_task("a", &["missing"])]),
            proposal(
                &fixture,
                vec![read_only_task("a", &["b"]), read_only_task("b", &["a"])],
            ),
            {
                let mut task = read_only_task("a", &[]);
                task["scope"]["allowed_paths"] = json!(["../escape"]);
                proposal(&fixture, vec![task])
            },
            {
                let mut task = read_only_task("a", &[]);
                task["worker"] = json!("UNKNOWN");
                proposal(&fixture, vec![task])
            },
            {
                let mut task = read_only_task("a", &[]);
                task["verification"] = json!([]);
                proposal(&fixture, vec![task])
            },
        ];
        for candidate in invalid {
            assert!(apply(&fixture, &candidate).is_err());
            assert_eq!(bytes(&fixture), before);
            let durable = fixture
                .store
                .load_goal(&fixture.session.id, &fixture.goal_id)
                .unwrap();
            assert_eq!(durable.revision(), 1);
            assert_eq!(durable.plan_revision(), 0);
            assert_eq!(durable.status(), GoalStatus::Planning);
            assert!(durable.tasks().is_empty());
        }
    }

    #[test]
    fn persistence_failure_preserves_exact_bytes() {
        let fixture = fixture();
        let before = bytes(&fixture);
        let faulty = TaskStore::with_fault(fixture.state.clone(), FaultPoint::BeforeReplace);
        let result = materialize_initial_plan_output(
            &faulty,
            &fixture.session,
            &fixture.goal_id,
            1,
            &proposal(&fixture, vec![read_only_task("a", &[])]),
        );
        assert!(matches!(
            result,
            Err(PlannerError::Store(OrchestratorError::PersistenceIo(_)))
        ));
        assert_eq!(bytes(&fixture), before);
        assert_eq!(
            fixture
                .store
                .load_goal(&fixture.session.id, &fixture.goal_id)
                .unwrap()
                .revision(),
            1
        );
    }

    #[test]
    fn stale_revision_conflict_preserves_newer_state() {
        let fixture = fixture();
        let original_proposal = proposal(&fixture, vec![read_only_task("a", &[])]);
        let newer = fixture
            .store
            .mutate_goal_snapshot(&fixture.session.id, &fixture.goal_id, 1, |goal, _| {
                goal.add_blocker(GoalBlocker::new("newer", "newer mutation", false))
            })
            .unwrap();
        assert_eq!(newer.revision(), 2);
        let before = bytes(&fixture);
        let result = materialize_initial_plan_output(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            1,
            &original_proposal,
        );
        assert!(matches!(
            result,
            Err(PlannerError::PlanConflict {
                expected: 1,
                actual: 2
            })
        ));
        assert_eq!(bytes(&fixture), before);
        assert_eq!(
            fixture
                .store
                .load_goal(&fixture.session.id, &fixture.goal_id)
                .unwrap()
                .revision(),
            2
        );
    }

    #[test]
    fn duplicate_delivery_cannot_duplicate_tasks() {
        let fixture = fixture();
        let output = proposal(&fixture, vec![read_only_task("a", &[])]);
        let first = apply(&fixture, &output).unwrap();
        assert_eq!(first.tasks().len(), 1);
        let before = bytes(&fixture);
        let replay = materialize_initial_plan_output(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            2,
            &output,
        );
        assert!(matches!(
            replay,
            Err(PlannerError::PlanNotApplicable(_)) | Err(PlannerError::PlanConflict { .. })
        ));
        assert_eq!(bytes(&fixture), before);
        assert_eq!(
            fixture
                .store
                .load_goal(&fixture.session.id, &fixture.goal_id)
                .unwrap()
                .tasks()
                .len(),
            1
        );
    }

    #[test]
    fn unordered_mutation_branches_are_rejected() {
        let fixture = fixture();
        let result = apply(
            &fixture,
            &proposal(
                &fixture,
                vec![
                    mutation_task("writer_a", &[]),
                    mutation_task("writer_b", &[]),
                ],
            ),
        );
        assert!(matches!(
            result,
            Err(PlannerError::PlannerSchemaViolation(_))
        ));
    }

    #[test]
    fn ordered_mutation_tasks_are_accepted() {
        let fixture = fixture();
        let durable = apply(
            &fixture,
            &proposal(
                &fixture,
                vec![
                    mutation_task("writer_a", &[]),
                    mutation_task("writer_b", &["writer_a"]),
                ],
            ),
        )
        .unwrap();
        assert_eq!(durable.tasks().len(), 2);
        assert_eq!(
            durable
                .tasks()
                .values()
                .filter(|task| task.status() == TaskStatus::Ready)
                .count(),
            1
        );
    }

    #[test]
    fn mandatory_task_cannot_depend_on_optional_task() {
        let fixture = fixture();
        let optional = json!({
            "proposal_id": "optional",
            "title": "optional",
            "objective": "optional work",
            "mandatory": false,
            "worker": "CODEX_READONLY",
            "dependencies": [],
            "scope": {
                "allowed_paths": ["."],
                "forbidden_paths": [],
                "operation_kind": "READ_ONLY",
                "replay_safety": "SAFE_READ_ONLY"
            },
            "verification": [{"kind":"STRUCTURED_EVIDENCE","requirement_id":"optional"}]
        });
        let result = apply(
            &fixture,
            &proposal(
                &fixture,
                vec![optional, read_only_task("mandatory", &["optional"])],
            ),
        );
        assert!(matches!(
            result,
            Err(PlannerError::PlannerSchemaViolation(_))
        ));
    }

    #[test]
    fn effectful_scope_cannot_claim_safe_read_only_replay() {
        let fixture = fixture();
        let mut task = mutation_task("writer", &[]);
        task["scope"]["replay_safety"] = json!("SAFE_READ_ONLY");
        let result = apply(&fixture, &proposal(&fixture, vec![task]));
        assert!(matches!(
            result,
            Err(PlannerError::PlannerSchemaViolation(_))
        ));
    }

    #[test]
    fn mutation_scope_rejects_git_internals() {
        let fixture = fixture();
        let mut task = mutation_task("writer", &[]);
        task["scope"]["allowed_paths"] = json!([".git/config"]);
        let result = apply(&fixture, &proposal(&fixture, vec![task]));
        assert!(matches!(
            result,
            Err(PlannerError::PlanAuthorityViolation(_))
        ));
    }

    struct FixturePlanner {
        output: Vec<u8>,
    }

    impl PlannerBackend for FixturePlanner {
        fn propose_initial_plan(&self, _request: &PlannerRequest) -> Result<Vec<u8>, PlannerError> {
            Ok(self.output.clone())
        }
    }

    #[test]
    fn planner_backend_contract_is_injectable_and_network_free_for_tests() {
        let fixture = fixture();
        let planner = FixturePlanner {
            output: proposal(&fixture, vec![read_only_task("inspect", &[])]),
        };
        let durable =
            plan_initial_goal(&fixture.store, &fixture.session, &fixture.goal_id, &planner)
                .unwrap();
        assert_eq!(durable.status(), GoalStatus::Running);
        assert_eq!(durable.tasks().len(), 1);
    }

    #[test]
    fn planner_module_has_no_low_level_execution_coupling() {
        let source = include_str!("planner.rs");
        for forbidden in [
            concat!("execution", "::"),
            concat!("sandbox", "::"),
            concat!("start_", "command"),
            concat!("write_", "file"),
            concat!("without_", "sandbox"),
            concat!("run_", "unrestricted"),
        ] {
            assert!(
                !source.contains(forbidden),
                "forbidden execution coupling: {forbidden}"
            );
        }
    }
}
