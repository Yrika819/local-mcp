//! Managed Worktrees V1 — Phase 2 read-only observation seam.
//!
//! This module is the only part of Phase 2 that starts Git or inspects a path.
//! Command and path observations fail closed; parsing and classification remain
//! separately testable and perform no I/O.

// Phase 2 discovery is not wired to Goal lifecycle code. Keep the frozen
// observation model available for later phases without dead-code noise.
#![expect(
    dead_code,
    reason = "Managed Worktrees Phase 2 observation is not reachable from production until Phase 3 creation authority is authorized."
)]

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::managed_worktree_discovery::{
    ALL_IN_PROGRESS_OPERATIONS, DiscoveryError, InProgressOperation, PathObservation,
    PrimaryWorkspaceStatus, RepositoryObservation, WorktreeInventory,
    parse_worktree_list_porcelain_z, same_path_identity,
};

/// Git subcommands that have an exact read-only argument form below.
pub(crate) const READ_ONLY_GIT_SUBCOMMANDS: [&str; 5] = [
    "rev-parse",
    "symbolic-ref",
    "show-ref",
    "status",
    "worktree",
];

/// The only read-only `git worktree` verb exposed by this seam.
pub(crate) const READ_ONLY_WORKTREE_VERBS: [&str; 1] = ["list"];

const MAX_SEQUENCER_TODO_BYTES: u64 = 1024 * 1024;

const PATH_MARKERS: [&str; 9] = [
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_START",
    "rebase-merge",
    "rebase-apply",
    "rebase-apply/applying",
    "sequencer/todo",
    "BISECT_LOG",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitCommandOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: String,
    pub(crate) exit_code: i32,
}

/// An injectable seam for read-only Git observation.
pub(crate) trait ReadOnlyGit {
    fn run(&self, args: &[&str], cwd: &Path) -> Result<GitCommandOutput, DiscoveryError>;

    fn ref_exists(&self, ref_name: &str, cwd: &Path) -> Result<bool, DiscoveryError>;
}

/// Refuse every invocation except the exact read-only forms used below.
pub(crate) fn assert_read_only(args: &[&str]) -> Result<(), DiscoveryError> {
    let valid = match args {
        ["worktree", "list", "--porcelain", "-z"] => true,
        ["rev-parse", "--show-toplevel"] => true,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"] => true,
        ["rev-parse", "--git-path", marker] => PATH_MARKERS.contains(marker),
        ["symbolic-ref", "-q", "HEAD"] => true,
        ["status", "--porcelain=v1", "-z", "--untracked-files=all"] => true,
        ["show-ref", "--verify", "--quiet", "--", ref_name] => {
            ref_name.starts_with("refs/heads/local-mcp/goal/")
                && ref_name.len() > "refs/heads/local-mcp/goal/".len()
                && ref_name.bytes().all(|byte| byte.is_ascii_graphic())
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(DiscoveryError::ObservationUnavailable(format!(
            "git invocation is not on the exact read-only allowlist: {args:?}"
        )))
    }
}

/// A read-only Git observer backed by the host executable.
pub(crate) struct HostGit {
    executable: PathBuf,
}

impl HostGit {
    pub(crate) fn new() -> Self {
        Self {
            executable: PathBuf::from("git"),
        }
    }
}

impl Default for HostGit {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadOnlyGit for HostGit {
    fn run(&self, args: &[&str], cwd: &Path) -> Result<GitCommandOutput, DiscoveryError> {
        assert_read_only(args)?;
        let mut command = Command::new(&self.executable);
        for (key, _) in std::env::vars_os() {
            if is_git_environment_variable(&key) {
                command.env_remove(key);
            }
        }
        let output = command
            .args(["-c", "core.fsmonitor=false", "--no-optional-locks"])
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|error| DiscoveryError::GitCommand {
                command: args.join(" "),
                detail: error.to_string(),
            })?;
        Ok(GitCommandOutput {
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }

    fn ref_exists(&self, ref_name: &str, cwd: &Path) -> Result<bool, DiscoveryError> {
        let output = self.run(&["show-ref", "--verify", "--quiet", "--", ref_name], cwd)?;
        match output.exit_code {
            0 => Ok(true),
            1 => Ok(false),
            code => Err(command_error(
                "show-ref --verify --quiet",
                code,
                &output.stderr,
            )),
        }
    }
}

pub(crate) fn is_git_environment_variable(key: &OsStr) -> bool {
    key.to_str()
        .and_then(|name| name.get(..4))
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("GIT_"))
}

fn command_error(command: &str, exit_code: i32, stderr: &str) -> DiscoveryError {
    DiscoveryError::GitCommand {
        command: command.to_owned(),
        detail: if stderr.trim().is_empty() {
            format!("exit status {exit_code}")
        } else {
            stderr.trim().to_owned()
        },
    }
}

fn require_success(command: &str, output: &GitCommandOutput) -> Result<(), DiscoveryError> {
    if output.exit_code == 0 {
        Ok(())
    } else {
        Err(command_error(command, output.exit_code, &output.stderr))
    }
}

/// Remove exactly Git's line terminator, not whitespace that may belong to a
/// valid path (including a path ending in spaces or newlines).
fn output_line<'a>(command: &str, output: &'a GitCommandOutput) -> Result<&'a str, DiscoveryError> {
    let bytes = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
    std::str::from_utf8(bytes).map_err(|error| {
        DiscoveryError::ObservationUnavailable(format!(
            "{command} output is not valid UTF-8: {error}"
        ))
    })
}

