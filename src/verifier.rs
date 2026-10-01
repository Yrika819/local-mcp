use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

#[cfg(test)]
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config;

use crate::fallback::SideEffectState;
use crate::goal::{Goal, GoalId, GoalStatus};
use crate::orchestrator_error::OrchestratorError;
use crate::task::{
    AttemptId, TaskBlocker, TaskEvidence, TaskId, TaskOperationKind, TaskScope, TaskStatus,
    TaskTransitionContext, VerificationCheckResult, VerificationOutcome, VerificationResult,
    VerificationSpec,
};
use crate::task_store::{TaskStore, utc_now_rfc3339};

use crate::verifier_git_observation::{self, GitObservation};

const VERIFIER_SOURCE: &str = "HOST_DETERMINISTIC_VERIFIER";

pub(crate) struct VerifierCompletionAuthority {
    _private: (),
}

impl VerifierCompletionAuthority {
    fn new() -> Self {
        Self { _private: () }
    }
}

#[derive(Debug)]
pub(crate) enum VerifierError {
    Store(OrchestratorError),
    InvalidState(String),
    Observation(String),
}

impl fmt::Display for VerifierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(f, "verifier durable-state error: {error}"),
            Self::InvalidState(reason) => write!(f, "verifier state rejected: {reason}"),
            Self::Observation(reason) => write!(f, "verifier observation failed: {reason}"),
        }
    }
}

impl std::error::Error for VerifierError {}

