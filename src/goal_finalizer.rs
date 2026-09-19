use crate::goal::{CheckpointReason, Goal, GoalId, GoalStatus};
use crate::orchestrator_error::OrchestratorError;
use crate::task::{TaskId, TaskStatus, VerificationOutcome};
use crate::task_store::TaskStore;

pub(crate) struct GoalFinalizationAuthority {
    _private: (),
}

impl GoalFinalizationAuthority {
    fn new() -> Self {
        Self { _private: () }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GoalFinalizationOutcome {
    ReadyToComplete,
    NotReady,
    Blocked,
    Failed,
    AlreadyCompleted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GoalFinalizationBlocker {
    code: String,
    task_id: Option<TaskId>,
    detail: String,
}

impl GoalFinalizationBlocker {
    fn new(code: impl Into<String>, task_id: Option<TaskId>, detail: impl Into<String>) -> Self {
        Self { code: code.into(), task_id, detail: detail.into() }
    }
    pub(crate) fn code(&self) -> &str { &self.code }
    pub(crate) fn task_id(&self) -> Option<&TaskId> { self.task_id.as_ref() }
    pub(crate) fn detail(&self) -> &str { &self.detail }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GoalFinalizationEvidence {
    goal_revision: u64,
    plan_revision: u32,
    goal_status: GoalStatus,
    mandatory_task_states: Vec<(TaskId, TaskStatus, Option<VerificationOutcome>)>,
    final_verification_id: Option<String>,
    final_verification_outcome: Option<VerificationOutcome>,
    unknown_side_effect_tasks: Vec<TaskId>,
    goal_blocker_codes: Vec<String>,
    task_blocker_count: usize,
    checkpoint_count: usize,
}

impl GoalFinalizationEvidence {
    pub(crate) fn goal_revision(&self) -> u64 { self.goal_revision }
    pub(crate) fn plan_revision(&self) -> u32 { self.plan_revision }
    pub(crate) fn goal_status(&self) -> GoalStatus { self.goal_status }
    pub(crate) fn mandatory_task_states(&self) -> &[(TaskId, TaskStatus, Option<VerificationOutcome>)] { &self.mandatory_task_states }
    pub(crate) fn final_verification_id(&self) -> Option<&str> { self.final_verification_id.as_deref() }
    pub(crate) fn final_verification_outcome(&self) -> Option<VerificationOutcome> { self.final_verification_outcome }
    pub(crate) fn unknown_side_effect_tasks(&self) -> &[TaskId] { &self.unknown_side_effect_tasks }
    pub(crate) fn goal_blocker_codes(&self) -> &[String] { &self.goal_blocker_codes }
    pub(crate) fn task_blocker_count(&self) -> usize { self.task_blocker_count }
    pub(crate) fn checkpoint_count(&self) -> usize { self.checkpoint_count }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GoalFinalizationDecision {
    outcome: GoalFinalizationOutcome,
    summary: String,
    blockers: Vec<GoalFinalizationBlocker>,
    evidence: GoalFinalizationEvidence,
}

impl GoalFinalizationDecision {
    pub(crate) fn outcome(&self) -> GoalFinalizationOutcome { self.outcome }
    pub(crate) fn summary(&self) -> &str { &self.summary }
    pub(crate) fn blockers(&self) -> &[GoalFinalizationBlocker] { &self.blockers }
    pub(crate) fn evidence(&self) -> &GoalFinalizationEvidence { &self.evidence }
}

#[derive(Clone, Debug)]
pub(crate) struct GoalFinalizationSnapshot {
    goal: Goal,
    revision: u64,
    plan_revision: u32,
}

impl GoalFinalizationSnapshot {
    fn from_goal(goal: Goal) -> Self {
        let revision = goal.revision();
        let plan_revision = goal.plan_revision();
        Self { goal, revision, plan_revision }
    }
    pub(crate) fn goal_id(&self) -> &GoalId { self.goal.id() }
    pub(crate) fn session_id(&self) -> &str { self.goal.session_id() }
    pub(crate) fn revision(&self) -> u64 { self.revision }
    pub(crate) fn plan_revision(&self) -> u32 { self.plan_revision }
    pub(crate) fn goal(&self) -> &Goal { &self.goal }
}

#[derive(Debug)]
pub(crate) enum GoalFinalizerError {
    Store(OrchestratorError),
    DecisionNotReady(GoalFinalizationOutcome),
    InvalidDurableState(String),
}

impl std::fmt::Display for GoalFinalizerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "goal finalizer durable-state error: {error}"),
            Self::DecisionNotReady(outcome) => write!(f, "goal finalization decision is not ready: {outcome:?}"),
            Self::InvalidDurableState(detail) => write!(f, "goal finalizer rejected durable state: {detail}"),
        }
    }
}
impl std::error::Error for GoalFinalizerError {}
impl From<OrchestratorError> for GoalFinalizerError {
    fn from(value: OrchestratorError) -> Self { Self::Store(value) }
}

#[derive(Clone, Debug)]
pub(crate) struct GoalFinalizationResult {
    decision: GoalFinalizationDecision,
    goal: Goal,
}
impl GoalFinalizationResult {
    pub(crate) fn decision(&self) -> &GoalFinalizationDecision { &self.decision }
    pub(crate) fn goal(&self) -> &Goal { &self.goal }
}

pub(crate) fn prepare_goal_finalization(store: &TaskStore, session_id: &str, goal_id: &GoalId) -> Result<GoalFinalizationSnapshot, GoalFinalizerError> {
    let goal = store.load_goal(session_id, goal_id)?;
    goal.validate().map_err(|error| GoalFinalizerError::InvalidDurableState(error.to_string()))?;
    Ok(GoalFinalizationSnapshot::from_goal(goal))
}

pub(crate) fn evaluate_goal_finalization(snapshot: &GoalFinalizationSnapshot) -> Result<GoalFinalizationDecision, GoalFinalizerError> {
    evaluate_goal(&snapshot.goal).map_err(|error| GoalFinalizerError::InvalidDurableState(error.to_string()))
}
fn evaluate_goal(goal: &Goal) -> Result<GoalFinalizationDecision, OrchestratorError> {
    goal.validate()?;
    goal.validate_dag()?;
    let mandatory_task_states = goal
        .tasks()
        .iter()
        .filter(|(_, task)| task.is_active_plan_authority() && task.mandatory())
        .map(|(id, task)| {
            (
                id.clone(),
                task.status(),
                task.verification_results()
                    .last()
                    .map(|result| result.outcome()),
            )
        })
        .collect::<Vec<_>>();
    let unknown_side_effect_tasks = goal.tasks().iter().filter_map(|(id, task)| task.has_unknown_side_effect().then_some(id.clone())).collect::<Vec<_>>();
    let evidence = GoalFinalizationEvidence {
        goal_revision: goal.revision(), plan_revision: goal.plan_revision(), goal_status: goal.status(), mandatory_task_states,
        final_verification_id: goal.latest_applicable_final_verification().map(|r| r.id().as_str().to_owned()),
        final_verification_outcome: goal.latest_applicable_final_verification().map(|r| r.outcome()),
        unknown_side_effect_tasks: unknown_side_effect_tasks.clone(),
        goal_blocker_codes: goal.blockers().iter().map(|b| b.code().to_owned()).collect(),
        task_blocker_count: goal.tasks().values().map(|t| t.blockers().len()).sum(), checkpoint_count: goal.checkpoints().len(),
    };
    if goal.status() == GoalStatus::Completed {
        return Ok(GoalFinalizationDecision { outcome: GoalFinalizationOutcome::AlreadyCompleted, summary: "already completed".into(), blockers: vec![], evidence });
    }
    if matches!(goal.status(), GoalStatus::Failed | GoalStatus::Cancelled) {
        return Ok(GoalFinalizationDecision { outcome: GoalFinalizationOutcome::Failed, summary: "terminal non-completed Goal".into(), blockers: vec![GoalFinalizationBlocker::new("TERMINAL_NON_COMPLETED_GOAL", None, "FAILED/CANCELLED is immutable")], evidence });
    }
    let mut blockers = Vec::new();
    if goal.status() != GoalStatus::Verifying {
        blockers.push(GoalFinalizationBlocker::new("GOAL_NOT_VERIFYING", None, format!("eligible state is VERIFYING, found {:?}", goal.status())));
    }
    for (task_id, task) in goal.tasks() {
        if task.is_active_plan_authority()
            && task.mandatory()
            && task.status() != TaskStatus::Completed
        {
            blockers.push(GoalFinalizationBlocker::new("MANDATORY_TASK_NOT_COMPLETED", Some(task_id.clone()), format!("mandatory Task is {:?}", task.status())));
        }
        if task.is_active_plan_authority()
            && task.mandatory()
            && task.status() == TaskStatus::Completed
            && task.verification_results().last().map(|r| r.outcome())
                != Some(VerificationOutcome::Passed)
        {
            blockers.push(GoalFinalizationBlocker::new("MANDATORY_TASK_VERIFICATION_NOT_PASSED", Some(task_id.clone()), "latest verification is not PASSED"));
        }
        if matches!(task.status(), TaskStatus::Running | TaskStatus::Verifying | TaskStatus::NeedsReplan) {
            blockers.push(GoalFinalizationBlocker::new("TASK_STATE_UNSETTLED_FOR_FINALIZATION", Some(task_id.clone()), format!("Task state {:?} is unsettled", task.status())));
        }
        for blocker in task.blockers() {
            if (task.is_active_plan_authority() && task.mandatory()) || blocker.mandatory() {
                blockers.push(GoalFinalizationBlocker::new("UNRESOLVED_TASK_BLOCKER", Some(task_id.clone()), format!("{}: {}", blocker.code(), blocker.detail())));
            }
        }
    }
    for blocker in goal.blockers() {
        blockers.push(GoalFinalizationBlocker::new("UNRESOLVED_GOAL_BLOCKER", None, format!("{}: {}", blocker.code(), blocker.detail())));
    }
    for task_id in &unknown_side_effect_tasks {
        blockers.push(GoalFinalizationBlocker::new("UNKNOWN_SIDE_EFFECT", Some(task_id.clone()), "unreconciled side-effect state remains"));
    }
    match goal.latest_applicable_final_verification().map(|r| r.outcome()) {
        None => blockers.push(GoalFinalizationBlocker::new("FINAL_VERIFICATION_PENDING", None, "final verification absent")),
        Some(VerificationOutcome::Passed) => {}
        Some(VerificationOutcome::Failed) => blockers.push(GoalFinalizationBlocker::new("FINAL_VERIFICATION_FAILED", None, "final verification failed")),
        Some(VerificationOutcome::Indeterminate) => blockers.push(GoalFinalizationBlocker::new("FINAL_VERIFICATION_INDETERMINATE", None, "indeterminate never completes")),
    }
    let outcome = if blockers.is_empty() { GoalFinalizationOutcome::ReadyToComplete }
        else if blockers.iter().any(|b| b.code == "FINAL_VERIFICATION_FAILED") { GoalFinalizationOutcome::Failed }
        else if blockers.iter().any(|b| matches!(b.code.as_str(), "UNRESOLVED_GOAL_BLOCKER" | "UNRESOLVED_TASK_BLOCKER" | "UNKNOWN_SIDE_EFFECT" | "FINAL_VERIFICATION_INDETERMINATE")) { GoalFinalizationOutcome::Blocked }
        else { GoalFinalizationOutcome::NotReady };
    let summary = match outcome {
        GoalFinalizationOutcome::ReadyToComplete => "all deterministic Goal completion gates passed",
        GoalFinalizationOutcome::NotReady => "Goal is not yet eligible for completion",
        GoalFinalizationOutcome::Blocked => "Goal completion is blocked by durable state",
        GoalFinalizationOutcome::Failed => "Goal final verification rejects completion",
        GoalFinalizationOutcome::AlreadyCompleted => unreachable!(),
    }.to_owned();
    Ok(GoalFinalizationDecision { outcome, summary, blockers, evidence })
}
pub(crate) fn commit_goal_finalization(store: &TaskStore, snapshot: &GoalFinalizationSnapshot, decision: &GoalFinalizationDecision) -> Result<Goal, GoalFinalizerError> {
    if decision.outcome == GoalFinalizationOutcome::AlreadyCompleted {
        return Ok(store.load_goal(snapshot.session_id(), snapshot.goal_id())?);
    }
    if decision.outcome != GoalFinalizationOutcome::ReadyToComplete {
        return Err(GoalFinalizerError::DecisionNotReady(decision.outcome));
    }
    let expected_plan_revision = snapshot.plan_revision();
    let authority = GoalFinalizationAuthority::new();
    let goal = store.mutate_goal_snapshot(snapshot.session_id(), snapshot.goal_id(), snapshot.revision(), |goal, now| {
        if goal.plan_revision() != expected_plan_revision {
            return Err(OrchestratorError::InvalidDag(format!("goal finalization plan revision changed: expected {expected_plan_revision}, found {}", goal.plan_revision())));
        }
        let current = evaluate_goal(goal)?;
        if current.outcome != GoalFinalizationOutcome::ReadyToComplete {
            return Err(OrchestratorError::InvalidDag("goal finalization decision became stale before commit".to_owned()));
        }
        let matching_checkpoint = goal.checkpoints().iter().any(|checkpoint| {
            checkpoint.reason() == CheckpointReason::FinalVerification
                && checkpoint.goal_status() == GoalStatus::Verifying
                && checkpoint.goal_revision() == goal.revision()
                && checkpoint.plan_revision() == goal.plan_revision()
        });
        if !matching_checkpoint {
            goal.add_checkpoint(CheckpointReason::FinalVerification, now)?;
        }
        goal.complete_from_finalizer(&authority, now)?;
        Ok(())
    })?;
    Ok(goal)
}
fn finalize_prepared_goal(
    store: &TaskStore,
    session_id: &str,
    goal_id: &GoalId,
    snapshot: GoalFinalizationSnapshot,
) -> Result<GoalFinalizationResult, GoalFinalizerError> {
    let decision = evaluate_goal_finalization(&snapshot)?;
    let goal = match decision.outcome {
        GoalFinalizationOutcome::ReadyToComplete => commit_goal_finalization(store, &snapshot, &decision)?,
        GoalFinalizationOutcome::AlreadyCompleted => store.load_goal(session_id, goal_id)?,
        _ => snapshot.goal.clone(),
    };
    Ok(GoalFinalizationResult { decision, goal })
}

pub(crate) fn finalize_goal_at_revision(
    store: &TaskStore,
    session_id: &str,
    goal_id: &GoalId,
    expected_revision: u64,
) -> Result<GoalFinalizationResult, GoalFinalizerError> {
    let snapshot = prepare_goal_finalization(store, session_id, goal_id)?;
    if snapshot.revision() != expected_revision {
        return Err(GoalFinalizerError::Store(OrchestratorError::RevisionConflict {
            expected: expected_revision,
            actual: snapshot.revision(),
        }));
    }
    finalize_prepared_goal(store, session_id, goal_id, snapshot)
}

pub(crate) fn finalize_goal(store: &TaskStore, session_id: &str, goal_id: &GoalId) -> Result<GoalFinalizationResult, GoalFinalizerError> {
    let snapshot = prepare_goal_finalization(store, session_id, goal_id)?;
    finalize_prepared_goal(store, session_id, goal_id, snapshot)
}

#[cfg(test)]
pub(crate) fn complete_goal_for_test(goal: &mut Goal, now: &str) -> Result<(), OrchestratorError> {
    let authority = GoalFinalizationAuthority::new();
    goal.complete_from_finalizer(&authority, now)
}
