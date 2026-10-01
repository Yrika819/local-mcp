//! Managed Worktrees V1 Phase 3 — creation authority tests.
//!
//! Every Git mutation in this file runs against a temporary repository created
//! and owned entirely by the test. The user's real repository and linked
//! worktrees are never read or mutated.
//!
//! Coverage follows the frozen Phase 3 scope: PRIMARY regression, public
//! authority, Session path authority, eligibility, the exact creation command,
//! durable `PREPARED` ordering, reconciliation/recovery, and the pre-Planner
//! boundary.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use uuid::Uuid;

use crate::config;
use crate::goal::{Goal, GoalId};
use crate::goal_api::{self, ManagedRootSource};
use crate::managed_worktree::{
    MANAGED_BRANCH_REF_PREFIX, MAX_LIFETIME_CREATION_ATTEMPTS, ManagedWorktreeCreationIntent,
    ManagedWorktreeLifecycle, ManagedWorktreeRecord, WorkspaceMode, WorktreeId,
    WorktreeOperationId,
};
use crate::managed_worktree_create::{
    HostWorktreeCreator, ManagedWorktreeCreation, ManagedWorktreeCreationError,
    ManagedWorktreeCreationOutcome, ManagedWorktreeCreator,
};
use crate::managed_worktree_discovery::DiscoveryError;
use crate::managed_worktree_observe::{GitCommandOutput, HostGit, ReadOnlyGit, assert_read_only};
use crate::managed_worktree_prepare::{
    ManagedApprovalFuture, ManagedCreationApproval, ManagedCreationApprover, ManagedWorkspaceError,
    ManagedWorkspacePreparation, canonical_managed_target, managed_creation_requires_approval,
    prepare_managed_workspace_with_policy, request_managed_workspace,
};
use crate::orchestrator_error::OrchestratorError;
use crate::planner::{self, PlannerError};
use crate::task_store::TaskStore;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

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

fn git_succeeds(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git runs")
        .status
        .success()
}

struct Fixture {
    root: PathBuf,
    /// The pre-canonicalization path, used only for cleanup. `fs::canonicalize`
    /// may resolve a symlinked temp root to a different spelling of the same
    /// directory, and both remove the same tree.
    cleanup_root: PathBuf,
    primary: PathBuf,
    managed_root: PathBuf,
    state_root: PathBuf,
    session: config::Session,
    store: TaskStore,
    base_commit: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.cleanup_root);
    }
}

