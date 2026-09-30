//! Managed Worktrees V1 — Phase 2 read-only discovery and reconciliation.
//!
//! Frozen contract: `docs/MANAGED_WORKTREES_V1_DESIGN.md` sections 6, 11, 20,
//! 23, and the Phase 2 slice of section 26.
//!
//! This module answers exactly one question:
//!
//! > What does Git and repository state currently prove?
//!
//! It never answers "how can we mutate it into the state we want". There is no
//! worktree creation, branch creation, lock or unlock, removal, prune, or ref
//! mutation here; no `PREPARED` to `ACTIVE` transition through Git; no Session
//! permission-root change; and no Planner execution against a managed root.
//!
//! The three layers are deliberately separable and independently testable:
//!
//! 1. [`parse_worktree_list_porcelain_z`] — a pure parser over
//!    `git worktree list --porcelain -z` bytes. No I/O.
//! 2. [`observe_repository`] — read-only command execution against a
//!    [`ReadOnlyGit`] seam, producing a normalized [`RepositoryObservation`].
//! 3. [`classify_eligibility`] and [`classify_reconciliation`] — pure functions
//!    over durable state plus a normalized observation. No I/O.
//!
//! Ambiguity is never silently converted into "safe to retry". Anything the
//! parser or observer cannot trust becomes [`DiscoveryError`] or an explicit
//! ambiguous state, and ambiguous states never permit automatic retry.

// Phase 2 adds discovery and classification only. No Goal lifecycle is routed
// through it yet, so most of the surface is unreachable from production until
// Phase 3 creation authority is authorized. The contract is frozen here so that
// Phase 3 wires to an already-reviewed model rather than inventing one.
#![expect(
    dead_code,
    reason = "Managed Worktrees Phase 2 discovery is not reachable from production until Phase 3 creation authority is authorized."
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use crate::managed_worktree::{
    ManagedWorktreeCreationIntent, ManagedWorktreeLifecycle, ManagedWorktreeRecord,
};
use crate::orchestrator_error::OrchestratorError;

/// A SHA-1 all-zero object ID, which Git reports for an unborn `HEAD`.
///
/// Git reports this in `git worktree list --porcelain` for a repository with no
/// commits, and `git rev-parse HEAD` fails. A managed worktree must never be
/// created against an unresolved `HEAD` (design section 6).
pub(crate) const UNRESOLVED_HEAD: &str = "0000000000000000000000000000000000000000";

/// Read-only discovery failure.
///
/// Discovery never repairs, retries destructively, or coerces an unreadable
/// observation into a safe-looking one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DiscoveryError {
    /// `git worktree list --porcelain -z` could not be parsed.
    MalformedWorktreeList(String),
    /// A read-only Git command failed.
    GitCommand { command: String, detail: String },
    /// A required observation was not available.
    ObservationUnavailable(String),
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedWorktreeList(reason) => {
                write!(f, "malformed worktree list: {reason}")
            }
            Self::GitCommand { command, detail } => {
                write!(f, "read-only git observation failed: {command}: {detail}")
            }
            Self::ObservationUnavailable(reason) => {
                write!(f, "git observation unavailable: {reason}")
            }
        }
    }
}

impl std::error::Error for DiscoveryError {}

/// Read-only filesystem status for an observed or expected worktree path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathObservation {
    /// The path exists and is a canonical directory.
    Directory,
    /// No filesystem entry exists at the exact path.
    Missing,
    /// A filesystem entry exists, but it is not a canonical directory.
    Occupied,
    /// The path could not be inspected or was not included in the observation.
    Unknown,
}

impl PathObservation {
    fn is_occupied(self) -> bool {
        matches!(self, Self::Directory | Self::Occupied)
    }
}

/// Compare already-absolute host paths using the platform's filesystem identity
/// rules. Git prints ordinary Windows paths while `canonicalize` may return an
/// extended-length prefix, so that prefix and case must not create false
/// ownership mismatches on Windows.
pub(crate) fn same_path_identity(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        fn normalized(path: &Path) -> Option<String> {
            let path = path.to_str()?.replace('/', "\\");
            let mut path = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
                format!(r"\\{rest}")
            } else if let Some(rest) = path.strip_prefix(r"\\?\") {
                rest.to_owned()
            } else {
                path
            };
            while path.len() > 3 && path.ends_with('\\') {
                path.pop();
            }
            Some(path)
        }
        normalized(left)
            .zip(normalized(right))
            .is_some_and(|(left, right)| left.eq_ignore_ascii_case(&right))
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

