//! Real-Git acceptance coverage for Phase 5 managed workspace evidence.
//!
//! This deliberately exercises the production snapshot observer, canonical
//! manifest, durable Goal store, and async finalizer against an owned temporary
//! repository and linked worktree. It does not invoke the complete Goal verifier
//! orchestration path; the valid final-verification record is host-fixture data.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;
use uuid::Uuid;

use crate::config::Session;
use crate::goal::{
    Goal, GoalCriterionBinding, GoalFinalVerificationRecord, GoalFinalVerificationSpec, GoalId,
    GoalVerificationRequirement, ManagedWorkspaceVerificationEvidenceV1,
};
use crate::goal_finalizer::{
    GoalFinalizationOutcome, finalize_goal_at_revision, finalize_goal_at_revision_with_session,
};
use crate::managed_worktree::{ManagedWorktreeRecord, WorktreeId};
use crate::managed_worktree_snapshot_coherence::observe_stable_managed_workspace;
use crate::managed_worktree_snapshot_manifest::assemble_workspace_snapshot_manifest;
use crate::task::{
    ReplaySafety, Task, TaskOperationKind, TaskScope, TaskStatus, TaskTransitionContext,
    VerificationId, VerificationOutcome, VerificationResult, WorkerKind,
};
use crate::task_store::TaskStore;

const NOW: &str = "2026-10-12T00:00:00Z";

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|error| panic!("could not run git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned()
}

struct Fixture {
    cleanup_root: PathBuf,
    primary: PathBuf,
    worktree: PathBuf,
    state: PathBuf,
    session: Session,
    store: TaskStore,
    goal: Goal,
    base: String,
    primary_head: String,
    primary_bytes: Vec<u8>,
}

impl Fixture {
    fn new() -> Self {
        let cleanup_root =
            std::env::temp_dir().join(format!("local-mcp-phase5-3c-{}", Uuid::new_v4()));
        let primary = cleanup_root.join("primary");
        let worktree = cleanup_root.join("managed").join("candidate");
        let state = cleanup_root.join("state");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&state).unwrap();

        git(&primary, &["init", "-q", "."]);
        git(&primary, &["config", "core.autocrlf", "false"]);
        git(
            &primary,
            &["config", "user.email", "phase5@example.invalid"],
        );
        git(&primary, &["config", "user.name", "Phase 5 Acceptance"]);
        git(&primary, &["config", "commit.gpgsign", "false"]);
        std::fs::write(primary.join("tracked.txt"), "base content\n").unwrap();
        git(&primary, &["add", "tracked.txt"]);
        git(&primary, &["commit", "-q", "-m", "base"]);
        let base = git(&primary, &["rev-parse", "HEAD"]);
        let primary_head = base.clone();
        let primary_bytes = std::fs::read(primary.join("tracked.txt")).unwrap();

