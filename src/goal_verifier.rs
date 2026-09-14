use crate::goal::{
    CompletionCriterionId, Goal, GoalBlocker, GoalCriterionVerificationResult,
    GoalFinalVerificationRecord, GoalId, GoalRequirementObservation, GoalStatus,
    GoalVerificationRequirement, VerificationOrigin,
};
use crate::orchestrator_error::OrchestratorError;
use crate::task::{TaskId, TaskStatus, VerificationId, VerificationOutcome};
use crate::task_store::{TaskStore, utc_now_rfc3339};

pub(crate) struct GoalVerificationEntryAuthority {
    _private: (),
}

impl GoalVerificationEntryAuthority {
    fn new() -> Self { Self { _private: () } }
}

pub(crate) struct GoalFinalVerificationWriteAuthority {
    _private: (),
}

impl GoalFinalVerificationWriteAuthority {
    fn new() -> Self { Self { _private: () } }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GoalVerificationSnapshot {
    goal: Goal,
    revision: u64,
    plan_revision: u32,
    contract_digest: String,
    task_verification_identities: Vec<(TaskId, TaskStatus, Option<VerificationId>, Option<VerificationOutcome>)>,
    started_at: String,
}

impl GoalVerificationSnapshot {
    pub(crate) fn goal(&self) -> &Goal { &self.goal }
    pub(crate) fn revision(&self) -> u64 { self.revision }
    pub(crate) fn plan_revision(&self) -> u32 { self.plan_revision }
    pub(crate) fn contract_digest(&self) -> &str { &self.contract_digest }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GoalVerificationDecision {
    outcome: VerificationOutcome,
    criterion_results: Vec<GoalCriterionVerificationResult>,
}

impl GoalVerificationDecision {
    pub(crate) fn outcome(&self) -> VerificationOutcome { self.outcome }
    pub(crate) fn criterion_results(&self) -> &[GoalCriterionVerificationResult] { &self.criterion_results }
}

#[derive(Clone, Debug)]
pub(crate) struct GoalVerificationResult {
    decision: GoalVerificationDecision,
    goal: Goal,
    record_id: Option<VerificationId>,
    applied: bool,
}

impl GoalVerificationResult {
    pub(crate) fn decision(&self) -> &GoalVerificationDecision { &self.decision }
    pub(crate) fn goal(&self) -> &Goal { &self.goal }
    pub(crate) fn record_id(&self) -> Option<&VerificationId> { self.record_id.as_ref() }
    pub(crate) fn applied(&self) -> bool { self.applied }
}

#[derive(Debug)]
pub(crate) enum GoalVerifierError {
    Store(OrchestratorError),
    NotApplicable(String),
    InvalidDurableState(String),
    StaleEvaluation(String),
}

impl std::fmt::Display for GoalVerifierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "Goal Verifier durable-state error: {error}"),
            Self::NotApplicable(detail) => write!(f, "Goal Verifier is not applicable: {detail}"),
            Self::InvalidDurableState(detail) => write!(f, "Goal Verifier rejected durable state: {detail}"),
            Self::StaleEvaluation(detail) => write!(f, "Goal Verifier evaluation is stale: {detail}"),
        }
    }
}

impl std::error::Error for GoalVerifierError {}

impl From<OrchestratorError> for GoalVerifierError {
    fn from(value: OrchestratorError) -> Self { Self::Store(value) }
}

pub(crate) fn prepare_goal_verification(
    store: &TaskStore,
    session_id: &str,
    goal_id: &GoalId,
) -> Result<GoalVerificationSnapshot, GoalVerifierError> {
    let goal = store.load_goal(session_id, goal_id)?;
    prepare_goal_verification_from_goal(goal)
}

fn prepare_goal_verification_from_goal(goal: Goal) -> Result<GoalVerificationSnapshot, GoalVerifierError> {
    goal.validate().map_err(|error| GoalVerifierError::InvalidDurableState(error.to_string()))?;
    if goal.status() != GoalStatus::Running {
        return Err(GoalVerifierError::NotApplicable(format!(
            "normal final verification entry requires RUNNING, found {:?}", goal.status()
        )));
    }
    goal.validate_dag().map_err(|error| GoalVerifierError::InvalidDurableState(error.to_string()))?;
    goal.validate_final_verification_contract()
        .map_err(|error| GoalVerifierError::InvalidDurableState(error.to_string()))?;
    let spec = goal.final_verification_spec().ok_or_else(|| {
        GoalVerifierError::InvalidDurableState("structured final-verification contract is missing".to_owned())
    })?;
    let contract_digest = spec.canonical_digest();
    let task_verification_identities = referenced_task_identities(&goal);
    let revision = goal.revision();
    let plan_revision = goal.plan_revision();
    Ok(GoalVerificationSnapshot {
        goal,
        revision,
        plan_revision,
        contract_digest,
        task_verification_identities,
        started_at: utc_now_rfc3339(),
    })
}