/// One worktree exactly as Git reports it.
///
/// Field values are reported verbatim. Paths are unquoted because `-z` is used,
/// so a path may legitimately contain spaces or other unusual bytes.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct ObservedWorktree {
    path: PathBuf,
    head: Option<String>,
    branch_ref: Option<String>,
    detached: bool,
    bare: bool,
    locked: bool,
    lock_reason: Option<String>,
    prunable: bool,
    prunable_reason: Option<String>,
    /// Attributes this build does not recognize. Preserved rather than dropped
    /// so a future Git attribute can never be silently ignored when deciding
    /// whether an observation is trustworthy.
    unknown_attributes: Vec<String>,
}

impl ObservedWorktree {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The reported `HEAD`, or `None` for a bare repository.
    pub(crate) fn head(&self) -> Option<&str> {
        self.head.as_deref()
    }

    /// The reported `HEAD` only when it resolves to a real commit.
    pub(crate) fn resolved_head(&self) -> Option<&str> {
        self.head.as_deref().filter(|head| !is_zero_object_id(head))
    }

    /// The checked-out branch ref, or `None` when detached or bare.
    pub(crate) fn branch_ref(&self) -> Option<&str> {
        self.branch_ref.as_deref()
    }

    pub(crate) fn is_detached(&self) -> bool {
        self.detached
    }

    pub(crate) fn is_bare(&self) -> bool {
        self.bare
    }

    pub(crate) fn is_locked(&self) -> bool {
        self.locked
    }

    pub(crate) fn lock_reason(&self) -> Option<&str> {
        self.lock_reason.as_deref()
    }

    pub(crate) fn prunable_reason(&self) -> Option<&str> {
        self.prunable_reason.as_deref()
    }

    pub(crate) fn unknown_attributes(&self) -> &[String] {
        &self.unknown_attributes
    }

    fn is_trustworthy(&self) -> bool {
        !self.bare && self.unknown_attributes.is_empty()
    }
}

/// The parsed contents of one `git worktree list --porcelain -z` invocation.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) struct WorktreeInventory {
    entries: Vec<ObservedWorktree>,
}

impl WorktreeInventory {
    pub(crate) fn entries(&self) -> &[ObservedWorktree] {
        &self.entries
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Worktrees registered at `path`.
    ///
    /// More than one match is itself a contradiction: Git registers a single
    /// administrative entry per path, so duplicates mean the observation cannot
    /// be trusted.
    pub(crate) fn at_path(&self, path: &Path) -> Vec<&ObservedWorktree> {
        self.entries
            .iter()
            .filter(|entry| same_path_identity(entry.path(), path))
            .collect()
    }

    /// The worktree that registered `ref`, if any.
    pub(crate) fn with_branch(&self, ref_name: &str) -> Vec<&ObservedWorktree> {
        self.entries
            .iter()
            .filter(|entry| entry.branch_ref() == Some(ref_name))
            .collect()
    }

    /// A conservative, order-independent description of contradictions in the
    /// inventory. An empty vector means nothing self-contradictory was seen.
    pub(crate) fn contradictions(&self) -> Vec<String> {
        let mut contradictions = Vec::new();
        let mut paths: Vec<&Path> = Vec::new();
        for entry in &self.entries {
            if paths
                .iter()
                .any(|path| same_path_identity(path, entry.path()))
            {
                contradictions.push(format!(
                    "worktree path {} is registered more than once",
                    entry.path().display()
                ));
            } else {
                paths.push(entry.path());
            }
        }
        if self.entries.iter().all(|entry| entry.bare) && self.entries.len() > 1 {
            contradictions.push("bare repository reported multiple worktrees".to_owned());
        }
        let mut branches = BTreeSet::new();
        for entry in &self.entries {
            if let Some(branch) = entry.branch_ref()
                && !branches.insert(branch)
            {
                contradictions.push(format!(
                    "branch {branch:?} is checked out by more than one worktree"
                ));
            }
        }
        contradictions
    }
}

/// Parse `git worktree list --porcelain -z` output.
///
/// The `-z` format is NUL-separated: every attribute is one token terminated by
/// NUL, and records are separated by an additional empty token. Attribute values
/// are everything after the first space, so values may contain spaces, and paths
/// are not quoted.
///
/// This function is pure: it performs no I/O and touches no Git state.
pub(crate) fn parse_worktree_list_porcelain_z(
    input: &str,
) -> Result<WorktreeInventory, DiscoveryError> {
    if !input.is_empty() && !input.ends_with("\0\0") {
        return Err(DiscoveryError::MalformedWorktreeList(
            "output ended before the final NUL record separator".to_owned(),
        ));
    }
    let mut entries: Vec<ObservedWorktree> = Vec::new();
    let mut current: Option<ObservedWorktree> = None;
    let mut saw_path = false;

    for token in input.split('\0') {
        if token.is_empty() {
            // Record separator. A trailing empty token closes the final record,
            // and so does end-of-input, so truncated output still flushes.
            if let Some(entry) = current.take() {
                finish_entry(entry, saw_path, &mut entries)?;
            }
            saw_path = false;
            continue;
        }
        let (key, value) = match token.split_once(' ') {
            Some((key, rest)) => (key, Some(rest)),
            None => (token, None),
        };
        if current.is_none() && !saw_path && key != "worktree" {
            return Err(DiscoveryError::MalformedWorktreeList(format!(
                "record begins with {key:?} instead of a worktree path"
            )));
        }
        let entry = current.get_or_insert_with(ObservedWorktree::default);
        apply_attribute(entry, key, value, &mut saw_path)?;
    }
    if let Some(entry) = current.take() {
        finish_entry(entry, saw_path, &mut entries)?;
    }
    Ok(WorktreeInventory { entries })
}

fn require_attribute_value<'a>(
    key: &str,
    value: Option<&'a str>,
) -> Result<&'a str, DiscoveryError> {
    value.filter(|text| !text.is_empty()).ok_or_else(|| {
        DiscoveryError::MalformedWorktreeList(format!("attribute {key:?} requires a value"))
    })
}