        let goal_id = GoalId::new();
        let worktree_id = WorktreeId::parse(&Uuid::new_v4().to_string()).unwrap();
        let branch = format!("local-mcp/goal/{}", goal_id.as_str());
        let branch_ref = format!("refs/heads/{branch}");
        let worktree_string = worktree.to_string_lossy().into_owned();
        git(
            &primary,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                &worktree_string,
                &base,
            ],
        );
        let lock_reason = crate::managed_worktree::managed_lock_reason(&goal_id);
        git(
            &primary,
            &[
                "worktree",
                "lock",
                "--reason",
                &lock_reason,
                &worktree_string,
            ],
        );

        let primary = std::fs::canonicalize(primary).unwrap();
        let worktree = std::fs::canonicalize(worktree).unwrap();
        let common_reported = git(
            &primary,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );
        let common_dir = std::fs::canonicalize(common_reported).unwrap();
        let session = Session {
            id: format!("s{}", Uuid::new_v4().simple()),
            cwd: primary.clone(),
            permitted_directories: vec![primary.clone(), worktree.parent().unwrap().to_path_buf()],
        };
        let record = ManagedWorktreeRecord::requested(
            &goal_id,
            worktree_id.clone(),
            primary.clone(),
            common_dir,
            worktree.clone(),
            base.clone(),
            Some(branch_ref),
            1,
            0,
        )
        .unwrap()
        .to_prepared()
        .unwrap()
        .consume_creation_attempt()
        .unwrap()
        .to_active(&base, NOW)
        .unwrap();

        let mut goal = Goal::new_managed_with_id(
            goal_id,
            session.id.clone(),
            primary.clone(),
            "verify managed candidate",
            None,
            Vec::new(),
            vec!["candidate is valid".to_owned()],
            NOW,
            record,
        )
        .unwrap();
        let task = Task::new(
            "acceptance task",
            "prove candidate verification is bound",
            true,
            WorkerKind::CodexReadonly,
            TaskScope::new(
                vec![primary.join("tracked.txt")],
                Vec::new(),
                TaskOperationKind::ReadOnly,
                ReplaySafety::SafeReadOnly,
            ),
            Vec::new(),
            1,
            1,
            NOW,
        )
        .unwrap();
        let task_id = task.id().clone();
        let criterion_id = goal.completion_criteria()[0].id().clone();
        goal.materialize_initial_plan_with_contract(
            vec![task],
            GoalFinalVerificationSpec::new(
                1,
                vec![GoalCriterionBinding::new(
                    criterion_id,
                    vec![GoalVerificationRequirement::TaskVerified {
                        task_id: task_id.clone(),
                    }],
                )],
            ),
            NOW,
        )
        .unwrap();
        let settled = TaskTransitionContext {
            active_worker_stopped: true,
            side_effect_reconciled: true,
        };
        goal.transition_task(&task_id, TaskStatus::Running, settled, NOW)
            .unwrap();
        goal.transition_task(&task_id, TaskStatus::Verifying, settled, NOW)
            .unwrap();
        let task_verification_id = VerificationId::new();
        let mut task_verification = serde_json::to_value(VerificationResult::new(
            VerificationOutcome::Passed,
            Vec::new(),
            NOW,
            NOW,
        ))
        .unwrap();
        task_verification["id"] = json!(task_verification_id);
        goal.task_record_verification_result(
            &task_id,
            serde_json::from_value(task_verification).unwrap(),
        )
        .unwrap();
        goal.transition_task(&task_id, TaskStatus::Completed, settled, NOW)
            .unwrap();
        goal.enter_verifying_for_test(NOW).unwrap();

        let final_verification_id = VerificationId::new();
        let revision = goal.revision();
        let record = GoalFinalVerificationRecord::new(
            VerificationOutcome::Passed,
            goal.id().clone(),
            revision,
            revision,
            goal.plan_revision(),
            goal.final_verification_spec().unwrap().canonical_digest(),
            Vec::new(),
            NOW,
            NOW,
        );
        let mut value = serde_json::to_value(&goal).unwrap();
        let mut record_value = serde_json::to_value(record).unwrap();
        record_value["id"] = json!(final_verification_id);
        value["final_verifications"] = json!([record_value]);
        let mut goal: Goal = serde_json::from_value(value).unwrap();
        goal.validate().unwrap();

        let writer_paths =
            crate::managed_worktree_writer_paths::derive_writer_mutated_paths(&goal).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let observation = runtime
            .block_on(observe_stable_managed_workspace(
                &goal,
                &session,
                &writer_paths,
            ))
            .unwrap();
        let manifest =
            assemble_workspace_snapshot_manifest(goal.managed_worktree().unwrap(), &observation)
                .unwrap();
        let task_verification_id = goal.tasks()[&task_id]
            .verification_results()
            .last()
            .unwrap()
            .id()
            .clone();
        let evidence = ManagedWorkspaceVerificationEvidenceV1::new(
            goal.id().clone(),
            goal.revision(),
            goal.plan_revision(),
            worktree_id,
            base.clone(),
            git(&worktree, &["rev-parse", "HEAD"]),
            manifest.digest().to_owned(),
            final_verification_id,
            vec![(task_id, task_verification_id)],
            None,
            NOW.to_owned(),
        )
        .unwrap();
        goal.bind_managed_workspace_verification_evidence(evidence)
            .unwrap();
        goal.validate().unwrap();

        let store = TaskStore::with_state_root(state.clone());
        store.create_goal(&goal).unwrap();
        let reloaded = store.load_goal(&session.id, goal.id()).unwrap();
        assert_eq!(
            reloaded.managed_workspace_verification_evidence(),
            goal.managed_workspace_verification_evidence(),
            "durable evidence must reload unchanged"
        );

        Self {
            cleanup_root,
            primary,
            worktree,
            state,
            session,
            store,
            goal: reloaded,
            base,
            primary_head,
            primary_bytes,
        }
    }

    fn assert_primary_unchanged(&self) {
        assert_eq!(
            git(&self.primary, &["rev-parse", "HEAD"]),
            self.primary_head
        );
        assert_eq!(
            std::fs::read(self.primary.join("tracked.txt")).unwrap(),
            self.primary_bytes
        );
        assert_eq!(git(&self.primary, &["status", "--porcelain"]), "");
        assert_eq!(git(&self.worktree, &["rev-parse", "HEAD"]), self.base);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = (&self.state, &self.goal);
        let _ = std::fs::remove_dir_all(&self.cleanup_root);
    }
}

