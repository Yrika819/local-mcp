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
use std::time::Duration;

use crate::managed_worktree::{
    ManagedWorktreeCreationIntent, managed_branch_name, managed_lock_reason,
};
use crate::managed_worktree_discovery::same_path_identity;

/// The exact frozen global-option head.
///
/// Kept as a literal so the runtime self-check below can compare against it
/// position by position. A future edit that widens this list is a visible diff
/// *and* a self-check failure.
/// Deadline for one `git worktree add` invocation.
///
/// This is a local, non-networked operation on an already-reconciled repository.
/// The bound exists so a wedged Git cannot hold a runtime thread forever and
/// strand the durable creation sequence before reconciliation ever runs; it is
/// not a statement that the operation is expected to be slow.
const MANAGED_CREATION_TIMEOUT: Duration = Duration::from_secs(120);
const MANAGED_FILTER_QUERY_TIMEOUT: Duration = Duration::from_secs(15);

const FROZEN_GIT_OPTION_HEAD: [&str; 5] = [
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
    /// The host Git executable identity could not be established, so no command
    /// was run at all.
    ExecutableIdentity(String),
}

impl std::fmt::Display for ManagedWorktreeCreationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn { detail } => write!(f, "git worktree add could not start: {detail}"),
            Self::ExecutableIdentity(detail) => {
                write!(
                    f,
                    "git executable identity could not be established: {detail}"
                )
            }
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
        let mut argv: Vec<OsString> = FROZEN_GIT_OPTION_HEAD
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
    /// This is a self-check on the builder above, not an authority check. Its
    /// real value is structural: `expected` is built position by position, so an
    /// inserted, removed, or reordered element in the argument tail is caught
    /// here, before any spawn. That is what makes `-B`, `--force`, and any other
    /// extra flag impossible to express.
    ///
    /// It deliberately does **not** pretend to police the global-option head.
    /// Both sides of that comparison come from the same constant, so widening
    /// `FROZEN_GIT_OPTION_HEAD` would move both and still pass; claiming
    /// otherwise would be a false guarantee. The head is pinned instead by a
    /// literal expected-argv test, which a widening commit has to edit visibly.
    fn assert_exact_shape(&self) -> Result<(), crate::orchestrator_error::OrchestratorError> {
        use crate::orchestrator_error::OrchestratorError;

        let argv = self.argv();
        let expected: Vec<OsString> = FROZEN_GIT_OPTION_HEAD
            .iter()
            .map(OsString::from)
            .chain([
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--lock"),
                OsString::from("--reason"),
                OsString::from(self.lock_reason()),
                OsString::from("-b"),
                OsString::from(self.branch_name()),
                self.worktree_root().to_path_buf().into_os_string(),
                OsString::from(self.base_commit()),
            ])
            .collect();
        if argv != expected {
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

/// The host Git executable, resolved and validated once.
///
/// This reuses the same resolver the `git_stage_paths` mutation uses, so the
/// mutating worktree-creation seam runs the *same* host Git identity: the
/// platform-correct executable file names (including `git.exe` on Windows), all
/// `PATH` entries validated before any candidate is examined, non-executable
/// candidates skipped rather than treated as fatal, UTF-8 required, and the
/// result canonicalized. Duplicating that logic here is how a mutating seam
/// silently ends up weaker than the existing trusted-Git path.
///
/// A previously resolved identity is cached by the caller, so the executable is
/// established once per process rather than re-resolved per attempt.
fn resolve_host_git() -> Result<PathBuf, ManagedWorktreeCreationError> {
    crate::execution::host_git_path()
        .map_err(|error| ManagedWorktreeCreationError::ExecutableIdentity(format!("{error:#}")))
}

fn reject_repository_filter_drivers(
    executable: &Path,
    primary_root: &Path,
) -> Result<(), ManagedWorktreeCreationError> {
    for scope in ["--local", "--worktree"] {
        let mut command = Command::new(executable);
        command
            .env_clear()
            .envs(crate::sandbox::clean_git_environment())
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=",
                "--no-optional-locks",
                "config",
                scope,
                "--get-regexp",
                "^filter\\.",
            ])
            .current_dir(primary_root);
        let output = crate::process_blocking::run_bounded_blocking(
            &mut command,
            MANAGED_FILTER_QUERY_TIMEOUT,
        )
        .map_err(|error| ManagedWorktreeCreationError::Spawn {
            detail: format!("Git filter configuration query failed: {error}"),
        })?;
        if output.timed_out || output.capture_incomplete || output.output_overflow {
            return Err(ManagedWorktreeCreationError::Spawn {
                detail: "Git filter configuration query was incomplete".to_owned(),
            });
        }
        match output.status.code() {
            Some(0) if !output.stdout.is_empty() => {
                return Err(ManagedWorktreeCreationError::Spawn {
                    detail: "repository has configured Git filter drivers; refusing host checkout execution".to_owned(),
                });
            }
            Some(1) => {}
            status => {
                return Err(ManagedWorktreeCreationError::Spawn {
                    detail: format!("Git filter configuration query returned status {status:?}"),
                });
            }
        }
    }
    Ok(())
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
        // A configured absolute executable is honored only when it resolves to
        // the established host Git identity; anything else falls back to that
        // identity, so this seam never runs an unverified binary.
        let trusted = resolve_host_git()?;
        let executable = if self.executable.is_absolute() {
            match std::fs::canonicalize(&self.executable) {
                Ok(candidate) if same_path_identity(&candidate, &trusted) => candidate,
                _ => trusted,
            }
        } else {
            trusted
        };

        reject_repository_filter_drivers(&executable, primary_root)?;

        let mut command = Command::new(&executable);
        command
            .env_clear()
            .envs(crate::sandbox::clean_git_environment());
        // Bounded and tree-contained rather than a bare blocking `output()`.
        //
        // This is the mutating `git worktree add`, so the deadline must not be
        // mistaken for evidence about side effects. A timeout means Git was
        // started and its outcome is **unknown**: the attempt stays consumed and
        // the sequence blocks for reconciliation, exactly as an ambiguous
        // observation already does. It never becomes "the mutation did not
        // happen", and it never replenishes the retry budget.
        let output = crate::process_blocking::run_bounded_blocking(
            command.args(creation.argv()).current_dir(primary_root),
            MANAGED_CREATION_TIMEOUT,
        )
        .map_err(|error| ManagedWorktreeCreationError::Spawn {
            detail: error.to_string(),
        })?;
        if output.timed_out || output.capture_incomplete || output.output_overflow {
            return Err(ManagedWorktreeCreationError::Spawn {
                detail: format!(
                    "git worktree add did not produce a complete result within {}s or exceeded its output bound; its process tree was terminated and the outcome of this attempt is unknown",
                    MANAGED_CREATION_TIMEOUT.as_secs()
                ),
            });
        }
        Ok(ManagedWorktreeCreationOutcome {
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