fn reject_attribute_value(key: &str, value: Option<&str>) -> Result<(), DiscoveryError> {
    if value.is_some() {
        return Err(DiscoveryError::MalformedWorktreeList(format!(
            "attribute {key:?} does not take a value"
        )));
    }
    Ok(())
}

fn apply_attribute(
    entry: &mut ObservedWorktree,
    key: &str,
    value: Option<&str>,
    saw_path: &mut bool,
) -> Result<(), DiscoveryError> {
    match key {
        "worktree" => {
            if *saw_path {
                return Err(DiscoveryError::MalformedWorktreeList(
                    "duplicate worktree path in one record".to_owned(),
                ));
            }
            entry.path = PathBuf::from(require_attribute_value(key, value)?);
            *saw_path = true;
        }
        "HEAD" => {
            let head = require_attribute_value(key, value)?;
            validate_object_id(head)?;
            if entry.head.is_some() {
                return Err(DiscoveryError::MalformedWorktreeList(
                    "duplicate HEAD in one record".to_owned(),
                ));
            }
            entry.head = Some(head.to_owned());
        }
        "branch" => {
            let branch = require_attribute_value(key, value)?;
            if !is_full_refname(branch) {
                return Err(DiscoveryError::MalformedWorktreeList(format!(
                    "branch attribute is not a valid full ref: {branch:?}"
                )));
            }
            if entry.branch_ref.is_some() {
                return Err(DiscoveryError::MalformedWorktreeList(
                    "duplicate branch in one record".to_owned(),
                ));
            }
            entry.branch_ref = Some(branch.to_owned());
        }
        "detached" => {
            reject_attribute_value(key, value)?;
            if entry.detached {
                return Err(DiscoveryError::MalformedWorktreeList(
                    "duplicate detached attribute in one record".to_owned(),
                ));
            }
            entry.detached = true;
        }
        "bare" => {
            reject_attribute_value(key, value)?;
            if entry.bare {
                return Err(DiscoveryError::MalformedWorktreeList(
                    "duplicate bare attribute in one record".to_owned(),
                ));
            }
            entry.bare = true;
        }
        "locked" => {
            if entry.locked {
                return Err(DiscoveryError::MalformedWorktreeList(
                    "duplicate locked in one record".to_owned(),
                ));
            }
            entry.locked = true;
            entry.lock_reason = value.map(str::to_owned);
        }
        "prunable" => {
            if entry.prunable {
                return Err(DiscoveryError::MalformedWorktreeList(
                    "duplicate prunable in one record".to_owned(),
                ));
            }
            entry.prunable = true;
            entry.prunable_reason = Some(value.unwrap_or_default().to_owned());
        }
        other => {
            // Preserve unknown attributes so they can force an ambiguous
            // classification instead of being silently dropped.
            entry.unknown_attributes.push(match value {
                Some(text) => format!("{other} {text}"),
                None => other.to_owned(),
            });
        }
    }
    Ok(())
}