fn canonical_git_path(command: &str, output: &GitCommandOutput) -> Result<PathBuf, DiscoveryError> {
    require_success(command, output)?;
    let value = output_line(command, output)?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(DiscoveryError::ObservationUnavailable(format!(
            "{command} did not return an absolute path"
        )));
    }
    // `fs::canonicalize` is kept verbatim here on purpose: the durable
    // `primary_root` must stay byte-equal to the session's `cwd`, which
    // `create_session` also produced with `fs::canonicalize`. Only paths handed
    // to a child process are de-verbatim.
    fs::canonicalize(&path).map_err(|error| {
        DiscoveryError::ObservationUnavailable(format!(
            "{command} path {} could not be canonicalized: {error}",
            path.display()
        ))
    })
}

/// Observe repository identity, Git lifecycle markers, status, refs, and exact
/// path presence without changing the index, refs, worktrees, or filesystem.
pub(crate) fn observe_repository(
    git: &dyn ReadOnlyGit,
    cwd: &Path,
    refs_of_interest: &[String],
    paths_of_interest: &[&Path],
) -> Result<RepositoryObservation, DiscoveryError> {
    let top_level = canonical_git_path(
        "rev-parse --show-toplevel",
        &git.run(&["rev-parse", "--show-toplevel"], cwd)?,
    )?;
    let common_dir = canonical_git_path(
        "rev-parse --path-format=absolute --git-common-dir",
        &git.run(
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            cwd,
        )?,
    )?;

    let list = git.run(&["worktree", "list", "--porcelain", "-z"], cwd)?;
    require_success("worktree list --porcelain -z", &list)?;
    let list_text = std::str::from_utf8(&list.stdout).map_err(|error| {
        DiscoveryError::ObservationUnavailable(format!(
            "worktree porcelain contains a non-UTF-8 path or attribute: {error}"
        ))
    })?;
    let inventory: WorktreeInventory = parse_worktree_list_porcelain_z(list_text)?;

    let primary_entries = inventory.at_path(&top_level);
    if primary_entries.len() != 1 {
        return Err(DiscoveryError::ObservationUnavailable(
            "worktree inventory does not register the primary top-level exactly once".to_owned(),
        ));
    }
    let primary_entry = primary_entries[0];
    let head = primary_entry.resolved_head().map(str::to_owned);
    let head_ref = primary_entry.branch_ref().map(str::to_owned);

    let in_progress = observe_in_progress(git, cwd)?;

    let status_output = git.run(
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        &top_level,
    )?;
    require_success(
        "status --porcelain=v1 -z --untracked-files=all",
        &status_output,
    )?;
    let status = parse_primary_status(&status_output.stdout)?;

    let mut observed_refs = BTreeMap::new();
    for ref_name in refs_of_interest {
        observed_refs.insert(ref_name.clone(), git.ref_exists(ref_name, cwd)?);
    }

    let mut paths: Vec<PathBuf> = Vec::new();
    for path in inventory
        .entries()
        .iter()
        .map(|entry| entry.path())
        .chain(paths_of_interest.iter().copied())
    {
        if !paths
            .iter()
            .any(|observed| same_path_identity(observed, path))
        {
            paths.push(path.to_path_buf());
        }
    }
    let path_observations = paths
        .into_iter()
        .map(|path| {
            let state = observe_path(&path);
            (path, state)
        })
        .collect();

    Ok(RepositoryObservation::new(
        top_level,
        common_dir,
        head,
        head_ref,
        inventory,
        in_progress,
        status,
        path_observations,
        observed_refs,
    ))
}

