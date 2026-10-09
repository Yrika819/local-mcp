//! Managed Worktrees V1 Phase 5 Slice 2A — Git candidate metadata observer.
//!
//! Bounded, closed, read-only Git metadata observation for a future workspace
//! snapshot. This slice covers **metadata only**:
//!
//! * a closed [`SnapshotGitQuery`] enum with exact read-only argv (no caller,
//!   model, or planner input can extend it);
//! * a hardened host-Git execution envelope (`--no-replace-objects`,
//!   `core.hooksPath=<null>`, `core.fsmonitor=false`, `--no-optional-locks`,
//!   cleared environment);
//! * pure, separately testable parsers for `status --porcelain=v1 -z`,
//!   `ls-files --stage -z`, `ls-files -v -z`, `rev-parse HEAD`, and the two
//!   staged-name queries;
//! * global special-state rejection (gitlink, unmerged, skip-worktree,
//!   assume-unchanged, intent-to-add) over the **whole index**, not just the
//!   snapshot union;
//! * a sorted/canonical [`SnapshotGitMetadata`] result.
//!
//! Deliberately absent (later sub-slices): filesystem content hashing,
//! `cat-file` blob streaming, PRE/MID/POST coherence, retry,
//! `WorkspaceSnapshotManifestV1` assembly, `TaskStore`/Goal/finalizer wiring,
//! and any Git mutation.

#![expect(
    dead_code,
    reason = "Slice 2A freezes the closed metadata seam before later snapshots, coherence, and finalizer consumers are authorized to call it."
)]
//!
//! ## Intent-to-add detection (real-Git finding)
//!
//! `git add -N <path>` (intent-to-add, ITA) is invisible to the obvious
//! signals: `ls-files --stage` reports a stage-0 `100644` entry with the
//! **empty-blob OID** (`e69de29...`), byte-identical to a genuinely staged
//! empty file, and `ls-files -v` reports tag `H`, byte-identical to a normal
//! tracked file. OID-only or tag-only detection would confuse ITA with a
//! legitimate empty file, so 2A detects ITA exactly by comparing two closed
//! staged-name queries over the same index:
//!
//! * `diff --cached --ita-visible-in-index` lists ITA paths;
//! * `diff --cached --ita-invisible-in-index` hides them;
//! * a genuinely staged file (including a genuinely empty one) appears in
//!   **both**.
//!
//! Therefore `visible − invisible ≠ ∅` proves ITA and fails closed with
//! [`SnapshotMetadataError::UnsupportedGitState`]. As defense in depth, a
//! worktree-side `A` in status (`" A <path>"`) is also rejected: `A` is not a
//! documented worktree-column value and empirically marks ITA placeholders.
//!
//! ## Rename suppression
//!
//! Status runs with `--no-renames`, so rename/copy heuristic records never
//! appear; any `R`/`C` status byte is a version surprise and fails closed. Both
//! staged-name queries also carry `--no-renames` so their path sets stay
//! comparable with the status-derived staged set regardless of the repository's
//! `diff.renames` / `status.renames` configuration.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::execution;
use crate::process_blocking;
use crate::sandbox;
use crate::workspace_snapshot::{
    MAX_CHANGED_PATHS, MAX_STAGED_PATHS, MAX_UNTRACKED_PATHS, NormalizedWorkspacePath,
    WorkspaceSnapshotError,
};

/// Deadline for one read-only snapshot Git observation.
///
/// Matches the existing managed-worktree observation bound: generous enough
/// for a large checkout, terminal for a wedged Git.
const SNAPSHOT_GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounded stderr retained for diagnostics (the runner already caps capture).
const MAX_STDERR_EVIDENCE_CHARS: usize = 500;

/// The empty-blob OID. Named for documentation only: 2A never classifies an
/// entry by OID (an ITA placeholder and a genuinely empty file share it).
#[cfg(test)]
pub(crate) const EMPTY_BLOB_SHA1: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";

/// Every Git observation Slice 2A may perform, and nothing else.
///
/// There is deliberately no way to express arbitrary argv: callers select a
/// variant, and the exact tail below is the only thing that can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SnapshotGitQuery {
    /// `rev-parse HEAD` — the observed worktree HEAD (must resolve).
    Head,
    /// Effective repository `core.fileMode` after Git config precedence.
    FileMode,
    /// Machine-readable status with renames disabled and submodules ignored.
    Status,
    /// Full index entries: mode, OID, stage, path.
    IndexStage,
    /// Full index flags: one status tag per path.
    IndexFlags,
    /// Staged names with ITA entries hidden (rename detection off).
    StagedNamesNoIta,
    /// Staged names with ITA entries visible (rename detection off).
    ///
    /// Exists only so the ITA cross-check (`visible − invisible`) is exact.
    /// Never used as a path source on its own.
    StagedNamesVisibleIta,
}