fn assert_external_workspace_change_blocks_finalization(fixture: &Fixture) {
    fixture.assert_primary_unchanged();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime
        .block_on(finalize_goal_at_revision_with_session(
            &fixture.store,
            &fixture.session,
            fixture.goal.id(),
            fixture.goal.revision(),
        ))
        .unwrap();
    assert_eq!(
        result.decision().outcome(),
        GoalFinalizationOutcome::Blocked
    );
    assert!(
        result
            .decision()
            .blockers()
            .iter()
            .any(|blocker| blocker.code() == "MANAGED_WORKSPACE_SNAPSHOT_MISMATCH")
    );
    assert_eq!(result.goal().status(), crate::goal::GoalStatus::Verifying);
    fixture.assert_primary_unchanged();
}

#[test]
fn real_linked_worktree_snapshot_persists_and_finalizer_reobserves() {
    let synchronous = Fixture::new();
    let synchronous_result = finalize_goal_at_revision(
        &synchronous.store,
        &synchronous.session.id,
        synchronous.goal.id(),
        synchronous.goal.revision(),
    )
    .unwrap();
    assert_eq!(
        synchronous_result.decision().outcome(),
        GoalFinalizationOutcome::Blocked
    );
    assert!(
        synchronous_result
            .decision()
            .blockers()
            .iter()
            .any(|blocker| { blocker.code() == "MANAGED_WORKSPACE_REOBSERVATION_REQUIRED" })
    );
    assert_eq!(
        synchronous_result.goal().status(),
        crate::goal::GoalStatus::Verifying
    );
    synchronous.assert_primary_unchanged();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let clean = Fixture::new();
    clean.assert_primary_unchanged();
    let clean_result = runtime
        .block_on(finalize_goal_at_revision_with_session(
            &clean.store,
            &clean.session,
            clean.goal.id(),
            clean.goal.revision(),
        ))
        .unwrap();
    assert_eq!(
        clean_result.decision().outcome(),
        GoalFinalizationOutcome::ReadyToComplete
    );
    assert_eq!(
        clean_result.goal().status(),
        crate::goal::GoalStatus::Completed
    );
    clean.assert_primary_unchanged();

    let mutated_content = Fixture::new();
    std::fs::write(
        mutated_content.worktree.join("tracked.txt"),
        "external edit\n",
    )
    .unwrap();
    assert_external_workspace_change_blocks_finalization(&mutated_content);

    let added_file = Fixture::new();
    std::fs::write(added_file.worktree.join("untracked.txt"), "new file\n").unwrap();
    assert_external_workspace_change_blocks_finalization(&added_file);

    let deleted_file = Fixture::new();
    std::fs::remove_file(deleted_file.worktree.join("tracked.txt")).unwrap();
    assert_external_workspace_change_blocks_finalization(&deleted_file);

    let changed_index = Fixture::new();
    std::fs::write(changed_index.worktree.join("tracked.txt"), "staged edit\n").unwrap();
    git(&changed_index.worktree, &["add", "tracked.txt"]);
    std::fs::write(
        changed_index.worktree.join("tracked.txt"),
        "unstaged edit\n",
    )
    .unwrap();
    assert_external_workspace_change_blocks_finalization(&changed_index);
}

#[test]
fn moved_managed_worktree_cannot_finalize() {
    let fixture = Fixture::new();
    let moved = fixture.cleanup_root.join("managed").join("moved-candidate");
    std::fs::rename(&fixture.worktree, &moved).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime
        .block_on(finalize_goal_at_revision_with_session(
            &fixture.store,
            &fixture.session,
            fixture.goal.id(),
            fixture.goal.revision(),
        ))
        .unwrap();
    assert_ne!(result.goal().status(), crate::goal::GoalStatus::Completed);
    assert!(matches!(
        result.decision().outcome(),
        GoalFinalizationOutcome::Blocked | GoalFinalizationOutcome::NotReady
    ));
    assert_eq!(
        git(&fixture.primary, &["rev-parse", "HEAD"]),
        fixture.primary_head
    );
}