fn finish_entry(
    entry: ObservedWorktree,
    saw_path: bool,
    entries: &mut Vec<ObservedWorktree>,
) -> Result<(), DiscoveryError> {
    if !saw_path {
        return Err(DiscoveryError::MalformedWorktreeList(
            "record without a worktree path".to_owned(),
        ));
    }
    if entry.bare {
        if entry.head.is_some() {
            return Err(DiscoveryError::MalformedWorktreeList(
                "bare record also reported a HEAD".to_owned(),
            ));
        }
    } else if entry.head.is_none() {
        return Err(DiscoveryError::MalformedWorktreeList(format!(
            "worktree {} reported no HEAD",
            entry.path.display()
        )));
    }
    if entry.detached && entry.branch_ref.is_some() {
        return Err(DiscoveryError::MalformedWorktreeList(format!(
            "worktree {} is both detached and on a branch",
            entry.path.display()
        )));
    }
    if entry.bare && (entry.branch_ref.is_some() || entry.detached) {
        return Err(DiscoveryError::MalformedWorktreeList(
            "bare record also reported a branch or detached state".to_owned(),
        ));
    }
    entries.push(entry);
    Ok(())
}

fn is_full_refname(value: &str) -> bool {
    value.starts_with("refs/")
        && !value.contains("..")
        && !value.contains("@{")
        && !value.bytes().any(|byte| {
            byte.is_ascii_whitespace()
                || byte.is_ascii_control()
                || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
        && value.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && !component.ends_with('.')
                && !component.ends_with(".lock")
        })
}

fn is_zero_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte == b'0')
}

fn validate_object_id(value: &str) -> Result<(), DiscoveryError> {
    let plausible = matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    if !plausible {
        return Err(DiscoveryError::MalformedWorktreeList(format!(
            "HEAD is not a full lowercase hex object ID: {value:?}"
        )));
    }
    Ok(())
}

/// A Git operation that must not be in progress for managed creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum InProgressOperation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

impl InProgressOperation {
    /// The pseudo-ref or marker Git uses for this operation.
    pub(crate) fn marker(self) -> &'static str {
        match self {
            Self::Merge => "MERGE_HEAD",
            Self::Rebase => "REBASE_HEAD",
            Self::CherryPick => "CHERRY_PICK_HEAD",
            Self::Revert => "REVERT_HEAD",
            Self::Bisect => "BISECT_START",
        }
    }
}

impl fmt::Display for InProgressOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
            Self::Bisect => "bisect",
        };
        f.write_str(name)
    }
}

/// Every operation marker, in a fixed order so observation is deterministic.
pub(crate) const ALL_IN_PROGRESS_OPERATIONS: [InProgressOperation; 5] = [
    InProgressOperation::Merge,
    InProgressOperation::Rebase,
    InProgressOperation::CherryPick,
    InProgressOperation::Revert,
    InProgressOperation::Bisect,
];

/// The state of the primary worktree's index and working tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct PrimaryWorkspaceStatus {
    /// Tracked modifications or unmerged entries.
    pub(crate) tracked_dirty: bool,
    /// Staged changes.
    pub(crate) staged: bool,
    /// Untracked files, which the design deliberately counts as dirty.
    pub(crate) untracked: bool,
}

impl PrimaryWorkspaceStatus {
    pub(crate) fn is_clean(&self) -> bool {
        !self.tracked_dirty && !self.staged && !self.untracked
    }
}

/// A normalized, read-only observation of the repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RepositoryObservation {
    top_level: PathBuf,
    common_dir: PathBuf,
    head: Option<String>,
    head_ref: Option<String>,
    inventory: WorktreeInventory,
    in_progress: Vec<InProgressOperation>,
    status: PrimaryWorkspaceStatus,
    path_observations: Vec<(PathBuf, PathObservation)>,
    /// Branch refs queried and whether each was observed to exist.
    observed_refs: BTreeMap<String, bool>,
}