impl SnapshotGitQuery {
    /// The exact Git subcommand tail for this observation.
    pub(crate) fn tail(self) -> &'static [&'static str] {
        match self {
            Self::Head => &["rev-parse", "HEAD"],
            Self::FileMode => &["config", "--bool", "--get", "core.fileMode"],
            Self::Status => &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--no-renames",
                "--ignore-submodules=all",
            ],
            Self::IndexStage => &["ls-files", "--stage", "-z", "--full-name"],
            Self::IndexFlags => &["ls-files", "-v", "-z", "--full-name"],
            Self::StagedNamesNoIta => &[
                "diff",
                "--cached",
                "--name-only",
                "-z",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--ita-invisible-in-index",
                "--ignore-submodules=all",
            ],
            Self::StagedNamesVisibleIta => &[
                "diff",
                "--cached",
                "--name-only",
                "-z",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--ita-visible-in-index",
                "--ignore-submodules=all",
            ],
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Head => "rev-parse HEAD",
            Self::FileMode => "config --bool --get core.fileMode",
            Self::Status => "status --porcelain=v1 -z",
            Self::IndexStage => "ls-files --stage -z",
            Self::IndexFlags => "ls-files -v -z",
            Self::StagedNamesNoIta => "diff --cached --name-only -z (ita-invisible)",
            Self::StagedNamesVisibleIta => "diff --cached --name-only -z (ita-visible)",
        }
    }
}

/// All queries Slice 2A may run, in a fixed order.
pub(crate) const ALL_SNAPSHOT_QUERIES: [SnapshotGitQuery; 7] = [
    SnapshotGitQuery::Head,
    SnapshotGitQuery::FileMode,
    SnapshotGitQuery::Status,
    SnapshotGitQuery::IndexStage,
    SnapshotGitQuery::IndexFlags,
    SnapshotGitQuery::StagedNamesNoIta,
    SnapshotGitQuery::StagedNamesVisibleIta,
];

/// Failure of Git candidate metadata observation.
///
/// Every variant fails closed: an unavailable, ambiguous, or special-state
/// observation is an error, never an empty metadata set that would read as a
/// clean candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotMetadataError {
    /// A read-only Git invocation failed, timed out, or overflowed capture.
    GitCommand { query: &'static str, detail: String },
    /// A required observation was missing, truncated, or contradictory.
    GitObservationUnavailable(String),
    /// The repository carries index/worktree state V1 refuses to snapshot
    /// (gitlink, unmerged, skip-worktree, assume-unchanged, intent-to-add,
    /// rename-heuristic leak). Never silently accepted.
    UnsupportedGitState(String),
    /// A Git-emitted path is not a valid [`NormalizedWorkspacePath`]
    /// (including non-UTF-8 bytes; lossy conversion is forbidden).
    InvalidPath(String),
    /// A Slice 1 collection bound was reached while parsing.
    LimitExceeded(&'static str),
}

impl std::fmt::Display for SnapshotMetadataError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GitCommand { query, detail } => {
                write!(f, "snapshot git observation failed: {query}: {detail}")
            }
            Self::GitObservationUnavailable(reason) => {
                write!(f, "snapshot git observation unavailable: {reason}")
            }
            Self::UnsupportedGitState(reason) => {
                write!(f, "snapshot unsupported git state: {reason}")
            }
            Self::InvalidPath(reason) => write!(f, "snapshot invalid path: {reason}"),
            Self::LimitExceeded(limit) => write!(f, "snapshot {limit} limit exceeded"),
        }
    }
}

impl std::error::Error for SnapshotMetadataError {}

/// Raw output of one closed snapshot Git query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotCommandOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: String,
    pub(crate) exit_code: i32,
}

/// Injectable seam for closed snapshot Git observation.
///
/// The trait takes a [`SnapshotGitQuery`], not argv, so test doubles and
/// future callers cannot smuggle an arbitrary Git invocation through it.
pub(crate) trait SnapshotGit {
    fn run(
        &self,
        query: SnapshotGitQuery,
        cwd: &Path,
    ) -> Result<SnapshotCommandOutput, SnapshotMetadataError>;
}