pub(crate) fn evaluate_goal_verification(
    snapshot: &GoalVerificationSnapshot,
) -> Result<GoalVerificationDecision, GoalVerifierError> {
    let goal = &snapshot.goal;
    if goal.status() != GoalStatus::Running {
        return Err(GoalVerifierError::NotApplicable("snapshot Goal is not RUNNING".to_owned()));
    }
    goal.validate_dag().map_err(|error| GoalVerifierError::InvalidDurableState(error.to_string()))?;
    goal.validate_final_verification_contract()
        .map_err(|error| GoalVerifierError::InvalidDurableState(error.to_string()))?;

    let mut aggregate = evaluate_global_gates(goal);
    let spec = goal.final_verification_spec().expect("validated contract exists");
    let mut bindings = spec.criterion_bindings().to_vec();
    bindings.sort_by(|left, right| left.criterion_id().cmp(right.criterion_id()));
    let criterion_by_id = goal.completion_criteria().iter()
        .map(|criterion| (criterion.id().clone(), criterion))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut criterion_results = Vec::with_capacity(bindings.len());

    for binding in bindings {
        let Some(criterion) = criterion_by_id.get(binding.criterion_id()) else {
            return Err(GoalVerifierError::InvalidDurableState(
                "criterion binding references unknown host criterion".to_owned(),
            ));
        };
        if !criterion.required() {
            return Err(GoalVerifierError::InvalidDurableState(
                "optional authoritative criterion is unsupported in V1".to_owned(),
            ));
        }
        let mut requirements = binding.requirements().to_vec();
        requirements.sort();
        let mut observations = Vec::with_capacity(requirements.len());
        let mut criterion_outcome = VerificationOutcome::Passed;
        for requirement in requirements {
            let observation = evaluate_requirement(goal, &requirement);
            criterion_outcome = combine_outcome(criterion_outcome, observation.outcome());
            observations.push(observation);
        }
        aggregate = combine_outcome(aggregate, criterion_outcome);
        criterion_results.push(GoalCriterionVerificationResult::new(
            criterion.id().clone(),
            criterion_outcome,
            observations,
        ));
    }

    if criterion_results.len() != goal.completion_criteria().len() {
        return Err(GoalVerifierError::InvalidDurableState(
            "required completion criterion coverage is incomplete".to_owned(),
        ));
    }
    Ok(GoalVerificationDecision { outcome: aggregate, criterion_results })
}

fn evaluate_global_gates(goal: &Goal) -> VerificationOutcome {
    let mut outcome = VerificationOutcome::Passed;
    for task in goal.tasks().values().filter(|task| task.mandatory()) {
        let task_outcome = match task.status() {
            TaskStatus::Completed => {
                if task.verification_specs().is_empty() {
                    VerificationOutcome::Indeterminate
                } else {
                    match task.verification_results().last().map(|result| result.outcome()) {
                        Some(VerificationOutcome::Passed) => VerificationOutcome::Passed,
                        Some(VerificationOutcome::Failed) => VerificationOutcome::Failed,
                        Some(VerificationOutcome::Indeterminate) | None => VerificationOutcome::Indeterminate,
                    }
                }
            }
            TaskStatus::Failed | TaskStatus::Cancelled => VerificationOutcome::Failed,
            _ => VerificationOutcome::Indeterminate,
        };
        outcome = combine_outcome(outcome, task_outcome);
        if !task.blockers().is_empty() || task.has_unknown_side_effect() {
            outcome = combine_outcome(outcome, VerificationOutcome::Indeterminate);
        }
    }
    if goal.blockers().iter().any(|blocker| blocker.mandatory())
        || goal.has_unknown_side_effects()
        || goal.has_active_tasks()
        || goal.has_task_status(TaskStatus::NeedsReplan)
    {
        outcome = combine_outcome(outcome, VerificationOutcome::Indeterminate);
    }
    outcome
}

