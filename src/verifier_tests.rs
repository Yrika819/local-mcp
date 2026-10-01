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
    /// Whether this fixture created `root` and may therefore delete it.
    ///
    /// A caller that supplies its own workspace (a write probe that needs several
    /// surviving directories) owns the cleanup. Without this, dropping the first
    /// fixture would delete the shared workspace and every later assertion would
    /// pass for the wrong reason.
    owns_root: bool,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.owns_root {
            let _ = fs::remove_dir_all(&self.root);
        }
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
    let session = config::Session {
        id: Uuid::new_v4().to_string(),
        cwd: root.clone(),
        permitted_directories: vec![root.clone()],
    };
    let scope = TaskScope::new(
        vec![root.clone()],
        vec![],
        TaskOperationKind::ReadOnly,
        ReplaySafety::SafeReadOnly,
    );
    let mut fixture = read_fixture_in(specs, max_attempts, evidence, root, scope, session);
    // This fixture created the workspace, so it may clean it up.
    fixture.owns_root = true;
    fixture
}

/// The same fixture, but with a caller-chosen workspace, TaskScope, and Session
/// authority.
///
/// Used to build a Session that genuinely permits a directory the TaskScope
/// never granted, so a write probe can distinguish "not permitted" from "not
/// authorized for this Task". The caller keeps ownership of `root`.
fn read_fixture_in(
    specs: Vec<VerificationSpec>,
    max_attempts: u32,
    evidence: Vec<TaskEvidence>,
    root: PathBuf,
    scope: TaskScope,
    session: config::Session,
) -> Fixture {
    let state = temp_dir("state");
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
        scope,
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
        owns_root: false,
    }
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Legacy COMMAND_EXIT never spawns, even for a formerly approved observation.
///
/// This proves the unsupported durable-state path is a one-time terminal block,
/// and that commands targeting TaskScope-allowed/forbidden and other Session roots
/// cannot create or alter files.
#[tokio::test]
async fn legacy_command_exit_blocks_without_spawning_or_writing_anywhere() {
    let base = temp_dir("write-probe");
    let session_root = base.join("session");
    let allowed = session_root.join("allowed");
    let forbidden = session_root.join("forbidden");
    // A second root the Session genuinely permits but the TaskScope never grants.
    let elsewhere = base.join("elsewhere");
    for directory in [&allowed, &forbidden, &elsewhere] {
        fs::create_dir_all(directory).unwrap();
    }
    let session = config::Session {
        id: Uuid::new_v4().to_string(),
        cwd: session_root.clone(),
        // The Session permits all three. The claim is not about Session
        // authority; it is that a pure observation has no write authority at all.
        permitted_directories: vec![session_root.clone(), elsewhere.clone()],
    };
    let scope = TaskScope::new(
        vec![allowed.clone()],
        vec![forbidden.clone()],
        TaskOperationKind::ReadOnly,
        ReplaySafety::SafeReadOnly,
    );

    // Two pre-existing files, one to try to create and one to try to alter, in
    // each of the three destinations.
    let mut targets: Vec<(PathBuf, PathBuf, &'static str)> = Vec::new();
    for directory in [&allowed, &forbidden, &elsewhere] {
        let sentinel = directory.join("sentinel.txt");
        fs::write(&sentinel, b"do-not-modify\n").unwrap();
        targets.push((
            directory.join("created.txt"),
            sentinel,
            match directory.as_path() {
                p if p == allowed.as_path() => "task-scope-allowed",
                p if p == forbidden.as_path() => "task-scope-forbidden",
                _ => "other-session-permitted",
            },
        ));
    }

    let read_fixture_for = |command: Vec<String>| {
        read_fixture_in(
            vec![VerificationSpec::CommandExit {
                command,
                cwd: None,
                accepted_exit_codes: vec![0],
            }],
            2,
            Vec::new(),
            session_root.clone(),
            TaskScope::new(
                vec![allowed.clone()],
                vec![forbidden.clone()],
                TaskOperationKind::ReadOnly,
                ReplaySafety::SafeReadOnly,
            ),
            config::Session {
                id: session.id.clone(),
                cwd: session.cwd.clone(),
                permitted_directories: session.permitted_directories.clone(),
            },
        )
    };

    for (create_target, sentinel, label) in &targets {
        for command in [
            // A shell write.
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("printf 'x' > {}", create_target.display()),
            ],
            vec![
                "bash".to_owned(),
                "-c".to_owned(),
                format!("printf 'x' > {}", create_target.display()),
            ],
            // A direct create and a direct alter.
            vec!["touch".to_owned(), create_target.display().to_string()],
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("printf 'x' > {}", sentinel.display()),
            ],
            // A script.
            vec![
                "python3".to_owned(),
                "-c".to_owned(),
                format!(
                    "open({:?}, 'w').write('x')",
                    create_target.display().to_string()
                ),
            ],
            // An argv that starts from a host-approved shape and then smuggles a
            // write through Git's own configuration.
            vec![
                "git".to_owned(),
                "-c".to_owned(),
                format!("core.fsmonitor=touch {}", create_target.display()),
                "rev-parse".to_owned(),
                "--show-toplevel".to_owned(),
            ],
            // An approved shape with the destination appended as an extra argument.
            vec![
                "git".to_owned(),
                "rev-parse".to_owned(),
                "--show-toplevel".to_owned(),
                create_target.display().to_string(),
            ],
            // A mutating Git shape aimed at the same destination.
            vec![
                "git".to_owned(),
                "worktree".to_owned(),
                "add".to_owned(),
                create_target.display().to_string(),
            ],
        ] {
            let fixture = read_fixture_for(command.clone());
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
            let after = result.expect("legacy COMMAND_EXIT is durably blocked");
            assert_eq!(
                after.tasks()[&fixture.task_id].status(),
                TaskStatus::Blocked,
                "{label}: {command:?} must block once without spawn"
            );
            assert!(
                after.revision() > before.revision(),
                "the block must be durable"
            );
            assert!(
                !create_target.exists(),
                "{label}: {command:?} created {}",
                create_target.display()
            );
            assert_eq!(
                fs::read(sentinel).unwrap(),
                b"do-not-modify\n",
                "{label}: {command:?} altered {}",
                sentinel.display()
            );
        }
    }

    // A formerly host-approved pure observation is equally unsupported and never runs.
    let approved = read_fixture_for(vec![
        "git".to_owned(),
        "rev-parse".to_owned(),
        "--show-toplevel".to_owned(),
    ]);
    let before = approved
        .store
        .load_goal(&approved.session.id, &approved.goal_id)
        .unwrap();
    let approved_result = verify_task(
        &approved.store,
        &approved.session,
        &approved.goal_id,
        &approved.task_id,
        before.revision(),
    )
    .await
    .expect("legacy command must be blocked");
    assert_eq!(
        approved_result.tasks()[&approved.task_id].status(),
        TaskStatus::Blocked
    );
    let _ = scope;
    for (create_target, sentinel, label) in &targets {
        assert!(
            !create_target.exists(),
            "{label}: the approved pure observation created {}",
            create_target.display()
        );
        assert_eq!(
            fs::read(sentinel).unwrap(),
            b"do-not-modify\n",
            "{label}: the approved pure observation altered {}",
            sentinel.display()
        );
    }

    let _ = fs::remove_dir_all(base);
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