/// Build the exact host-owned argv for `query`.
///
/// The executable is the host-resolved Git identity; everything after it is
/// fixed by this module. The caller never chooses any element.
pub(crate) fn snapshot_argv(query: SnapshotGitQuery) -> Result<Vec<String>, String> {
    let executable = execution::host_git_path().map_err(|error| error.to_string())?;
    let executable = executable
        .to_str()
        .ok_or_else(|| "host Git path is not valid UTF-8".to_owned())?;
    let mut argv = vec![
        executable.to_owned(),
        // No pager, ever: `--no-pager` disables Git's pager entirely, so
        // unlike overriding `core.pager` with `cat` it cannot spawn any
        // external program even when a repository forces pager use.
        "--no-pager".to_owned(),
        // Replace objects would let repository-controlled refs rewrite the
        // observed OIDs; every snapshot query disables them uniformly.
        "--no-replace-objects".to_owned(),
        // Hooks cannot run for an observation.
        "-c".to_owned(),
        format!("core.hooksPath={}", sandbox::null_device_path()),
        // An external filesystem monitor is a host program the host did not
        // choose; it must not run as a side effect of observation.
        "-c".to_owned(),
        "core.fsmonitor=false".to_owned(),
        // Ordinary status may refresh and write index stat metadata; this
        // observation must not.
        "--no-optional-locks".to_owned(),
    ];
    argv.extend(query.tail().iter().map(|arg| (*arg).to_owned()));
    Ok(argv)
}

/// The process result is a verdict about the repository only if Git
/// delivered its complete stdout/stderr within the capture bounds. This
/// gate runs before any parser: `timed_out`, `capture_incomplete`, or
/// `output_overflow` means the bytes are not a trustworthy result, and a
/// truncated-at-a-NUL-boundary capture must never parse as a shorter but
/// "valid" index.
pub(crate) fn enforce_complete_process_output(
    query: SnapshotGitQuery,
    timed_out: bool,
    capture_incomplete: bool,
    output_overflow: bool,
) -> Result<(), SnapshotMetadataError> {
    if timed_out || capture_incomplete || output_overflow {
        return Err(SnapshotMetadataError::GitCommand {
            query: query.label(),
            detail: format!(
                "git did not produce a complete result within {}s or exceeded its output bound; its process tree was terminated",
                SNAPSHOT_GIT_TIMEOUT.as_secs()
            ),
        });
    }
    Ok(())
}

/// A snapshot Git observer backed by the host executable.
pub(crate) struct HostSnapshotGit;

impl SnapshotGit for HostSnapshotGit {
    fn run(
        &self,
        query: SnapshotGitQuery,
        cwd: &Path,
    ) -> Result<SnapshotCommandOutput, SnapshotMetadataError> {
        let argv = snapshot_argv(query).map_err(|detail| SnapshotMetadataError::GitCommand {
            query: query.label(),
            detail,
        })?;
        let executable = argv[0].clone();
        let mut command = Command::new(executable);
        command
            .env_clear()
            .envs(sandbox::clean_git_environment())
            .args(&argv[1..])
            .current_dir(cwd);
        // Bounded and tree-contained: this seam may run from a runtime thread,
        // so a wedged Git must terminate the attempt instead of holding the
        // thread forever. A deadline is terminal for the attempt and is
        // reported as an unavailable observation, never as a verdict about the
        // repository, and never as proof that Git did not run.
        let output = process_blocking::run_bounded_blocking(&mut command, SNAPSHOT_GIT_TIMEOUT)
            .map_err(|error| SnapshotMetadataError::GitCommand {
                query: query.label(),
                detail: error.to_string(),
            })?;
        enforce_complete_process_output(
            query,
            output.timed_out,
            output.capture_incomplete,
            output.output_overflow,
        )?;
        Ok(SnapshotCommandOutput {
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(MAX_STDERR_EVIDENCE_CHARS)
                .collect(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }
}

/// Index mode semantics for one snapshot index entry (index side only;
/// working-tree executable semantics belong to Slice 2B).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotIndexMode {
    Regular { executable: bool },
    Symlink,
}

/// One validated stage-0 index entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotIndexEntry {
    pub(crate) path: NormalizedWorkspacePath,
    pub(crate) mode: SnapshotIndexMode,
    pub(crate) oid: String,
}

/// Bounded, sorted Git candidate metadata (Slice 2A result).
///
/// Paths are canonical byte-sorted logical paths. `index_entries` carry the
/// staged ∪ changed candidate entries only; the whole-index scan (its special
/// states, flags, and coverage cross-check) runs during observation but is
/// deliberately not retained in the result. Blob content identity (hashing)
/// belongs to Slice 2C, so entries carry the index OID, not a content digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SnapshotGitMetadata {
    pub(crate) head: String,
    pub(crate) file_mode_policy: GitFileModePolicy,
    pub(crate) changed_paths: Vec<NormalizedWorkspacePath>,
    pub(crate) staged_paths: Vec<NormalizedWorkspacePath>,
    pub(crate) untracked_paths: Vec<NormalizedWorkspacePath>,
    pub(crate) index_entries: Vec<SnapshotIndexEntry>,
}

