use std::fs;
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config;
use crate::fallback::{SideEffectClass, SideEffectState};
use crate::goal::{Goal, GoalId, GoalStatus};
use crate::orchestrator_error::OrchestratorError;
use crate::task::{
    AttemptId, ReplaySafety, Task, TaskEvidence, TaskId, TaskOperationKind, TaskScope, TaskStatus,
    TaskTransitionContext, VerificationOutcome, VerificationSpec, WorkerKind,
};
use crate::task_store::{FaultPoint, TaskStore};
use crate::verifier::{VerifierError, commit, evaluate, prepare, verify_task};

const NOW: &str = "2026-09-13T00:00:00Z";

struct Fixture {
    root: PathBuf,
    state: PathBuf,
    session: config::Session,
    store: TaskStore,
    goal_id: GoalId,
    task_id: TaskId,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.state);
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("local-mcp-phase6-{label}-{}", Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    fs::canonicalize(path).unwrap()
}

fn read_fixture(
    specs: Vec<VerificationSpec>,
    max_attempts: u32,
    evidence: Vec<TaskEvidence>,
) -> Fixture {
    let root = temp_dir("workspace");
    let state = temp_dir("state");
    let session = config::Session {
        id: Uuid::new_v4().to_string(),
        cwd: root.clone(),
        permitted_directories: vec![root.clone()],
    };
    let mut goal = Goal::new(
        session.id.clone(),
        root.clone(),
        "phase6 verifier fixture",
        None,
        Vec::new(),
        vec!["mechanical verification".to_owned()],
        NOW,
    )
    .unwrap();
    let task = Task::new(
        "verify fixture",
        "mechanically verify fixture",
        true,
        WorkerKind::CodexReadonly,
        TaskScope::new(
            vec![root.clone()],
            Vec::new(),
            TaskOperationKind::ReadOnly,
            ReplaySafety::SafeReadOnly,
        ),
        specs,
        max_attempts,
        1,
        NOW,
    )
    .unwrap();
    let task_id = task.id().clone();
    goal.materialize_initial_plan(vec![task], NOW).unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Running,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    for item in evidence {
        goal.task_add_evidence(&task_id, item).unwrap();
    }
    goal.transition_task(
        &task_id,
        TaskStatus::Verifying,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    let goal_id = goal.id().clone();
    let store = TaskStore::with_state_root(state.clone());
    store.create_goal(&goal).unwrap();
    Fixture {
        root,
        state,
        session,
        store,
        goal_id,
        task_id,
    }
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn structured_ok(id: &str) -> TaskEvidence {
    TaskEvidence::StructuredObservation {
        requirement_id: id.to_owned(),
        source: "host-test".to_owned(),
        passed: true,
        detail: "mechanically present".to_owned(),
    }
}

#[tokio::test]
async fn verifier_pass_completes_task_once_but_never_goal() {
    let root = temp_dir("seed");
    let file = root.join("exists.txt");
    fs::write(&file, b"ok").unwrap();
    let fixture = read_fixture(
        vec![VerificationSpec::StructuredEvidence {
            requirement_id: "ok".into(),
        }],
        2,
        vec![structured_ok("ok")],
    );
    let before = fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .unwrap();
    let result = verify_task(
        &fixture.store,
        &fixture.session,
        &fixture.goal_id,
        &fixture.task_id,
        before.revision(),
    )
    .await
    .unwrap();
    assert_eq!(result.revision(), before.revision() + 1);
    assert_eq!(
        result.tasks()[&fixture.task_id].status(),
        TaskStatus::Completed
    );
    assert_eq!(
        result.tasks()[&fixture.task_id]
            .verification_results()
            .len(),
        1
    );
    assert!(
        result.tasks()[&fixture.task_id].evidence_count()
            > before.tasks()[&fixture.task_id].evidence_count()
    );
    assert_eq!(result.status(), GoalStatus::Running);
    assert_ne!(result.status(), GoalStatus::Completed);
    let completed_revision = result.revision();
    let second = verify_task(
        &fixture.store,
        &fixture.session,
        &fixture.goal_id,
        &fixture.task_id,
        completed_revision,
    )
    .await;
    assert!(matches!(second, Err(VerifierError::InvalidState(_))));
    assert_eq!(
        fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap()
            .revision(),
        completed_revision
    );
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn file_exists_and_digest_use_current_host_state_not_claimed_hash() {
    let root = temp_dir("file-check");
    let state = temp_dir("file-state");
    let target = root.join("target.txt");
    fs::write(&target, b"current").unwrap();
    let session = config::Session {
        id: Uuid::new_v4().to_string(),
        cwd: root.clone(),
        permitted_directories: vec![root.clone()],
    };
    let mut goal = Goal::new(
        session.id.clone(),
        root.clone(),
        "file checks",
        None,
        vec![],
        vec!["verified".into()],
        NOW,
    )
    .unwrap();
    let task = Task::new(
        "file",
        "file",
        true,
        WorkerKind::CodexReadonly,
        TaskScope::new(
            vec![root.clone()],
            vec![],
            TaskOperationKind::ReadOnly,
            ReplaySafety::SafeReadOnly,
        ),
        vec![
            VerificationSpec::FileExists {
                path: target.clone(),
                must_be_file: true,
            },
            VerificationSpec::FileDigest {
                path: target.clone(),
                expected_sha256: sha(b"current"),
            },
        ],
        2,
        1,
        NOW,
    )
    .unwrap();
    let task_id = task.id().clone();
    goal.materialize_initial_plan(vec![task], NOW).unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Running,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.task_add_evidence(
        &task_id,
        TaskEvidence::FileSnapshot {
            path: target.clone(),
            exists: true,
            size: Some(5),
            sha256: Some(sha(b"wrong")),
        },
    )
    .unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Verifying,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    let goal_id = goal.id().clone();
    let store = TaskStore::with_state_root(state.clone());
    store.create_goal(&goal).unwrap();
    let result = verify_task(&store, &session, &goal_id, &task_id, 1)
        .await
        .unwrap();
    assert_eq!(result.tasks()[&task_id].status(), TaskStatus::Completed);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(state);

    let root = temp_dir("changed-file-check");
    let state = temp_dir("changed-file-state");
    let target = root.join("target.txt");
    fs::write(&target, b"before").unwrap();
    let session = config::Session {
        id: Uuid::new_v4().to_string(),
        cwd: root.clone(),
        permitted_directories: vec![root.clone()],
    };
    let mut goal = Goal::new(
        session.id.clone(),
        root.clone(),
        "changed file",
        None,
        vec![],
        vec!["verified".into()],
        NOW,
    )
    .unwrap();
    let task = Task::new(
        "changed file",
        "changed file",
        true,
        WorkerKind::CodexReadonly,
        TaskScope::new(
            vec![root.clone()],
            vec![],
            TaskOperationKind::ReadOnly,
            ReplaySafety::SafeReadOnly,
        ),
        vec![VerificationSpec::FileDigest {
            path: target.clone(),
            expected_sha256: sha(b"before"),
        }],
        2,
        1,
        NOW,
    )
    .unwrap();
    let task_id = task.id().clone();
    goal.materialize_initial_plan(vec![task], NOW).unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Running,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.task_add_evidence(
        &task_id,
        TaskEvidence::FileSnapshot {
            path: target.clone(),
            exists: true,
            size: Some(6),
            sha256: Some(sha(b"before")),
        },
    )
    .unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Verifying,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    let goal_id = goal.id().clone();
    let store = TaskStore::with_state_root(state.clone());
    store.create_goal(&goal).unwrap();
    fs::write(&target, b"after").unwrap();
    let changed = verify_task(&store, &session, &goal_id, &task_id, 1)
        .await
        .unwrap();
    assert_eq!(changed.tasks()[&task_id].status(), TaskStatus::Retryable);
    assert_eq!(
        changed.tasks()[&task_id].verification_results()[0].outcome(),
        VerificationOutcome::Failed
    );
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(state);
}

#[tokio::test]
async fn missing_file_is_retryable_when_attempt_budget_remains() {
    let root = temp_dir("missing-seed");
    let missing = root.join("missing.txt");
    let state = temp_dir("missing-state");
    let session = config::Session {
        id: Uuid::new_v4().to_string(),
        cwd: root.clone(),
        permitted_directories: vec![root.clone()],
    };
    let mut goal = Goal::new(
        session.id.clone(),
        root.clone(),
        "missing",
        None,
        vec![],
        vec!["verified".into()],
        NOW,
    )
    .unwrap();
    let task = Task::new(
        "missing",
        "missing",
        true,
        WorkerKind::CodexReadonly,
        TaskScope::new(
            vec![root.clone()],
            vec![],
            TaskOperationKind::ReadOnly,
            ReplaySafety::SafeReadOnly,
        ),
        vec![VerificationSpec::FileExists {
            path: missing,
            must_be_file: true,
        }],
        2,
        1,
        NOW,
    )
    .unwrap();
    let task_id = task.id().clone();
    goal.materialize_initial_plan(vec![task], NOW).unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Running,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Verifying,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    let goal_id = goal.id().clone();
    let store = TaskStore::with_state_root(state.clone());
    store.create_goal(&goal).unwrap();
    let result = verify_task(&store, &session, &goal_id, &task_id, 1)
        .await
        .unwrap();
    assert_eq!(result.tasks()[&task_id].status(), TaskStatus::Retryable);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(state);
}

#[tokio::test]
async fn digest_mismatch_needs_replan_when_retry_budget_exhausted() {
    let root = temp_dir("digest-seed");
    let target = root.join("target.txt");
    fs::write(&target, b"new").unwrap();
    let state = temp_dir("digest-state");
    let session = config::Session {
        id: Uuid::new_v4().to_string(),
        cwd: root.clone(),
        permitted_directories: vec![root.clone()],
    };
    let mut goal = Goal::new(
        session.id.clone(),
        root.clone(),
        "digest",
        None,
        vec![],
        vec!["verified".into()],
        NOW,
    )
    .unwrap();
    let task = Task::new(
        "digest",
        "digest",
        true,
        WorkerKind::CodexReadonly,
        TaskScope::new(
            vec![root.clone()],
            vec![],
            TaskOperationKind::ReadOnly,
            ReplaySafety::SafeReadOnly,
        ),
        vec![VerificationSpec::FileDigest {
            path: target,
            expected_sha256: sha(b"old"),
        }],
        1,
        1,
        NOW,
    )
    .unwrap();
    let task_id = task.id().clone();
    goal.materialize_initial_plan(vec![task], NOW).unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Running,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Verifying,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    let goal_id = goal.id().clone();
    let plan_revision = goal.plan_revision();
    let task_count = goal.tasks().len();
    let store = TaskStore::with_state_root(state.clone());
    store.create_goal(&goal).unwrap();
    let result = verify_task(&store, &session, &goal_id, &task_id, 1)
        .await
        .unwrap();
    assert_eq!(result.tasks()[&task_id].status(), TaskStatus::NeedsReplan);
    assert_eq!(result.plan_revision(), plan_revision);
    assert_eq!(result.tasks().len(), task_count);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(state);
}

#[tokio::test]
async fn missing_and_negative_structured_evidence_never_complete() {
    let missing = read_fixture(
        vec![VerificationSpec::StructuredEvidence {
            requirement_id: "required".into(),
        }],
        2,
        vec![],
    );
    let goal = missing
        .store
        .load_goal(&missing.session.id, &missing.goal_id)
        .unwrap();
    let blocked = verify_task(
        &missing.store,
        &missing.session,
        &missing.goal_id,
        &missing.task_id,
        goal.revision(),
    )
    .await
    .unwrap();
    assert_eq!(
        blocked.tasks()[&missing.task_id].status(),
        TaskStatus::Blocked
    );
    assert_eq!(
        blocked.tasks()[&missing.task_id].verification_results()[0].outcome(),
        VerificationOutcome::Indeterminate
    );

    let negative = read_fixture(
        vec![VerificationSpec::StructuredEvidence {
            requirement_id: "required".into(),
        }],
        2,
        vec![TaskEvidence::StructuredObservation {
            requirement_id: "required".into(),
            source: "host-test".into(),
            passed: false,
            detail: "false".into(),
        }],
    );
    let goal = negative
        .store
        .load_goal(&negative.session.id, &negative.goal_id)
        .unwrap();
    let failed = verify_task(
        &negative.store,
        &negative.session,
        &negative.goal_id,
        &negative.task_id,
        goal.revision(),
    )
    .await
    .unwrap();
    assert_eq!(
        failed.tasks()[&negative.task_id].status(),
        TaskStatus::Failed
    );
}

#[tokio::test]
async fn review_gate_is_advisory_evidence_rechecked_by_host_verifier() {
    let accepted = read_fixture(
        vec![VerificationSpec::ReviewGate {
            max_blocking_findings: 0,
        }],
        2,
        vec![TaskEvidence::ReviewResult {
            summary: "clear".into(),
            blocking_findings: 0,
        }],
    );
    let goal = accepted
        .store
        .load_goal(&accepted.session.id, &accepted.goal_id)
        .unwrap();
    let completed = verify_task(
        &accepted.store,
        &accepted.session,
        &accepted.goal_id,
        &accepted.task_id,
        goal.revision(),
    )
    .await
    .unwrap();
    assert_eq!(
        completed.tasks()[&accepted.task_id].status(),
        TaskStatus::Completed
    );

    let blocked = read_fixture(
        vec![VerificationSpec::ReviewGate {
            max_blocking_findings: 0,
        }],
        2,
        vec![TaskEvidence::ReviewResult {
            summary: "one blocker".into(),
            blocking_findings: 1,
        }],
    );
    let goal = blocked
        .store
        .load_goal(&blocked.session.id, &blocked.goal_id)
        .unwrap();
    let result = verify_task(
        &blocked.store,
        &blocked.session,
        &blocked.goal_id,
        &blocked.task_id,
        goal.revision(),
    )
    .await
    .unwrap();
    assert_eq!(
        result.tasks()[&blocked.task_id].status(),
        TaskStatus::Blocked
    );
}

#[cfg(not(windows))]
#[tokio::test]
async fn command_exit_pass_and_failure_use_existing_execution_authority() {
    let pass = read_fixture(
        vec![VerificationSpec::CommandExit {
            command: vec!["/usr/bin/true".into()],
            cwd: None,
            accepted_exit_codes: vec![0],
        }],
        2,
        vec![],
    );
    let goal = pass
        .store
        .load_goal(&pass.session.id, &pass.goal_id)
        .unwrap();
    let done = verify_task(
        &pass.store,
        &pass.session,
        &pass.goal_id,
        &pass.task_id,
        goal.revision(),
    )
    .await
    .unwrap();
    assert_eq!(done.tasks()[&pass.task_id].status(), TaskStatus::Completed);

    let fail = read_fixture(
        vec![VerificationSpec::CommandExit {
            command: vec!["/usr/bin/false".into()],
            cwd: None,
            accepted_exit_codes: vec![0],
        }],
        2,
        vec![],
    );
    let goal = fail
        .store
        .load_goal(&fail.session.id, &fail.goal_id)
        .unwrap();
    let result = verify_task(
        &fail.store,
        &fail.session,
        &fail.goal_id,
        &fail.task_id,
        goal.revision(),
    )
    .await
    .unwrap();
    assert_eq!(
        result.tasks()[&fail.task_id].status(),
        TaskStatus::Retryable
    );
}

#[tokio::test]
async fn mutating_verification_command_is_rejected_without_state_change() {
    let fixture = read_fixture(
        vec![VerificationSpec::CommandExit {
            command: vec!["git".into(), "commit".into()],
            cwd: None,
            accepted_exit_codes: vec![0],
        }],
        2,
        vec![],
    );
    let before = fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .unwrap();
    let result = verify_task(
        &fixture.store,
        &fixture.session,
        &fixture.goal_id,
        &fixture.task_id,
        before.revision(),
    )
    .await;
    assert!(matches!(result, Err(VerifierError::InvalidState(_))));
    let after = fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .unwrap();
    assert_eq!(after, before);
}

#[tokio::test]
async fn stale_result_is_rejected_by_revision_cas() {
    let fixture = read_fixture(
        vec![VerificationSpec::StructuredEvidence {
            requirement_id: "ok".into(),
        }],
        2,
        vec![structured_ok("ok")],
    );
    let current = fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .unwrap();
    let snapshot = prepare(
        &fixture.store,
        &fixture.session,
        &fixture.goal_id,
        &fixture.task_id,
        current.revision(),
    )
    .unwrap();
    let decision = evaluate(&snapshot, &fixture.session).await.unwrap();
    fixture
        .store
        .mutate_goal_snapshot(
            &fixture.session.id,
            &fixture.goal_id,
            current.revision(),
            |goal, _| goal.task_add_evidence(&fixture.task_id, structured_ok("later")),
        )
        .unwrap();
    let advanced = fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .unwrap();
    let stale = commit(&fixture.store, &fixture.session, &snapshot, decision);
    assert!(matches!(
        stale,
        Err(VerifierError::Store(
            OrchestratorError::RevisionConflict { .. }
        ))
    ));
    assert_eq!(
        fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap(),
        advanced
    );
    assert_eq!(
        advanced.tasks()[&fixture.task_id].status(),
        TaskStatus::Verifying
    );
}

#[tokio::test]
async fn stale_attempt_identity_cannot_commit_completion() {
    let fixture = read_fixture(
        vec![VerificationSpec::StructuredEvidence {
            requirement_id: "ok".into(),
        }],
        2,
        vec![structured_ok("ok")],
    );
    let before = fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .unwrap();
    let snapshot = prepare(
        &fixture.store,
        &fixture.session,
        &fixture.goal_id,
        &fixture.task_id,
        before.revision(),
    )
    .unwrap();
    let decision = evaluate(&snapshot, &fixture.session).await.unwrap();
    let forged = snapshot.with_test_attempt_id(AttemptId::new());
    let result = commit(&fixture.store, &fixture.session, &forged, decision);
    assert!(matches!(
        result,
        Err(VerifierError::Store(OrchestratorError::InvalidDag(message)))
            if message == "stale verifier attempt identity"
    ));
    assert_eq!(
        fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap(),
        before
    );
    assert_eq!(
        before.tasks()[&fixture.task_id].status(),
        TaskStatus::Verifying
    );
}

#[tokio::test]
async fn persistence_failure_is_transactional() {
    let fixture = read_fixture(
        vec![VerificationSpec::StructuredEvidence {
            requirement_id: "ok".into(),
        }],
        2,
        vec![structured_ok("ok")],
    );
    let current = fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .unwrap();
    let path = fixture
        .state
        .join("goals")
        .join(&fixture.session.id)
        .join(format!("{}.json", fixture.goal_id.as_str()));
    let before = fs::read(&path).unwrap();
    let fault = TaskStore::with_fault(fixture.state.clone(), FaultPoint::BeforeReplace);
    let result = verify_task(
        &fault,
        &fixture.session,
        &fixture.goal_id,
        &fixture.task_id,
        current.revision(),
    )
    .await;
    assert!(matches!(
        result,
        Err(VerifierError::Store(OrchestratorError::PersistenceIo(_)))
    ));
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn unknown_side_effect_blocks_even_when_file_looks_right() {
    let root = temp_dir("unknown-workspace");
    let state = temp_dir("unknown-state");
    let target = root.join("target.txt");
    fs::write(&target, b"ok").unwrap();
    let session = config::Session {
        id: Uuid::new_v4().to_string(),
        cwd: root.clone(),
        permitted_directories: vec![root.clone()],
    };
    let mut goal = Goal::new(
        session.id.clone(),
        root.clone(),
        "unknown",
        None,
        vec![],
        vec!["verified".into()],
        NOW,
    )
    .unwrap();
    let task = Task::new(
        "unknown",
        "unknown",
        true,
        WorkerKind::LocalOperation,
        TaskScope::new(
            vec![root.clone()],
            vec![],
            TaskOperationKind::LocalMutation,
            ReplaySafety::VerifyBeforeRetry,
        ),
        vec![VerificationSpec::FileDigest {
            path: target.clone(),
            expected_sha256: sha(b"ok"),
        }],
        2,
        1,
        NOW,
    )
    .unwrap();
    let task_id = task.id().clone();
    goal.materialize_initial_plan(vec![task], NOW).unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Running,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.task_bind_latest_attempt_execution(
        &task_id,
        Some("op".into()),
        Some("scope".into()),
        Some("req".into()),
        Some(SideEffectClass::LocalMutation),
        Some(SideEffectState::ConfirmedPerformed),
        Some(0),
        Some(0),
    )
    .unwrap();
    goal.task_add_evidence(
        &task_id,
        TaskEvidence::FileSnapshot {
            path: target.clone(),
            exists: true,
            size: Some(2),
            sha256: Some(sha(b"ok")),
        },
    )
    .unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Verifying,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.task_bind_latest_attempt_execution(
        &task_id,
        None,
        None,
        None,
        Some(SideEffectClass::LocalMutation),
        Some(SideEffectState::Unknown),
        Some(0),
        Some(0),
    )
    .unwrap();
    let goal_id = goal.id().clone();
    let store = TaskStore::with_state_root(state.clone());
    store.create_goal(&goal).unwrap();
    let result = verify_task(&store, &session, &goal_id, &task_id, 1)
        .await
        .unwrap();
    assert_eq!(result.tasks()[&task_id].status(), TaskStatus::Blocked);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(state);
}

#[tokio::test]
async fn verifier_targets_only_explicit_task_and_runs_no_scheduler() {
    let root = temp_dir("multi-workspace");
    let state = temp_dir("multi-state");
    let session = config::Session {
        id: Uuid::new_v4().to_string(),
        cwd: root.clone(),
        permitted_directories: vec![root.clone()],
    };
    let mut goal = Goal::new(
        session.id.clone(),
        root.clone(),
        "multi",
        None,
        vec![],
        vec!["verified".into()],
        NOW,
    )
    .unwrap();
    let make = |title: &str| {
        Task::new(
            title,
            title,
            true,
            WorkerKind::CodexReadonly,
            TaskScope::new(
                vec![root.clone()],
                vec![],
                TaskOperationKind::ReadOnly,
                ReplaySafety::SafeReadOnly,
            ),
            vec![VerificationSpec::StructuredEvidence {
                requirement_id: "ok".into(),
            }],
            2,
            1,
            NOW,
        )
        .unwrap()
    };
    let a = make("a");
    let a_id = a.id().clone();
    let b = make("b");
    let b_id = b.id().clone();
    goal.materialize_initial_plan(vec![a, b], NOW).unwrap();
    for id in [&a_id, &b_id] {
        goal.transition_task(
            id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.task_add_evidence(id, structured_ok("ok")).unwrap();
        goal.transition_task(
            id,
            TaskStatus::Verifying,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
    }
    let goal_id = goal.id().clone();
    let store = TaskStore::with_state_root(state.clone());
    store.create_goal(&goal).unwrap();
    let result = verify_task(&store, &session, &goal_id, &a_id, 1)
        .await
        .unwrap();
    assert_eq!(result.tasks()[&a_id].status(), TaskStatus::Completed);
    assert_eq!(result.tasks()[&b_id].status(), TaskStatus::Verifying);
    assert_eq!(result.status(), GoalStatus::Running);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(state);
}

#[cfg(not(windows))]
fn init_git(root: &PathBuf) {
    let status = std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(not(windows))]
#[tokio::test]
async fn git_scope_and_forbidden_changes_are_host_observed() {
    let root = temp_dir("git-workspace");
    init_git(&root);
    let allowed = root.join("allowed.txt");
    fs::write(&allowed, b"ok").unwrap();
    let state = temp_dir("git-state");
    let session = config::Session {
        id: Uuid::new_v4().to_string(),
        cwd: root.clone(),
        permitted_directories: vec![root.clone()],
    };
    let mut goal = Goal::new(
        session.id.clone(),
        root.clone(),
        "git",
        None,
        vec![],
        vec!["verified".into()],
        NOW,
    )
    .unwrap();
    let task = Task::new(
        "git",
        "git",
        true,
        WorkerKind::CodexReadonly,
        TaskScope::new(
            vec![root.clone()],
            vec![],
            TaskOperationKind::ReadOnly,
            ReplaySafety::SafeReadOnly,
        ),
        vec![VerificationSpec::GitScope {
            allowed_changed_paths: vec![allowed.clone()],
            require_no_other_changes: true,
        }],
        2,
        1,
        NOW,
    )
    .unwrap();
    let task_id = task.id().clone();
    goal.materialize_initial_plan(vec![task], NOW).unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Running,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Verifying,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    let goal_id = goal.id().clone();
    let store = TaskStore::with_state_root(state.clone());
    store.create_goal(&goal).unwrap();
    let result = verify_task(&store, &session, &goal_id, &task_id, 1)
        .await
        .unwrap();
    assert_eq!(result.tasks()[&task_id].status(), TaskStatus::Completed);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(state);

    let root = temp_dir("forbidden-workspace");
    init_git(&root);
    let forbidden = root.join("forbidden.txt");
    fs::write(&forbidden, b"no").unwrap();
    let state = temp_dir("forbidden-state");
    let session = config::Session {
        id: Uuid::new_v4().to_string(),
        cwd: root.clone(),
        permitted_directories: vec![root.clone()],
    };
    let mut goal = Goal::new(
        session.id.clone(),
        root.clone(),
        "forbidden",
        None,
        vec![],
        vec!["verified".into()],
        NOW,
    )
    .unwrap();
    let task = Task::new(
        "forbidden",
        "forbidden",
        true,
        WorkerKind::CodexReadonly,
        TaskScope::new(
            vec![root.clone()],
            vec![],
            TaskOperationKind::ReadOnly,
            ReplaySafety::SafeReadOnly,
        ),
        vec![VerificationSpec::NoForbiddenChanges {
            forbidden_paths: vec![forbidden.clone()],
        }],
        2,
        1,
        NOW,
    )
    .unwrap();
    let task_id = task.id().clone();
    goal.materialize_initial_plan(vec![task], NOW).unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Running,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Verifying,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    let goal_id = goal.id().clone();
    let store = TaskStore::with_state_root(state.clone());
    store.create_goal(&goal).unwrap();
    let result = verify_task(&store, &session, &goal_id, &task_id, 1)
        .await
        .unwrap();
    assert_eq!(result.tasks()[&task_id].status(), TaskStatus::Blocked);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(state);
}

#[cfg(not(windows))]
#[tokio::test]
async fn verification_command_timeout_is_bounded_and_never_passes() {
    let fixture = read_fixture(
        vec![VerificationSpec::StructuredEvidence {
            requirement_id: "ok".into(),
        }],
        2,
        vec![structured_ok("ok")],
    );
    let command = vec!["/bin/sleep".to_owned(), "1".to_owned()];
    let result = crate::verifier::test_command_timeout(
        &command,
        &fixture.root,
        &fixture.session,
        std::time::Duration::from_millis(20),
    )
    .await;
    assert!(matches!(result, Err(VerifierError::Observation(_))));
}