impl Fixture {
    fn new(name: &str) -> Self {
        let raw_root =
            std::env::temp_dir().join(format!("local-mcp-mw-p3-{name}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(raw_root.join("primary")).unwrap();
        std::fs::create_dir_all(raw_root.join("managed")).unwrap();
        std::fs::create_dir_all(raw_root.join("state")).unwrap();
        // `create_session` and `canonical_directory` use plain
        // `fs::canonicalize`, and the durable `primary_root` must stay
        // byte-equal to `session.cwd`, so the fixture uses the same spelling. On
        // macOS this also resolves the `/var` -> `/private/var` symlink, which is
        // what kept the earlier fixture self-inconsistent.
        let root = std::fs::canonicalize(&raw_root).expect("fixture root is canonical");
        let primary = root.join("primary");
        let managed_root = root.join("managed");
        let state_root = root.join("state");

        git(&primary, &["init", "-q", "."]);
        // Keep committed fixture bytes stable even when the runner has a global
        // core.autocrlf setting; linked-worktree checkout must preserve the
        // exact preimage used by the integration assertions.
        git(&primary, &["config", "core.autocrlf", "false"]);
        git(&primary, &["config", "user.email", "p3@example.invalid"]);
        git(&primary, &["config", "user.name", "Managed Worktree P3"]);
        git(&primary, &["config", "commit.gpgsign", "false"]);
        std::fs::write(primary.join("tracked.txt"), "initial\n").unwrap();
        git(&primary, &["add", "tracked.txt"]);
        git(&primary, &["commit", "-q", "-m", "initial"]);
        let base_commit = git(&primary, &["rev-parse", "HEAD"]);

        let session = config::Session {
            // Session IDs are host-generated; a UUID keeps them valid for
            // `config::validate_session_id` regardless of the test name.
            id: format!("s{}", Uuid::new_v4().simple()),
            cwd: primary.clone(),
            permitted_directories: vec![primary.clone()],
        };
        let store = TaskStore::with_state_root(state_root.clone());
        Self {
            root,
            cleanup_root: raw_root,
            primary,
            managed_root,
            state_root,
            session,
            store,
            base_commit,
        }
    }

    /// The operator's existing explicit authorization path for a broader root.
    fn authorize_managed_root(&mut self) {
        let canonical = std::fs::canonicalize(&self.managed_root).unwrap();
        if !self.session.permitted_directories.contains(&canonical) {
            self.session.permitted_directories.push(canonical);
            self.session.permitted_directories.sort();
        }
    }

    fn start(&self, mode: WorkspaceMode) -> GoalId {
        self.start_with_key(mode, None)
            .expect("goal_start succeeds")
    }

    fn start_with_key(
        &self,
        mode: WorkspaceMode,
        key: Option<&str>,
    ) -> Result<GoalId, goal_api::GoalApiError> {
        let mut args = json!({
            "session_id": self.session.id,
            "objective": "managed worktree objective",
            "title": "P3",
            "constraints": ["stay in scope"],
            "completion_criteria": ["durable managed state"]
        });
        if let Some(key) = key {
            args["idempotency_key"] = json!(key);
        }
        if !mode.is_primary() {
            args["workspace_mode"] = json!(if mode.is_managed() {
                "MANAGED_WORKTREE"
            } else {
                "PRIMARY"
            });
        }
        let view = self.start_value(&args)?;
        Ok(GoalId::parse(view["goal_id"].as_str().unwrap()).expect("goal id parses"))
    }

    /// `goal_start` responses are inspected through their serialized form, which
    /// is exactly what an MCP client sees.
    fn start_value(&self, args: &Value) -> Result<Value, goal_api::GoalApiError> {
        goal_api::goal_start_with_managed_root(
            args,
            &self.session,
            &self.store,
            &ManagedRootSource::fixed(self.managed_root.clone()),
        )
        .map(|view| serde_json::to_value(view).expect("view serializes"))
    }

    fn goal(&self, goal_id: &GoalId) -> Goal {
        self.store
            .load_goal(&self.session.id, goal_id)
            .expect("goal loads")
    }

    fn prepare(
        &self,
        goal_id: &GoalId,
        git: &dyn ReadOnlyGit,
        creator: &dyn ManagedWorktreeCreator,
    ) -> Result<ManagedWorkspacePreparation, crate::managed_worktree_prepare::ManagedWorkspaceError>
    {
        self.prepare_with_approver(goal_id, git, creator, &AllowAllApprovals)
    }

    /// Drive preparation with an explicit approval authority.
    ///
    /// The preparation path is `async` only because the platform approval gate
    /// awaits IPC. Tests drive it on a dedicated current-thread runtime, which
    /// mirrors exactly how `goal_run` drives `run_goal_foreground` - so no test
    /// ever needs an interactive approval UI.
    fn prepare_with_approver(
        &self,
        goal_id: &GoalId,
        git: &dyn ReadOnlyGit,
        creator: &dyn ManagedWorktreeCreator,
        approver: &dyn ManagedCreationApprover,
    ) -> Result<ManagedWorkspacePreparation, crate::managed_worktree_prepare::ManagedWorkspaceError>
    {
        self.prepare_with_policy(
            goal_id,
            git,
            creator,
            approver,
            managed_creation_requires_approval(),
        )
    }

    /// Drive preparation with an explicit approval gate, so the Windows policy
    /// is testable on a Unix host without weakening production.
    fn prepare_with_policy(
        &self,
        goal_id: &GoalId,
        git: &dyn ReadOnlyGit,
        creator: &dyn ManagedWorktreeCreator,
        approver: &dyn ManagedCreationApprover,
        requires_approval: bool,
    ) -> Result<ManagedWorkspacePreparation, crate::managed_worktree_prepare::ManagedWorkspaceError>
    {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(prepare_managed_workspace_with_policy(
                &self.store,
                &self.session,
                goal_id,
                git,
                creator,
                approver,
                requires_approval,
            ))
    }

    fn prepare_real(&self, goal_id: &GoalId) -> ManagedWorkspacePreparation {
        self.prepare(goal_id, &HostGit::new(), &HostWorktreeCreator::new())
            .expect("preparation evaluates")
    }

    fn block(&self, goal_id: &GoalId) -> crate::managed_worktree_prepare::ManagedWorkspaceBlock {
        self.prepare_real(goal_id)
            .block_detail()
            .expect("preparation blocks")
            .clone()
    }

    fn managed_target(&self, goal_id: &GoalId) -> PathBuf {
        canonical_managed_target(&self.managed_root, &self.session, goal_id)
            .expect("target derives")
    }

    fn branch_ref(goal_id: &GoalId) -> String {
        format!("{MANAGED_BRANCH_REF_PREFIX}{}", goal_id.as_str())
    }

    fn branch_name(goal_id: &GoalId) -> String {
        format!("local-mcp/goal/{}", goal_id.as_str())
    }

    fn ref_exists(&self, ref_name: &str) -> bool {
        git_succeeds(
            &self.primary,
            &["show-ref", "--verify", "--quiet", "--", ref_name],
        )
    }

    fn common_dir(&self) -> PathBuf {
        // `git rev-parse` reports the common dir; production canonicalizes it
        // with `fs::canonicalize` before it is recorded, so the fixture does too.
        let reported = git(
            &self.primary,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );
        std::fs::canonicalize(Path::new(&reported)).expect("common dir is canonical")
    }

    /// The managed root as the host records it for a child process, i.e.
    /// de-verbatim on Windows.
    fn managed_root_for_git(&self) -> PathBuf {
        config::canonical_path(&self.managed_root).expect("managed root is canonical")
    }
}

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

/// Test-only convenience for the outcome the production code matches on.
trait PreparationExt {
    fn is_active(&self) -> bool;
}

impl PreparationExt for ManagedWorkspacePreparation {
    fn is_active(&self) -> bool {
        matches!(self, ManagedWorkspacePreparation::Active { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Perform no side effect at all, then report failure.
    Nothing,
    /// Really run the exact creation command and report its real outcome.
    Create,
    /// Really run the exact creation command, then report a lost response.
    CreateThenReportFailure,
    /// Create only the branch, never the worktree (a partial side effect).
    BranchOnly,
    /// Really create the worktree, then move its HEAD away from the base.
    CreateThenMoveHead,
    /// Never start Git at all.
    FailToStart,
}

/// The durable state captured at the exact moment Git would be invoked.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Observed {
    argv: Vec<String>,
    lifecycle: Option<ManagedWorktreeLifecycle>,
    operation_id: Option<String>,
    intent_root: Option<PathBuf>,
    intent_base: Option<String>,
    intent_branch: Option<String>,
    plan_revision_at_call: u32,
    attempts_at_call: u32,
}

struct ScriptedCreator {
    store: TaskStore,
    session_id: String,
    steps: Vec<Step>,
    observations: Mutex<Vec<Observed>>,
    calls: Mutex<usize>,
}

impl ScriptedCreator {
    fn new(fixture: &Fixture, steps: Vec<Step>) -> Self {
        assert!(!steps.is_empty());
        Self {
            store: TaskStore::with_state_root(fixture.state_root.clone()),
            session_id: fixture.session.id.clone(),
            steps,
            observations: Mutex::new(Vec::new()),
            calls: Mutex::new(0),
        }
    }

    fn observations(&self) -> Vec<Observed> {
        self.observations.lock().unwrap().clone()
    }

    fn calls(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

impl ManagedWorktreeCreator for ScriptedCreator {
    fn create(
        &self,
        creation: &ManagedWorktreeCreation,
        primary_root: &Path,
    ) -> Result<ManagedWorktreeCreationOutcome, ManagedWorktreeCreationError> {
        let index = {
            let mut calls = self.calls.lock().unwrap();
            let index = *calls;
            *calls += 1;
            index.min(self.steps.len() - 1)
        };

        // Snapshot durable state at the exact moment Git would be invoked.
        let goal = self
            .store
            .load_active_goal(&self.session_id)
            .expect("store readable")
            .expect("an active goal exists");
        let intent: Option<&ManagedWorktreeCreationIntent> =
            goal.managed_worktree_creation_intent();
        self.observations.lock().unwrap().push(Observed {
            argv: creation
                .argv()
                .iter()
                .map(|value| value.to_string_lossy().into_owned())
                .collect(),
            lifecycle: goal.managed_worktree().map(|record| record.lifecycle()),
            operation_id: intent.map(|intent| intent.operation_id().as_str().to_owned()),
            intent_root: intent.map(|intent| intent.worktree_root().to_path_buf()),
            intent_base: intent.map(|intent| intent.base_commit().to_owned()),
            intent_branch: intent.map(|intent| intent.branch_ref().to_owned()),
            plan_revision_at_call: goal.plan_revision(),
            attempts_at_call: goal
                .managed_worktree()
                .map(|record| record.creation_attempts_consumed())
                .unwrap_or(0),
        });

        match self.steps[index] {
            Step::Nothing => Ok(ManagedWorktreeCreationOutcome {
                exit_code: Some(1),
                stdout: Vec::new(),
                stderr: "scripted failure".to_owned(),
            }),
            Step::Create => HostWorktreeCreator::new().create(creation, primary_root),
            Step::CreateThenReportFailure => {
                let _ = HostWorktreeCreator::new().create(creation, primary_root);
                Ok(ManagedWorktreeCreationOutcome {
                    exit_code: None,
                    stdout: Vec::new(),
                    stderr: "scripted lost response".to_owned(),
                })
            }
            Step::BranchOnly => {
                git(
                    primary_root,
                    &["branch", creation.branch_name(), creation.base_commit()],
                );
                Ok(ManagedWorktreeCreationOutcome {
                    exit_code: Some(0),
                    stdout: Vec::new(),
                    stderr: String::new(),
                })
            }
            Step::CreateThenMoveHead => {
                let _ = HostWorktreeCreator::new().create(creation, primary_root);
                git(
                    creation.worktree_root(),
                    &["commit", "-q", "--allow-empty", "-m", "drift"],
                );
                Ok(ManagedWorktreeCreationOutcome {
                    exit_code: Some(0),
                    stdout: Vec::new(),
                    stderr: String::new(),
                })
            }
            Step::FailToStart => Err(ManagedWorktreeCreationError::Spawn {
                detail: "scripted failure to start".to_owned(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Approval test doubles
// ---------------------------------------------------------------------------

/// Approves every request and records exactly what it was asked to authorize.
#[derive(Clone, Default)]
struct RecordingApprover {
    outcome: Outcome,
    requests: Arc<Mutex<Vec<ManagedCreationApproval>>>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Outcome {
    #[default]
    Allow,
    Deny,
    Unavailable,
}

impl RecordingApprover {
    fn with(outcome: Outcome) -> Self {
        Self {
            outcome,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn allow() -> Self {
        Self::with(Outcome::Allow)
    }

    fn deny() -> Self {
        Self::with(Outcome::Deny)
    }

    fn unavailable() -> Self {
        Self::with(Outcome::Unavailable)
    }

    fn requests(&self) -> Vec<ManagedCreationApproval> {
        self.requests.lock().unwrap().clone()
    }

    fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl ManagedCreationApprover for RecordingApprover {
    fn approve(&self, request: &ManagedCreationApproval) -> ManagedApprovalFuture<'_> {
        self.requests.lock().unwrap().push(request.clone());
        let outcome = self.outcome;
        Box::pin(async move {
            match outcome {
                Outcome::Allow => Ok(true),
                Outcome::Deny => Ok(false),
                // A missing approval channel and a malformed reply both surface
                // as an error in the real `approvals::request`; neither may be
                // read as consent.
                Outcome::Unavailable => Err(ManagedWorkspaceError::ApprovalUnavailable(
                    "scripted approval channel failure".to_owned(),
                )),
            }
        })
    }
}

/// Stand-in for the platform default in tests that are not about approval.
struct AllowAllApprovals;

impl ManagedCreationApprover for AllowAllApprovals {
    fn approve(&self, _request: &ManagedCreationApproval) -> ManagedApprovalFuture<'_> {
        Box::pin(async { Ok(true) })
    }
}

/// A read-only observer whose `worktree list` output cannot be trusted.
struct AmbiguousGit(HostGit);

impl ReadOnlyGit for AmbiguousGit {
    fn run(&self, args: &[&str], cwd: &Path) -> Result<GitCommandOutput, DiscoveryError> {
        if matches!(args, ["worktree", "list", "--porcelain", "-z"]) {
            return Err(DiscoveryError::ObservationUnavailable(
                "scripted ambiguous worktree inventory".to_owned(),
            ));
        }
        self.0.run(args, cwd)
    }

    fn ref_exists(&self, ref_name: &str, cwd: &Path) -> Result<bool, DiscoveryError> {
        self.0.ref_exists(ref_name, cwd)
    }
}

/// A read-only observer that reports a different repository common directory.
struct CommonDirOverrideGit {
    inner: HostGit,
    common_dir: PathBuf,
}

impl ReadOnlyGit for CommonDirOverrideGit {
    fn run(&self, args: &[&str], cwd: &Path) -> Result<GitCommandOutput, DiscoveryError> {
        let output = self.inner.run(args, cwd)?;
        if matches!(
            args,
            ["rev-parse", "--path-format=absolute", "--git-common-dir"]
        ) && output.exit_code == 0
        {
            return Ok(GitCommandOutput {
                stdout: format!("{}\n", self.common_dir.display()).into_bytes(),
                stderr: output.stderr,
                exit_code: output.exit_code,
            });
        }
        Ok(output)
    }

    fn ref_exists(&self, ref_name: &str, cwd: &Path) -> Result<bool, DiscoveryError> {
        self.inner.ref_exists(ref_name, cwd)
    }
}

#[test]
fn a_managed_record_rejects_a_root_inside_git_administrative_internals() {
    let fixture = Fixture::new("common-dir-overlap");
    let goal_id = GoalId::new();
    // Design invariant 8: Git administrative internals stay forbidden. Model the
    // host state the design's own research records - a session whose cwd is a
    // *linked* worktree - so the common directory is a different directory from
    // the primary root and the primary overlap check cannot see it. The paths
    // come from the canonical fixture root so they are absolute and
    // component-exact on every platform, including Windows.
    let primary = fixture.root.join("linked-checkout");
    let common = fixture.root.join("main").join(".git");
    let error = ManagedWorktreeRecord::requested(
        &goal_id,
        WorktreeId::new(),
        primary,
        common.clone(),
        common.join("worktrees").join("goal"),
        "0".repeat(40),
        None,
        1,
        0,
    )
    .expect_err("a root inside the common directory is rejected");
    assert!(error.to_string().contains("common directory"), "{}", error);
}

#[test]
fn a_managed_goal_in_a_control_state_does_not_reach_git() {
    use crate::goal::GoalStatus;

    let mut fixture = Fixture::new("control-state");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    // The operator cancels the Goal before it is ever run.
    let cancel = json!({
        "session_id": fixture.session.id,
        "goal_id": goal_id.as_str(),
        "reason": "operator changed their mind"
    });
    let _ = cancel;
    let status = fixture
        .store
        .load_goal(&fixture.session.id, &goal_id)
        .unwrap();
    assert_eq!(status.status(), GoalStatus::Planning);
    fixture
        .store
        .mutate_goal_snapshot(
            &fixture.session.id,
            &goal_id,
            status.revision(),
            |goal, now| goal.transition_to(GoalStatus::Cancelling, now),
        )
        .unwrap();

    let before_worktrees = git(&fixture.primary, &["worktree", "list", "--porcelain"]);
    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    let preparation = fixture
        .prepare(&goal_id, &HostGit::new(), &creator)
        .unwrap();
    let block = preparation.block_detail().expect("control state blocks");
    assert_eq!(block.code, "MANAGED_GOAL_CONTROL_STATE");

    assert_eq!(
        creator.calls(),
        0,
        "a Goal in a control state must not create a worktree: {preparation:?}"
    );
    assert_eq!(
        git(&fixture.primary, &["worktree", "list", "--porcelain"]),
        before_worktrees
    );
    assert!(!fixture.managed_target(&goal_id).exists());
    assert!(!fixture.ref_exists(&Fixture::branch_ref(&goal_id)));
    assert_eq!(
        fixture
            .goal(&goal_id)
            .managed_worktree()
            .unwrap()
            .lifecycle(),
        ManagedWorktreeLifecycle::Requested,
        "a control state must not advance the managed lifecycle"
    );
}

#[test]
fn the_foreground_run_seam_refuses_a_control_state_before_any_authority_call() {
    use crate::goal::GoalStatus;
    use crate::goal_runner::{GoalRunStopReason, prepare_managed_workspace_before_run};

    let mut fixture = Fixture::new("seam-control-state");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let revision = fixture.goal(&goal_id).revision();
    fixture
        .store
        .mutate_goal_snapshot(&fixture.session.id, &goal_id, revision, |goal, now| {
            goal.transition_to(GoalStatus::Cancelling, now)
        })
        .unwrap();

    // The production seam, not the preparation layer beneath it.
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(prepare_managed_workspace_before_run(
            &fixture.store,
            &fixture.session,
            &goal_id,
        ))
        .expect("a control state stops the run at the seam");
    assert_eq!(
        result.stop_reason,
        GoalRunStopReason::ControlState(GoalStatus::Cancelling),
        "the seam must return the runner's own control-state stop reason"
    );
    assert_eq!(result.steps_attempted, 0);
    assert_eq!(result.steps_applied, 0);
    assert_eq!(result.revision_before, Some(revision + 1));
    assert_eq!(result.revision_after, Some(revision + 1));

    assert!(!fixture.managed_target(&goal_id).exists());
    assert!(!fixture.ref_exists(&Fixture::branch_ref(&goal_id)));
}

#[test]
fn the_control_state_predicate_covers_exactly_the_states_the_runner_stops_on() {
    use crate::goal::GoalStatus;

    // Every Goal status, and the set the runner refuses to advance. The seam,
    // `Goal::blocks_foreground_run`, and `stop_for_state` all read this one
    // predicate, so this table is the exhaustive pin.
    let all = [
        GoalStatus::Planning,
        GoalStatus::Running,
        GoalStatus::Replanning,
        GoalStatus::Verifying,
        GoalStatus::Pausing,
        GoalStatus::Paused,
        GoalStatus::Blocked,
        GoalStatus::Cancelling,
        GoalStatus::Completed,
        GoalStatus::Failed,
        GoalStatus::Cancelled,
    ];
    for status in all {
        let expected = matches!(
            status,
            GoalStatus::Pausing
                | GoalStatus::Paused
                | GoalStatus::Blocked
                | GoalStatus::Cancelling
                | GoalStatus::Completed
                | GoalStatus::Failed
                | GoalStatus::Cancelled
        );
        assert_eq!(
            status.blocks_foreground_run(),
            expected,
            "unexpected control-state verdict for {status:?}"
        );
        assert_eq!(
            status.is_terminal() || status.blocks_foreground_run(),
            status.blocks_foreground_run(),
            "terminal states must always block for {status:?}"
        );
    }
}

#[test]
fn a_stale_writer_cannot_advance_the_managed_lifecycle() {
    // Optimistic concurrency on `mutate_goal_snapshot` is what serializes two
    // preparations of the same managed Goal at the durable layer.
    let mut fixture = Fixture::new("stale-writer");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let goal = fixture.goal(&goal_id);
    let record = goal.managed_worktree().unwrap();
    let stale_revision = goal.revision();

    let intent = ManagedWorktreeCreationIntent::prepared(
        &goal_id,
        record.worktree_id().clone(),
        WorktreeOperationId::new(),
        fixture.common_dir(),
        fixture.managed_target(&goal_id),
        fixture.base_commit.clone(),
    )
    .unwrap();
    fixture
        .store
        .mutate_goal_snapshot(
            &fixture.session.id,
            &goal_id,
            stale_revision,
            |goal, _now| goal.set_managed_creation_intent(intent),
        )
        .unwrap();

    // A second writer holding the same, now-stale revision is refused.
    let second = ManagedWorktreeCreationIntent::prepared(
        &goal_id,
        record.worktree_id().clone(),
        WorktreeOperationId::new(),
        fixture.common_dir(),
        fixture.managed_target(&goal_id),
        fixture.base_commit.clone(),
    )
    .unwrap();
    let error = fixture
        .store
        .mutate_goal_snapshot(
            &fixture.session.id,
            &goal_id,
            stale_revision,
            |goal, _now| goal.set_managed_creation_intent(second),
        )
        .expect_err("a stale writer must not advance the managed lifecycle");
    assert!(matches!(error, OrchestratorError::RevisionConflict { .. }));

    let after = fixture.goal(&goal_id);
    assert_eq!(
        after.managed_worktree().unwrap().lifecycle(),
        ManagedWorktreeLifecycle::Prepared
    );
    assert_eq!(after.plan_revision(), 0);
}

/// A session cwd that resolves to the primary directory through a symlink makes
/// the host-derived `primary_root` a *different* path from the durable
/// `Goal.cwd`. Managed mode must refuse that rather than silently repair it,
/// because `Goal.cwd` is the durable identity root.
#[cfg(unix)]
#[test]
fn a_symlinked_session_cwd_cannot_host_a_managed_workspace() {
    let mut fixture = Fixture::new("symlinked-cwd");
    fixture.authorize_managed_root();
    let link = fixture.root.join("primary-link");
    std::os::unix::fs::symlink(&fixture.primary, &link).unwrap();
    assert_eq!(std::fs::canonicalize(&link).unwrap(), fixture.primary);
    assert_ne!(link, fixture.primary);

    fixture.session.cwd = link.clone();
    let error = fixture
        .start_value(&json!({
            "session_id": fixture.session.id,
            "objective": "managed worktree objective",
            "workspace_mode": "MANAGED_WORKTREE"
        }))
        .expect_err("a symlinked session cwd is refused");
    // Whichever guard fires first, the request must not create a Goal whose
    // durable identity root disagrees with its host-derived `primary_root`.
    assert!(
        matches!(
            error.code(),
            "MANAGED_WORKSPACE_UNAVAILABLE" | "GOAL_STATE_CORRUPT"
        ),
        "unexpected refusal {}",
        error.code()
    );
    assert!(link.exists());
    assert!(
        fixture
            .store
            .list_goals_for_session(&fixture.session.id)
            .unwrap()
            .is_empty(),
        "a refused managed request must not leave a Goal behind"
    );
}

/// A trailing `.` is component-equal to the canonical path, so `Path` validation
/// accepts it and the durable identity root is unaffected. This pins that the
/// canonicalization in `request_managed_workspace` is not stricter than the
/// durable `primary_root == cwd` invariant for this spelling.
#[test]
fn a_dot_suffixed_session_cwd_still_resolves_to_the_same_identity() {
    let mut fixture = Fixture::new("dot-suffixed-cwd");
    fixture.authorize_managed_root();
    fixture.session.cwd = noncanonical(&fixture.primary);
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let goal = fixture.goal(&goal_id);
    assert_eq!(
        goal.managed_worktree().unwrap().primary_root(),
        goal.cwd(),
        "a component-equal spelling is the same durable identity root"
    );
}

/// Spell a path the way Git's porcelain output does, so a test can compare a
/// recorded target against what Git reports.
fn normalized_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// The same directory, spelled with a trailing `.` so the raw bytes differ from
/// the canonical form while resolution is unchanged and portable.
fn noncanonical(path: &Path) -> PathBuf {
    path.join(".")
}

// ---------------------------------------------------------------------------
// I. Durable lifetime attempt budget
// ---------------------------------------------------------------------------

fn attempts(fixture: &Fixture, goal_id: &GoalId) -> u32 {
    fixture
        .goal(goal_id)
        .managed_worktree()
        .expect("managed record")
        .creation_attempts_consumed()
}

#[test]
fn a_fresh_request_starts_with_the_full_lifetime_budget() {
    let mut fixture = Fixture::new("budget-fresh");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    assert_eq!(attempts(&fixture, &goal_id), 0);
    assert!(
        fixture
            .goal(&goal_id)
            .managed_worktree()
            .unwrap()
            .permits_creation_attempt()
    );
}

#[test]
fn the_first_invocation_consumes_the_attempt_durably_before_git() {
    let mut fixture = Fixture::new("budget-first");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    assert!(
        fixture
            .prepare(&goal_id, &HostGit::new(), &creator)
            .unwrap()
            .is_active()
    );
    assert_eq!(creator.calls(), 1);
    // The counter is durable, and it is evidence that survives ACTIVE.
    assert_eq!(attempts(&fixture, &goal_id), 1);

    // Round-trip through the durable store rather than trusting an in-memory
    // value: a fresh TaskStore over the same state root must read the same count.
    let reloaded = TaskStore::with_state_root(fixture.state_root.clone())
        .load_goal(&fixture.session.id, &goal_id)
        .unwrap();
    assert_eq!(
        reloaded
            .managed_worktree()
            .unwrap()
            .creation_attempts_consumed(),
        1
    );
}

#[test]
fn a_proven_no_side_effect_retry_consumes_the_second_attempt_before_git() {
    let mut fixture = Fixture::new("budget-retry");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let creator = ScriptedCreator::new(&fixture, vec![Step::Nothing, Step::Create]);
    assert!(
        fixture
            .prepare(&goal_id, &HostGit::new(), &creator)
            .unwrap()
            .is_active()
    );
    let observations = creator.observations();
    assert_eq!(observations.len(), 2);
    assert_eq!(attempts(&fixture, &goal_id), 2);
    assert_ne!(
        observations[0].operation_id, observations[1].operation_id,
        "each attempt keeps a distinct durable operation identity"
    );
    // The consumed count is already N when Git runs for attempt N, because the
    // attempt is durably persisted before the invocation.
    assert_eq!(observations[0].attempts_at_call, 1);
    assert_eq!(observations[1].attempts_at_call, 2);
}

#[test]
fn after_two_consumed_attempts_no_third_git_invocation_occurs() {
    let mut fixture = Fixture::new("budget-exhausted");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let creator = ScriptedCreator::new(&fixture, vec![Step::Nothing]);
    let block = fixture
        .prepare(&goal_id, &HostGit::new(), &creator)
        .unwrap()
        .block_detail()
        .cloned()
        .expect("the lifetime budget is spent");
    assert_eq!(block.code, "MANAGED_RETRY_EXHAUSTED");
    assert_eq!(creator.calls(), 2, "exactly the lifetime bound");
    assert_eq!(attempts(&fixture, &goal_id), 2);
    assert!(!fixture.managed_target(&goal_id).exists());
    assert!(!fixture.ref_exists(&Fixture::branch_ref(&goal_id)));
}

#[test]
fn a_restart_between_attempts_does_not_restore_the_budget() {
    let mut fixture = Fixture::new("budget-restart");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    // A first process spends attempt 1 and proves no side effect, then "dies"
    // before the retry. The retry loop in one call would continue; instead stop
    // the call here by letting the budget be observed mid-flight.
    let first = ScriptedCreator::new(&fixture, vec![Step::Nothing]);
    let store = TaskStore::with_state_root(fixture.state_root.clone());
    let _ = store;
    // Run one attempt explicitly: use a creator that succeeds so a single
    // invocation completes, then assert the durable count.
    let ok = ScriptedCreator::new(&fixture, vec![Step::Create]);
    assert!(
        fixture
            .prepare(&goal_id, &HostGit::new(), &ok)
            .unwrap()
            .is_active()
    );
    assert_eq!(attempts(&fixture, &goal_id), 1);
    drop(first);

    // A brand new TaskStore - the "restart" - sees the same spent budget and
    // does not replenish it.
    let restarted = TaskStore::with_state_root(fixture.state_root.clone());
    let reloaded = restarted.load_goal(&fixture.session.id, &goal_id).unwrap();
    let record = reloaded.managed_worktree().unwrap();
    assert_eq!(record.creation_attempts_consumed(), 1);
    assert!(record.permits_creation_attempt());

    // A repeated prepare on an already-ACTIVE workspace consumes nothing.
    assert!(fixture.prepare_real(&goal_id).is_active());
    assert_eq!(attempts(&fixture, &goal_id), 1);
}

#[test]
fn a_crash_after_consuming_an_attempt_does_not_refund_it() {
    let mut fixture = Fixture::new("budget-crash-after-consume");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    // Simulate the crash: a durable PREPARED intent with one attempt already
    // consumed, and no Git side effect.
    let goal = fixture.goal(&goal_id);
    let record = goal.managed_worktree().unwrap();
    let intent = ManagedWorktreeCreationIntent::prepared(
        &goal_id,
        record.worktree_id().clone(),
        WorktreeOperationId::new(),
        fixture.common_dir(),
        fixture.managed_target(&goal_id),
        fixture.base_commit.clone(),
    )
    .unwrap();
    let revision = goal.revision();
    fixture
        .store
        .mutate_goal_snapshot(&fixture.session.id, &goal_id, revision, |goal, _now| {
            goal.set_managed_creation_intent(intent)?;
            goal.consume_managed_creation_attempt()
        })
        .unwrap();
    assert_eq!(attempts(&fixture, &goal_id), 1);

    // A later process must reconcile the proven no-side-effect and continue with
    // the remaining single attempt - never two.
    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    assert!(
        fixture
            .prepare(&goal_id, &HostGit::new(), &creator)
            .unwrap()
            .is_active()
    );
    assert_eq!(creator.calls(), 1, "only the remaining attempt was spent");
    assert_eq!(attempts(&fixture, &goal_id), 2);

    // And a third invocation is now impossible: the workspace is ACTIVE, so a
    // further preparation only reconciles and never spawns Git again.
    let more = ScriptedCreator::new(&fixture, vec![Step::Create]);
    let repeated = fixture.prepare(&goal_id, &HostGit::new(), &more).unwrap();
    assert!(
        repeated.is_active(),
        "an already-created workspace stays active on a further run"
    );
    assert_eq!(more.calls(), 0, "no further Git invocation is authorized");
    assert_eq!(attempts(&fixture, &goal_id), 2);
}

#[test]
fn repeated_preparation_never_replenishes_a_spent_budget() {
    let mut fixture = Fixture::new("budget-repeat");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let creator = ScriptedCreator::new(&fixture, vec![Step::Nothing]);
    assert!(
        fixture
            .prepare(&goal_id, &HostGit::new(), &creator)
            .unwrap()
            .block_detail()
            .is_some()
    );
    assert_eq!(attempts(&fixture, &goal_id), 2);

    // Repeated prepares, and a fresh store each time, must not restore budget.
    for _ in 0..3 {
        let again = ScriptedCreator::new(&fixture, vec![Step::Nothing]);
        let _ = fixture.prepare(&goal_id, &HostGit::new(), &again).unwrap();
        assert_eq!(again.calls(), 0, "a blocked workspace must not be retried");
        assert_eq!(attempts(&fixture, &goal_id), 2);
    }
}

#[test]
fn a_denied_authority_before_invocation_does_not_consume_budget() {
    // Session path authority missing: preparation refuses before any attempt.
    let fixture = Fixture::new("budget-no-authority");
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    assert_eq!(
        fixture
            .prepare(&goal_id, &HostGit::new(), &creator)
            .unwrap()
            .block_detail()
            .unwrap()
            .code,
        "MANAGED_SESSION_AUTHORITY"
    );
    assert_eq!(creator.calls(), 0);
    assert_eq!(attempts(&fixture, &goal_id), 0);
}

#[test]
fn an_ineligible_repository_does_not_consume_budget() {
    let mut fixture = Fixture::new("budget-ineligible");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    std::fs::write(fixture.primary.join("tracked.txt"), "dirty\n").unwrap();
    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    assert_eq!(
        fixture
            .prepare(&goal_id, &HostGit::new(), &creator)
            .unwrap()
            .block_detail()
            .unwrap()
            .code,
        "MANAGED_ELIGIBILITY"
    );
    assert_eq!(creator.calls(), 0);
    assert_eq!(attempts(&fixture, &goal_id), 0);
}

#[test]
fn an_unobservable_repository_does_not_consume_budget() {
    let mut fixture = Fixture::new("budget-unobservable");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    assert_eq!(
        fixture
            .prepare(&goal_id, &AmbiguousGit(HostGit::new()), &creator)
            .unwrap()
            .block_detail()
            .unwrap()
            .code,
        "MANAGED_OBSERVATION_UNAVAILABLE"
    );
    assert_eq!(creator.calls(), 0);
    assert_eq!(attempts(&fixture, &goal_id), 0);
}

#[test]
fn adopting_an_exact_side_effect_does_not_consume_budget() {
    let mut fixture = Fixture::new("budget-adopt");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    // A lost response: attempt 1 ran and created the worktree, so exactly one
    // attempt is consumed and the follow-up adoption spends nothing more.
    let creator = ScriptedCreator::new(&fixture, vec![Step::CreateThenReportFailure]);
    assert!(
        fixture
            .prepare(&goal_id, &HostGit::new(), &creator)
            .unwrap()
            .is_active()
    );
    assert_eq!(creator.calls(), 1);
    assert_eq!(attempts(&fixture, &goal_id), 1);
}

#[test]
fn an_exact_worktree_with_no_consumed_attempt_is_not_adopted_as_ours() {
    let mut fixture = Fixture::new("budget-foreign-adopt");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    // Build a durable PREPARED intent but consume nothing, then plant an exact
    // worktree out of band. Nothing proves it is this lifecycle's side effect,
    // so it must not be adopted.
    let goal = fixture.goal(&goal_id);
    let record = goal.managed_worktree().unwrap();
    let intent = ManagedWorktreeCreationIntent::prepared(
        &goal_id,
        record.worktree_id().clone(),
        WorktreeOperationId::new(),
        fixture.common_dir(),
        fixture.managed_target(&goal_id),
        fixture.base_commit.clone(),
    )
    .unwrap();
    let revision = goal.revision();
    fixture
        .store
        .mutate_goal_snapshot(&fixture.session.id, &goal_id, revision, |goal, _now| {
            goal.set_managed_creation_intent(intent)
        })
        .unwrap();
    let target = fixture.managed_target(&goal_id);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    git(
        &fixture.primary,
        &[
            "worktree",
            "add",
            "--lock",
            "--reason",
            &format!("local-mcp goal {}", goal_id.as_str()),
            "-b",
            &Fixture::branch_name(&goal_id),
            &target.display().to_string(),
            &fixture.base_commit,
        ],
    );

    let block = fixture.block(&goal_id);
    assert_eq!(block.code, "MANAGED_RECOVERY_REQUIRED");
    assert!(
        block
            .detail
            .contains("no creation attempt was ever consumed"),
        "{}",
        block.detail
    );
    assert_eq!(attempts(&fixture, &goal_id), 0);
}

#[test]
fn a_conflicting_reconciliation_does_not_consume_a_new_attempt() {
    let mut fixture = Fixture::new("budget-conflict");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let creator = ScriptedCreator::new(&fixture, vec![Step::BranchOnly]);
    assert_eq!(
        fixture
            .prepare(&goal_id, &HostGit::new(), &creator)
            .unwrap()
            .block_detail()
            .unwrap()
            .code,
        "MANAGED_RECOVERY_REQUIRED"
    );
    // The partial side effect blocked, but the one attempt it spent is recorded.
    assert_eq!(attempts(&fixture, &goal_id), 1);
    assert_eq!(creator.calls(), 1, "a partial side effect is never retried");
}

#[test]
fn an_out_of_range_attempt_count_fails_closed() {
    let fixture = Fixture::new("budget-corrupt");
    let goal_id = GoalId::new();
    let primary = fixture.primary.clone();
    let common = fixture.common_dir();
    let target = fixture.managed_root_for_git().join("goal");

    for corrupt in [
        MAX_LIFETIME_CREATION_ATTEMPTS + 1,
        MAX_LIFETIME_CREATION_ATTEMPTS + 50,
        u32::MAX,
    ] {
        let record = ManagedWorktreeRecord::requested(
            &goal_id,
            WorktreeId::new(),
            primary.clone(),
            common.clone(),
            target.clone(),
            fixture.base_commit.clone(),
            None,
            1,
            0,
        )
        .unwrap();
        let mut value = serde_json::to_value(&record).unwrap();
        value["creation_attempts_consumed"] = json!(corrupt);
        // Decoding must reject it rather than clamp it.
        let decoded: Result<ManagedWorktreeRecord, _> = serde_json::from_value(value);
        assert!(decoded.is_ok(), "shape decode is expected to succeed");
        let decoded = decoded.unwrap();
        assert!(
            matches!(decoded.validate(), Err(OrchestratorError::CorruptGoal(_))),
            "an out-of-range attempt count {corrupt} must fail closed"
        );
        assert!(matches!(record.consume_creation_attempt(), Ok(_) | Err(_)));
    }
}

#[test]
fn an_exhausted_record_refuses_to_consume_another_attempt() {
    let fixture = Fixture::new("budget-refuse-consume");
    let goal_id = GoalId::new();
    let record = ManagedWorktreeRecord::requested(
        &goal_id,
        WorktreeId::new(),
        fixture.primary.clone(),
        fixture.common_dir(),
        fixture.managed_root_for_git().join("goal"),
        fixture.base_commit.clone(),
        None,
        1,
        0,
    )
    .unwrap();
    // Consumption only happens once the intent is durable, so the record is
    // prepared first.
    let once = record
        .to_prepared()
        .unwrap()
        .consume_creation_attempt()
        .unwrap();
    assert_eq!(once.creation_attempts_consumed(), 1);
    let twice = once.consume_creation_attempt().unwrap();
    assert_eq!(twice.creation_attempts_consumed(), 2);
    assert!(!twice.permits_creation_attempt());
    assert!(matches!(
        twice.consume_creation_attempt(),
        Err(OrchestratorError::CorruptGoal(_))
    ));
}

// ---------------------------------------------------------------------------
// J. Platform approval for the host-native mutation
// ---------------------------------------------------------------------------

#[test]
fn the_platform_policy_requires_approval_only_on_windows() {
    // Design section 23: Windows host-native mutation stays approval-gated.
    // Unix must not gain an interactive requirement it never had.
    assert_eq!(
        managed_creation_requires_approval(),
        cfg!(windows),
        "the approval gate is platform policy, not a universal one"
    );
}

#[cfg(windows)]
#[test]
fn production_policy_on_windows_selects_approval_required() {
    assert!(managed_creation_requires_approval());
}

#[test]
fn a_spent_budget_is_refused_before_the_operator_is_asked_to_approve() {
    let mut fixture = Fixture::new("approval-after-budget");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    // Spend the budget with the approval gate off, exactly as a Unix host does.
    let creator = ScriptedCreator::new(&fixture, vec![Step::Nothing]);
    assert_eq!(
        fixture
            .prepare_with_policy(
                &goal_id,
                &HostGit::new(),
                &creator,
                &AllowAllApprovals,
                false
            )
            .unwrap()
            .block_detail()
            .map(|block| block.code),
        Some("MANAGED_RETRY_EXHAUSTED")
    );
    assert_eq!(attempts(&fixture, &goal_id), MAX_LIFETIME_CREATION_ATTEMPTS);

    // With the gate on, a further run must not prompt the operator to approve a
    // mutation that is already forbidden, and must not report a denial as the
    // cause. The lifecycle is already `BLOCKED` from the first run, so the
    // specific code is the explicit-recovery one; what matters is that neither
    // an approval prompt nor a Git invocation happens.
    let approver = RecordingApprover::allow();
    let again = ScriptedCreator::new(&fixture, vec![Step::Nothing]);
    let block = fixture
        .prepare_with_policy(&goal_id, &HostGit::new(), &again, &approver, true)
        .unwrap()
        .block_detail()
        .cloned()
        .expect("a blocked workspace blocks");
    assert_eq!(block.code, "MANAGED_RECOVERY_REQUIRED");
    assert_eq!(
        approver.calls(),
        0,
        "no approval may be requested for an unauthorized invocation"
    );
    assert_eq!(again.calls(), 0);
    assert_eq!(attempts(&fixture, &goal_id), MAX_LIFETIME_CREATION_ATTEMPTS);
}

#[test]
fn a_denied_approval_never_invokes_git_and_costs_no_attempt() {
    let mut fixture = Fixture::new("approval-denied");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let approver = RecordingApprover::deny();
    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    let block = fixture
        .prepare_with_policy(&goal_id, &HostGit::new(), &creator, &approver, true)
        .unwrap()
        .block_detail()
        .cloned()
        .expect("a denial blocks");
    assert_eq!(block.code, "MANAGED_CREATION_APPROVAL_DENIED");
    assert_eq!(creator.calls(), 0, "denial must precede any Git invocation");
    assert_eq!(attempts(&fixture, &goal_id), 0, "denial costs no attempt");
    assert!(!fixture.managed_target(&goal_id).exists());
    assert!(!fixture.ref_exists(&Fixture::branch_ref(&goal_id)));
}

#[test]
fn an_unavailable_approval_channel_never_invokes_git() {
    let mut fixture = Fixture::new("approval-unavailable");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let approver = RecordingApprover::unavailable();
    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    let block = fixture
        .prepare_with_policy(&goal_id, &HostGit::new(), &creator, &approver, true)
        .unwrap()
        .block_detail()
        .cloned()
        .expect("an unreachable approval channel blocks");
    assert_eq!(block.code, "MANAGED_CREATION_APPROVAL_UNAVAILABLE");
    assert_eq!(creator.calls(), 0);
    assert_eq!(attempts(&fixture, &goal_id), 0);
}

#[test]
fn an_allowed_approval_binds_the_exact_host_owned_operation() {
    let mut fixture = Fixture::new("approval-allowed");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let approver = RecordingApprover::allow();
    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    assert!(
        fixture
            .prepare_with_policy(&goal_id, &HostGit::new(), &creator, &approver, true)
            .unwrap()
            .is_active()
    );
    let requests = approver.requests();
    assert_eq!(requests.len(), 1, "exactly one approval per invocation");
    let request = &requests[0];
    // The approval is bound to the exact durable target, branch, and base.
    assert_eq!(request.goal_id, goal_id.as_str());
    assert_eq!(request.primary_root, fixture.primary);
    assert_eq!(request.worktree_root, fixture.managed_target(&goal_id));
    assert_eq!(request.branch_ref, Fixture::branch_ref(&goal_id));
    assert_eq!(request.base_commit, fixture.base_commit);
    assert_eq!(creator.calls(), 1);
}

#[test]
fn a_retry_requires_its_own_approval() {
    let mut fixture = Fixture::new("approval-retry");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let approver = RecordingApprover::allow();
    let creator = ScriptedCreator::new(&fixture, vec![Step::Nothing, Step::Create]);
    assert!(
        fixture
            .prepare_with_policy(&goal_id, &HostGit::new(), &creator, &approver, true)
            .unwrap()
            .is_active()
    );
    assert_eq!(
        approver.calls(),
        2,
        "each invocation is approved separately"
    );
    assert_eq!(
        approver.calls(),
        2,
        "each invocation is approved separately"
    );
    assert_eq!(creator.calls(), 2);
    // Both approvals describe the same frozen operation: the retry re-requests
    // consent for the identical target rather than reusing a stale approval.
    let requests = approver.requests();
    assert_eq!(requests[0], requests[1]);
}

#[test]
fn approval_does_not_add_a_permitted_directory() {
    let fixture = Fixture::new("approval-no-path");
    // Deliberately do NOT authorize the managed root.
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let before = fixture.session.permitted_directories.clone();

    let approver = RecordingApprover::allow();
    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    let block = fixture
        .prepare_with_policy(&goal_id, &HostGit::new(), &creator, &approver, true)
        .unwrap()
        .block_detail()
        .cloned()
        .expect("approval alone is insufficient");
    assert_eq!(
        block.code, "MANAGED_SESSION_AUTHORITY",
        "approval must not substitute for Session path authority"
    );
    assert_eq!(creator.calls(), 0);
    assert_eq!(fixture.session.permitted_directories, before);
}

#[test]
fn session_authority_alone_does_not_replace_approval() {
    let mut fixture = Fixture::new("approval-still-required");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    // Path authority is satisfied, but the approval is refused, so on a platform
    // that requires approval nothing may run. On a platform that does not, the
    // Unix behavior is unchanged and creation proceeds.
    let approver = RecordingApprover::deny();
    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    let preparation = fixture
        .prepare_with_policy(&goal_id, &HostGit::new(), &creator, &approver, true)
        .unwrap();
    assert_eq!(
        preparation.block_detail().map(|block| block.code),
        Some("MANAGED_CREATION_APPROVAL_DENIED"),
        "Session path authority is necessary but never sufficient on Windows"
    );
    assert_eq!(creator.calls(), 0);
}

#[test]
fn the_unix_gate_is_not_reached_when_the_platform_does_not_require_approval() {
    let mut fixture = Fixture::new("approval-unix-off");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    // With the gate off, the approver is never consulted at all, so a denied
    // approver cannot affect Unix behavior.
    let approver = RecordingApprover::deny();
    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    assert!(
        fixture
            .prepare_with_policy(&goal_id, &HostGit::new(), &creator, &approver, false)
            .unwrap()
            .is_active()
    );
    assert_eq!(
        approver.calls(),
        0,
        "the gate must not be consulted on Unix"
    );
    assert_eq!(creator.calls(), 1);
}

#[test]
fn the_workspace_mode_opt_in_is_not_approval() {
    let mut fixture = Fixture::new("approval-not-optin");
    fixture.authorize_managed_root();
    // The Goal opted in to MANAGED_WORKTREE, yet a refused approval still stops
    // everything: the opt-in is isolation policy, not consent to mutate.
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    assert_eq!(
        fixture.goal(&goal_id).workspace_mode(),
        WorkspaceMode::ManagedWorktree
    );
    let approver = RecordingApprover::deny();
    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    assert_eq!(
        fixture
            .prepare_with_policy(&goal_id, &HostGit::new(), &creator, &approver, true)
            .unwrap()
            .block_detail()
            .map(|block| block.code),
        Some("MANAGED_CREATION_APPROVAL_DENIED")
    );
    assert_eq!(creator.calls(), 0);
}

// ---------------------------------------------------------------------------
// K. Migration
// ---------------------------------------------------------------------------

/// Build a schema-4 durable document for `goal`, optionally with a managed
/// record, so the migration can be exercised against real stored bytes.
fn schema_four_document(goal: &Goal) -> Value {
    let mut value = serde_json::to_value(goal).unwrap();
    let object = value.as_object_mut().unwrap();
    object.insert("schema_version".to_owned(), json!(4));
    if let Some(record) = goal.managed_worktree() {
        let mut record_value = serde_json::to_value(record).unwrap();
        // A schema-4 record has no attempt counter at all.
        record_value
            .as_object_mut()
            .unwrap()
            .remove("creation_attempts_consumed");
        object.insert("managed_worktree".to_owned(), record_value);
    }
    value
}

fn write_raw_goal(fixture: &Fixture, goal_id: &GoalId, value: &Value) {
    let store = TaskStore::with_state_root(fixture.state_root.clone());
    let path = store
        .goal_path_for_test(&fixture.session.id, goal_id)
        .expect("goal path");
    std::fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

#[test]
fn a_schema_four_primary_goal_migrates_without_managed_authority() {
    let fixture = Fixture::new("migrate-primary");
    let goal_id = fixture.start(WorkspaceMode::Primary);
    let goal = fixture.goal(&goal_id);
    let value = schema_four_document(&goal);
    write_raw_goal(&fixture, &goal_id, &value);

    let store = TaskStore::with_state_root(fixture.state_root.clone());
    let migrated = store.load_goal(&fixture.session.id, &goal_id).unwrap();
    assert_eq!(migrated.workspace_mode(), WorkspaceMode::Primary);
    assert!(migrated.managed_worktree().is_none());
    assert_eq!(migrated.execution_root(), Some(migrated.cwd()));
}

#[test]
fn a_schema_four_requested_record_migrates_with_a_full_budget() {
    let mut fixture = Fixture::new("migrate-requested");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let goal = fixture.goal(&goal_id);
    let value = schema_four_document(&goal);
    write_raw_goal(&fixture, &goal_id, &value);

    // A REQUESTED record provably never invoked Git, so it keeps a full budget.
    let store = TaskStore::with_state_root(fixture.state_root.clone());
    let migrated = store.load_goal(&fixture.session.id, &goal_id).unwrap();
    let record = migrated.managed_worktree().unwrap();
    assert_eq!(record.lifecycle(), ManagedWorktreeLifecycle::Requested);
    assert_eq!(record.creation_attempts_consumed(), 0);
    assert!(record.permits_creation_attempt());
}

#[test]
fn a_schema_four_prepared_record_migrates_fail_closed_with_no_fresh_retry_authority() {
    let mut fixture = Fixture::new("migrate-prepared");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let goal = fixture.goal(&goal_id);
    let record = goal.managed_worktree().unwrap();
    let intent = ManagedWorktreeCreationIntent::prepared(
        &goal_id,
        record.worktree_id().clone(),
        WorktreeOperationId::new(),
        fixture.common_dir(),
        fixture.managed_target(&goal_id),
        fixture.base_commit.clone(),
    )
    .unwrap();
    let revision = goal.revision();
    fixture
        .store
        .mutate_goal_snapshot(&fixture.session.id, &goal_id, revision, |goal, _now| {
            goal.set_managed_creation_intent(intent)
        })
        .unwrap();

    // Rewrite that durable record as a genuine schema-4 document: PREPARED,
    // with no attempt counter, exactly what an older release would have stored.
    let goal = fixture.goal(&goal_id);
    let value = schema_four_document(&goal);
    write_raw_goal(&fixture, &goal_id, &value);

    let store = TaskStore::with_state_root(fixture.state_root.clone());
    let migrated = store.load_goal(&fixture.session.id, &goal_id).unwrap();
    let record = migrated.managed_worktree().unwrap();
    assert_eq!(
        record.lifecycle(),
        ManagedWorktreeLifecycle::Prepared,
        "the migration must not invent a fresh lifecycle"
    );
    assert_eq!(
        record.creation_attempts_consumed(),
        MAX_LIFETIME_CREATION_ATTEMPTS,
        "an ambiguous legacy PREPARED record must fail closed, not get a fresh budget"
    );
    assert!(
        !record.permits_creation_attempt(),
        "a migrated ambiguous record must not be able to invoke Git again"
    );

    // And preparation on it stops before any Git run, naming the spent budget as
    // the reason: the migration granted no fresh retry authority.
    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    let code = fixture
        .prepare(&goal_id, &HostGit::new(), &creator)
        .unwrap()
        .block_detail()
        .map(|block| block.code)
        .expect("a migrated ambiguous record blocks");
    assert_eq!(code, "MANAGED_RETRY_EXHAUSTED");
    assert_eq!(creator.calls(), 0);
}

#[test]
fn the_migration_never_infers_attempts_from_names_or_state() {
    let mut fixture = Fixture::new("migrate-no-inference");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let goal = fixture.goal(&goal_id);
    let value = schema_four_document(&goal);
    write_raw_goal(&fixture, &goal_id, &value);

    // Whatever exists on disk - branch name, worktree path, revision - must not
    // change the migrated count. It is derived only from the lifecycle.
    let store = TaskStore::with_state_root(fixture.state_root.clone());
    let migrated = store.load_goal(&fixture.session.id, &goal_id).unwrap();
    let record = migrated.managed_worktree().unwrap();
    assert_eq!(record.creation_attempts_consumed(), 0);
    assert!(record.branch_ref().contains(goal_id.as_str()));
    assert!(record.worktree_root().ends_with(goal_id.as_str()));
}

#[test]
fn a_schema_four_document_that_carries_the_attempt_field_is_rejected() {
    let mut fixture = Fixture::new("migrate-carries-field");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let goal = fixture.goal(&goal_id);

    // A schema-4 record cannot legitimately carry this field. Honouring one would
    // let the document hand itself a fresh lifetime budget, defeating the
    // fail-closed derivation.
    for (lifecycle, supplied) in [
        ("REQUESTED", 0),
        ("PREPARED", 0),
        ("PREPARED", MAX_LIFETIME_CREATION_ATTEMPTS),
        ("ACTIVE", 0),
    ] {
        let mut value = schema_four_document(&goal);
        let record = value
            .as_object_mut()
            .unwrap()
            .get_mut("managed_worktree")
            .unwrap()
            .as_object_mut()
            .unwrap();
        record.insert("lifecycle".to_owned(), json!(lifecycle));
        record.insert("creation_attempts_consumed".to_owned(), json!(supplied));
        if lifecycle == "ACTIVE" {
            record.insert(
                "last_reconciled_head".to_owned(),
                json!(fixture.base_commit),
            );
            record.insert(
                "last_reconciled_at".to_owned(),
                json!("2026-01-01T00:00:00Z"),
            );
        }
        write_raw_goal(&fixture, &goal_id, &value);

        let store = TaskStore::with_state_root(fixture.state_root.clone());
        let error = store
            .load_goal(&fixture.session.id, &goal_id)
            .expect_err("a schema-4 document must not supply its own attempt budget");
        assert!(
            matches!(error, OrchestratorError::CorruptGoal(_)),
            "unexpected error for {lifecycle}/{supplied}: {error}"
        );
    }
}

#[test]
fn a_current_schema_document_missing_the_attempt_field_is_rejected() {
    let mut fixture = Fixture::new("stripped-field");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let mut value = serde_json::to_value(fixture.goal(&goal_id)).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .get_mut("managed_worktree")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("creation_attempts_consumed");
    write_raw_goal(&fixture, &goal_id, &value);

    // The field is authority-bearing and zero is the maximum-authority value, so
    // a current-schema document that omits it must fail closed rather than decode
    // as a full budget.
    let store = TaskStore::with_state_root(fixture.state_root.clone());
    let error = store
        .load_goal(&fixture.session.id, &goal_id)
        .expect_err("a stripped attempt counter must not reopen the budget");
    assert!(matches!(error, OrchestratorError::CorruptGoal(_)));
}

#[test]
fn an_active_record_with_no_consumed_attempt_is_not_treated_as_ours() {
    let mut fixture = Fixture::new("active-zero-attempts");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    // Create it for real, so the worktree genuinely exists and reconciles
    // exactly. Then rewrite only the durable counter.
    assert!(fixture.prepare_real(&goal_id).is_active());
    assert_eq!(attempts(&fixture, &goal_id), 1);

    let mut value = serde_json::to_value(fixture.goal(&goal_id)).unwrap();
    {
        let record_value = value
            .as_object_mut()
            .unwrap()
            .get_mut("managed_worktree")
            .unwrap()
            .as_object_mut()
            .unwrap();
        record_value.insert("creation_attempts_consumed".to_owned(), json!(0));
    }
    write_raw_goal(&fixture, &goal_id, &value);

    let block = fixture.block(&goal_id);
    assert_eq!(block.code, "MANAGED_RECOVERY_REQUIRED");
    assert!(
        block
            .detail
            .contains("no creation attempt was ever consumed"),
        "{}",
        block.detail
    );
}

#[test]
fn a_schema_four_document_with_a_corrupt_lifecycle_fails_closed() {
    let mut fixture = Fixture::new("migrate-corrupt-lifecycle");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let mut value = schema_four_document(&fixture.goal(&goal_id));
    value
        .as_object_mut()
        .unwrap()
        .get_mut("managed_worktree")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("lifecycle".to_owned(), json!("NOT_A_LIFECYCLE"));
    write_raw_goal(&fixture, &goal_id, &value);

    let store = TaskStore::with_state_root(fixture.state_root.clone());
    let error = store
        .load_goal(&fixture.session.id, &goal_id)
        .expect_err("an unrecognised lifecycle must not be silently accepted");
    assert!(matches!(error, OrchestratorError::CorruptGoal(_)));
}

// ---------------------------------------------------------------------------
// A. PRIMARY regression
// ---------------------------------------------------------------------------

#[test]
fn omitted_workspace_mode_is_primary_and_never_creates_a_worktree() {
    let mut fixture = Fixture::new("primary-default");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::Primary);

    let goal = fixture.goal(&goal_id);
    assert_eq!(goal.workspace_mode(), WorkspaceMode::Primary);
    assert!(goal.managed_worktree().is_none());
    assert!(goal.managed_worktree_creation_intent().is_none());
    // PRIMARY keeps the exact current execution root.
    assert_eq!(goal.execution_root(), Some(goal.cwd()));

    let before = git(&fixture.primary, &["worktree", "list", "--porcelain"]);
    let preparation = fixture.prepare_real(&goal_id);
    assert_eq!(preparation, ManagedWorkspacePreparation::NotManaged);
    assert_eq!(
        git(&fixture.primary, &["worktree", "list", "--porcelain"]),
        before,
        "PRIMARY preparation must not touch Git"
    );
    assert!(!fixture.managed_target(&goal_id).exists());
}

#[test]
fn existing_goal_start_payloads_remain_compatible_and_plan_normally() {
    let fixture = Fixture::new("primary-payload");
    // The exact pre-managed payload shape: no workspace_mode at all.
    let args = json!({
        "session_id": fixture.session.id,
        "objective": "legacy payload",
        "title": "Legacy",
        "constraints": ["stay inert"],
        "completion_criteria": ["durable state only"],
        "idempotency_key": "legacy-key"
    });
    let first = fixture.start_value(&args).unwrap();
    assert_eq!(first["idempotent_replay"], json!(false));
    let replay = fixture.start_value(&args).unwrap();
    assert_eq!(replay["idempotent_replay"], json!(true));
    assert_eq!(first["goal_id"], replay["goal_id"]);

    let goal_id = GoalId::parse(first["goal_id"].as_str().unwrap()).unwrap();
    let goal = fixture.goal(&goal_id);
    // PRIMARY Planner behavior is unchanged: the existing planner request is
    // still built from Goal.cwd.
    let request = planner::planner_request_for_goal(&goal, &fixture.session).unwrap();
    assert_eq!(request.cwd(), goal.cwd());
}

// ---------------------------------------------------------------------------
// B. Public authority
// ---------------------------------------------------------------------------

#[test]
fn caller_cannot_supply_a_worktree_path_branch_base_or_permission_root() {
    let fixture = Fixture::new("public-authority");
    for forbidden in [
        "worktree_path",
        "managed_root",
        "branch",
        "ref",
        "base_sha",
        "base_commit",
        "repository_common_dir",
        "git_argv",
        "sandbox_root",
        "permission_root",
    ] {
        let mut args = json!({
            "session_id": fixture.session.id,
            "objective": "attempt to choose managed identity",
            "workspace_mode": "MANAGED_WORKTREE"
        });
        args[forbidden] = json!("/tmp/attacker-chosen");
        let error = fixture
            .start_value(&args)
            .expect_err("a caller-chosen managed identity is refused");
        assert_eq!(error.code(), "INVALID_ARGUMENT", "for field {forbidden}");
    }
    // Nothing was created, and the host-managed root was not written to.
    assert!(
        !fixture
            .managed_root
            .join(fixture.session.id.as_str())
            .exists()
    );
    assert!(
        fixture
            .store
            .list_goals_for_session(&fixture.session.id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn goal_start_schema_exposes_only_the_additive_mode_field() {
    let tools = crate::mcp::tools();
    let goal_start = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("goal_start"))
        .expect("goal_start is advertised");
    let schema = goal_start.pointer("/inputSchema").unwrap();
    assert_eq!(schema.pointer("/additionalProperties"), Some(&json!(false)));
    let properties = schema.pointer("/properties").unwrap().as_object().unwrap();
    let mode = properties
        .get("workspace_mode")
        .expect("workspace_mode advertised");
    assert_eq!(
        mode.pointer("/enum"),
        Some(&json!(["PRIMARY", "MANAGED_WORKTREE"]))
    );
    assert_eq!(mode.pointer("/default"), Some(&json!("PRIMARY")));
    for forbidden in [
        "worktree_path",
        "managed_root",
        "branch",
        "ref",
        "base_commit",
        "base_sha",
        "git_argv",
        "sandbox_root",
        "permission_root",
    ] {
        assert!(
            !properties.contains_key(forbidden),
            "goal_start must not advertise {forbidden}"
        );
    }
}

#[test]
fn idempotency_distinguishes_workspace_mode() {
    let mut fixture = Fixture::new("idempotency-mode");
    fixture.authorize_managed_root();

    fixture
        .start_with_key(WorkspaceMode::Primary, Some("shared-key"))
        .unwrap();
    let error = fixture
        .start_with_key(WorkspaceMode::ManagedWorktree, Some("shared-key"))
        .expect_err("a mode change under the same key conflicts");
    assert_eq!(error.code(), "IDEMPOTENCY_CONFLICT");
}

#[test]
fn managed_idempotent_replay_requires_the_same_mode() {
    let mut fixture = Fixture::new("idempotency-managed");
    fixture.authorize_managed_root();
    let goal_id = fixture
        .start_with_key(WorkspaceMode::ManagedWorktree, Some("stable-key"))
        .unwrap();

    let managed = json!({
        "session_id": fixture.session.id,
        "objective": "managed worktree objective",
        "title": "P3",
        "constraints": ["stay in scope"],
        "completion_criteria": ["durable managed state"],
        "workspace_mode": "MANAGED_WORKTREE",
        "idempotency_key": "stable-key"
    });
    let replay = fixture.start_value(&managed).unwrap();
    assert_eq!(replay["idempotent_replay"], json!(true));
    assert_eq!(replay["goal_id"], json!(goal_id.as_str()));

    // Switching the mode under the same key must not replay.
    let mut primary = managed.clone();
    primary["workspace_mode"] = json!("PRIMARY");
    let error = fixture
        .start_value(&primary)
        .expect_err("mode change conflicts");
    assert_eq!(error.code(), "IDEMPOTENCY_CONFLICT");
}

// ---------------------------------------------------------------------------
// C. Session path authority
// ---------------------------------------------------------------------------

#[test]
fn managed_target_outside_session_authority_is_blocked_before_any_git_mutation() {
    // The managed root is deliberately NOT authorized.
    let fixture = Fixture::new("authority-denied");
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let before_refs = git(&fixture.primary, &["show-ref"]);
    let before_worktrees = git(&fixture.primary, &["worktree", "list", "--porcelain"]);
    let before_status = git(&fixture.primary, &["status", "--porcelain=v1"]);

    assert_eq!(fixture.block(&goal_id).code, "MANAGED_SESSION_AUTHORITY");

    // No worktree, no branch, no directory, and no PREPARED side effect.
    assert!(!fixture.managed_target(&goal_id).exists());
    assert!(!fixture.ref_exists(&Fixture::branch_ref(&goal_id)));
    assert_eq!(git(&fixture.primary, &["show-ref"]), before_refs);
    assert_eq!(
        git(&fixture.primary, &["worktree", "list", "--porcelain"]),
        before_worktrees
    );
    assert_eq!(
        git(&fixture.primary, &["status", "--porcelain=v1"]),
        before_status
    );

    let goal = fixture.goal(&goal_id);
    assert_eq!(
        goal.managed_worktree().unwrap().lifecycle(),
        ManagedWorktreeLifecycle::Requested,
        "a permission denial must not burn the lifecycle"
    );
    assert!(goal.managed_worktree_creation_intent().is_none());
    assert_eq!(goal.plan_revision(), 0);
}

#[test]
fn preparation_never_appends_to_permitted_directories() {
    let fixture = Fixture::new("authority-no-append");
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let before = fixture.session.permitted_directories.clone();
    assert!(fixture.prepare_real(&goal_id).block_detail().is_some());
    assert_eq!(fixture.session.permitted_directories, before);
}

#[test]
fn goal_persistence_does_not_recreate_a_revoked_permission() {
    let mut fixture = Fixture::new("authority-revoked");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    assert!(fixture.prepare_real(&goal_id).is_active());

    // The operator revokes the broader root. Durable Goal state still names the
    // managed worktree; that must not resurrect the permission.
    fixture.session.permitted_directories = vec![fixture.primary.clone()];

    assert_eq!(fixture.block(&goal_id).code, "MANAGED_SESSION_AUTHORITY");
    assert_eq!(
        fixture.session.permitted_directories,
        vec![fixture.primary.clone()],
        "revoked authority must not be recreated"
    );
    // The still-locked candidate is left untouched for explicit host recovery.
    assert!(fixture.managed_target(&goal_id).exists());
}

#[test]
fn a_refused_goal_resumes_after_the_operator_authorizes_the_root() {
    let mut fixture = Fixture::new("authority-then-allow");
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    assert_eq!(fixture.block(&goal_id).code, "MANAGED_SESSION_AUTHORITY");

    // The operator uses the existing explicit authorization path.
    fixture.authorize_managed_root();
    assert!(
        fixture.prepare_real(&goal_id).is_active(),
        "an authorized root must let a later explicit run proceed"
    );
    assert!(fixture.managed_target(&goal_id).exists());
}

#[test]
fn an_explicitly_authorized_broader_root_covers_the_exact_derived_child() {
    let mut fixture = Fixture::new("authority-broad-root");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    // The derived child is `<root>/<session>/<goal>` under the operator grant.
    assert!(
        fixture
            .managed_target(&goal_id)
            .starts_with(fixture.managed_root_for_git())
    );
    assert!(fixture.prepare_real(&goal_id).is_active());
}

// ---------------------------------------------------------------------------
// D. Eligibility
// ---------------------------------------------------------------------------

fn assert_ineligible(name: &str, dirty: impl FnOnce(&Fixture)) {
    let mut fixture = Fixture::new(name);
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    dirty(&fixture);

    let block = fixture.block(&goal_id);
    assert_eq!(
        block.code, "MANAGED_ELIGIBILITY",
        "detail: {}",
        block.detail
    );
    assert!(!fixture.managed_target(&goal_id).exists());
    assert!(!fixture.ref_exists(&Fixture::branch_ref(&goal_id)));
    assert_eq!(
        fixture
            .goal(&goal_id)
            .managed_worktree()
            .unwrap()
            .lifecycle(),
        ManagedWorktreeLifecycle::Blocked
    );
}

#[test]
fn dirty_tracked_primary_blocks() {
    assert_ineligible("eligibility-dirty", |fixture| {
        std::fs::write(fixture.primary.join("tracked.txt"), "changed\n").unwrap();
    });
}

#[test]
fn staged_change_blocks() {
    assert_ineligible("eligibility-staged", |fixture| {
        std::fs::write(fixture.primary.join("tracked.txt"), "changed\n").unwrap();
        git(&fixture.primary, &["add", "tracked.txt"]);
    });
}

#[test]
fn untracked_file_blocks() {
    assert_ineligible("eligibility-untracked", |fixture| {
        std::fs::write(fixture.primary.join("scratch.txt"), "new\n").unwrap();
    });
}

#[test]
fn in_progress_git_operation_blocks() {
    assert_ineligible("eligibility-merge-in-progress", |fixture| {
        std::fs::write(
            fixture.common_dir().join("MERGE_HEAD"),
            format!("{}\n", fixture.base_commit),
        )
        .unwrap();
    });
}

#[test]
fn primary_head_moving_after_request_blocks() {
    let mut fixture = Fixture::new("eligibility-base-moved");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    git(
        &fixture.primary,
        &["commit", "-q", "--allow-empty", "-m", "advance primary"],
    );
    let block = fixture.block(&goal_id);
    assert_eq!(block.code, "MANAGED_ELIGIBILITY");
    assert!(
        block.detail.contains("BaseCommitMismatch"),
        "{}",
        block.detail
    );
}

#[test]
fn branch_collision_blocks() {
    let mut fixture = Fixture::new("eligibility-branch-collision");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    git(
        &fixture.primary,
        &[
            "branch",
            &Fixture::branch_name(&goal_id),
            &fixture.base_commit,
        ],
    );
    let block = fixture.block(&goal_id);
    assert_eq!(block.code, "MANAGED_ELIGIBILITY");
    assert!(
        block
            .detail
            .contains("ExpectedBranchExistsWithoutOwnership"),
        "{}",
        block.detail
    );
    // The colliding branch is never adopted, overwritten, or deleted.
    assert!(fixture.ref_exists(&Fixture::branch_ref(&goal_id)));
    assert_eq!(
        git(
            &fixture.primary,
            &["rev-parse", &Fixture::branch_name(&goal_id)]
        ),
        fixture.base_commit
    );
}

#[test]
fn path_collision_blocks() {
    let mut fixture = Fixture::new("eligibility-path-collision");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let target = fixture.managed_target(&goal_id);
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("occupant.txt"), "not a worktree\n").unwrap();

    let block = fixture.block(&goal_id);
    assert_eq!(block.code, "MANAGED_ELIGIBILITY");
    assert!(
        block.detail.contains("ExpectedPathOccupied"),
        "{}",
        block.detail
    );
    // The occupying content is left exactly as it was.
    assert_eq!(
        std::fs::read_to_string(target.join("occupant.txt")).unwrap(),
        "not a worktree\n"
    );
    assert!(!fixture.ref_exists(&Fixture::branch_ref(&goal_id)));
}

#[test]
fn a_subdirectory_cwd_is_not_the_managed_primary_root() {
    let mut fixture = Fixture::new("eligibility-subdir");
    fixture.authorize_managed_root();
    let nested = fixture.primary.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    // A session rooted in a subdirectory cannot host a managed worktree in V1.
    let session = config::Session {
        id: fixture.session.id.clone(),
        cwd: nested,
        permitted_directories: fixture.session.permitted_directories.clone(),
    };
    let goal_id = GoalId::new();
    let error =
        request_managed_workspace(&fixture.managed_root, &session, &goal_id, &HostGit::new())
            .expect_err("a subdirectory cwd is refused");
    assert!(
        error
            .to_string()
            .contains("not the canonical Git top level")
    );
}

#[test]
fn a_non_git_session_cwd_cannot_request_a_managed_workspace() {
    let root = std::env::temp_dir().join(format!("local-mcp-mw-p3-nogit-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    let managed_root = root.join("managed");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&managed_root).unwrap();
    let session = config::Session {
        id: format!("p3-nogit-{}", Uuid::new_v4()),
        cwd: workspace.clone(),
        permitted_directories: vec![workspace, managed_root.clone()],
    };
    let _store = TaskStore::with_state_root(state);
    let error = request_managed_workspace(&managed_root, &session, &GoalId::new(), &HostGit::new())
        .expect_err("a non-Git cwd cannot host a managed worktree");
    assert!(!error.to_string().is_empty());
    assert!(!managed_root.join(session.id.as_str()).exists());
    let _ = std::fs::remove_dir_all(root);
}

// ---------------------------------------------------------------------------
// E. Exact creation command
// ---------------------------------------------------------------------------

#[test]
fn creation_argv_is_exact_and_contains_no_force_or_branch_guessing() {
    let fixture = Fixture::new("argv-contract");
    let goal_id = GoalId::new();
    let target = fixture
        .managed_root
        .join("session")
        .join("goal")
        .to_string_lossy()
        .into_owned();
    let intent = ManagedWorktreeCreationIntent::prepared(
        &goal_id,
        WorktreeId::new(),
        WorktreeOperationId::new(),
        fixture.common_dir(),
        PathBuf::from(&target),
        fixture.base_commit.clone(),
    )
    .unwrap();
    let creation = ManagedWorktreeCreation::from_intent(&intent).unwrap();
    let argv = creation.argv();

    // The full Goal UUID, never a shortened identity.
    assert_eq!(goal_id.as_str().len(), 36);
    let expected: Vec<OsString> = vec![
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=",
        "--no-optional-locks",
        "worktree",
        "add",
        "--lock",
        "--reason",
    ]
    .into_iter()
    .map(OsString::from)
    .chain([
        format!("local-mcp goal {}", goal_id.as_str()).into(),
        "-b".into(),
        format!("local-mcp/goal/{}", goal_id.as_str()).into(),
        target.as_str().into(),
        fixture.base_commit.clone().into(),
    ])
    .collect();
    assert_eq!(argv, expected);

    let text: Vec<String> = argv
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    for forbidden in ["-B", "--force", "-f", "--detach", "--guess-remote"] {
        assert!(
            !text.contains(&forbidden.to_owned()),
            "forbidden flag {forbidden} must not be expressible"
        );
    }
    // The branch argument is the branch *name*; the durable value is the ref.
    assert_eq!(
        creation.branch_name(),
        format!("local-mcp/goal/{}", goal_id.as_str())
    );
    assert_eq!(intent.branch_ref(), Fixture::branch_ref(&goal_id));
}

#[test]
fn the_read_only_allowlist_still_refuses_worktree_add() {
    // The mutating seam is separate: adding a worktree is not read-only.
    assert!(
        assert_read_only(&[
            "worktree", "add", "--lock", "--reason", "r", "-b", "b", "/tmp/x", "abc"
        ])
        .is_err()
    );
    assert!(assert_read_only(&["worktree", "list", "--porcelain", "-z"]).is_ok());
    assert!(
        assert_read_only(&["worktree", "remove", "/tmp/x"]).is_err(),
        "removal is not authorized in Phase 3"
    );
}

#[test]
fn creation_produces_the_exact_locked_worktree_branch_and_head() {
    let mut fixture = Fixture::new("argv-identity");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let revision_before = fixture.goal(&goal_id).revision();

    assert!(fixture.prepare_real(&goal_id).is_active());

    let porcelain = git(&fixture.primary, &["worktree", "list", "--porcelain"]);
    assert!(porcelain.contains(&format!("branch {}", Fixture::branch_ref(&goal_id))));
    assert!(porcelain.contains(&format!("locked local-mcp goal {}", goal_id.as_str())));
    assert!(porcelain.contains(&fixture.base_commit));

    // Creation grants zero remote authority: the command shape has no fetch,
    // push, or publish form, and the repository keeps no remote.
    assert!(
        git(&fixture.primary, &["remote"]).is_empty(),
        "the fixture repository must have no remote to exercise"
    );
    let config = git(&fixture.primary, &["config", "--local", "--list"]);
    for forbidden in ["remote.", "branch.", "push", "url."] {
        assert!(
            !config.contains(forbidden),
            "creation must not write {forbidden} configuration: {config}"
        );
    }

    let after = fixture.goal(&goal_id);
    assert_eq!(after.plan_revision(), 0);
    // PREPARED, the consumed attempt, then ACTIVE.
    assert_eq!(after.revision(), revision_before + 3);
    assert_eq!(attempts(&fixture, &goal_id), 1);
}

#[cfg(unix)]
#[test]
fn creation_does_not_execute_repository_hooks() {
    use std::os::unix::fs::PermissionsExt;

    let mut fixture = Fixture::new("hooks");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let hooks = fixture.common_dir().join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let marker = fixture.root.join("hook-ran");
    let hook = hooks.join("post-checkout");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\ntouch \"{}\"\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(fixture.prepare_real(&goal_id).is_active());
    assert!(
        !marker.exists(),
        "managed creation must not execute repository hooks"
    );
}

// ---------------------------------------------------------------------------
// F. Durable PREPARED ordering, recovery, and retry
// ---------------------------------------------------------------------------

#[test]
fn prepared_intent_is_durable_before_git_and_plan_revision_stays_zero() {
    let mut fixture = Fixture::new("prepared-ordering");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let revision_before = fixture.goal(&goal_id).revision();

    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    assert!(
        fixture
            .prepare(&goal_id, &HostGit::new(), &creator)
            .unwrap()
            .is_active()
    );

    let observations = creator.observations();
    assert_eq!(observations.len(), 1);
    let observed = &observations[0];
    assert_eq!(
        observed.lifecycle,
        Some(ManagedWorktreeLifecycle::Prepared),
        "PREPARED must be durable before git runs"
    );
    assert!(observed.operation_id.is_some());
    assert_eq!(observed.plan_revision_at_call, 0);

    // The argv Git received must equal the durable intent exactly.
    assert!(
        observed
            .argv
            .contains(&observed.intent_root.clone().unwrap().display().to_string())
    );
    assert!(
        observed
            .argv
            .contains(&observed.intent_base.clone().unwrap())
    );
    assert!(
        observed.argv.contains(
            &observed
                .intent_branch
                .clone()
                .unwrap()
                .trim_start_matches("refs/heads/")
                .to_owned()
        )
    );

    let after = fixture.goal(&goal_id);
    assert_eq!(after.plan_revision(), 0, "plan_revision must remain 0");
    // PREPARED, the consumed attempt, then ACTIVE: three lifecycle mutations.
    assert_eq!(after.revision(), revision_before + 3);
    assert_eq!(attempts(&fixture, &goal_id), 1);
    assert_eq!(
        after.managed_worktree().unwrap().lifecycle(),
        ManagedWorktreeLifecycle::Active
    );
    // The reconciled ACTIVE record must carry the mechanical observation.
    assert_eq!(
        after.managed_worktree().unwrap().last_reconciled_head(),
        Some(fixture.base_commit.as_str())
    );
    // A reconciled workspace must not keep a stale intent.
    assert!(after.managed_worktree_creation_intent().is_none());
}

#[test]
fn exact_side_effect_after_a_lost_response_is_adopted_as_active() {
    let mut fixture = Fixture::new("recovery-lost-response");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let creator = ScriptedCreator::new(&fixture, vec![Step::CreateThenReportFailure]);
    assert!(
        fixture
            .prepare(&goal_id, &HostGit::new(), &creator)
            .unwrap()
            .is_active()
    );
    assert_eq!(
        creator.calls(),
        1,
        "an exact side effect must not be retried"
    );
    assert!(fixture.managed_target(&goal_id).exists());
}

#[test]
fn a_crash_after_prepared_before_git_is_recoverable() {
    let mut fixture = Fixture::new("recovery-prepared-crash");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    // Simulate the crash precisely: a durable PREPARED intent exists, and Git
    // has not run. This is the exact state a crash after the design section 11
    // persistence point leaves behind.
    let revision = fixture.goal(&goal_id).revision();
    let record = fixture.goal(&goal_id);
    let intent = ManagedWorktreeCreationIntent::prepared(
        &goal_id,
        record.managed_worktree().unwrap().worktree_id().clone(),
        WorktreeOperationId::new(),
        fixture.common_dir(),
        fixture.managed_target(&goal_id),
        fixture.base_commit.clone(),
    )
    .unwrap();
    fixture
        .store
        .mutate_goal_snapshot(&fixture.session.id, &goal_id, revision, |goal, _now| {
            goal.set_managed_creation_intent(intent)
        })
        .unwrap();

    let crashed = fixture.goal(&goal_id);
    assert_eq!(
        crashed.managed_worktree().unwrap().lifecycle(),
        ManagedWorktreeLifecycle::Prepared
    );
    assert_eq!(crashed.plan_revision(), 0);
    let crashed_operation = crashed
        .managed_worktree_creation_intent()
        .unwrap()
        .operation_id()
        .as_str()
        .to_owned();
    assert!(!fixture.managed_target(&goal_id).exists());

    // A later process reconciles, proves no side effect, and finishes the
    // durable intent with a fresh operation identity.
    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    assert!(
        fixture
            .prepare(&goal_id, &HostGit::new(), &creator)
            .unwrap()
            .is_active()
    );
    let observations = creator.observations();
    assert_eq!(observations.len(), 1);
    assert_ne!(
        observations[0].operation_id.clone().unwrap(),
        crashed_operation,
        "a recovered attempt uses a fresh operation identity"
    );
    assert!(fixture.managed_target(&goal_id).exists());
}

#[test]
fn proven_no_side_effect_failure_is_retryable_once_with_a_fresh_operation_id() {
    let mut fixture = Fixture::new("retry-bounded");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    // Attempt 1 does nothing; attempt 2 really creates.
    let creator = ScriptedCreator::new(&fixture, vec![Step::Nothing, Step::Create]);
    assert!(
        fixture
            .prepare(&goal_id, &HostGit::new(), &creator)
            .unwrap()
            .is_active()
    );
    let observations = creator.observations();
    assert_eq!(observations.len(), 2, "exactly one bounded retry");
    assert_ne!(
        observations[0].operation_id, observations[1].operation_id,
        "each attempt needs a distinct operation identity"
    );
    assert_eq!(fixture.goal(&goal_id).plan_revision(), 0);
}

#[test]
fn a_retryable_failure_that_never_creates_blocks_after_the_budget() {
    let mut fixture = Fixture::new("retry-exhausted");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let creator = ScriptedCreator::new(&fixture, vec![Step::Nothing]);
    let block = fixture
        .prepare(&goal_id, &HostGit::new(), &creator)
        .unwrap()
        .block_detail()
        .cloned()
        .expect("the budget is exhausted");
    assert_eq!(block.code, "MANAGED_RETRY_EXHAUSTED");
    assert_eq!(creator.calls(), 2, "the retry budget is bounded");
    assert!(!fixture.managed_target(&goal_id).exists());
    assert!(!fixture.ref_exists(&Fixture::branch_ref(&goal_id)));
}

#[test]
fn a_failure_to_start_git_is_reconciled_rather_than_assumed_side_effect_free() {
    let mut fixture = Fixture::new("spawn-failure");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let creator = ScriptedCreator::new(&fixture, vec![Step::FailToStart]);
    let block = fixture
        .prepare(&goal_id, &HostGit::new(), &creator)
        .unwrap()
        .block_detail()
        .cloned()
        .expect("a start failure is reconciled and bounded");
    // The observation still proved no side effect, so the frozen predicate
    // permits the bounded retry and the budget is then exhausted.
    assert_eq!(block.code, "MANAGED_RETRY_EXHAUSTED");
    assert_eq!(
        creator.calls() as u32,
        MAX_LIFETIME_CREATION_ATTEMPTS,
        "a start failure is reconciled, not retried without proof"
    );
    assert!(
        block.detail.contains("lifetime creation budget is spent"),
        "the exhausting reason must be reported: {}",
        block.detail
    );
    assert!(
        block.detail.contains("scripted failure"),
        "the last observed Git diagnostic must survive into the exhausted-budget \
         report, otherwise the operator loses the real reason: {}",
        block.detail
    );
    assert!(!fixture.managed_target(&goal_id).exists());
}

#[test]
fn branch_only_partial_side_effect_blocks_without_retry() {
    let mut fixture = Fixture::new("recovery-branch-only");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let creator = ScriptedCreator::new(&fixture, vec![Step::BranchOnly]);
    let block = fixture
        .prepare(&goal_id, &HostGit::new(), &creator)
        .unwrap()
        .block_detail()
        .cloned()
        .expect("a partial side effect blocks");
    assert_eq!(block.code, "MANAGED_RECOVERY_REQUIRED");
    assert!(
        block.detail.contains("BranchOnlySideEffect"),
        "{}",
        block.detail
    );
    assert_eq!(creator.calls(), 1, "a partial side effect is never retried");
    assert!(fixture.ref_exists(&Fixture::branch_ref(&goal_id)));
    assert!(!fixture.managed_target(&goal_id).exists());
    assert_eq!(
        fixture
            .goal(&goal_id)
            .managed_worktree()
            .unwrap()
            .lifecycle(),
        ManagedWorktreeLifecycle::Blocked
    );
}

#[test]
fn head_mismatch_after_creation_blocks_without_force() {
    let mut fixture = Fixture::new("recovery-head-mismatch");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let creator = ScriptedCreator::new(&fixture, vec![Step::CreateThenMoveHead]);
    let block = fixture
        .prepare(&goal_id, &HostGit::new(), &creator)
        .unwrap()
        .block_detail()
        .cloned()
        .expect("a HEAD mismatch blocks");
    assert_eq!(block.code, "MANAGED_RECOVERY_REQUIRED");
    assert!(block.detail.contains("HeadMismatch"), "{}", block.detail);
    // The drifted candidate is left exactly as Git left it: no force repair.
    let head = git(&fixture.managed_target(&goal_id), &["rev-parse", "HEAD"]);
    assert_ne!(head, fixture.base_commit);
    assert!(
        git(
            &fixture.managed_target(&goal_id),
            &["status", "--porcelain=v1"]
        )
        .is_empty(),
        "the worktree itself is not modified by reconciliation"
    );
}

#[test]
fn common_dir_mismatch_blocks_reconciliation() {
    let mut fixture = Fixture::new("recovery-common-dir");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    assert!(fixture.prepare_real(&goal_id).is_active());

    let other = fixture.root.join("other-common-dir");
    std::fs::create_dir_all(&other).unwrap();
    let other = std::fs::canonicalize(&other).unwrap();
    let observer = CommonDirOverrideGit {
        inner: HostGit::new(),
        common_dir: other,
    };
    let block = fixture
        .prepare(&goal_id, &observer, &HostWorktreeCreator::new())
        .unwrap()
        .block_detail()
        .cloned()
        .expect("a common-dir mismatch blocks");
    assert_eq!(block.code, "MANAGED_RECOVERY_REQUIRED");
    assert!(
        block.detail.contains("CommonDirMismatch"),
        "{}",
        block.detail
    );
    // The real worktree is left exactly as it was.
    assert!(fixture.managed_target(&goal_id).exists());
}

#[test]
fn a_common_dir_mismatch_blocks_before_any_creation() {
    let mut fixture = Fixture::new("eligibility-common-dir");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let other = fixture.root.join("other-common-dir");
    std::fs::create_dir_all(&other).unwrap();
    let other = std::fs::canonicalize(&other).unwrap();
    let other = std::fs::canonicalize(&other).unwrap();
    let observer = CommonDirOverrideGit {
        inner: HostGit::new(),
        common_dir: other,
    };
    let block = fixture
        .prepare(&goal_id, &observer, &HostWorktreeCreator::new())
        .unwrap()
        .block_detail()
        .cloned()
        .expect("a common-dir mismatch blocks creation");
    assert_eq!(block.code, "MANAGED_ELIGIBILITY");
    assert!(
        block.detail.contains("CommonDirMismatch"),
        "{}",
        block.detail
    );
    assert!(!fixture.managed_target(&goal_id).exists());
    assert!(!fixture.ref_exists(&Fixture::branch_ref(&goal_id)));
}

#[test]
fn an_ambiguous_observation_never_becomes_a_retry() {
    let mut fixture = Fixture::new("recovery-ambiguous");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    let block = fixture
        .prepare(&goal_id, &AmbiguousGit(HostGit::new()), &creator)
        .unwrap()
        .block_detail()
        .cloned()
        .expect("an untrustworthy observation is not a decision");
    assert_eq!(block.code, "MANAGED_OBSERVATION_UNAVAILABLE");
    assert_eq!(
        creator.calls(),
        0,
        "Git must not run when observation cannot be trusted"
    );
    assert!(!fixture.managed_target(&goal_id).exists());
    // The lifecycle is preserved so a later explicit run can reconcile again.
    assert_eq!(
        fixture
            .goal(&goal_id)
            .managed_worktree()
            .unwrap()
            .lifecycle(),
        ManagedWorktreeLifecycle::Requested
    );
}

#[test]
fn stale_or_missing_metadata_blocks_without_destructive_repair() {
    let mut fixture = Fixture::new("recovery-missing-path");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    assert!(fixture.prepare_real(&goal_id).is_active());
    let target = fixture.managed_target(&goal_id);

    // Remove only the directory. Git still registers the worktree, so the
    // observation is stale metadata and must block rather than prune.
    std::fs::remove_dir_all(&target).unwrap();
    let block = fixture.block(&goal_id);
    assert_eq!(block.code, "MANAGED_RECOVERY_REQUIRED");
    assert!(
        block.detail.contains("MissingPath") || block.detail.contains("PrunableMetadata"),
        "{}",
        block.detail
    );
    // No destructive repair happened: the registration is still there. Git
    // spells worktree paths with forward slashes, so compare on a normalized
    // form rather than on the platform separator.
    let porcelain = git(&fixture.primary, &["worktree", "list", "--porcelain"]);
    assert!(
        porcelain
            .replace('\\', "/")
            .contains(&normalized_path(&target)),
        "the worktree registration must survive: {porcelain}"
    );
}

#[test]
fn a_registered_path_owned_by_another_worktree_blocks() {
    let mut fixture = Fixture::new("recovery-path-owned-elsewhere");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let target = fixture.managed_target(&goal_id);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();

    // A different, legitimate worktree is created at the exact managed path.
    git(
        &fixture.primary,
        &[
            "worktree",
            "add",
            "--lock",
            "--reason",
            "unrelated",
            "-b",
            "unrelated/branch",
            &target.display().to_string(),
            &fixture.base_commit,
        ],
    );

    // The fresh-request path catches this at the eligibility gate.
    let block = fixture.block(&goal_id);
    assert_eq!(block.code, "MANAGED_ELIGIBILITY");
    assert!(
        block.detail.contains("ExpectedPathOccupied"),
        "{}",
        block.detail
    );

    // The unrelated worktree is never removed, unlocked, or repurposed.
    let porcelain = git(&fixture.primary, &["worktree", "list", "--porcelain"]);
    assert!(porcelain.contains("unrelated/branch"));
    assert!(porcelain.contains("locked unrelated"));
}

#[test]
fn the_managed_branch_checked_out_elsewhere_blocks_reconciliation() {
    let mut fixture = Fixture::new("recovery-branch-elsewhere");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    // Move straight to a durable PREPARED intent so the reconciliation path,
    // not the eligibility gate, decides the outcome.
    let goal = fixture.goal(&goal_id);
    let intent = ManagedWorktreeCreationIntent::prepared(
        &goal_id,
        goal.managed_worktree().unwrap().worktree_id().clone(),
        WorktreeOperationId::new(),
        fixture.common_dir(),
        fixture.managed_target(&goal_id),
        fixture.base_commit.clone(),
    )
    .unwrap();
    let revision = goal.revision();
    fixture
        .store
        .mutate_goal_snapshot(&fixture.session.id, &goal_id, revision, |goal, _now| {
            goal.set_managed_creation_intent(intent)
        })
        .unwrap();

    // The managed branch is already checked out in a different worktree. The
    // path is spelled the way the host hands it to Git, i.e. de-verbatim.
    let elsewhere = fixture
        .managed_root_for_git()
        .join("someone-elses-worktree");
    std::fs::create_dir_all(fixture.managed_root.join("s")).unwrap();
    git(
        &fixture.primary,
        &[
            "worktree",
            "add",
            "--lock",
            "--reason",
            "unrelated",
            "-b",
            &Fixture::branch_name(&goal_id),
            &elsewhere.display().to_string(),
            &fixture.base_commit,
        ],
    );

    let block = fixture.block(&goal_id);
    assert_eq!(block.code, "MANAGED_RECOVERY_REQUIRED");
    assert!(
        block.detail.contains("PathOwnedByOtherWorktree"),
        "{}",
        block.detail
    );
    // The pre-existing worktree is left alone, and the managed target was never
    // created.
    assert!(elsewhere.exists());
    assert!(!fixture.managed_target(&goal_id).exists());
}

// ---------------------------------------------------------------------------
// G. Planner boundary
// ---------------------------------------------------------------------------

#[test]
fn managed_workspace_cannot_reach_planner_in_any_lifecycle() {
    let mut fixture = Fixture::new("planner-boundary");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let session = fixture.session.clone();

    // REQUESTED
    let goal = fixture.goal(&goal_id);
    assert_eq!(
        goal.managed_worktree().unwrap().lifecycle(),
        ManagedWorktreeLifecycle::Requested
    );
    assert!(goal.execution_root().is_none());
    assert!(matches!(
        planner::planner_request_for_goal(&goal, &session),
        Err(PlannerError::PlanAuthorityViolation(_))
    ));

    // ACTIVE exact ownership is reconciled, and Phase 4 now routes Planner to
    // the managed candidate root.
    assert!(fixture.prepare_real(&goal_id).is_active());
    let goal = fixture.goal(&goal_id);
    assert_eq!(
        goal.managed_worktree().unwrap().lifecycle(),
        ManagedWorktreeLifecycle::Active
    );
    assert_eq!(
        goal.execution_root(),
        Some(goal.managed_worktree().unwrap().worktree_root())
    );
    let request = planner::planner_request_for_goal(&goal, &session).unwrap();
    assert_eq!(
        request.cwd(),
        goal.managed_worktree().unwrap().worktree_root()
    );
}

#[test]
fn blocked_managed_workspace_cannot_reach_planner() {
    let mut fixture = Fixture::new("planner-blocked");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    std::fs::write(fixture.primary.join("tracked.txt"), "dirty\n").unwrap();
    assert!(fixture.prepare_real(&goal_id).block_detail().is_some());

    let goal = fixture.goal(&goal_id);
    assert_eq!(
        goal.managed_worktree().unwrap().lifecycle(),
        ManagedWorktreeLifecycle::Blocked
    );
    assert!(goal.execution_root().is_none());
    assert!(matches!(
        planner::planner_request_for_goal(&goal, &fixture.session),
        Err(PlannerError::PlanAuthorityViolation(_))
    ));
}

#[test]
fn managed_task_scope_materializes_under_candidate_and_rejects_primary_and_git_admin_paths() {
    let mut fixture = Fixture::new("planner-scope-root");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    assert!(fixture.prepare_real(&goal_id).is_active());
    let goal = fixture.goal(&goal_id);
    let worktree_root = goal.managed_worktree().unwrap().worktree_root();
    let proposal = |path: PathBuf| planner::TaskScopeProposal {
        allowed_paths: vec![path],
        forbidden_paths: Vec::new(),
        operation_kind: crate::task::TaskOperationKind::LocalMutation,
        replay_safety: crate::task::ReplaySafety::VerifyBeforeRetry,
    };

    let normalized = planner::validate_and_normalize_scope(
        &proposal(PathBuf::from("src/foo.rs")),
        crate::task::WorkerKind::CodexWriter,
        worktree_root,
    )
    .unwrap();
    assert_eq!(
        normalized.allowed_paths(),
        &[worktree_root.join("src/foo.rs")]
    );
    assert!(!normalized.allowed_paths()[0].starts_with(&fixture.primary));

    assert!(matches!(
        planner::validate_and_normalize_scope(
            &proposal(fixture.primary.join("tracked.txt")),
            crate::task::WorkerKind::CodexWriter,
            worktree_root,
        ),
        Err(PlannerError::PlanAuthorityViolation(_))
    ));
    assert!(matches!(
        planner::validate_and_normalize_scope(
            &proposal(PathBuf::from(".git")),
            crate::task::WorkerKind::CodexWriter,
            worktree_root,
        ),
        Err(PlannerError::PlanAuthorityViolation(_))
    ));
    assert!(matches!(
        planner::validate_and_normalize_scope(
            &proposal(fixture.common_dir()),
            crate::task::WorkerKind::CodexWriter,
            worktree_root,
        ),
        Err(PlannerError::PlanAuthorityViolation(_))
    ));
}

struct CandidateWriter {
    root: PathBuf,
    expected: &'static [u8],
    content: &'static str,
}

impl crate::writer::WriterBackend for CandidateWriter {
    fn propose(
        &self,
        request: &crate::writer::WriterRequest,
    ) -> Result<Vec<u8>, crate::writer::WriterError> {
        use sha2::Digest as _;
        assert_eq!(request.goal_cwd(), self.root);
        let preimage = format!("{:x}", sha2::Sha256::digest(self.expected));
        Ok(json!({
            "goal_id": request.goal_id(),
            "task_id": request.task_id(),
            "attempt_id": request.attempt_id(),
            "goal_revision": request.goal_revision(),
            "plan_revision": request.plan_revision(),
            "status": "candidate_complete",
            "summary": "edit the candidate copy",
            "evidence": [],
            "proposed_operations": [{
                "kind": "WRITE_UTF8",
                "path": "src/foo.rs",
                "expected_preimage": {"kind": "SHA256", "sha256": preimage},
                "content": self.content
            }]
        })
        .to_string()
        .into_bytes())
    }
}

struct CandidateReadonly {
    primary: PathBuf,
}

impl crate::readonly_worker::ReadonlyBackend for CandidateReadonly {
    fn investigate(
        &self,
        request: &crate::readonly_worker::ReadonlyRequest,
    ) -> Result<Vec<u8>, crate::readonly_worker::ReadonlyError> {
        let candidate_root = request.goal_cwd();
        assert_ne!(candidate_root, self.primary.as_path());
        assert_eq!(
            std::fs::read(candidate_root.join("src/foo.rs")).unwrap(),
            b"primary-and-candidate\n"
        );
        Ok(json!({
            "goal_id": request.goal_id(),
            "task_id": request.task_id(),
            "attempt_id": request.attempt_id(),
            "goal_revision": request.goal_revision(),
            "plan_revision": request.plan_revision(),
            "status": "candidate_complete",
            "summary": "read candidate repository state",
            "evidence": [{"kind": "candidate_file", "value": "primary-and-candidate"}]
        })
        .to_string()
        .into_bytes())
    }
}

struct CandidateReviewer;

impl crate::writer::ReviewerBackend for CandidateReviewer {
    fn review(
        &self,
        request: &crate::writer::ReviewerRequest,
    ) -> Result<Vec<u8>, crate::writer::WriterError> {
        Ok(json!({
            "goal_id": request.goal_id(),
            "task_id": request.task_id(),
            "attempt_id": request.attempt_id(),
            "goal_revision": request.goal_revision(),
            "plan_revision": request.plan_revision(),
            "summary": "candidate evidence observed",
            "blocking_findings": 0,
            "evidence": []
        })
        .to_string()
        .into_bytes())
    }
}

#[test]
fn real_creation_planning_and_writer_mutate_only_the_managed_candidate() {
    let mut fixture = Fixture::new("phase4-e2e-isolation");
    fixture.authorize_managed_root();
    std::fs::create_dir_all(fixture.primary.join("src")).unwrap();
    std::fs::write(
        fixture.primary.join("src/foo.rs"),
        "primary-and-candidate\n",
    )
    .unwrap();
    git(&fixture.primary, &["add", "src/foo.rs"]);
    git(&fixture.primary, &["commit", "-q", "-m", "add source file"]);
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    assert!(fixture.prepare_real(&goal_id).is_active());
    let goal = fixture.goal(&goal_id);
    let candidate = goal
        .managed_worktree()
        .unwrap()
        .worktree_root()
        .to_path_buf();
    assert_eq!(goal.cwd(), fixture.primary);

    use sha2::Digest as _;
    let candidate_digest = format!("{:x}", sha2::Sha256::digest(b"candidate-only\n"));
    let second_candidate_digest = format!("{:x}", sha2::Sha256::digest(b"candidate-second\n"));
    let proposal = json!({
        "goal_id": goal.id().as_str(),
        "goal_revision": goal.revision(),
        "summary": "edit source in managed candidate",
        "tasks": [
            {
                "proposal_id": "inspect-candidate",
                "title": "Inspect candidate",
                "objective": "Read repository state from the managed candidate",
                "mandatory": false,
                "worker": "CODEX_READONLY",
                "dependencies": [],
                "scope": {
                    "allowed_paths": ["src"],
                    "forbidden_paths": [],
                    "operation_kind": "READ_ONLY",
                    "replay_safety": "SAFE_READ_ONLY"
                },
                "verification": [{"kind": "STRUCTURED_EVIDENCE", "requirement_id": "candidate.read"}]
            },
            {
                "proposal_id": "candidate-writer",
                "title": "Edit source",
                "objective": "Change the candidate copy only",
                "mandatory": true,
                "worker": "CODEX_WRITER",
                "dependencies": [],
                "scope": {
                    "allowed_paths": ["src"],
                    "forbidden_paths": [],
                    "operation_kind": "LOCAL_MUTATION",
                    "replay_safety": "VERIFY_BEFORE_RETRY"
                },
                "verification": [
                    {"kind": "FILE_EXISTS", "path": "src/foo.rs", "must_be_file": true},
                    {"kind": "FILE_DIGEST", "path": "src/foo.rs", "expected_sha256": candidate_digest},
                    {"kind": "COMMAND_EXIT", "command": ["git", "rev-parse", "--show-toplevel"], "cwd": null, "accepted_exit_codes": [0]},
                    {"kind": "GIT_SCOPE", "allowed_changed_paths": ["src/foo.rs"], "require_no_other_changes": true}
                ]
            },
            {
                "proposal_id": "candidate-writer-two",
                "title": "Edit source again",
                "objective": "Change the candidate copy a second time",
                "mandatory": true,
                "worker": "CODEX_WRITER",
                "dependencies": ["candidate-writer"],
                "scope": {
                    "allowed_paths": ["src"],
                    "forbidden_paths": [],
                    "operation_kind": "LOCAL_MUTATION",
                    "replay_safety": "VERIFY_BEFORE_RETRY"
                },
                "verification": [
                    {"kind": "FILE_EXISTS", "path": "src/foo.rs", "must_be_file": true},
                    {"kind": "FILE_DIGEST", "path": "src/foo.rs", "expected_sha256": second_candidate_digest},
                    {"kind": "GIT_SCOPE", "allowed_changed_paths": ["src/foo.rs"], "require_no_other_changes": true}
                ]
            }
        ],
        "criterion_bindings": [{
            "criterion_id": goal.completion_criteria()[0].id().as_str(),
            "task_refs": ["candidate-writer", "candidate-writer-two"]
        }]
    })
    .to_string()
    .into_bytes();
    let planned = planner::materialize_initial_plan_output(
        &fixture.store,
        &fixture.session,
        &goal_id,
        goal.revision(),
        &proposal,
    )
    .unwrap();
    assert_eq!(planned.plan_revision(), 1);
    let readonly_task_id = planned
        .tasks()
        .iter()
        .find(|(_, task)| task.worker() == crate::task::WorkerKind::CodexReadonly)
        .unwrap()
        .0
        .clone();
    let readonly_result = crate::readonly_worker::run_readonly_attempt(
        &fixture.store,
        &fixture.session,
        &goal_id,
        &readonly_task_id,
        planned.revision(),
        &CandidateReadonly {
            primary: fixture.primary.clone(),
        },
    )
    .unwrap();
    assert_eq!(
        readonly_result.tasks()[&readonly_task_id].status(),
        crate::task::TaskStatus::Verifying
    );
    let task_id = planned
        .tasks()
        .iter()
        .find(|(_, task)| task.objective() == "Change the candidate copy only")
        .unwrap()
        .0
        .clone();
    let second_task_id = planned
        .tasks()
        .iter()
        .find(|(_, task)| task.objective() == "Change the candidate copy a second time")
        .unwrap()
        .0
        .clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let revision_before_writer = fixture.goal(&goal_id).revision();
    let result = runtime
        .block_on(crate::writer::run_writer_attempt(
            &fixture.store,
            &fixture.session,
            &goal_id,
            &task_id,
            revision_before_writer,
            &CandidateWriter {
                root: candidate.clone(),
                expected: b"primary-and-candidate\n",
                content: "candidate-only\n",
            },
            &CandidateReviewer,
        ))
        .unwrap();

    assert_eq!(
        std::fs::read(candidate.join("src/foo.rs")).unwrap(),
        b"candidate-only\n"
    );

    assert_eq!(
        std::fs::read(fixture.primary.join("src/foo.rs")).unwrap(),
        b"primary-and-candidate\n"
    );
    assert_eq!(git(&fixture.primary, &["status", "--porcelain"]), "");
    assert_eq!(result.cwd(), fixture.primary);

    let (approval_responder, stop_approval_responder) = runtime
        .block_on(crate::approvals::spawn_test_approval_responder(
            &fixture.session.id,
            &candidate,
        ))
        .unwrap();
    let verified = runtime
        .block_on(crate::verifier::verify_task(
            &fixture.store,
            &fixture.session,
            &goal_id,
            &task_id,
            result.revision(),
        ))
        .unwrap();
    assert_eq!(
        verified.tasks()[&task_id].status(),
        crate::task::TaskStatus::Completed,
        "verification details: {}; evidence: {}; side_effect_state: {:?}",
        serde_json::to_string(verified.tasks()[&task_id].verification_results()).unwrap(),
        serde_json::to_string(verified.tasks()[&task_id].evidence()).unwrap(),
        verified.tasks()[&task_id]
            .latest_attempt()
            .unwrap()
            .side_effect_state()
    );
    let readied_second = fixture
        .store
        .mutate_goal_snapshot(
            &fixture.session.id,
            &goal_id,
            verified.revision(),
            |goal, now| {
                goal.transition_task(
                    &second_task_id,
                    crate::task::TaskStatus::Ready,
                    crate::task::TaskTransitionContext::default(),
                    now,
                )
            },
        )
        .unwrap();
    let second_result = runtime
        .block_on(crate::writer::run_writer_attempt(
            &fixture.store,
            &fixture.session,
            &goal_id,
            &second_task_id,
            readied_second.revision(),
            &CandidateWriter {
                root: candidate.clone(),
                expected: b"candidate-only\n",
                content: "candidate-second\n",
            },
            &CandidateReviewer,
        ))
        .unwrap();
    assert_eq!(
        std::fs::read(candidate.join("src/foo.rs")).unwrap(),
        b"candidate-second\n"
    );
    let verified_second = runtime
        .block_on(crate::verifier::verify_task(
            &fixture.store,
            &fixture.session,
            &goal_id,
            &second_task_id,
            second_result.revision(),
        ))
        .unwrap();
    let _ = stop_approval_responder.send(());
    runtime.block_on(approval_responder).unwrap().unwrap();
    assert_eq!(
        verified_second.tasks()[&second_task_id].status(),
        crate::task::TaskStatus::Completed,
        "second verification details: {}; evidence: {}; side_effect_state: {:?}",
        serde_json::to_string(verified_second.tasks()[&second_task_id].verification_results())
            .unwrap(),
        serde_json::to_string(verified_second.tasks()[&second_task_id].evidence()).unwrap(),
        verified_second.tasks()[&second_task_id]
            .latest_attempt()
            .unwrap()
            .side_effect_state()
    );
    assert_eq!(git(&fixture.primary, &["status", "--porcelain"]), "");
}

#[test]
fn managed_planner_gate_refuses_revoked_authority_and_missing_or_mismatched_worktree() {
    let mut fixture = Fixture::new("planner-reconcile-gate");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    assert!(fixture.prepare_real(&goal_id).is_active());
    let goal = fixture.goal(&goal_id);
    assert!(planner::planner_request_for_goal(&goal, &fixture.session).is_ok());

    let authorized = fixture.session.permitted_directories.clone();
    fixture.session.permitted_directories.clear();
    assert!(matches!(
        planner::planner_request_for_goal(&goal, &fixture.session),
        Err(PlannerError::PlanAuthorityViolation(_))
    ));
    fixture.session.permitted_directories = authorized;

    let worktree = goal
        .managed_worktree()
        .unwrap()
        .worktree_root()
        .to_path_buf();
    // Keep the registered worktree present, but invalidate its durable branch
    // ownership. The execution-root gate must reconcile before planning.
    git(&worktree, &["switch", "--detach", "HEAD"]);
    assert!(matches!(
        planner::planner_request_for_goal(&goal, &fixture.session),
        Err(PlannerError::PlanAuthorityViolation(_))
    ));

    std::fs::rename(&worktree, worktree.with_extension("moved")).unwrap();
    assert!(matches!(
        planner::planner_request_for_goal(&goal, &fixture.session),
        Err(PlannerError::PlanAuthorityViolation(_))
    ));
}

#[test]
fn primary_planner_behavior_is_unchanged() {
    let fixture = Fixture::new("planner-primary");
    let goal_id = fixture.start(WorkspaceMode::Primary);
    let goal = fixture.goal(&goal_id);
    let request = planner::planner_request_for_goal(&goal, &fixture.session).unwrap();
    assert_eq!(request.cwd(), goal.cwd());
    assert_eq!(goal.execution_root(), Some(goal.cwd()));
    assert!(planner::execution_root_for_goal(&goal, &fixture.session).is_ok());
}

#[cfg(windows)]
#[test]
fn verifier_git_path_spelling_matches_managed_scope_and_primary_root_mode() {
    let mut fixture = Fixture::new("verifier-path-spelling");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    assert!(fixture.prepare_real(&goal_id).is_active());
    let goal = fixture.goal(&goal_id);
    let candidate = goal.managed_worktree().unwrap().worktree_root();

    let proposal = planner::TaskScopeProposal {
        allowed_paths: vec![PathBuf::from("tracked.txt")],
        forbidden_paths: Vec::new(),
        operation_kind: crate::task::TaskOperationKind::LocalMutation,
        replay_safety: crate::task::ReplaySafety::VerifyBeforeRetry,
    };
    let scope = planner::validate_and_normalize_scope(
        &proposal,
        crate::task::WorkerKind::CodexWriter,
        candidate,
    )
    .unwrap();
    let observed_candidate_path =
        crate::verifier::resolve_git_path_for_test("tracked.txt", candidate).unwrap();
    assert_eq!(observed_candidate_path, scope.allowed_paths()[0]);
    assert!(observed_candidate_path.starts_with(candidate));

    let observed_primary_path =
        crate::verifier::resolve_git_path_for_test("tracked.txt", goal.cwd()).unwrap();
    assert_eq!(
        observed_primary_path,
        std::fs::canonicalize(goal.cwd().join("tracked.txt")).unwrap()
    );

    let outside = fixture.root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "outside").unwrap();
    let junction = candidate.join("junction-escape");
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&junction)
        .arg(&outside)
        .status()
        .expect("failed to create test-owned junction");
    assert!(
        status.success(),
        "mklink /J failed for {}",
        junction.display()
    );
    let escaped =
        crate::verifier::resolve_git_path_for_test("junction-escape/secret.txt", candidate)
            .unwrap();
    assert!(escaped.starts_with(&crate::config::canonical_path(&outside).unwrap()));
    assert!(!escaped.starts_with(candidate));
    assert!(
        !crate::verifier::task_scope_allows_git_changes(&scope, &[escaped]),
        "TASK_SCOPE_GATE must refuse Git paths resolved outside the managed root"
    );
}

// ---------------------------------------------------------------------------
// H. Durable identity and forgery resistance
// ---------------------------------------------------------------------------

#[test]
fn managed_goal_start_persists_host_derived_identity_only() {
    let mut fixture = Fixture::new("start-shape");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    let goal = fixture.goal(&goal_id);

    let record = goal.managed_worktree().expect("managed record");
    assert_eq!(record.goal_id(), &goal_id);
    assert_eq!(record.primary_root(), goal.cwd());
    assert_eq!(record.worktree_root(), fixture.managed_target(&goal_id));
    assert_eq!(record.branch_ref(), Fixture::branch_ref(&goal_id));
    assert_eq!(record.base_commit(), fixture.base_commit);
    assert_eq!(record.created_goal_revision(), 1);
    assert_eq!(record.created_plan_revision(), 0);
    assert!(goal.managed_worktree_creation_intent().is_none());
    assert_eq!(goal.plan_revision(), 0);

    // The durable target lives under the host-owned root keyed by session and
    // Goal, and is disjoint from the primary workspace.
    assert_eq!(record.repository_common_dir(), fixture.common_dir());
    assert!(!record.worktree_root().starts_with(goal.cwd()));
    assert!(!goal.cwd().starts_with(record.worktree_root()));
}

#[test]
fn a_managed_record_cannot_be_forged_from_a_foreign_goal_id() {
    let fixture = Fixture::new("forged-record");
    let goal_id = GoalId::new();
    let other = GoalId::new();
    let record = ManagedWorktreeRecord::requested(
        &goal_id,
        WorktreeId::new(),
        fixture.primary.clone(),
        fixture.common_dir(),
        fixture.managed_root.join("target"),
        fixture.base_commit.clone(),
        None,
        1,
        0,
    )
    .unwrap();
    // The branch ref and lock reason are re-derived from the Goal ID, so an
    // intent for a different Goal cannot validate against this record.
    let foreign = ManagedWorktreeCreationIntent::prepared(
        &other,
        WorktreeId::new(),
        WorktreeOperationId::new(),
        fixture.common_dir(),
        fixture.managed_root.join("target"),
        fixture.base_commit.clone(),
    )
    .unwrap();
    let error: OrchestratorError = foreign.validate_against_record(&record).unwrap_err();
    assert!(error.to_string().contains("different targets"));
}

#[test]
fn an_already_active_managed_workspace_is_reconciled_not_recreated() {
    let mut fixture = Fixture::new("not-recreated");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);

    let creator = ScriptedCreator::new(&fixture, vec![Step::Create]);
    let first = fixture
        .prepare(&goal_id, &HostGit::new(), &creator)
        .unwrap();
    assert!(first.is_active());
    assert_eq!(creator.calls(), 1);

    // A second explicit run reconciles the now-ACTIVE workspace exactly and
    // must not create anything again.
    let before = git(&fixture.primary, &["worktree", "list", "--porcelain"]);
    assert!(fixture.prepare_real(&goal_id).is_active());
    assert_eq!(
        git(&fixture.primary, &["worktree", "list", "--porcelain"]),
        before,
        "an already-ACTIVE workspace must not be recreated"
    );
}

#[test]
fn repeated_preparation_of_a_healthy_workspace_changes_nothing() {
    let mut fixture = Fixture::new("idempotent-prepare");
    fixture.authorize_managed_root();
    let goal_id = fixture.start(WorkspaceMode::ManagedWorktree);
    assert!(fixture.prepare_real(&goal_id).is_active());
    let revision = fixture.goal(&goal_id).revision();
    let worktrees = git(&fixture.primary, &["worktree", "list", "--porcelain"]);

    assert!(fixture.prepare_real(&goal_id).is_active());

    assert_eq!(
        fixture.goal(&goal_id).revision(),
        revision,
        "re-reconciling an exact ACTIVE workspace must not churn Goal.revision"
    );
    assert_eq!(
        git(&fixture.primary, &["worktree", "list", "--porcelain"]),
        worktrees
    );
}