fn evaluate_requirement(goal: &Goal, requirement: &GoalVerificationRequirement) -> GoalRequirementObservation {
    match requirement {
        GoalVerificationRequirement::TaskVerified { task_id } => {
            let Some(task) = goal.tasks().get(task_id) else {
                return GoalRequirementObservation::TaskVerified {
                    task_id: task_id.clone(),
                    task_status: TaskStatus::Blocked,
                    verification_id: None,
                    verification_outcome: None,
                    outcome: VerificationOutcome::Indeterminate,
                };
            };
            let latest = task.verification_results().last();
            let outcome = if !task.mandatory() || task.verification_specs().is_empty() {
                VerificationOutcome::Indeterminate
            } else {
                match task.status() {
                    TaskStatus::Completed => match latest.map(|result| result.outcome()) {
                        Some(VerificationOutcome::Passed) => VerificationOutcome::Passed,
                        Some(VerificationOutcome::Failed) => VerificationOutcome::Failed,
                        Some(VerificationOutcome::Indeterminate) | None => VerificationOutcome::Indeterminate,
                    },
                    TaskStatus::Failed | TaskStatus::Cancelled => VerificationOutcome::Failed,
                    _ => match latest.map(|result| result.outcome()) {
                        Some(VerificationOutcome::Failed) => VerificationOutcome::Failed,
                        _ => VerificationOutcome::Indeterminate,
                    },
                }
            };
            GoalRequirementObservation::TaskVerified {
                task_id: task_id.clone(),
                task_status: task.status(),
                verification_id: latest.map(|result| result.id().clone()),
                verification_outcome: latest.map(|result| result.outcome()),
                outcome,
            }
        }
    }
}

fn combine_outcome(left: VerificationOutcome, right: VerificationOutcome) -> VerificationOutcome {
    match (left, right) {
        (VerificationOutcome::Failed, _) | (_, VerificationOutcome::Failed) => VerificationOutcome::Failed,
        (VerificationOutcome::Indeterminate, _) | (_, VerificationOutcome::Indeterminate) => VerificationOutcome::Indeterminate,
        _ => VerificationOutcome::Passed,
    }
}

fn referenced_task_identities(goal: &Goal) -> Vec<(TaskId, TaskStatus, Option<VerificationId>, Option<VerificationOutcome>)> {
    let Some(spec) = goal.final_verification_spec() else { return Vec::new(); };
    let mut ids = spec.criterion_bindings().iter()
        .flat_map(|binding| binding.requirements())
        .map(GoalVerificationRequirement::task_id)
        .cloned()
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    ids.into_iter().map(|task_id| {
        let task = goal.tasks().get(&task_id);
        let latest = task.and_then(|task| task.verification_results().last());
        (
            task_id,
            task.map(|task| task.status()).unwrap_or(TaskStatus::Blocked),
            latest.map(|result| result.id().clone()),
            latest.map(|result| result.outcome()),
        )
    }).collect()
}

fn identities_match(snapshot: &GoalVerificationSnapshot, goal: &Goal) -> bool {
    referenced_task_identities(goal) == snapshot.task_verification_identities
}