impl RepositoryObservation {
    /// Build a normalized observation from already-gathered read-only values.
    ///
    /// Kept crate-private so the only production path is [`observe_repository`],
    /// which cannot populate it without going through the read-only seam.
    #[expect(
        clippy::too_many_arguments,
        reason = "The normalized snapshot preserves independently observed repository facts without lossy grouping."
    )]
    pub(crate) fn new(
        top_level: PathBuf,
        common_dir: PathBuf,
        head: Option<String>,
        head_ref: Option<String>,
        inventory: WorktreeInventory,
        in_progress: Vec<InProgressOperation>,
        status: PrimaryWorkspaceStatus,
        path_observations: Vec<(PathBuf, PathObservation)>,
        observed_refs: BTreeMap<String, bool>,
    ) -> Self {
        Self {
            top_level,
            common_dir,
            head,
            head_ref,
            inventory,
            in_progress,
            status,
            path_observations,
            observed_refs,
        }
    }

    pub(crate) fn top_level(&self) -> &Path {
        &self.top_level
    }

    pub(crate) fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    /// The primary `HEAD`, or `None` when it does not resolve to a commit.
    pub(crate) fn head(&self) -> Option<&str> {
        self.head.as_deref()
    }

    /// The symbolic ref the primary `HEAD` points at, if attached.
    pub(crate) fn head_ref(&self) -> Option<&str> {
        self.head_ref.as_deref()
    }

    pub(crate) fn inventory(&self) -> &WorktreeInventory {
        &self.inventory
    }

    pub(crate) fn in_progress(&self) -> &[InProgressOperation] {
        &self.in_progress
    }

    pub(crate) fn status(&self) -> PrimaryWorkspaceStatus {
        self.status
    }

    pub(crate) fn path_observation(&self, path: &Path) -> PathObservation {
        self.path_observations
            .iter()
            .find(|(observed, _)| same_path_identity(observed, path))
            .map(|(_, state)| *state)
            .unwrap_or(PathObservation::Unknown)
    }

    pub(crate) fn with_path_observation(mut self, path: PathBuf, state: PathObservation) -> Self {
        if let Some((_, observed_state)) = self
            .path_observations
            .iter_mut()
            .find(|(observed, _)| same_path_identity(observed, &path))
        {
            *observed_state = state;
        } else {
            self.path_observations.push((path, state));
        }
        self
    }

    pub(crate) fn ref_exists(&self, ref_name: &str) -> Option<bool> {
        self.observed_refs.get(ref_name).copied()
    }

    pub(crate) fn with_ref_observation(mut self, ref_name: String, exists: bool) -> Self {
        self.observed_refs.insert(ref_name, exists);
        self
    }

    /// Whether the observation is internally consistent enough to classify.
    ///
    /// This deliberately covers only *consistency* of what was observed: a
    /// duplicate registration, an empty inventory, or a bare repository means
    /// the inventory cannot be reasoned about. An unresolved `HEAD` is not a
    /// consistency problem — it is a definite, well-understood observation that
    /// eligibility classification reports precisely.
    pub(crate) fn is_trustworthy(&self) -> Result<(), String> {
        let contradictions = self.inventory.contradictions();
        if !contradictions.is_empty() {
            return Err(contradictions.join("; "));
        }
        if self.inventory.is_empty() {
            return Err("worktree list was empty".to_owned());
        }
        if self.inventory.entries().iter().any(|entry| entry.bare) {
            return Err("repository reports a bare worktree".to_owned());
        }
        if let Some(entry) = self
            .inventory
            .entries()
            .iter()
            .find(|entry| !entry.unknown_attributes().is_empty())
        {
            return Err(format!(
                "worktree {} reported unrecognized attributes: {:?}",
                entry.path().display(),
                entry.unknown_attributes()
            ));
        }
        let primary = self.inventory.at_path(self.top_level());
        if primary.len() != 1 {
            return Err("primary top-level is not registered exactly once".to_owned());
        }
        if self.path_observation(self.top_level()) != PathObservation::Directory {
            return Err("primary top-level is not an observed canonical directory".to_owned());
        }
        if primary[0].resolved_head() != self.head() || primary[0].branch_ref() != self.head_ref() {
            return Err("primary HEAD/ref disagrees with its worktree-list record".to_owned());
        }
        Ok(())
    }
}

/// What managed creation intends to create, derived from durable state only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExpectedWorktreeTarget {
    primary_root: PathBuf,
    worktree_root: PathBuf,
    branch_ref: String,
    base_commit: String,
    repository_common_dir: PathBuf,
    lock_reason: String,
    lifecycle: ManagedWorktreeLifecycle,
    prepared_operation_id: Option<String>,
}

impl ExpectedWorktreeTarget {
    /// Derive the expected target from a durable record. Pure: no observation.
    pub(crate) fn from_record(record: &ManagedWorktreeRecord) -> Self {
        Self {
            primary_root: record.primary_root().to_path_buf(),
            worktree_root: record.worktree_root().to_path_buf(),
            branch_ref: record.branch_ref().to_owned(),
            base_commit: record.base_commit().to_owned(),
            repository_common_dir: record.repository_common_dir().to_path_buf(),
            lock_reason: record.lock_reason().to_owned(),
            lifecycle: record.lifecycle(),
            prepared_operation_id: None,
        }
    }

    /// Build an expected target bound to the exact persisted PREPARED intent.
    /// The intent must validate against the record and the record must be in the
    /// lifecycle state that permits creation/reconciliation.
    pub(crate) fn from_record_and_intent(
        record: &ManagedWorktreeRecord,
        intent: &ManagedWorktreeCreationIntent,
    ) -> Result<Self, OrchestratorError> {
        intent.validate_against_record(record)?;
        if !record.permits_creation_intent() {
            return Err(OrchestratorError::CorruptGoal(
                "managed-worktree lifecycle cannot carry a prepared creation intent".to_owned(),
            ));
        }
        let mut expected = Self::from_record(record);
        expected.prepared_operation_id = Some(intent.operation_id().as_str().to_owned());
        Ok(expected)
    }

