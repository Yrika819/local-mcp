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
use std::sync::Mutex;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::config;
use crate::goal::{Goal, GoalId};
use crate::goal_api::{self, ManagedRootSource};
use crate::managed_worktree::{
    MANAGED_BRANCH_REF_PREFIX, ManagedWorktreeCreationIntent, ManagedWorktreeLifecycle,
    ManagedWorktreeRecord, WorkspaceMode, WorktreeId, WorktreeOperationId,
};
use crate::managed_worktree_create::{
    HostWorktreeCreator, ManagedWorktreeCreation, ManagedWorktreeCreationError,
    ManagedWorktreeCreationOutcome, ManagedWorktreeCreator,
};
use crate::managed_worktree_discovery::DiscoveryError;
use crate::managed_worktree_observe::{GitCommandOutput, HostGit, ReadOnlyGit, assert_read_only};
use crate::managed_worktree_prepare::{
    ManagedWorkspaceError, ManagedWorkspacePreparation, canonical_managed_target,
    prepare_managed_workspace, request_managed_workspace,
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
    primary: PathBuf,
    managed_root: PathBuf,
    state_root: PathBuf,
    session: config::Session,
    store: TaskStore,
    base_commit: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("local-mcp-mw-p3-{name}-{}", Uuid::new_v4()));
        let primary = root.join("primary");
        let managed_root = root.join("managed");
        let state_root = root.join("state");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&managed_root).unwrap();
        std::fs::create_dir_all(&state_root).unwrap();

        git(&primary, &["init", "-q", "."]);
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
            // `create_session` always canonicalizes `cwd`, and managed mode
            // requires the durable `primary_root` to equal the durable
            // `Goal.cwd`. A temp root that is not already canonical (a
            // `/var/...` temp dir behind a symlink, for example) would make
            // every managed fixture fail closed for the wrong reason.
            cwd: std::fs::canonicalize(&primary).expect("primary is canonical"),
            permitted_directories: vec![
                std::fs::canonicalize(&primary).expect("primary is canonical"),
            ],
        };
        let store = TaskStore::with_state_root(state_root.clone());
        Self {
            root,
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
    ) -> Result<ManagedWorkspacePreparation, ManagedWorkspaceError> {
        prepare_managed_workspace(&self.store, &self.session, goal_id, git, creator)
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
        PathBuf::from(git(
            &self.primary,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ))
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
fn a_non_canonical_session_cwd_cannot_host_a_managed_workspace() {
    // Production sessions always canonicalize their cwd. A non-canonical cwd
    // would make the host-derived `primary_root` differ from the durable
    // `Goal.cwd`, which managed mode must reject rather than silently repair.
    let mut fixture = Fixture::new("noncanonical-cwd");
    fixture.authorize_managed_root();
    fixture.session.cwd = noncanonical(&fixture.primary);
    let error = fixture
        .start_value(&json!({
            "session_id": fixture.session.id,
            "objective": "managed worktree objective",
            "workspace_mode": "MANAGED_WORKTREE"
        }))
        .expect_err("a non-canonical session cwd is refused");
    assert_eq!(error.code(), "MANAGED_WORKSPACE_UNAVAILABLE");
}

fn noncanonical(path: &Path) -> PathBuf {
    let mut result = PathBuf::from("/tmp");
    let canonical = std::fs::canonicalize(path).unwrap();
    for component in canonical.components().skip(1) {
        result.push(component);
    }
    // A `.` component is dropped by Path::components(), so the raw bytes differ
    // from the canonical form while resolving to the same location.
    result.join(".")
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
            .starts_with(&fixture.managed_root)
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
    assert_eq!(after.revision(), revision_before + 2);
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
    assert_eq!(after.revision(), revision_before + 2);
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
        crate::managed_worktree_prepare::MAX_CREATION_ATTEMPTS,
        "a start failure is reconciled, not retried without proof"
    );
    assert!(
        block.detail.contains("could not run"),
        "Git's own failure must reach the evidence: {}",
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
    // No destructive repair happened: the registration is still there.
    let porcelain = git(&fixture.primary, &["worktree", "list", "--porcelain"]);
    assert!(porcelain.contains(&target.display().to_string()));
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

    // The managed branch is already checked out in a different worktree.
    let elsewhere = fixture.managed_root.join("someone-elses-worktree");
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

    // ACTIVE is still refused: Phase 3 does not route the execution root.
    assert!(fixture.prepare_real(&goal_id).is_active());
    let goal = fixture.goal(&goal_id);
    assert_eq!(
        goal.managed_worktree().unwrap().lifecycle(),
        ManagedWorktreeLifecycle::Active
    );
    assert!(goal.execution_root().is_some());
    assert!(matches!(
        planner::planner_request_for_goal(&goal, &session),
        Err(PlannerError::PlanAuthorityViolation(_))
    ));
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
fn primary_planner_behavior_is_unchanged() {
    let fixture = Fixture::new("planner-primary");
    let goal_id = fixture.start(WorkspaceMode::Primary);
    let goal = fixture.goal(&goal_id);
    let request = planner::planner_request_for_goal(&goal, &fixture.session).unwrap();
    assert_eq!(request.cwd(), goal.cwd());
    assert_eq!(goal.execution_root(), Some(goal.cwd()));
    assert!(planner::ensure_workspace_is_plannable(&goal).is_ok());
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