/// A legacy COMMAND_EXIT that was once host-approved cannot execute after the
/// product decision to require mechanically evaluated verification.
#[tokio::test]
async fn legacy_command_exit_is_blocked_even_for_host_git_observation_shape() {
    let fixture = read_fixture(
        vec![VerificationSpec::CommandExit {
            command: vec![
                "git".to_owned(),
                "rev-parse".to_owned(),
                "--show-toplevel".to_owned(),
            ],
            cwd: None,
            accepted_exit_codes: vec![0],
        }],
        2,
        vec![],
    );
    init_git(&fixture.root);
    let goal = fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .unwrap();
    let result = verify_task(
        &fixture.store,
        &fixture.session,
        &fixture.goal_id,
        &fixture.task_id,
        goal.revision(),
    )
    .await
    .unwrap();
    assert_eq!(
        result.tasks()[&fixture.task_id].status(),
        TaskStatus::Blocked
    );
    assert_eq!(
        result.tasks()[&fixture.task_id].status(),
        TaskStatus::Blocked
    );
    for _ in 0..3 {
        assert!(!matches!(
            crate::scheduler::select_next_action(&result).unwrap(),
            crate::scheduler::SchedulerDecision::VerifyTask { .. }
        ));
    }
}

/// Unsupported legacy commands are all durably blocked without spawning.
#[tokio::test]
async fn unapproved_verification_commands_are_refused_before_spawn() {
    for command in [
        // A generic executable is never host-approved authority.
        vec!["/usr/bin/true".to_owned()],
        vec!["/usr/bin/false".to_owned()],
        // Shells and interpreters.
        vec!["sh".to_owned(), "-c".to_owned(), "git status".to_owned()],
        vec!["bash".to_owned(), "-lc".to_owned(), "git status".to_owned()],
        vec!["zsh".to_owned(), "-c".to_owned(), "git status".to_owned()],
        vec!["cmd".to_owned(), "/c".to_owned(), "git status".to_owned()],
        vec![
            "powershell".to_owned(),
            "-Command".to_owned(),
            "git status".to_owned(),
        ],
        // Scripts and interpreters.
        vec!["python".to_owned(), "scripts/check.py".to_owned()],
        vec!["python3".to_owned(), "-c".to_owned(), "print(1)".to_owned()],
        // Mutating and ambiguous Git.
        vec!["git".to_owned(), "commit".to_owned()],
        vec!["git".to_owned(), "add".to_owned(), "--".to_owned()],
        vec!["touch".to_owned(), "file".to_owned()],
        vec!["rm".to_owned(), "-rf".to_owned(), "dir".to_owned()],
        vec![
            "git".to_owned(),
            "symbolic-ref".to_owned(),
            "HEAD".to_owned(),
            "refs/heads/x".to_owned(),
        ],
        vec![
            "git".to_owned(),
            "symbolic-ref".to_owned(),
            "--delete".to_owned(),
            "HEAD".to_owned(),
        ],
        vec!["git".to_owned(), "branch".to_owned(), "new-name".to_owned()],
        // A build or test driver is not pure observation.
        vec!["cargo".to_owned(), "test".to_owned()],
        vec!["gradle".to_owned(), "test".to_owned()],
        vec!["./gradlew".to_owned(), "test".to_owned()],
        vec!["npm".to_owned(), "test".to_owned()],
        // An authority-shaping global must not redirect a verification command
        // away from the validated execution root.
        vec![
            "git".to_owned(),
            "-C".to_owned(),
            "/elsewhere".to_owned(),
            "rev-parse".to_owned(),
            "--show-toplevel".to_owned(),
        ],
        vec!["some-unknown-tool".to_owned(), "--read-only".to_owned()],
    ] {
        let fixture = read_fixture(
            vec![VerificationSpec::CommandExit {
                command: command.clone(),
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
        let after = result.expect("legacy COMMAND_EXIT must be blocked without spawn");
        assert_eq!(
            after.tasks()[&fixture.task_id].status(),
            TaskStatus::Blocked
        );
        assert!(after.revision() > before.revision());
    }
}

/// `sandbox_process_finished` is an observation of the process the host waited
/// on. It is never evidence that the requested command ran, so the Verifier can
/// never read it as command completion.
#[test]
fn wrapper_completion_is_not_requested_command_completion() {
    // The confirmed black-box result: the wrapper finished, the requested
    // command never started, and the reported exit code is still zero.
    let unproven = r#"{
        "request_id": "r1",
        "host_reached": true,
        "command_started": false,
        "command_start_proof": "UNPROVEN",
        "command_finished": false,
        "sandbox_process_finished": true,
        "exit_code": 0
    }"#;
    let lifecycle = crate::verifier::parse_command_lifecycle_for_test(unproven).unwrap();
    assert!(!lifecycle.command_finished);
    assert_eq!(lifecycle.command_start_proof, "UNPROVEN");

    // A proven start is the only thing that makes the requested command's own
    // completion usable.
    let proven = r#"{
        "request_id": "r2",
        "host_reached": true,
        "command_started": true,
        "command_start_proof": "PROVEN",
        "command_finished": true,
        "sandbox_process_finished": true,
        "exit_code": 0
    }"#;
    let lifecycle = crate::verifier::parse_command_lifecycle_for_test(proven).unwrap();
    assert!(lifecycle.command_finished);
    assert_eq!(lifecycle.command_start_proof, "PROVEN");

    // A refuted start is never completion either.
    let refuted = r#"{
        "request_id": "r3",
        "host_reached": true,
        "command_started": false,
        "command_start_proof": "REFUTED",
        "command_finished": false,
        "sandbox_process_finished": true,
        "exit_code": 0
    }"#;
    let lifecycle = crate::verifier::parse_command_lifecycle_for_test(refuted).unwrap();
    assert!(!lifecycle.command_finished);
}