    pub(crate) fn primary_root(&self) -> &Path {
        &self.primary_root
    }

    pub(crate) fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    pub(crate) fn branch_ref(&self) -> &str {
        &self.branch_ref
    }

    pub(crate) fn base_commit(&self) -> &str {
        &self.base_commit
    }

    pub(crate) fn repository_common_dir(&self) -> &Path {
        &self.repository_common_dir
    }

    pub(crate) fn lock_reason(&self) -> &str {
        &self.lock_reason
    }
}

/// Why a repository is not eligible for managed worktree creation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ineligibility {
    /// `Goal.cwd` is not the canonical Git top level.
    NonTopLevelCwd { top_level: PathBuf },
    /// The common directory could not be observed.
    CommonDirUnobservable,
    /// The observed repository common directory differs from the durable target.
    CommonDirMismatch {
        expected: PathBuf,
        observed: PathBuf,
    },
    /// The primary HEAD moved after the durable base commit was captured.
    BaseCommitMismatch { expected: String, observed: String },
    /// The durable primary root differs from the requested Git top-level.
    PrimaryRootMismatch {
        expected: PathBuf,
        observed: PathBuf,
    },
    /// `HEAD` does not resolve to a commit.
    HeadNotACommit,
    /// The primary workspace has tracked, staged, or untracked changes.
    PrimaryWorkspaceDirty(PrimaryWorkspaceStatus),
    /// A Git operation is in progress in the primary workspace.
    OperationInProgress(Vec<InProgressOperation>),
    /// The expected managed path is already occupied.
    ExpectedPathOccupied,
    /// The expected managed path could not be inspected.
    ExpectedPathUnknown,
    /// The managed branch ref already exists without matching durable ownership.
    ExpectedBranchExistsWithoutOwnership,
    /// This durable lifecycle state is not permitted to enter creation eligibility.
    LifecycleDoesNotPermitCreation(ManagedWorktreeLifecycle),
    /// The observation itself could not be trusted.
    Ambiguous(String),
}

/// The eligibility verdict for managed worktree creation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Eligibility {
    /// Every design section 6 precondition holds.
    Eligible,
    Ineligible(Ineligibility),
}

impl Eligibility {
    pub(crate) fn is_eligible(&self) -> bool {
        matches!(self, Self::Eligible)
    }
}

/// Classify whether a repository may host a managed worktree creation.
///
/// Pure: it reads only the normalized observation and the expected target. It
/// performs no I/O, and it never reports an ambiguous observation as eligible.
pub(crate) fn classify_eligibility(
    cwd: &Path,
    expected: &ExpectedWorktreeTarget,
    observation: &RepositoryObservation,
) -> Eligibility {
    if let Err(reason) = observation.is_trustworthy() {
        return Eligibility::Ineligible(Ineligibility::Ambiguous(reason));
    }
    // `Goal.cwd` must be the canonical Git top level; a subdirectory is blocked
    // in Managed Worktrees V1 (design section 6, test group B).
    if !same_path_identity(observation.top_level(), cwd) {
        return Eligibility::Ineligible(Ineligibility::NonTopLevelCwd {
            top_level: observation.top_level().to_path_buf(),
        });
    }
    match expected.lifecycle {
        ManagedWorktreeLifecycle::Requested => {}
        ManagedWorktreeLifecycle::Prepared if expected.prepared_operation_id.is_some() => {}
        lifecycle => {
            return Eligibility::Ineligible(Ineligibility::LifecycleDoesNotPermitCreation(
                lifecycle,
            ));
        }
    }
    if !same_path_identity(expected.primary_root(), cwd) {
        return Eligibility::Ineligible(Ineligibility::PrimaryRootMismatch {
            expected: expected.primary_root().to_path_buf(),
            observed: cwd.to_path_buf(),
        });
    }
    if observation.common_dir.as_os_str().is_empty() {
        return Eligibility::Ineligible(Ineligibility::CommonDirUnobservable);
    }
    if !same_path_identity(observation.common_dir(), expected.repository_common_dir()) {
        return Eligibility::Ineligible(Ineligibility::CommonDirMismatch {
            expected: expected.repository_common_dir().to_path_buf(),
            observed: observation.common_dir().to_path_buf(),
        });
    }
    let Some(head) = observation.head() else {
        return Eligibility::Ineligible(Ineligibility::HeadNotACommit);
    };
    if head != expected.base_commit() {
        return Eligibility::Ineligible(Ineligibility::BaseCommitMismatch {
            expected: expected.base_commit().to_owned(),
            observed: head.to_owned(),
        });
    }
    if !observation.status().is_clean() {
        return Eligibility::Ineligible(Ineligibility::PrimaryWorkspaceDirty(observation.status()));
    }
    if !observation.in_progress().is_empty() {
        return Eligibility::Ineligible(Ineligibility::OperationInProgress(
            observation.in_progress().to_vec(),
        ));
    }
    match observation.path_observation(expected.worktree_root()) {
        PathObservation::Missing => {}
        PathObservation::Directory | PathObservation::Occupied => {
            return Eligibility::Ineligible(Ineligibility::ExpectedPathOccupied);
        }
        PathObservation::Unknown => {
            return Eligibility::Ineligible(Ineligibility::ExpectedPathUnknown);
        }
    }
    match observation.ref_exists(expected.branch_ref()) {
        Some(true) => Eligibility::Ineligible(Ineligibility::ExpectedBranchExistsWithoutOwnership),
        Some(false) => Eligibility::Eligible,
        None => Eligibility::Ineligible(Ineligibility::Ambiguous(
            "expected branch ref was not observed".to_owned(),
        )),
    }
}

