//! Managed Worktrees V1 — host Git worktree creation authority (Phase 3).
//!
//! Frozen contract: `docs/MANAGED_WORKTREES_V1_DESIGN.md` sections 2, 4, 10, 11,
//! 12, and 22.
//!
//! This is the **only** module in Managed Worktrees V1 that mutates Git, and it
//! performs exactly one mutation: the host-owned `git worktree add` that creates
//! one linked worktree for one Goal. It is deliberately separate from the Phase 2
//! `ReadOnlyGit` seam:
//!
//! - `assert_read_only` is untouched, and `worktree add` is **not** added to the
//!   Phase 2 read-only allowlist;
//! - there is no generic "run git" API here. The mutation entry point takes a
//!   typed [`ManagedWorktreeCreation`], which is constructible only from a
//!   durable `PREPARED` [`ManagedWorktreeCreationIntent`], so an arbitrary argv,
//!   caller-chosen path, caller-chosen branch, and caller-chosen commit-ish are
//!   not expressible;
//! - the argv is additionally asserted against the exact frozen shape before it
//!   is spawned, so a future edit to the builder cannot silently widen it.
//!
//! Nothing here decides *whether* to create. The eligibility gate, the Session
//! path-authority gate, and post-command reconciliation live in
//! `managed_worktree_prepare`, and both are host-owned decisions that this
//! module cannot influence.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::managed_worktree::{
    ManagedWorktreeCreationIntent, managed_branch_name, managed_lock_reason,
};
use crate::managed_worktree_observe::is_git_environment_variable;

/// Git global options applied to every managed creation command.
///
/// - `core.fsmonitor=false` mirrors the Phase 2 observer so a configured
///   filesystem monitor cannot influence creation;
/// - `core.hooksPath=` **disables hooks**. `git worktree add` performs a
///   checkout and therefore runs `post-checkout`; a repository-local hook is
///   untrusted executable content that must not be reachable from a
///   host-internal lifecycle mutation, so the hook path is neutralized rather
///   than inherited;
/// - `--no-optional-locks` matches the observer, so creation does not take the
///   optional index lock and cannot be confused with writer activity.
///
/// Inherited `GIT_*` environment variables are removed exactly as the Phase 2
/// observer removes them, so a traced, aliased, or otherwise redirected Git
/// process cannot widen this into different authority.
const CREATION_GIT_OPTIONS: [&str; 5] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=",
    "--no-optional-locks",
];

/// The single authorized creation operation's outcome.
///
/// A non-zero exit is deliberately **not** an error. Design section 11 forbids
/// treating the process exit as proof either way, so this type only records what
/// the host observed about the process; whether a side effect happened is
/// decided afterwards by read-only reconciliation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedWorktreeCreationOutcome {
    /// `None` when the process was terminated by a signal or never reported an
    /// exit code, which is the "crash/timeout" case that still requires
    /// reconciliation.
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: String,
}

/// The only failure this seam can produce. Every other condition is an outcome to
/// reconcile, not an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ManagedWorktreeCreationError {
    /// The host Git executable could not be started, so no command ran. This is
    /// still reconciled before any retry: no start is not proof of no side
    /// effect.
    Spawn { detail: String },
}

impl std::fmt::Display for ManagedWorktreeCreationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn { detail } => write!(f, "git worktree add could not start: {detail}"),
        }
    }
}

impl std::error::Error for ManagedWorktreeCreationError {}

/// The exact, typed `git worktree add` request for one Goal.
///
/// Every field is host-derived. The path, branch, base commit, and lock reason
/// all come from a durable `PREPARED` intent whose identity values were
/// mechanically re-derived from the Goal ID, so no caller and no model can
/// choose them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedWorktreeCreation {
    worktree_root: PathBuf,
    /// The `-b` argument: the branch **name**, i.e. the durable
    /// `refs/heads/local-mcp/goal/<full-goal-uuid>` ref without its
    /// `refs/heads/` prefix.
    branch_name: String,
    base_commit: String,
    lock_reason: String,
}

impl ManagedWorktreeCreation {
    /// Build the creation request from the durable intent persisted before Git.
    pub(crate) fn from_intent(
        intent: &ManagedWorktreeCreationIntent,
    ) -> Result<Self, crate::orchestrator_error::OrchestratorError> {
        intent.validate()?;
        let creation = Self {
            worktree_root: intent.worktree_root().to_path_buf(),
            branch_name: managed_branch_name(intent.branch_ref())?,
            base_commit: intent.base_commit().to_owned(),
            lock_reason: managed_lock_reason(intent.goal_id()),
        };
        creation.assert_exact_shape()?;
        Ok(creation)
    }