pub(crate) fn commit_goal_verification(
    store: &TaskStore,
    snapshot: &GoalVerificationSnapshot,
    decision: &GoalVerificationDecision,
) -> Result<GoalVerificationResult, GoalVerifierError> {
    let expected_plan_revision = snapshot.plan_revision;
    let expected_digest = snapshot.contract_digest.clone();
    let entry_authority = GoalVerificationEntryAuthority::new();
    let write_authority = GoalFinalVerificationWriteAuthority::new();
    let criterion_results = decision.criterion_results.clone();
    let outcome = decision.outcome;
    let started_at = snapshot.started_at.clone();
    let evaluated_revision = snapshot.revision;
    let committed_revision = snapshot.revision.checked_add(1).ok_or_else(|| {
        GoalVerifierError::InvalidDurableState("Goal revision overflow".to_owned())
    })?;
    let record_id = std::cell::RefCell::new(None::<VerificationId>);

    let goal = store.mutate_goal_snapshot(
        snapshot.goal.session_id(),
        snapshot.goal.id(),
        snapshot.revision,
        |goal, now| {
            if goal.plan_revision() != expected_plan_revision {
                return Err(OrchestratorError::InvalidDag(
                    "stale Goal verification plan revision".to_owned(),
                ));
            }
            let current_digest = goal.final_verification_spec().ok_or_else(|| {
                OrchestratorError::InvalidDag("Goal final-verification contract disappeared".to_owned())
            })?.canonical_digest();
            if current_digest != expected_digest {
                return Err(OrchestratorError::InvalidDag(
                    "stale Goal verification contract digest".to_owned(),
                ));
            }
            if !identities_match(snapshot, goal) {
                return Err(OrchestratorError::InvalidDag(
                    "stale Goal verification Task/Verification identity".to_owned(),
                ));
            }
            let current_snapshot = prepare_goal_verification_from_goal(goal.clone())
                .map_err(|error| OrchestratorError::InvalidDag(error.to_string()))?;
            let current_decision = evaluate_goal_verification(&current_snapshot)
                .map_err(|error| OrchestratorError::InvalidDag(error.to_string()))?;
            if current_decision != *decision {
                return Err(OrchestratorError::InvalidDag(
                    "Goal verification decision became stale before commit".to_owned(),
                ));
            }

            goal.enter_verifying_from_goal_verifier(&entry_authority, now)?;
            let record = GoalFinalVerificationRecord::new(
                outcome,
                goal.id().clone(),
                evaluated_revision,
                committed_revision,
                expected_plan_revision,
                expected_digest.clone(),
                criterion_results.clone(),
                started_at.clone(),
                now.to_owned(),
            );
            *record_id.borrow_mut() = Some(record.id().clone());
            goal.append_final_verification_from_goal_verifier(&write_authority, record)?;
            match outcome {
                VerificationOutcome::Passed => {}
                VerificationOutcome::Failed => goal.transition_to(GoalStatus::Failed, now)?,
                VerificationOutcome::Indeterminate => {
                    goal.add_blocker(GoalBlocker::new(
                        "GOAL_FINAL_VERIFICATION_INDETERMINATE",
                        "deterministic Goal final verification could not safely establish all required proof",
                        true,
                    ))?;
                    goal.transition_to(GoalStatus::Blocked, now)?;
                }
            }
            Ok(())
        },
    ).map_err(|error| match error {
        OrchestratorError::RevisionConflict { .. } => GoalVerifierError::Store(error),
        OrchestratorError::InvalidDag(detail) if detail.starts_with("stale Goal verification")
            || detail.contains("became stale") => GoalVerifierError::StaleEvaluation(detail),
        other => GoalVerifierError::Store(other),
    })?;

    Ok(GoalVerificationResult {
        decision: decision.clone(),
        goal,
        record_id: record_id.into_inner(),
        applied: true,
    })
}

pub(crate) fn verify_goal(
    store: &TaskStore,
    session_id: &str,
    goal_id: &GoalId,
    expected_revision: u64,
) -> Result<GoalVerificationResult, GoalVerifierError> {
    let current = store.load_goal(session_id, goal_id)?;
    if current.revision() != expected_revision {
        return Err(GoalVerifierError::Store(OrchestratorError::RevisionConflict {
            expected: expected_revision,
            actual: current.revision(),
        }));
    }
    if matches!(current.status(), GoalStatus::Verifying | GoalStatus::Failed | GoalStatus::Blocked) {
        if let Some(record) = current.latest_applicable_final_verification() {
            let decision = GoalVerificationDecision {
                outcome: record.outcome(),
                criterion_results: record.criterion_results().to_vec(),
            };
            let record_id = record.id().clone();
            return Ok(GoalVerificationResult {
                decision,
                goal: current,
                record_id: Some(record_id),
                applied: false,
            });
        }
    }
    let snapshot = prepare_goal_verification_from_goal(current)?;
    let decision = evaluate_goal_verification(&snapshot)?;
    commit_goal_verification(store, &snapshot, &decision)
}

pub(crate) fn is_authoritative_record(record: &GoalFinalVerificationRecord) -> bool {
    record.source() == VerificationOrigin::HostDeterministicGoalVerifier
}

#[cfg(test)]
pub(crate) fn evaluate_outcome_for_test(
    goal: Goal,
) -> Result<(VerificationOutcome, Vec<GoalCriterionVerificationResult>), GoalVerifierError> {
    let snapshot = prepare_goal_verification_from_goal(goal)?;
    let decision = evaluate_goal_verification(&snapshot)?;
    Ok((decision.outcome, decision.criterion_results))
}

#[cfg(test)]
pub(crate) fn criterion_id_for_test(value: &str) -> Result<CompletionCriterionId, OrchestratorError> {
    CompletionCriterionId::parse(value)
}