/// The exact reconciliation state of a durable managed-worktree record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Reconciliation {
    /// Exact durable ownership proven: the expected path is registered, on the
    /// managed branch, at the frozen base commit, and locked as the design
    /// requires.
    ActiveExact { head: String },
    /// Neither the path nor the branch side effect exists. A bounded retry is
    /// permitted only when an exact PREPARED operation ID is durably bound.
    NoSideEffect { operation_id: Option<String> },
    /// The worktree is registered but Git reports it as prunable.
    PrunableMetadata { reason: String },
    /// No registration, but the expected path is occupied by unknown content.
    PathOccupied,
    /// Git still registers the target, but its path is missing without a
    /// machine-readable prunable marker.
    MissingPath,
    /// The managed branch exists but no worktree is registered at the expected
    /// path: a partial side effect that blocks rather than retries.
    BranchOnlySideEffect,
    /// The expected path is registered but not to this record's branch.
    PathOwnedByOtherWorktree { observed_branch: Option<String> },
    /// Registered at the expected path on a different branch.
    BranchMismatch { observed: Option<String> },
    /// Registered at the expected path but at a different `HEAD`.
    HeadMismatch { observed: Option<String> },
    /// The observed repository common directory is not the recorded one.
    CommonDirMismatch { observed: PathBuf },
    /// The observed canonical primary top-level is not the recorded one.
    PrimaryRootMismatch { observed: PathBuf },
    /// Registered at the expected path but not in the required locked state.
    LockMismatch { observed: Option<String> },
    /// The managed branch no longer exists.
    BranchMissing,
    /// No registration at the expected path and the path is absent: the
    /// worktree was removed exactly. The local branch may be retained or
    /// separately deleted, as permitted by the design's cleanup contract.
    RemovedExact,
    /// The observation cannot be trusted; automatic retry is never permitted.
    Ambiguous { reason: String },
}

impl Reconciliation {
    /// Whether exact durable ownership is proven.
    pub(crate) fn is_exact(&self) -> bool {
        matches!(self, Self::ActiveExact { .. } | Self::RemovedExact)
    }

    /// Whether a bounded creation retry may be attempted.
    ///
    /// Only [`Reconciliation::NoSideEffect`] qualifies. Ambiguity never becomes
    /// a retry, and every other state is a real side effect that must be
    /// reconciled explicitly first (design sections 11 and 20).
    pub(crate) fn permits_bounded_retry(&self) -> bool {
        matches!(
            self,
            Self::NoSideEffect {
                operation_id: Some(_)
            }
        )
    }

    /// Whether the state needs explicit host recovery rather than any automatic
    /// action.
    pub(crate) fn requires_explicit_recovery(&self) -> bool {
        !matches!(
            self,
            Self::ActiveExact { .. }
                | Self::RemovedExact
                | Self::NoSideEffect { .. }
                | Self::Ambiguous { .. }
        )
    }
}