/// Whether Git treats working-tree executable bits as authoritative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GitFileModePolicy {
    TrustExecutableBit,
    IgnoreExecutableBit,
}

/// Parse Git's normalized `config --bool --get core.fileMode` result.
/// Exit 1 with no output means unset; Git's documented default is true.
pub(crate) fn parse_file_mode_policy(
    output: &SnapshotCommandOutput,
) -> Result<GitFileModePolicy, SnapshotMetadataError> {
    match output.exit_code {
        0 => match output.stdout.as_slice() {
            b"true\n" => Ok(GitFileModePolicy::TrustExecutableBit),
            b"false\n" => Ok(GitFileModePolicy::IgnoreExecutableBit),
            _ => Err(SnapshotMetadataError::GitObservationUnavailable(
                "core.fileMode query returned malformed or non-canonical boolean output".to_owned(),
            )),
        },
        1 if output.stdout.is_empty() => Ok(GitFileModePolicy::TrustExecutableBit),
        _ => Err(SnapshotMetadataError::GitObservationUnavailable(format!(
            "core.fileMode query exited {} with unexpected output",
            output.exit_code
        ))),
    }
}

/// Observe closed Git candidate metadata with `cwd`.
///
/// `cwd` must already be a Session-authorized managed worktree root; this
/// slice performs no Session lookup and no authority grant. Every query runs
/// through the closed [`SnapshotGit`] seam, every special index state fails
/// closed, and the two staged-name queries must agree up to the exact ITA
/// difference (which itself fails closed).
pub(crate) fn observe_snapshot_metadata(
    git: &dyn SnapshotGit,
    cwd: &Path,
) -> Result<SnapshotGitMetadata, SnapshotMetadataError> {
    let head_output = git.run(SnapshotGitQuery::Head, cwd)?;
    require_success(SnapshotGitQuery::Head, &head_output)?;
    let head = parse_head(&head_output.stdout)?;

    let file_mode_output = git.run(SnapshotGitQuery::FileMode, cwd)?;
    let file_mode_policy = parse_file_mode_policy(&file_mode_output)?;

    let status_output = git.run(SnapshotGitQuery::Status, cwd)?;
    require_success(SnapshotGitQuery::Status, &status_output)?;
    let status_sets = parse_status(&status_output.stdout)?;

    let stage_output = git.run(SnapshotGitQuery::IndexStage, cwd)?;
    require_success(SnapshotGitQuery::IndexStage, &stage_output)?;
    let index_entries = parse_index_stage(&stage_output.stdout)?;

    let flags_output = git.run(SnapshotGitQuery::IndexFlags, cwd)?;
    require_success(SnapshotGitQuery::IndexFlags, &flags_output)?;
    check_index_flags(&flags_output.stdout, &index_entries)?;

    let invisible_output = git.run(SnapshotGitQuery::StagedNamesNoIta, cwd)?;
    require_success(SnapshotGitQuery::StagedNamesNoIta, &invisible_output)?;
    let invisible =
        parse_nul_name_set(SnapshotGitQuery::StagedNamesNoIta, &invisible_output.stdout)?;

    let visible_output = git.run(SnapshotGitQuery::StagedNamesVisibleIta, cwd)?;
    require_success(SnapshotGitQuery::StagedNamesVisibleIta, &visible_output)?;
    let visible = parse_nul_name_set(
        SnapshotGitQuery::StagedNamesVisibleIta,
        &visible_output.stdout,
    )?;

    check_staged_cross_check(&status_sets.staged, &invisible, &visible)?;

    // The whole-index scan above validates every entry; the durable result
    // keeps only entries a later sub-slice can act on: the observed snapshot
    // candidate paths (staged ∪ changed). Clean tracked paths elsewhere in
    // the index are not retained.
    let candidate_paths: BTreeSet<&NormalizedWorkspacePath> = status_sets
        .staged
        .iter()
        .chain(status_sets.changed.iter())
        .collect();
    let index_entries = index_entries
        .into_iter()
        .filter(|entry| candidate_paths.contains(&entry.path))
        .collect::<Vec<_>>();

    Ok(SnapshotGitMetadata {
        head,
        file_mode_policy,
        changed_paths: status_sets.changed,
        staged_paths: status_sets.staged,
        untracked_paths: status_sets.untracked,
        index_entries,
    })
}