fn marker_path(git: &dyn ReadOnlyGit, cwd: &Path, name: &str) -> Result<PathBuf, DiscoveryError> {
    let output = git.run(&["rev-parse", "--git-path", name], cwd)?;
    require_success("rev-parse --git-path", &output)?;
    let value = output_line("rev-parse --git-path", &output)?;
    if value.is_empty() {
        return Err(DiscoveryError::ObservationUnavailable(format!(
            "git returned an empty path for marker {name:?}"
        )));
    }
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

fn marker_exists(git: &dyn ReadOnlyGit, cwd: &Path, name: &str) -> Result<bool, DiscoveryError> {
    let path = marker_path(git, cwd, name)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(DiscoveryError::ObservationUnavailable(format!(
            "Git operation marker {} could not be inspected: {error}",
            path.display()
        ))),
    }
}

fn observe_in_progress(
    git: &dyn ReadOnlyGit,
    cwd: &Path,
) -> Result<Vec<InProgressOperation>, DiscoveryError> {
    let mut operations = Vec::with_capacity(ALL_IN_PROGRESS_OPERATIONS.len());

    if marker_exists(git, cwd, "MERGE_HEAD")? {
        operations.push(InProgressOperation::Merge);
    }
    if marker_exists(git, cwd, "rebase-merge")? {
        operations.push(InProgressOperation::Rebase);
    }
    let rebase_apply = marker_path(git, cwd, "rebase-apply")?;
    match fs::symlink_metadata(&rebase_apply) {
        Ok(_) => {
            if marker_exists(git, cwd, "rebase-apply/applying")? {
                return Err(DiscoveryError::ObservationUnavailable(
                    "git am is in progress; managed-worktree eligibility cannot proceed".to_owned(),
                ));
            }
            operations.push(InProgressOperation::Rebase);
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(DiscoveryError::ObservationUnavailable(format!(
                "Git rebase marker {} could not be inspected: {error}",
                rebase_apply.display()
            )));
        }
    }
    if marker_exists(git, cwd, "CHERRY_PICK_HEAD")? {
        operations.push(InProgressOperation::CherryPick);
    }
    if marker_exists(git, cwd, "REVERT_HEAD")? {
        operations.push(InProgressOperation::Revert);
    }
    if marker_exists(git, cwd, "BISECT_START")? || marker_exists(git, cwd, "BISECT_LOG")? {
        operations.push(InProgressOperation::Bisect);
    }

    let todo = marker_path(git, cwd, "sequencer/todo")?;
    match fs::symlink_metadata(&todo) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.len() > MAX_SEQUENCER_TODO_BYTES {
                return Err(DiscoveryError::ObservationUnavailable(format!(
                    "Git sequencer state {} is not a bounded regular file",
                    todo.display()
                )));
            }
            let file = fs::File::open(&todo).map_err(|error| {
                DiscoveryError::ObservationUnavailable(format!(
                    "Git sequencer state {} could not be opened: {error}",
                    todo.display()
                ))
            })?;
            let mut bytes = Vec::new();
            file.take(MAX_SEQUENCER_TODO_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| {
                    DiscoveryError::ObservationUnavailable(format!(
                        "Git sequencer state {} could not be read: {error}",
                        todo.display()
                    ))
                })?;
            if bytes.len() as u64 > MAX_SEQUENCER_TODO_BYTES {
                return Err(DiscoveryError::ObservationUnavailable(format!(
                    "Git sequencer state {} exceeds the size limit",
                    todo.display()
                )));
            }
            let first = bytes
                .split(|byte| *byte == b'\n')
                .map(|line| line.split(|byte| *byte == b' ').next().unwrap_or_default())
                .find(|line| !line.is_empty() && !line.starts_with(b"#"));
            match first {
                Some(b"pick") if !operations.contains(&InProgressOperation::CherryPick) => {
                    operations.push(InProgressOperation::CherryPick);
                }
                Some(b"revert") if !operations.contains(&InProgressOperation::Revert) => {
                    operations.push(InProgressOperation::Revert);
                }
                Some(b"pick" | b"revert") => {}
                None => {
                    return Err(DiscoveryError::ObservationUnavailable(
                        "sequencer todo exists but has no recognizable operation".to_owned(),
                    ));
                }
                Some(_) => {
                    return Err(DiscoveryError::ObservationUnavailable(
                        "sequencer todo contains an unsupported operation".to_owned(),
                    ));
                }
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(DiscoveryError::ObservationUnavailable(format!(
                "Git sequencer state {} could not be inspected: {error}",
                todo.display()
            )));
        }
    }

    operations.sort_unstable();
    operations.dedup();
    Ok(operations)
}