/// Classify the durable record against a read-only observation.
///
/// Pure: reads the expected target (derived from the durable record by
/// [`ExpectedWorktreeTarget::from_record`]) plus the normalized observation, and
/// performs no I/O.
pub(crate) fn classify_reconciliation(
    expected: &ExpectedWorktreeTarget,
    observation: &RepositoryObservation,
) -> Reconciliation {
    if let Err(reason) = observation.is_trustworthy() {
        return Reconciliation::Ambiguous { reason };
    }
    if !same_path_identity(observation.common_dir(), expected.repository_common_dir()) {
        return Reconciliation::CommonDirMismatch {
            observed: observation.common_dir().to_path_buf(),
        };
    }
    if !same_path_identity(observation.top_level(), expected.primary_root()) {
        return Reconciliation::PrimaryRootMismatch {
            observed: observation.top_level().to_path_buf(),
        };
    }

    let path_observation = observation.path_observation(expected.worktree_root());
    if path_observation == PathObservation::Unknown {
        return Reconciliation::Ambiguous {
            reason: format!(
                "expected path {} could not be inspected",
                expected.worktree_root().display()
            ),
        };
    }

    let matches = observation.inventory().at_path(expected.worktree_root());
    if matches.len() > 1 {
        return Reconciliation::Ambiguous {
            reason: format!(
                "expected path {} is registered more than once",
                expected.worktree_root().display()
            ),
        };
    }

    let Some(entry) = matches.first().copied() else {
        // No registration at the expected path.
        let branch_registered_elsewhere = !observation
            .inventory()
            .with_branch(expected.branch_ref())
            .is_empty();
        if branch_registered_elsewhere {
            return Reconciliation::PathOwnedByOtherWorktree {
                observed_branch: Some(expected.branch_ref().to_owned()),
            };
        }
        if path_observation.is_occupied() {
            return Reconciliation::PathOccupied;
        }
        if expected.lifecycle == ManagedWorktreeLifecycle::Removed {
            return match observation.ref_exists(expected.branch_ref()) {
                Some(_) => Reconciliation::RemovedExact,
                None => Reconciliation::Ambiguous {
                    reason: "expected branch ref was not observed".to_owned(),
                },
            };
        }
        match observation.ref_exists(expected.branch_ref()) {
            Some(true) => return Reconciliation::BranchOnlySideEffect,
            None => {
                return Reconciliation::Ambiguous {
                    reason: "expected branch ref was not observed".to_owned(),
                };
            }
            Some(false) => {}
        }
        return match expected.lifecycle {
            ManagedWorktreeLifecycle::Removed => Reconciliation::RemovedExact,
            ManagedWorktreeLifecycle::Active | ManagedWorktreeLifecycle::CleanupEligible => {
                Reconciliation::BranchMissing
            }
            ManagedWorktreeLifecycle::Blocked => Reconciliation::Ambiguous {
                reason: "blocked lifecycle requires explicit host recovery".to_owned(),
            },
            ManagedWorktreeLifecycle::Requested | ManagedWorktreeLifecycle::Prepared => {
                Reconciliation::NoSideEffect {
                    operation_id: expected.prepared_operation_id.clone(),
                }
            }
        };
    };

    if entry.prunable {
        return Reconciliation::PrunableMetadata {
            reason: entry.prunable_reason().unwrap_or_default().to_owned(),
        };
    }
    match path_observation {
        PathObservation::Directory => {}
        PathObservation::Missing => return Reconciliation::MissingPath,
        PathObservation::Occupied => return Reconciliation::PathOccupied,
        PathObservation::Unknown => unreachable!("unknown was rejected above"),
    }
    if !entry.is_trustworthy() {
        return Reconciliation::Ambiguous {
            reason: format!(
                "worktree {} reported unrecognized attributes",
                entry.path().display()
            ),
        };
    }
    if entry.branch_ref() != Some(expected.branch_ref()) {
        return Reconciliation::BranchMismatch {
            observed: entry.branch_ref().map(str::to_owned),
        };
    }
    match observation.ref_exists(expected.branch_ref()) {
        Some(true) => {}
        Some(false) => return Reconciliation::BranchMissing,
        None => {
            return Reconciliation::Ambiguous {
                reason: "expected branch ref was not observed".to_owned(),
            };
        }
    }
    if !entry.is_locked() || entry.lock_reason() != Some(expected.lock_reason()) {
        return Reconciliation::LockMismatch {
            observed: entry.lock_reason().map(str::to_owned),
        };
    }
    if entry.resolved_head() != Some(expected.base_commit()) {
        return Reconciliation::HeadMismatch {
            observed: entry.resolved_head().map(str::to_owned),
        };
    }
    if matches!(
        expected.lifecycle,
        ManagedWorktreeLifecycle::Requested
            | ManagedWorktreeLifecycle::Blocked
            | ManagedWorktreeLifecycle::Removed
    ) {
        return Reconciliation::Ambiguous {
            reason: format!(
                "Git reports an exact-looking worktree for incompatible durable lifecycle {:?}",
                expected.lifecycle
            ),
        };
    }
    Reconciliation::ActiveExact {
        head: entry.resolved_head().unwrap_or_default().to_owned(),
    }
}