fn require_success(
    query: SnapshotGitQuery,
    output: &SnapshotCommandOutput,
) -> Result<(), SnapshotMetadataError> {
    if output.exit_code == 0 {
        return Ok(());
    }
    if query == SnapshotGitQuery::Head {
        // An unborn repository (or an otherwise unresolvable HEAD) is a
        // definite, well-understood repository state, not a command failure to
        // paper over: V1 never snapshots it.
        return Err(SnapshotMetadataError::UnsupportedGitState(format!(
            "HEAD does not resolve to a commit ({} exited {}): {}",
            query.label(),
            output.exit_code,
            output.stderr.trim()
        )));
    }
    Err(SnapshotMetadataError::GitCommand {
        query: query.label(),
        detail: if output.stderr.trim().is_empty() {
            format!("exit status {}", output.exit_code)
        } else {
            output.stderr.trim().to_owned()
        },
    })
}

/// Parse `rev-parse HEAD` output: one newline-terminated full lowercase hex ID.
pub(crate) fn parse_head(stdout: &[u8]) -> Result<String, SnapshotMetadataError> {
    let Some(oid) = stdout.strip_suffix(b"\n") else {
        return Err(SnapshotMetadataError::GitObservationUnavailable(
            "rev-parse HEAD output is not newline terminated".to_owned(),
        ));
    };
    if oid.is_empty() || oid.contains(&b'\n') || oid.contains(&0) {
        return Err(SnapshotMetadataError::GitObservationUnavailable(
            "rev-parse HEAD output is not a single object ID".to_owned(),
        ));
    }
    let oid = std::str::from_utf8(oid).map_err(|_| {
        SnapshotMetadataError::GitObservationUnavailable(
            "rev-parse HEAD output is not valid UTF-8".to_owned(),
        )
    })?;
    let plausible = matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    if !plausible {
        return Err(SnapshotMetadataError::GitObservationUnavailable(
            "rev-parse HEAD output is not a full lowercase hex object ID".to_owned(),
        ));
    }
    if oid.bytes().all(|byte| byte == b'0') {
        return Err(SnapshotMetadataError::UnsupportedGitState(
            "HEAD resolves to the all-zero object ID".to_owned(),
        ));
    }
    Ok(oid.to_owned())
}

/// Split `-z` NUL-terminated output into records.
///
/// Empty output means no records. Non-empty output must end in exactly one
/// NUL terminator; a missing terminator is truncation, never a clean result.
fn nul_records(
    query: SnapshotGitQuery,
    stdout: &[u8],
) -> Result<Vec<&[u8]>, SnapshotMetadataError> {
    if stdout.is_empty() {
        return Ok(Vec::new());
    }
    let Some(payload) = stdout.strip_suffix(&[0]) else {
        return Err(SnapshotMetadataError::GitObservationUnavailable(format!(
            "{} returned an unterminated NUL record",
            query.label()
        )));
    };
    let records = payload.split(|byte| *byte == 0).collect::<Vec<_>>();
    if records.iter().any(|record| record.is_empty()) {
        return Err(SnapshotMetadataError::GitObservationUnavailable(format!(
            "{} returned an empty NUL record",
            query.label()
        )));
    }
    Ok(records)
}

fn convert_git_path(raw: &[u8]) -> Result<NormalizedWorkspacePath, SnapshotMetadataError> {
    let text = std::str::from_utf8(raw).map_err(|_| {
        SnapshotMetadataError::InvalidPath(
            "Git emitted a non-UTF-8 path; lossy conversion is forbidden".to_owned(),
        )
    })?;
    NormalizedWorkspacePath::parse(text.to_owned()).map_err(|error| match error {
        WorkspaceSnapshotError::LimitExceeded(limit) => SnapshotMetadataError::LimitExceeded(limit),
        _ => SnapshotMetadataError::InvalidPath(format!(
            "Git emitted an invalid workspace path of {} bytes",
            text.len()
        )),
    })
}