fn observe_path(path: &Path) -> PathObservation {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            match crate::config::canonical_path(path) {
                Ok(canonical) if same_path_identity(&canonical, path) => PathObservation::Directory,
                _ => PathObservation::Occupied,
            }
        }
        Ok(_) => PathObservation::Occupied,
        Err(error) if error.kind() == ErrorKind::NotFound => PathObservation::Missing,
        Err(_) => PathObservation::Unknown,
    }
}

/// Classify `git status --porcelain=v1 -z --untracked-files=all` output. Paths
/// remain arbitrary bytes; only the fixed XY prefix is interpreted.
fn parse_primary_status(stdout: &[u8]) -> Result<PrimaryWorkspaceStatus, DiscoveryError> {
    let mut status = PrimaryWorkspaceStatus::default();
    if stdout.is_empty() {
        return Ok(status);
    }
    let Some(payload) = stdout.strip_suffix(&[0]) else {
        return Err(DiscoveryError::ObservationUnavailable(
            "git status returned an unterminated NUL record".to_owned(),
        ));
    };
    let mut records = payload.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            return Err(DiscoveryError::ObservationUnavailable(
                "git status returned an empty porcelain record".to_owned(),
            ));
        }
        if record.len() < 3 || record[2] != b' ' {
            return Err(DiscoveryError::ObservationUnavailable(
                "git status returned malformed porcelain output".to_owned(),
            ));
        }
        let (index, worktree) = (record[0], record[1]);
        let ordinary_status = |status| {
            matches!(
                status,
                b' ' | b'M' | b'T' | b'A' | b'D' | b'R' | b'C' | b'U'
            )
        };
        let valid_normal_pair = ordinary_status(index)
            && ordinary_status(worktree)
            && (index != b' ' || worktree != b' ');
        let valid_special_pair =
            (index == b'?' && worktree == b'?') || (index == b'!' && worktree == b'!');
        if !valid_normal_pair && !valid_special_pair {
            return Err(DiscoveryError::ObservationUnavailable(
                "git status returned invalid XY status bytes".to_owned(),
            ));
        }
        if valid_special_pair {
            status.untracked = true;
        } else {
            if index != b' ' {
                status.staged = true;
            }
            if worktree != b' ' {
                status.tracked_dirty = true;
            }
        }
        if matches!(index, b'R' | b'C') || matches!(worktree, b'R' | b'C') {
            match records.next() {
                Some(path) if !path.is_empty() => {}
                _ => {
                    return Err(DiscoveryError::ObservationUnavailable(
                        "git status rename/copy record omitted its second path".to_owned(),
                    ));
                }
            }
        }
    }
    Ok(status)
}