/// A mutating argv labelled `read_only_command` is still a mutation, and the
/// independent host classification is what reaches the Verifier.
#[test]
fn read_only_command_metadata_cannot_override_a_mutating_argv() {
    use crate::fallback::OperationIntent;
    use crate::fallback::OperationType;

    let labelled = OperationIntent {
        kind: OperationType::ReadOnlyCommand,
        operation_id: None,
        paths: vec![],
        argv: vec!["git".into(), "branch".into(), "new-name".into()],
        source: None,
        destination: None,
        target: None,
        create_only: false,
        force: false,
        attempt_budget_remaining: 1,
        side_effect_budget_remaining: 1,
    };
    assert_eq!(
        crate::fallback::infer_side_effect_class(
            &["git".to_owned(), "branch".to_owned(), "new-name".to_owned()],
            Some(&labelled)
        ),
        SideEffectClass::LocalMutation
    );
    assert_eq!(
        crate::fallback::infer_side_effect_class(
            &[
                "git".to_owned(),
                "symbolic-ref".to_owned(),
                "--delete".to_owned(),
                "HEAD".to_owned()
            ],
            Some(&labelled)
        ),
        SideEffectClass::LocalMutation
    );
}

#[tokio::test]
async fn mutating_legacy_verification_command_is_blocked_without_spawn() {
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
    let after = result.expect("legacy mutation proposal is durably blocked");
    assert_eq!(
        after.tasks()[&fixture.task_id].status(),
        TaskStatus::Blocked
    );
    assert!(after.revision() > before.revision());
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
async fn forbidden_rename_destination_is_refused_by_production_scope_gate() {
    let root = temp_dir("forbidden-destination-rename");
    let session = config::Session {
        id: Uuid::new_v4().to_string(),
        cwd: root.clone(),
        permitted_directories: vec![root.clone()],
    };
    let forbidden = root.join("ab forbidden destination.txt");
    let scope = TaskScope::new(
        vec![root.join("allowed source.txt")],
        vec![forbidden.clone()],
        TaskOperationKind::ReadOnly,
        ReplaySafety::SafeReadOnly,
    );
    let fixture = read_fixture_in(
        vec![VerificationSpec::NoForbiddenChanges {
            forbidden_paths: vec![forbidden.clone()],
        }],
        2,
        vec![],
        root.clone(),
        scope,
        session,
    );
    init_git(&fixture.root);
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&fixture.root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
    };
    git(&["config", "user.email", "verifier@example.invalid"]);
    git(&["config", "user.name", "Verifier Test"]);
    fs::write(fixture.root.join("allowed source.txt"), b"content\n").unwrap();
    git(&["add", "--", "allowed source.txt"]);
    git(&["commit", "-q", "-m", "add allowed source"]);
    fs::rename(fixture.root.join("allowed source.txt"), &forbidden).unwrap();
    let observed = crate::verifier_git_observation::observe(&fixture.root, &fixture.session)
        .await
        .expect("trusted Git observation parses real rename");
    assert!(
        observed
            .changed
            .contains(&fixture.root.join("allowed source.txt"))
    );
    assert!(observed.changed.contains(&forbidden));
    assert!(
        !crate::verifier::task_scope_allows_git_changes(
            fixture
                .store
                .load_goal(&fixture.session.id, &fixture.goal_id)
                .unwrap()
                .tasks()[&fixture.task_id]
                .scope(),
            &observed.changed,
        ),
        "the production TaskScope predicate must reject the forbidden rename destination"
    );
    let goal = fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .unwrap();
    let result = verify_task(
        &fixture.store,
        &fixture.session,
        &fixture.goal_id,
        &fixture.task_id,
        goal.revision(),
    )
    .await
    .unwrap();
    assert_eq!(
        result.tasks()[&fixture.task_id].status(),
        TaskStatus::Blocked
    );
    fs::remove_dir_all(root).unwrap();
}