/// Status-derived logical path sets in canonical order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedStatusSets {
    pub(crate) changed: Vec<NormalizedWorkspacePath>,
    pub(crate) staged: Vec<NormalizedWorkspacePath>,
    pub(crate) untracked: Vec<NormalizedWorkspacePath>,
}
/// Parse `status --porcelain=v1 -z --no-renames` output into canonical
/// (changed, staged, untracked) logical path sets.
///
/// With renames disabled each record is exactly `XY SP raw-path NUL`.
/// Rename/copy bytes, unmerged entries, ignored entries, and worktree-side
/// `A` (intent-to-add tripwire) all fail closed.
pub(crate) fn parse_status(stdout: &[u8]) -> Result<ParsedStatusSets, SnapshotMetadataError> {
    const QUERY: SnapshotGitQuery = SnapshotGitQuery::Status;
    let mut changed = Vec::new();
    let mut staged = Vec::new();
    let mut untracked = Vec::new();
    for record in nul_records(QUERY, stdout)? {
        if record.len() < 4 || record[2] != b' ' {
            return Err(SnapshotMetadataError::GitObservationUnavailable(
                "status returned a malformed porcelain record".to_owned(),
            ));
        }
        let (index, worktree) = (record[0], record[1]);
        if index == b'?' && worktree == b'?' {
            untracked.push(convert_git_path(&record[3..])?);
            continue;
        }
        if index == b'!' && worktree == b'!' {
            return Err(SnapshotMetadataError::UnsupportedGitState(
                "status reported an ignored path although ignored entries were not requested"
                    .to_owned(),
            ));
        }
        if index == b'U' || worktree == b'U' {
            return Err(SnapshotMetadataError::UnsupportedGitState(
                "status reported an unmerged index entry".to_owned(),
            ));
        }
        if (index == b'A' && worktree == b'A') || (index == b'D' && worktree == b'D') {
            // `AA` (both added) and `DD` (both deleted) are unmerged states
            // without a `U` byte; they never describe a resolvable candidate.
            return Err(SnapshotMetadataError::UnsupportedGitState(
                "status reported an unmerged added-added or deleted-deleted entry".to_owned(),
            ));
        }
        if matches!(index, b'R' | b'C') || matches!(worktree, b'R' | b'C') {
            return Err(SnapshotMetadataError::UnsupportedGitState(
                "status reported a rename/copy record although renames were disabled".to_owned(),
            ));
        }
        if worktree == b'A' {
            // `A` is not a documented worktree-column value; the observed
            // producer is an intent-to-add placeholder (`git add -N`), whose
            // index entry is an empty-blob stand-in rather than real content.
            return Err(SnapshotMetadataError::UnsupportedGitState(
                "status reported a worktree-side added path (intent-to-add placeholder)".to_owned(),
            ));
        }
        let ordinary = |status: u8| matches!(status, b' ' | b'M' | b'T' | b'A' | b'D');
        if !ordinary(index) || !ordinary(worktree) || (index == b' ' && worktree == b' ') {
            return Err(SnapshotMetadataError::GitObservationUnavailable(
                "status returned invalid XY status bytes".to_owned(),
            ));
        }
        let path = convert_git_path(&record[3..])?;
        if index != b' ' {
            staged.push(path.clone());
        }
        if worktree != b' ' {
            changed.push(path);
        }
    }
    Ok(ParsedStatusSets {
        changed: canonicalize_path_set(changed, MAX_CHANGED_PATHS, "changed path count")?,
        staged: canonicalize_path_set(staged, MAX_STAGED_PATHS, "staged path count")?,
        untracked: canonicalize_path_set(untracked, MAX_UNTRACKED_PATHS, "untracked path count")?,
    })
}

fn canonicalize_path_set(
    mut paths: Vec<NormalizedWorkspacePath>,
    limit: usize,
    label: &'static str,
) -> Result<Vec<NormalizedWorkspacePath>, SnapshotMetadataError> {
    if paths.len() > limit {
        return Err(SnapshotMetadataError::LimitExceeded(label));
    }
    // `NormalizedWorkspacePath` derives `Ord` over the inner logical string,
    // which is UTF-8 byte order for the accepted alphabet.
    paths.sort();
    if paths.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(SnapshotMetadataError::GitObservationUnavailable(
            "status reported a duplicate path".to_owned(),
        ));
    }
    Ok(paths)
}