impl From<OrchestratorError> for VerifierError {
    fn from(value: OrchestratorError) -> Self {
        Self::Store(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerificationDecisionOutcome {
    Pass,
    Retryable,
    Blocked,
    NeedsReplan,
    Failed,
}

impl VerificationDecisionOutcome {
    fn task_status(self) -> TaskStatus {
        match self {
            Self::Pass => TaskStatus::Completed,
            Self::Retryable => TaskStatus::Retryable,
            Self::Blocked => TaskStatus::Blocked,
            Self::NeedsReplan => TaskStatus::NeedsReplan,
            Self::Failed => TaskStatus::Failed,
        }
    }

    fn result_outcome(self) -> VerificationOutcome {
        match self {
            Self::Pass => VerificationOutcome::Passed,
            Self::Blocked => VerificationOutcome::Indeterminate,
            Self::Retryable | Self::NeedsReplan | Self::Failed => VerificationOutcome::Failed,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VerificationDecision {
    outcome: VerificationDecisionOutcome,
    summary: String,
    checks: Vec<VerificationCheckResult>,
    observations: Vec<Observation>,
    evidence: Vec<TaskEvidence>,
}

#[allow(
    dead_code,
    reason = "Frozen verification-decision accessors are retained for staged runner consumers."
)]
impl VerificationDecision {
    pub(crate) fn outcome(&self) -> VerificationDecisionOutcome {
        self.outcome
    }

    pub(crate) fn summary(&self) -> &str {
        &self.summary
    }
}

#[derive(Clone, Debug)]
struct Observation {
    index: Option<usize>,
    kind: String,
    fact: String,
    passed: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    goal_id: GoalId,
    revision: u64,
    plan_revision: u32,
    cwd: PathBuf,
    task_id: TaskId,
    attempt_id: AttemptId,
    operation_id: Option<String>,
    scope_identity: Option<String>,
    specs: Vec<VerificationSpec>,
    scope: TaskScope,
    evidence: Vec<TaskEvidence>,
    blockers: Vec<TaskBlocker>,
    side_effect_state: Option<SideEffectState>,
    retry_allowed: bool,
    started_at: String,
}

#[cfg(test)]
impl Snapshot {
    pub(crate) fn with_test_attempt_id(mut self, attempt_id: AttemptId) -> Self {
        self.attempt_id = attempt_id;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Disposition {
    Pass,
    Retryable,
    Blocked,
    NeedsReplan,
    Failed,
}

#[derive(Clone, Debug)]
struct Check {
    passed: bool,
    detail: String,
    disposition: Disposition,
    evidence: Vec<TaskEvidence>,
}

#[cfg(test)]
struct CommandObservation {
    command_finished: bool,
    command_start_proof: String,
}

pub(crate) async fn verify_task(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    expected_revision: u64,
) -> Result<Goal, VerifierError> {
    let snapshot = prepare(store, session, goal_id, task_id, expected_revision)?;
    let decision = evaluate(&snapshot, session).await?;
    commit(store, session, &snapshot, decision)
}

pub(crate) fn prepare(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    expected_revision: u64,
) -> Result<Snapshot, VerifierError> {
    let goal = store.load_goal(&session.id, goal_id)?;
    if goal.revision() != expected_revision {
        return Err(VerifierError::Store(OrchestratorError::RevisionConflict {
            expected: expected_revision,
            actual: goal.revision(),
        }));
    }
    validate_session_binding(&goal, session)?;
    if goal.status() != GoalStatus::Running {
        return Err(VerifierError::InvalidState(format!(
            "Task verification requires a RUNNING Goal, found {:?}",
            goal.status()
        )));
    }
    let task = goal
        .tasks()
        .get(task_id)
        .ok_or_else(|| VerifierError::InvalidState("verification Task is missing".to_owned()))?;
    if task.status() != TaskStatus::Verifying {
        return Err(VerifierError::InvalidState(format!(
            "verification requires VERIFYING, found {:?}",
            task.status()
        )));
    }
    let attempt = task
        .latest_attempt()
        .ok_or_else(|| VerifierError::InvalidState("VERIFYING Task lacks attempt".to_owned()))?;
    let execution_root = crate::planner::execution_root_for_goal(&goal, session)
        .map_err(|error| VerifierError::InvalidState(error.to_string()))?;
    Ok(Snapshot {
        goal_id: goal.id().clone(),
        revision: goal.revision(),
        plan_revision: goal.plan_revision(),
        cwd: execution_root,
        task_id: task_id.clone(),
        attempt_id: attempt.id().clone(),
        operation_id: attempt.operation_id().map(str::to_owned),
        scope_identity: attempt.scope_identity().map(str::to_owned),
        specs: task.verification_specs().to_vec(),
        scope: task.scope().clone(),
        evidence: task.evidence().to_vec(),
        blockers: task.blockers().to_vec(),
        side_effect_state: attempt.side_effect_state(),
        retry_allowed: task.verification_retry_allowed(),
        started_at: utc_now_rfc3339(),
    })
}

pub(crate) async fn evaluate(
    snapshot: &Snapshot,
    session: &config::Session,
) -> Result<VerificationDecision, VerifierError> {
    let mut checks = Vec::with_capacity(snapshot.specs.len());
    let mut observations = Vec::new();
    let mut evidence = Vec::new();
    let mut dispositions = Vec::new();
    let mut passed_count = 0usize;

    if !snapshot.blockers.is_empty() {
        observations.push(Observation {
            index: None,
            kind: "TASK_BLOCKER_GATE".to_owned(),
            fact: format!("{} unresolved Task blocker(s)", snapshot.blockers.len()),
            passed: false,
        });
        dispositions.push(Disposition::Blocked);
    }

    let side_effect_ok = side_effect_reconciled(snapshot);
    observations.push(Observation {
        index: None,
        kind: "SIDE_EFFECT_GATE".to_owned(),
        fact: snapshot
            .side_effect_state
            .map(|state| format!("latest side-effect state is {}", state.as_str()))
            .unwrap_or_else(|| "no effectful side-effect state is recorded".to_owned()),
        passed: side_effect_ok,
    });
    if !side_effect_ok {
        dispositions.push(Disposition::Blocked);
    }

    let git_required = snapshot.specs.iter().any(|spec| {
        matches!(
            spec,
            VerificationSpec::GitScope { .. } | VerificationSpec::NoForbiddenChanges { .. }
        )
    });
    let git_for_scope = snapshot.scope.operation_kind() != TaskOperationKind::ReadOnly;
    let git = if git_required || git_for_scope {
        observe_git(&snapshot.cwd, session).await.ok()
    } else {
        None
    };

    if git_required && git.is_none() {
        observations.push(Observation {
            index: None,
            kind: "GIT_OBSERVATION_GATE".to_owned(),
            fact: "required Git state could not be observed through execution authority".to_owned(),
            passed: false,
        });
        dispositions.push(Disposition::Blocked);
    }

    if let Some(git) = git.as_ref() {
        evidence.push(TaskEvidence::GitSnapshot {
            head: git.head.clone(),
            changed_paths: git.changed.clone(),
            staged_paths: git.staged.clone(),
        });
        if git_for_scope {
            let scope_ok = task_scope_allows_git_changes(&snapshot.scope, &git.changed);
            observations.push(Observation {
                index: None,
                kind: "TASK_SCOPE_GATE".to_owned(),
                fact: if scope_ok {
                    "all current Git changes are within durable TaskScope".to_owned()
                } else {
                    "Git change exists outside allowed scope or inside forbidden scope".to_owned()
                },
                passed: scope_ok,
            });
            if !scope_ok {
                dispositions.push(Disposition::Blocked);
            }
        }
    } else if git_for_scope {
        let paths = snapshot.evidence.iter().filter_map(|item| match item {
            TaskEvidence::FileSnapshot { path, .. } => Some(path),
            _ => None,
        });
        let mut any = false;
        let mut scope_ok = true;
        for path in paths {
            any = true;
            scope_ok &= in_allowed(path, snapshot.scope.allowed_paths())
                && !in_boundaries(path, snapshot.scope.forbidden_paths());
        }
        observations.push(Observation {
            index: None,
            kind: "TASK_SCOPE_GATE".to_owned(),
            fact: if any {
                "host FileSnapshot paths checked against TaskScope".to_owned()
            } else {
                "no Git state or host FileSnapshot exists for mutating scope reconciliation"
                    .to_owned()
            },
            passed: any && scope_ok,
        });
        if !any || !scope_ok {
            dispositions.push(Disposition::Blocked);
        }
    }

    for (index, spec) in snapshot.specs.iter().enumerate() {
        let check = evaluate_spec(spec, snapshot, session, git.as_ref()).await?;
        if check.passed {
            passed_count += 1;
        }
        checks.push(VerificationCheckResult::new(
            index,
            check.passed,
            Some(check.detail.clone()),
        ));
        observations.push(Observation {
            index: Some(index),
            kind: kind_name(spec).to_owned(),
            fact: check.detail,
            passed: check.passed,
        });
        evidence.extend(check.evidence);
        dispositions.push(check.disposition);
    }

    let outcome = aggregate(&dispositions, snapshot.retry_allowed);
    Ok(VerificationDecision {
        outcome,
        summary: format!(
            "deterministic host verification: {passed_count}/{} durable checks passed; outcome={outcome:?}",
            snapshot.specs.len()
        ),
        checks,
        observations,
        evidence,
    })
}

async fn evaluate_spec(
    spec: &VerificationSpec,
    snapshot: &Snapshot,
    _session: &config::Session,
    git: Option<&GitObservation>,
) -> Result<Check, VerifierError> {
    match spec {
        VerificationSpec::FileExists { path, must_be_file } => {
            let path = resolve_path(path, &snapshot.cwd)?;
            match fs::metadata(&path) {
                Ok(metadata) => {
                    let passed = !*must_be_file || metadata.is_file();
                    let digest = if metadata.is_file() {
                        hash_file(&path).ok()
                    } else {
                        None
                    };
                    Ok(Check {
                        passed,
                        detail: if passed {
                            format!("{} exists with required type", path.display())
                        } else {
                            format!("{} exists but is not a regular file", path.display())
                        },
                        disposition: if passed {
                            Disposition::Pass
                        } else {
                            Disposition::Retryable
                        },
                        evidence: vec![TaskEvidence::FileSnapshot {
                            path,
                            exists: true,
                            size: metadata.is_file().then_some(metadata.len()),
                            sha256: digest,
                        }],
                    })
                }
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(Check {
                    passed: false,
                    detail: format!("{} does not exist", path.display()),
                    disposition: Disposition::Retryable,
                    evidence: vec![TaskEvidence::FileSnapshot {
                        path,
                        exists: false,
                        size: None,
                        sha256: None,
                    }],
                }),
                Err(error) => Ok(Check {
                    passed: false,
                    detail: format!("cannot observe {}: {error}", path.display()),
                    disposition: Disposition::Blocked,
                    evidence: Vec::new(),
                }),
            }
        }
        VerificationSpec::FileDigest {
            path,
            expected_sha256,
        } => {
            let path = resolve_path(path, &snapshot.cwd)?;
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => {
                    let actual = hash_file(&path).map_err(|error| {
                        VerifierError::Observation(format!(
                            "cannot hash {}: {error}",
                            path.display()
                        ))
                    })?;
                    let passed = actual.eq_ignore_ascii_case(expected_sha256);
                    Ok(Check {
                        passed,
                        detail: if passed {
                            format!("{} SHA-256 matches durable expectation", path.display())
                        } else {
                            format!(
                                "{} SHA-256 mismatch: expected {}, observed {}",
                                path.display(),
                                expected_sha256,
                                actual
                            )
                        },
                        disposition: if passed {
                            Disposition::Pass
                        } else {
                            Disposition::Retryable
                        },
                        evidence: vec![TaskEvidence::FileSnapshot {
                            path,
                            exists: true,
                            size: Some(metadata.len()),
                            sha256: Some(actual),
                        }],
                    })
                }
                Ok(_) => Ok(Check {
                    passed: false,
                    detail: format!("{} is not a regular file", path.display()),
                    disposition: Disposition::Retryable,
                    evidence: vec![TaskEvidence::FileSnapshot {
                        path,
                        exists: true,
                        size: None,
                        sha256: None,
                    }],
                }),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(Check {
                    passed: false,
                    detail: format!("{} is absent", path.display()),
                    disposition: Disposition::Retryable,
                    evidence: vec![TaskEvidence::FileSnapshot {
                        path,
                        exists: false,
                        size: None,
                        sha256: None,
                    }],
                }),
                Err(error) => Ok(Check {
                    passed: false,
                    detail: format!("cannot observe {}: {error}", path.display()),
                    disposition: Disposition::Blocked,
                    evidence: Vec::new(),
                }),
            }
        }
        VerificationSpec::CommandExit { .. } => Ok(Check {
            passed: false,
            detail: "legacy COMMAND_EXIT verification is unsupported; no command was spawned; replace it with mechanically evaluated verification specifications".to_owned(),
            disposition: Disposition::Blocked,
            evidence: Vec::new(),
        }),
        VerificationSpec::GitScope {
            allowed_changed_paths,
            require_no_other_changes,
        } => {
            let Some(git) = git else {
                return Ok(Check {
                    passed: false,
                    detail: "Git scope cannot be evaluated because Git observation is unavailable"
                        .to_owned(),
                    disposition: Disposition::Blocked,
                    evidence: Vec::new(),
                });
            };
            let allowed = allowed_changed_paths
                .iter()
                .map(|path| resolve_path(path, &snapshot.cwd))
                .collect::<Result<Vec<_>, _>>()?;
            let outside = git
                .changed
                .iter()
                .filter(|path| !in_boundaries(path, &allowed))
                .count();
            let passed = !*require_no_other_changes || outside == 0;
            Ok(Check {
                passed,
                detail: if passed {
                    format!(
                        "Git scope accepted {} changed path(s) from {}",
                        git.changed.len(),
                        git.root.display()
                    )
                } else {
                    format!("Git contains {outside} changed path(s) outside allowed scope")
                },
                disposition: if passed {
                    Disposition::Pass
                } else {
                    Disposition::Blocked
                },
                evidence: Vec::new(),
            })
        }
        VerificationSpec::NoForbiddenChanges { forbidden_paths } => {
            let Some(git) = git else {
                return Ok(Check {
                    passed: false,
                    detail: "forbidden-path check cannot be evaluated without Git observation"
                        .to_owned(),
                    disposition: Disposition::Blocked,
                    evidence: Vec::new(),
                });
            };
            let forbidden = forbidden_paths
                .iter()
                .map(|path| resolve_path(path, &snapshot.cwd))
                .collect::<Result<Vec<_>, _>>()?;
            let violating = git
                .changed
                .iter()
                .filter(|path| in_boundaries(path, &forbidden))
                .count();
            let passed = violating == 0;
            Ok(Check {
                passed,
                detail: if passed {
                    "no forbidden Git path is changed".to_owned()
                } else {
                    format!("{violating} forbidden Git path(s) are changed")
                },
                disposition: if passed {
                    Disposition::Pass
                } else {
                    Disposition::Blocked
                },
                evidence: Vec::new(),
            })
        }
        VerificationSpec::StructuredEvidence { requirement_id } => {
            let found = snapshot.evidence.iter().rev().find_map(|item| match item {
                TaskEvidence::StructuredObservation {
                    requirement_id: id,
                    source,
                    passed,
                    detail,
                } if id == requirement_id => Some((source, *passed, detail)),
                _ => None,
            });
            match found {
                Some((source, passed, detail)) => Ok(Check {
                    passed,
                    detail: format!(
                        "structured requirement {requirement_id} from {source}: {detail}"
                    ),
                    disposition: if passed {
                        Disposition::Pass
                    } else {
                        Disposition::Failed
                    },
                    evidence: Vec::new(),
                }),
                None => Ok(Check {
                    passed: false,
                    detail: format!(
                        "required host structured evidence {requirement_id} is missing"
                    ),
                    disposition: Disposition::Blocked,
                    evidence: Vec::new(),
                }),
            }
        }
        VerificationSpec::ReviewGate {
            max_blocking_findings,
        } => {
            let found = snapshot.evidence.iter().rev().find_map(|item| match item {
                TaskEvidence::ReviewResult {
                    summary,
                    blocking_findings,
                } => Some((summary, *blocking_findings)),
                _ => None,
            });
            match found {
                Some((summary, count)) => {
                    let passed = count <= *max_blocking_findings;
                    Ok(Check {
                        passed,
                        detail: format!(
                            "review gate observed {count} blocking finding(s), maximum {max_blocking_findings}: {summary}"
                        ),
                        disposition: if passed {
                            Disposition::Pass
                        } else {
                            Disposition::Blocked
                        },
                        evidence: Vec::new(),
                    })
                }
                None => Ok(Check {
                    passed: false,
                    detail: "required durable ReviewResult evidence is missing".to_owned(),
                    disposition: Disposition::Blocked,
                    evidence: Vec::new(),
                }),
            }
        }
    }
}

pub(crate) fn commit(
    store: &TaskStore,
    session: &config::Session,
    snapshot: &Snapshot,
    decision: VerificationDecision,
) -> Result<Goal, VerifierError> {
    let finished_at = utc_now_rfc3339();
    let result = VerificationResult::new(
        decision.outcome.result_outcome(),
        decision.checks.clone(),
        snapshot.started_at.clone(),
        finished_at.clone(),
    );
    let verification_id = result.id().clone();
    let passed = decision.outcome == VerificationDecisionOutcome::Pass;
    let authority = VerifierCompletionAuthority::new();

    store
        .mutate_goal_snapshot(
            &session.id,
            &snapshot.goal_id,
            snapshot.revision,
            |goal, now| {
                if goal.status() != GoalStatus::Running
                    || goal.plan_revision() != snapshot.plan_revision
                {
                    return Err(OrchestratorError::InvalidDag(
                        "stale verifier Goal/plan state".to_owned(),
                    ));
                }
                let task = goal.tasks().get(&snapshot.task_id).ok_or_else(|| {
                    OrchestratorError::InvalidDag("verification Task disappeared".to_owned())
                })?;
                if task.status() != TaskStatus::Verifying
                    || task.verification_specs() != snapshot.specs.as_slice()
                {
                    return Err(OrchestratorError::InvalidDag(
                        "stale verifier Task/spec state".to_owned(),
                    ));
                }
                let attempt = task.latest_attempt().ok_or_else(|| {
                    OrchestratorError::CorruptGoal("VERIFYING Task lacks attempt".to_owned())
                })?;
                if attempt.id() != &snapshot.attempt_id
                    || attempt.operation_id() != snapshot.operation_id.as_deref()
                    || attempt.scope_identity() != snapshot.scope_identity.as_deref()
                {
                    return Err(OrchestratorError::InvalidDag(
                        "stale verifier attempt identity".to_owned(),
                    ));
                }

                goal.task_record_verification_result(&snapshot.task_id, result.clone())?;
                for item in decision.evidence.iter().cloned() {
                    goal.task_add_evidence(&snapshot.task_id, item)?;
                }
                for item in &decision.observations {
                    goal.task_add_evidence(
                        &snapshot.task_id,
                        TaskEvidence::VerificationObservation {
                            verification_id: verification_id.clone(),
                            attempt_id: snapshot.attempt_id.clone(),
                            verification_index: item.index,
                            verification_kind: item.kind.clone(),
                            observed_fact: item.fact.clone(),
                            passed: item.passed,
                            observed_at: finished_at.clone(),
                            source: VERIFIER_SOURCE.to_owned(),
                        },
                    )?;
                }
                goal.task_add_evidence(
                    &snapshot.task_id,
                    TaskEvidence::Verification {
                        verification_id: verification_id.clone(),
                        passed,
                    },
                )?;

                match decision.outcome {
                    VerificationDecisionOutcome::Pass => goal.complete_task_from_verifier(
                        &snapshot.task_id,
                        &authority,
                        TaskTransitionContext {
                            active_worker_stopped: true,
                            side_effect_reconciled: true,
                        },
                        now,
                    )?,
                    VerificationDecisionOutcome::Blocked => {
                        goal.task_add_blocker(
                            &snapshot.task_id,
                            TaskBlocker::new(
                                "VERIFICATION_BLOCKED",
                                decision.summary.clone(),
                                true,
                            ),
                        )?;
                        goal.transition_task(
                            &snapshot.task_id,
                            TaskStatus::Blocked,
                            TaskTransitionContext::default(),
                            now,
                        )?;
                    }
                    other => goal.transition_task(
                        &snapshot.task_id,
                        other.task_status(),
                        TaskTransitionContext::default(),
                        now,
                    )?,
                }
                Ok(())
            },
        )
        .map_err(VerifierError::from)
}

fn aggregate(values: &[Disposition], retry_allowed: bool) -> VerificationDecisionOutcome {
    if values.contains(&Disposition::Blocked) {
        VerificationDecisionOutcome::Blocked
    } else if values.contains(&Disposition::Failed) {
        VerificationDecisionOutcome::Failed
    } else if values.contains(&Disposition::NeedsReplan) {
        VerificationDecisionOutcome::NeedsReplan
    } else if values.contains(&Disposition::Retryable) {
        if retry_allowed {
            VerificationDecisionOutcome::Retryable
        } else {
            VerificationDecisionOutcome::NeedsReplan
        }
    } else {
        VerificationDecisionOutcome::Pass
    }
}

fn side_effect_reconciled(snapshot: &Snapshot) -> bool {
    if snapshot.side_effect_state == Some(SideEffectState::Unknown) {
        return false;
    }
    if snapshot.scope.operation_kind() == TaskOperationKind::ReadOnly {
        return true;
    }
    if snapshot.side_effect_state == Some(SideEffectState::ConfirmedPerformed) {
        return true;
    }
    snapshot.evidence.iter().rev().any(|item| {
        matches!(
            item,
            TaskEvidence::RecoveryReconciliation {
                side_effect_state: SideEffectState::ConfirmedPerformed,
                postcondition_proven: true,
                ..
            }
        )
    })
}

fn validate_session_binding(goal: &Goal, session: &config::Session) -> Result<(), VerifierError> {
    if goal.session_id() != session.id {
        return Err(VerifierError::InvalidState(
            "Goal/session binding mismatch".to_owned(),
        ));
    }
    let goal_cwd = fs::canonicalize(goal.cwd()).map_err(|error| {
        VerifierError::InvalidState(format!("Goal cwd cannot be canonicalized: {error}"))
    })?;
    let session_cwd = fs::canonicalize(&session.cwd).map_err(|error| {
        VerifierError::InvalidState(format!("session cwd cannot be canonicalized: {error}"))
    })?;
    if goal_cwd != session_cwd {
        return Err(VerifierError::InvalidState(
            "Goal cwd does not match session cwd".to_owned(),
        ));
    }
    config::validate_path_authority(session, &goal_cwd, config::PathIntent::ExecutionCwd).map_err(
        |error| VerifierError::InvalidState(format!("Goal cwd is outside session roots: {error}")),
    )?;
    crate::planner::execution_root_for_goal(goal, session)
        .map_err(|error| VerifierError::InvalidState(error.to_string()))?;
    Ok(())
}

fn kind_name(spec: &VerificationSpec) -> &'static str {
    match spec {
        VerificationSpec::CommandExit { .. } => "COMMAND_EXIT",
        VerificationSpec::FileExists { .. } => "FILE_EXISTS",
        VerificationSpec::FileDigest { .. } => "FILE_DIGEST",
        VerificationSpec::GitScope { .. } => "GIT_SCOPE",
        VerificationSpec::NoForbiddenChanges { .. } => "NO_FORBIDDEN_CHANGES",
        VerificationSpec::StructuredEvidence { .. } => "STRUCTURED_EVIDENCE",
        VerificationSpec::ReviewGate { .. } => "REVIEW_GATE",
    }
}

fn resolve_path(path: &Path, root: &Path) -> Result<PathBuf, VerifierError> {
    if path.components().any(|part| part == Component::ParentDir) {
        return Err(VerifierError::InvalidState(
            "verification path contains parent traversal".to_owned(),
        ));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let resolved = canonicalize_existing_prefix(&absolute, root)?;
    if !resolved.starts_with(root) {
        return Err(VerifierError::InvalidState(format!(
            "verification path escapes Goal cwd: {}",
            path.display()
        )));
    }
    Ok(resolved)
}

fn canonicalize_existing_prefix(
    path: &Path,
    spelling_root: &Path,
) -> Result<PathBuf, VerifierError> {
    // One implementation, shared with the Git observation seam, so the two
    // cannot drift into resolving a path differently.
    verifier_git_observation::canonicalize_existing_prefix(path, spelling_root).map_err(|error| {
        VerifierError::InvalidState(format!("cannot canonicalize verification path: {error}"))
    })
}

pub(crate) fn task_scope_allows_git_changes(scope: &TaskScope, changed: &[PathBuf]) -> bool {
    changed.iter().all(|path| {
        in_allowed(path, scope.allowed_paths()) && !in_boundaries(path, scope.forbidden_paths())
    })
}

fn in_allowed(path: &Path, allowed: &[PathBuf]) -> bool {
    !allowed.is_empty() && in_boundaries(path, allowed)
}

fn in_boundaries(path: &Path, boundaries: &[PathBuf]) -> bool {
    boundaries
        .iter()
        .any(|boundary| path == boundary || path.starts_with(boundary))
}

fn hash_file(path: &Path) -> std::io::Result<String> {
    Ok(hash_bytes(&fs::read(path)?))
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
fn parse_execution_payload(text: &str) -> Result<CommandObservation, VerifierError> {
    let value: Value = serde_json::from_str(text).map_err(|_| {
        VerifierError::Observation("execution authority returned malformed evidence".to_owned())
    })?;
    Ok(CommandObservation {
        // The execution layer keeps the requested command's lifecycle separate
        // from the sandbox wrapper's, and only the requested command's own
        // completion can satisfy a verification check. `sandbox_process_finished`
        // is deliberately not read here: where the sandbox is a separate wrapper
        // it describes the wrapper, and a wrapper that exited says nothing about
        // whether the requested command ever exec'd.
        command_finished: value
            .get("command_finished")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        command_start_proof: value
            .get("command_start_proof")
            .and_then(Value::as_str)
            .unwrap_or("UNAVAILABLE")
            .to_owned(),
    })
}

async fn observe_git(
    root: &Path,
    session: &config::Session,
) -> Result<GitObservation, VerifierError> {
    // Host-owned observation, not a model proposal and not the generic command
    // path: the Verifier's own Git state is gathered by a narrow seam that owns
    // the executable, the argv, and the lifecycle proof.
    verifier_git_observation::observe(root, session)
        .await
        .map_err(VerifierError::Observation)
}

#[cfg(test)]
#[allow(
    dead_code,
    reason = "Windows-only path spelling regression fixture calls this helper on that target."
)]
pub(crate) fn resolve_git_path_for_test(raw: &str, root: &Path) -> Result<PathBuf, VerifierError> {
    verifier_git_observation::resolve_git_path(raw, root).map_err(VerifierError::Observation)
}

/// The requested-command lifecycle facts the Verifier derives from an execution
/// payload, exposed so the wrapper/requested-command split can be tested
/// directly against synthetic host evidence.
#[cfg(test)]
pub(crate) struct CommandLifecycleForTest {
    pub(crate) command_finished: bool,
    pub(crate) command_start_proof: String,
}

#[cfg(test)]
pub(crate) fn parse_command_lifecycle_for_test(
    text: &str,
) -> Result<CommandLifecycleForTest, VerifierError> {
    let observation = parse_execution_payload(text)?;
    Ok(CommandLifecycleForTest {
        command_finished: observation.command_finished,
        command_start_proof: observation.command_start_proof,
    })
}

/// The same observation with an explicit approval policy.
///
/// A test that is not about the approval gate uses this, so it does not need a
/// live approval responder on the platform that has one. The production policy
/// is exercised end to end by the managed-worktree integration test, which spawns
/// a real approval responder on Windows.
#[cfg(test)]
pub(crate) async fn test_observe_git_with_policy(
    root: &Path,
    session: &config::Session,
    requires_approval: bool,
) -> Result<verifier_git_observation::GitObservation, VerifierError> {
    verifier_git_observation::observe_with_policy(root, session, requires_approval)
        .await
        .map_err(VerifierError::Observation)
}