    pub(crate) fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    pub(crate) fn branch_name(&self) -> &str {
        &self.branch_name
    }

    pub(crate) fn base_commit(&self) -> &str {
        &self.base_commit
    }

    pub(crate) fn lock_reason(&self) -> &str {
        &self.lock_reason
    }

    /// The exact argv Git is invoked with, excluding the executable.
    ///
    /// ```text
    /// -c core.fsmonitor=false -c core.hooksPath= --no-optional-locks
    /// worktree add --lock --reason <reason> -b <branch-name> <root> <base-commit>
    /// ```
    ///
    /// There is deliberately no `-B`, no `--force`, and no branch or commit
    /// guessing: Git must create the branch or fail.
    pub(crate) fn argv(&self) -> Vec<OsString> {
        let mut argv: Vec<OsString> = CREATION_GIT_OPTIONS
            .iter()
            .map(OsString::from)
            .collect::<Vec<_>>();
        argv.extend([
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("--lock"),
            OsString::from("--reason"),
            OsString::from(self.lock_reason()),
            OsString::from("-b"),
            OsString::from(self.branch_name()),
            self.worktree_root().to_path_buf().into_os_string(),
            OsString::from(self.base_commit()),
        ]);
        argv
    }

    /// Refuse to spawn anything that is not the exact frozen command shape.
    ///
    /// This is a self-check on the builder above, not an authority check: it
    /// exists so that widening the argv in this module fails loudly here instead
    /// of silently granting Git more capability. Because the tail is compared
    /// position by position against the frozen shape, `-B`, `--force`, and any
    /// other extra flag are structurally impossible.
    fn assert_exact_shape(&self) -> Result<(), crate::orchestrator_error::OrchestratorError> {
        use crate::orchestrator_error::OrchestratorError;

        let argv = self.argv();
        let expected_tail: Vec<OsString> = vec![
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("--lock"),
            OsString::from("--reason"),
            OsString::from(&self.lock_reason),
            OsString::from("-b"),
            OsString::from(&self.branch_name),
            self.worktree_root.clone().into_os_string(),
            OsString::from(&self.base_commit),
        ];
        if argv[CREATION_GIT_OPTIONS.len()..] != expected_tail[..] {
            return Err(OrchestratorError::CorruptGoal(
                "managed worktree creation argv does not match the frozen command shape".to_owned(),
            ));
        }
        Ok(())
    }
}

/// The single mutating Git operation Managed Worktrees V1 authorizes.
pub(crate) trait ManagedWorktreeCreator {
    /// Create the one linked worktree described by `creation`, running Git with
    /// `primary_root` as the working directory.
    ///
    /// The return value is an observation, not a verdict. Callers must reconcile
    /// read-only before concluding anything about side effects.
    fn create(
        &self,
        creation: &ManagedWorktreeCreation,
        primary_root: &Path,
    ) -> Result<ManagedWorktreeCreationOutcome, ManagedWorktreeCreationError>;
}

/// A creator backed by the host Git executable.
pub(crate) struct HostWorktreeCreator {
    executable: PathBuf,
}

impl HostWorktreeCreator {
    pub(crate) fn new() -> Self {
        Self {
            executable: PathBuf::from("git"),
        }
    }
}

impl Default for HostWorktreeCreator {
    fn default() -> Self {
        Self::new()
    }
}

impl ManagedWorktreeCreator for HostWorktreeCreator {
    fn create(
        &self,
        creation: &ManagedWorktreeCreation,
        primary_root: &Path,
    ) -> Result<ManagedWorktreeCreationOutcome, ManagedWorktreeCreationError> {
        let mut command = Command::new(&self.executable);
        for (key, _) in std::env::vars_os() {
            if is_git_environment_variable(&key) {
                command.env_remove(key);
            }
        }
        let output = command
            .args(creation.argv())
            .current_dir(primary_root)
            .output()
            .map_err(|error| ManagedWorktreeCreationError::Spawn {
                detail: error.to_string(),
            })?;
        Ok(ManagedWorktreeCreationOutcome {
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