#[cfg(not(windows))]
#[tokio::test]
async fn forbidden_rename_source_is_refused_by_production_scope_gate() {
    let root = temp_dir("forbidden-rename");
    init_git(&root);
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["config", "user.email", "verifier@example.invalid"]);
    git(&["config", "user.name", "Verifier Test"]);
    fs::write(root.join("ab forbidden.txt"), b"forbidden\n").unwrap();
    git(&["add", "--", "ab forbidden.txt"]);
    git(&["commit", "-q", "-m", "add forbidden source"]);
    fs::rename(root.join("ab forbidden.txt"), root.join("allowed.txt")).unwrap();
    let observed = crate::verifier_git_observation::observe(
        &root,
        &config::Session {
            id: "rename-observation".to_owned(),
            cwd: root.clone(),
            permitted_directories: vec![root.clone()],
        },
    )
    .await
    .expect("trusted Git observation parses real rename");
    assert!(observed.changed.contains(&root.join("ab forbidden.txt")));
    assert!(observed.changed.contains(&root.join("allowed.txt")));
    let scope = TaskScope::new(
        vec![root.join("allowed.txt")],
        vec![root.join("ab forbidden.txt")],
        TaskOperationKind::ReadOnly,
        ReplaySafety::SafeReadOnly,
    );
    assert!(
        !crate::verifier::task_scope_allows_git_changes(&scope, &observed.changed),
        "the production TaskScope predicate must reject the forbidden rename source"
    );

    let state = temp_dir("forbidden-rename-state");
    let session = config::Session {
        id: Uuid::new_v4().to_string(),
        cwd: root.clone(),
        permitted_directories: vec![root.clone()],
    };
    let mut goal = Goal::new(
        session.id.clone(),
        root.clone(),
        "forbidden rename source",
        None,
        vec![],
        vec!["forbidden paths remain unchanged".into()],
        NOW,
    )
    .unwrap();
    let task = Task::new(
        "rename",
        "rename forbidden file",
        true,
        WorkerKind::CodexReadonly,
        scope,
        vec![VerificationSpec::NoForbiddenChanges {
            forbidden_paths: vec![root.join("ab forbidden.txt")],
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
        .expect("verifier commits the forbidden-path refusal");
    assert_eq!(result.tasks()[&task_id].status(), TaskStatus::Blocked);
    assert_eq!(
        result.tasks()[&task_id]
            .verification_results()
            .last()
            .unwrap()
            .outcome(),
        crate::task::VerificationOutcome::Indeterminate
    );
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(state).unwrap();
}