/// Parse `ls-files --stage -z --full-name` into validated stage-0 entries.
///
/// Accepts exactly modes `100644` (non-executable), `100755` (executable),
/// and `120000` (symlink). Mode `160000` (gitlink) and every other mode,
/// every nonzero stage, every non-full-length or non-lowercase-hex OID, and
/// every duplicate stage-0 path fail closed. The scan covers the **whole
/// index**: special state anywhere rejects the observation even if the status
/// union never names the path. It is bounded by the process output bound
/// (64 MiB stdout), not by the snapshot-union path limit: a clean checkout
/// with more tracked files than `MAX_GIT_VISIBLE_PATHS` is a valid repository
/// and must not be rejected here.
pub(crate) fn parse_index_stage(
    stdout: &[u8],
) -> Result<Vec<SnapshotIndexEntry>, SnapshotMetadataError> {
    const QUERY: SnapshotGitQuery = SnapshotGitQuery::IndexStage;
    let mut entries = Vec::new();
    for record in nul_records(QUERY, stdout)? {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            return Err(SnapshotMetadataError::GitObservationUnavailable(
                "ls-files --stage record is missing its path separator".to_owned(),
            ));
        };
        let (meta, raw_path) = (&record[..tab], &record[tab + 1..]);
        if raw_path.is_empty() {
            return Err(SnapshotMetadataError::GitObservationUnavailable(
                "ls-files --stage record has an empty path".to_owned(),
            ));
        }
        let meta = std::str::from_utf8(meta).map_err(|_| {
            SnapshotMetadataError::GitObservationUnavailable(
                "ls-files --stage metadata is not valid UTF-8".to_owned(),
            )
        })?;
        let mut fields = meta.split(' ');
        let (mode, oid, stage) = match (fields.next(), fields.next(), fields.next(), fields.next())
        {
            (Some(mode), Some(oid), Some(stage), None) => (mode, oid, stage),
            _ => {
                return Err(SnapshotMetadataError::GitObservationUnavailable(
                    "ls-files --stage record does not have mode, object ID, and stage".to_owned(),
                ));
            }
        };
        let snapshot_mode = match mode {
            "100644" => SnapshotIndexMode::Regular { executable: false },
            "100755" => SnapshotIndexMode::Regular { executable: true },
            "120000" => SnapshotIndexMode::Symlink,
            "160000" => {
                return Err(SnapshotMetadataError::UnsupportedGitState(
                    "index contains a gitlink/submodule entry".to_owned(),
                ));
            }
            _ => {
                return Err(SnapshotMetadataError::UnsupportedGitState(format!(
                    "index contains an unsupported entry mode {mode:?}"
                )));
            }
        };
        if stage != "0" {
            return Err(SnapshotMetadataError::UnsupportedGitState(format!(
                "index contains an unmerged entry with stage {stage:?}"
            )));
        }
        let plausible = matches!(oid.len(), 40 | 64)
            && oid
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
        if !plausible {
            return Err(SnapshotMetadataError::GitObservationUnavailable(
                "ls-files --stage object ID is not a full lowercase hex ID".to_owned(),
            ));
        }
        entries.push(SnapshotIndexEntry {
            path: convert_git_path(raw_path)?,
            mode: snapshot_mode,
            oid: oid.to_owned(),
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    if entries.windows(2).any(|pair| pair[0].path == pair[1].path) {
        return Err(SnapshotMetadataError::GitObservationUnavailable(
            "ls-files --stage reported a duplicate path".to_owned(),
        ));
    }
    Ok(entries)
}

/// Validate `ls-files -v -z --full-name` tags against the parsed stage entries.
///
/// Exactly one tag per index path is required and the only accepted tag is
/// `H` (normal tracked file). `S` (skip-worktree) and every lowercase tag
/// (assume-unchanged) fail closed, as does every other tag (`M` unmerged,
/// `R`/`C`/`K`/`?`/unknown). A path-coverage mismatch between the two queries
/// over the same index fails closed as a contradictory observation. The scan
/// covers the whole index and is bounded by the process output bound, not by
/// any snapshot-union limit.
pub(crate) fn check_index_flags(
    stdout: &[u8],
    stage_entries: &[SnapshotIndexEntry],
) -> Result<(), SnapshotMetadataError> {
    const QUERY: SnapshotGitQuery = SnapshotGitQuery::IndexFlags;
    let mut flagged: Vec<NormalizedWorkspacePath> = Vec::new();
    for record in nul_records(QUERY, stdout)? {
        if record.len() < 3 || record[1] != b' ' {
            return Err(SnapshotMetadataError::GitObservationUnavailable(
                "ls-files -v record is missing its tag separator".to_owned(),
            ));
        }
        let (tag, raw_path) = (record[0], &record[2..]);
        if raw_path.is_empty() {
            return Err(SnapshotMetadataError::GitObservationUnavailable(
                "ls-files -v record has an empty path".to_owned(),
            ));
        }
        match tag {
            b'H' => {}
            b'S' => {
                return Err(SnapshotMetadataError::UnsupportedGitState(
                    "index contains a skip-worktree entry".to_owned(),
                ));
            }
            b'h' | b's' | b'm' | b'r' | b'c' | b'k' => {
                return Err(SnapshotMetadataError::UnsupportedGitState(format!(
                    "index contains an assume-unchanged entry (tag {})",
                    tag as char
                )));
            }
            b'M' => {
                return Err(SnapshotMetadataError::UnsupportedGitState(
                    "index contains an unmerged entry".to_owned(),
                ));
            }
            _ => {
                return Err(SnapshotMetadataError::UnsupportedGitState(format!(
                    "index contains an entry with an unrecognized ls-files tag {}",
                    tag as char
                )));
            }
        }
        flagged.push(convert_git_path(raw_path)?);
    }
    flagged.sort();
    let mut staged_paths: Vec<&NormalizedWorkspacePath> =
        stage_entries.iter().map(|entry| &entry.path).collect();
    staged_paths.sort();
    if staged_paths != flagged.iter().collect::<Vec<&NormalizedWorkspacePath>>() {
        return Err(SnapshotMetadataError::GitObservationUnavailable(
            "ls-files --stage and ls-files -v disagree on index path coverage".to_owned(),
        ));
    }
    Ok(())
}

/// Parse a `--name-only -z` query into a sorted logical path set.
pub(crate) fn parse_nul_name_set(
    query: SnapshotGitQuery,
    stdout: &[u8],
) -> Result<Vec<NormalizedWorkspacePath>, SnapshotMetadataError> {
    let mut paths = Vec::new();
    for record in nul_records(query, stdout)? {
        paths.push(convert_git_path(record)?);
        if paths.len() > MAX_STAGED_PATHS + 1 {
            // The staged-name queries are cross-checks, so they share the
            // staged bound with one slot of headroom to distinguish "over the
            // bound" from an internal accounting error.
            return Err(SnapshotMetadataError::LimitExceeded("staged path count"));
        }
    }
    paths.sort();
    if paths.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(SnapshotMetadataError::GitObservationUnavailable(format!(
            "{} reported a duplicate path",
            query.label()
        )));
    }
    Ok(paths)
}

/// Cross-check the status-derived staged set against both staged-name queries.
///
/// * `visible − invisible ≠ ∅` proves intent-to-add placeholders and fails
///   closed (`UnsupportedGitState`);
/// * `invisible − visible ≠ ∅` is a contradictory observation;
/// * a status-staged set that disagrees with the ITA-hidden staged set is a
///   contradictory observation (both exclude ITA placeholders by
///   construction, and both run with rename detection off).
fn check_staged_cross_check(
    status_staged: &[NormalizedWorkspacePath],
    invisible: &[NormalizedWorkspacePath],
    visible: &[NormalizedWorkspacePath],
) -> Result<(), SnapshotMetadataError> {
    let mut status_sorted = status_staged.to_vec();
    let mut invisible_sorted = invisible.to_vec();
    let mut visible_sorted = visible.to_vec();
    status_sorted.sort();
    invisible_sorted.sort();
    visible_sorted.sort();
    let invisible_set: BTreeSet<&NormalizedWorkspacePath> = invisible_sorted.iter().collect();
    let visible_set: BTreeSet<&NormalizedWorkspacePath> = visible_sorted.iter().collect();
    let hidden_count = visible_set.difference(&invisible_set).count();
    if hidden_count > 0 {
        return Err(SnapshotMetadataError::UnsupportedGitState(format!(
            "index contains {hidden_count} intent-to-add placeholder path(s) hidden by --ita-invisible-in-index"
        )));
    }
    if invisible_set != visible_set {
        return Err(SnapshotMetadataError::GitObservationUnavailable(
            "staged-name queries contradict each other outside intent-to-add handling".to_owned(),
        ));
    }
    let status_set: BTreeSet<&NormalizedWorkspacePath> = status_sorted.iter().collect();
    if status_set != invisible_set {
        return Err(SnapshotMetadataError::GitObservationUnavailable(
            "status staged paths disagree with diff --cached staged names".to_owned(),
        ));
    }
    Ok(())
}
