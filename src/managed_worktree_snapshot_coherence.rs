//! Managed Worktrees V1 Phase 5 Slice 2D — snapshot observation coherence.
//!
//! This slice assembles the already-frozen observers (Slice 2A metadata,
//! Slice 2B worktree objects, Slice 2C index blobs) into a bounded, sampled-
//! stable [`StableWorkspaceObservation`]. It deliberately performs **no**
//! durable Goal lookup (`TaskStore`), no manifest encoding, and no primary
//! branch/base decision. It re-proves managed authority through
//! [`crate::managed_worktree_prepare::validated_execution_root`] rather than
//! duplicating any authority rule.
//!
//! ## Fixed sampling sequence (design §6, §7)
//!
//! ```text
//! AUTHORITY A   prove the exact managed execution root
//! META A        observe candidate metadata
//! CONTENT A     INDEX A (per-pass OID cache) then WORKTREE A (one fresh budget)
//! META B        observe metadata again; require META A == META B before CONTENT B
//! CONTENT B     INDEX B (entirely new session; re-read) then WORKTREE B
//! META C        observe metadata a third time; require META A == META B == META C
//! AUTHORITY C   re-prove the exact execution root after all content/metadata work
//! compare       CONTENT A == CONTENT B at every identity
//! bound         semantic evidence bytes <= MAX_SNAPSHOT_CONTENT_BYTES
//! return        StableWorkspaceObservation
//! ```
//!
//! There is **no retry**. Three metadata observations bracket the second content
//! pass because metadata may change during CONTENT B, and AUTHORITY C runs after
//! all content and metadata work. See
//! `docs/MANAGED_WORKTREES_PHASE5_SNAPSHOT_EVIDENCE.md`.
//!
//! ## Accepted residuals (documented, not claimed away)
//!
//! * **`HOST_FILESYSTEM_STALL_RESIDUAL`**: regular filesystem syscalls have no
//!   proven hard wall-clock deadline, so no leaking timeout threads are added and
//!   a stall is never reinterpreted as success (Slice 2B owns this).
//! * **`NON_TRANSACTIONAL_ABA_RESIDUAL`**: 2D proves matching stable *sampled*
//!   states. It does not prove that no transient mutation occurred at any
//!   instant; a `A -> B -> A` change falling entirely between samples may be
//!   invisible. 2D does not claim temporal-history integrity.
//! * **Slice 2A internal non-atomicity**: one [`observe_metadata`] call contains
//!   sequential Git queries and is not an atomic repository transaction. 2D
//!   mitigates this with `A == B == C` plus two full content observations; a
//!   realistic internally-inconsistent-but-identical metadata triple is recorded
//!   as a residual of 2A, not papered over here.

#![expect(
    dead_code,
    reason = "Slice 2D freezes the coherence seam and its injectable observers before the finalizer/2E manifest assembly is authorized to call it."
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::managed_worktree_discovery::same_path_identity;
use crate::managed_worktree_snapshot_index_blob::{IndexBlobError, SnapshotIndexBlobSession};
use crate::managed_worktree_snapshot_object::{
    SnapshotHashBudget, SnapshotObjectError, observe_worktree_object,
};
use crate::managed_worktree_snapshot_observe::{
    GitFileModePolicy, HostSnapshotGit, SnapshotGitMetadata, SnapshotIndexEntry, SnapshotIndexMode,
    SnapshotMetadataError, observe_snapshot_metadata,
};
use crate::workspace_snapshot::{
    CandidateObjectIdentity, MAX_SNAPSHOT_CONTENT_BYTES, MAX_WRITER_MUTATED_PATHS,
    NormalizedWorkspacePath,
};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// A narrow Slice 2D semantic error. Sources stay typed: observer errors are
/// mapped through `From` impls rather than stringified, and the two authority
/// categories plus instability remain distinct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotCoherenceError {
    /// The managed execution root could not be proven at AUTHORITY A (session
    /// authority, observation, or reconciliation was unavailable right now).
    AuthorityUnavailable(String),
    /// Durable Goal/Session/record ownership does not bind (reported by a typed
    /// authority seam; the reused `validated_execution_root` collapses its own
    /// cause to a string, which the production adapter maps to
    /// [`Self::AuthorityUnavailable`]).
    OwnershipMismatch(String),
    /// Slice 2A metadata observation failed, timed out, or was contradictory.
    GitObservationUnavailable(String),
    /// Slice 2C index-blob observation failed, timed out, or was contradictory.
    IndexBlobObservationUnavailable(String),
    /// Slice 2B worktree-object observation hit an I/O failure.
    FilesystemObservationUnavailable(String),
    /// The repository carries index/worktree state V1 refuses to snapshot.
    UnsupportedGitState(String),
    /// A worktree object is a kind V1 cannot represent (directory, device,
    /// FIFO, unsupported special object).
    UnsupportedObjectType(String),
    /// A Slice 1 resource bound (writer path count, physical pass bytes, or
    /// semantic evidence bytes) was exceeded.
    LimitExceeded(&'static str),
    /// Two sampled states that must match did not: metadata A/B/C mismatch,
    /// content A/B identity mismatch, or authority that was exact at A but is no
    /// longer exact at C. Lower-level detail is preserved and never retried.
    SnapshotUnstable(String),
}

impl std::fmt::Display for SnapshotCoherenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthorityUnavailable(detail) => {
                write!(formatter, "snapshot authority unavailable: {detail}")
            }
            Self::OwnershipMismatch(detail) => {
                write!(formatter, "snapshot ownership mismatch: {detail}")
            }
            Self::GitObservationUnavailable(detail) => {
                write!(
                    formatter,
                    "snapshot git metadata observation unavailable: {detail}"
                )
            }
            Self::IndexBlobObservationUnavailable(detail) => {
                write!(
                    formatter,
                    "snapshot index blob observation unavailable: {detail}"
                )
            }
            Self::FilesystemObservationUnavailable(detail) => {
                write!(
                    formatter,
                    "snapshot filesystem observation unavailable: {detail}"
                )
            }
            Self::UnsupportedGitState(detail) => {
                write!(formatter, "snapshot unsupported git state: {detail}")
            }
            Self::UnsupportedObjectType(detail) => {
                write!(formatter, "snapshot unsupported object type: {detail}")
            }
            Self::LimitExceeded(limit) => write!(formatter, "snapshot {limit} limit exceeded"),
            Self::SnapshotUnstable(detail) => {
                write!(formatter, "snapshot unstable: {detail}")
            }
        }
    }
}

impl std::error::Error for SnapshotCoherenceError {}

impl From<SnapshotMetadataError> for SnapshotCoherenceError {
    fn from(error: SnapshotMetadataError) -> Self {
        match error {
            SnapshotMetadataError::UnsupportedGitState(reason) => Self::UnsupportedGitState(reason),
            SnapshotMetadataError::LimitExceeded(limit) => Self::LimitExceeded(limit),
            SnapshotMetadataError::GitCommand { detail, .. } => {
                Self::GitObservationUnavailable(detail)
            }
            SnapshotMetadataError::GitObservationUnavailable(reason) => {
                Self::GitObservationUnavailable(reason)
            }
            SnapshotMetadataError::InvalidPath(reason) => Self::GitObservationUnavailable(reason),
        }
    }
}

impl From<SnapshotObjectError> for SnapshotCoherenceError {
    fn from(error: SnapshotObjectError) -> Self {
        match error {
            // Slice 2B's own within-observation race detector is a coherence
            // instability signal (design §18).
            SnapshotObjectError::Unstable => {
                Self::SnapshotUnstable("worktree object changed while being observed".to_owned())
            }
            SnapshotObjectError::Unsupported(reason) => {
                Self::UnsupportedObjectType(reason.to_owned())
            }
            SnapshotObjectError::LimitExceeded(reason) => Self::LimitExceeded(reason),
            SnapshotObjectError::Io(error) => {
                Self::FilesystemObservationUnavailable(error.to_string())
            }
        }
    }
}

impl From<IndexBlobError> for SnapshotCoherenceError {
    fn from(error: IndexBlobError) -> Self {
        match error {
            IndexBlobError::LimitExceeded(reason) => Self::LimitExceeded(reason),
            other => Self::IndexBlobObservationUnavailable(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// Stable result types (deterministic, canonical order; no manifest bytes)
// ---------------------------------------------------------------------------

/// The canonical equality projection of one Slice 2A metadata observation.
///
/// Comparison is over `head`, `file_mode_policy`, the three canonical logical
/// path sets, and every index entry's `(path, mode, oid)` — never lengths or
/// arbitrary insertion order. Cross-set overlap (a path staged and changed) is
/// legal and preserved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StableSnapshotMetadata {
    pub(crate) head: String,
    pub(crate) file_mode_policy: GitFileModePolicy,
    pub(crate) changed_paths: Vec<NormalizedWorkspacePath>,
    pub(crate) staged_paths: Vec<NormalizedWorkspacePath>,
    pub(crate) untracked_paths: Vec<NormalizedWorkspacePath>,
    pub(crate) index_entries: Vec<StableSnapshotIndexEntry>,
}

/// One index entry projected for equality: identity only, path-sorted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StableSnapshotIndexEntry {
    pub(crate) path: NormalizedWorkspacePath,
    pub(crate) mode: SnapshotIndexMode,
    pub(crate) oid: String,
}

impl StableSnapshotMetadata {
    /// Build the canonical projection, sorting each set and the index entries by
    /// path bytes so that two observations differing only in `Vec` order compare
    /// equal.
    pub(crate) fn projected(metadata: &SnapshotGitMetadata) -> Self {
        let mut index_entries = metadata
            .index_entries
            .iter()
            .map(|entry| StableSnapshotIndexEntry {
                path: entry.path.clone(),
                mode: entry.mode,
                oid: entry.oid.clone(),
            })
            .collect::<Vec<_>>();
        index_entries.sort_by(|left, right| {
            left.path
                .as_str()
                .as_bytes()
                .cmp(right.path.as_str().as_bytes())
        });
        Self {
            head: metadata.head.clone(),
            file_mode_policy: metadata.file_mode_policy,
            changed_paths: canonical_unique_paths(metadata.changed_paths.iter()),
            staged_paths: canonical_unique_paths(metadata.staged_paths.iter()),
            untracked_paths: canonical_unique_paths(metadata.untracked_paths.iter()),
            index_entries,
        }
    }
}

/// One Git-visible record: the union path with its index object (from Slice 2C)
/// and its worktree object (from Slice 2B). `index_object` is `Absent` for a
/// union path with no index entry (untracked).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StableGitVisiblePath {
    pub(crate) path: NormalizedWorkspacePath,
    pub(crate) index_object: CandidateObjectIdentity,
    pub(crate) worktree_object: CandidateObjectIdentity,
}

/// One writer-mutated record. `object` retains the full
/// [`CandidateObjectIdentity`] (present, or `Absent` when the path no longer
/// exists) without constructing `WriterMutatedPathIdentity`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StableWriterPath {
    pub(crate) path: NormalizedWorkspacePath,
    pub(crate) object: CandidateObjectIdentity,
}

/// The sampled-stable result. Canonically ordered; carries no canonical
/// manifest bytes and no durable Goal IDs beyond what authority already proved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StableWorkspaceObservation {
    pub(crate) metadata: StableSnapshotMetadata,
    pub(crate) git_visible_paths: Vec<StableGitVisiblePath>,
    pub(crate) writer_mutated_paths: Vec<StableWriterPath>,
}

/// One content pass's identities, kept fully in memory so the two passes can be
/// compared by value and fed once into the semantic evidence calculator.
#[derive(Debug, PartialEq, Eq)]
struct ObservedPassContent {
    git_visible_paths: Vec<StableGitVisiblePath>,
    writer_mutated_paths: Vec<StableWriterPath>,
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Canonical, path-byte-sorted, de-duplicated logical paths.
fn canonical_unique_paths<'a>(
    paths: impl Iterator<Item = &'a NormalizedWorkspacePath>,
) -> Vec<NormalizedWorkspacePath> {
    paths
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Enforce the writer-mutated path bound and return the canonical, de-duplicated
/// writer set before any content work runs. Slice 1 `MAX_WRITER_MUTATED_PATHS`.
pub(crate) fn canonicalize_writer_paths(
    paths: &[NormalizedWorkspacePath],
) -> Result<Vec<NormalizedWorkspacePath>, SnapshotCoherenceError> {
    if paths.len() > MAX_WRITER_MUTATED_PATHS {
        return Err(SnapshotCoherenceError::LimitExceeded(
            "writer-mutated path count",
        ));
    }
    Ok(canonical_unique_paths(paths.iter()))
}

/// Total **semantic content evidence** bytes, computed exactly as the future
/// [`crate::workspace_snapshot::WorkspaceSnapshotManifestV1`] constructor sums:
/// `index_object + worktree_object` per Git-visible record, the present object
/// per writer-mutated record, and `0` per absent writer path. Logical
/// duplication is counted; the pass-local physical cache never reduces it.
/// Checked arithmetic plus the semantic bound fail closed on over-budget or
/// overflow instead of deferring discovery to a later slice.
pub(crate) fn semantic_evidence_bytes(
    git_visible: &[StableGitVisiblePath],
    writer_mutated: &[StableWriterPath],
) -> Result<u64, SnapshotCoherenceError> {
    let limit = || SnapshotCoherenceError::LimitExceeded("snapshot content byte");
    let mut total = 0_u64;
    for record in git_visible {
        total = total
            .checked_add(record.index_object.size_bytes())
            .and_then(|value| value.checked_add(record.worktree_object.size_bytes()))
            .ok_or_else(limit)?;
    }
    for record in writer_mutated {
        total = total
            .checked_add(record.object.size_bytes())
            .ok_or_else(limit)?;
    }
    if total > MAX_SNAPSHOT_CONTENT_BYTES {
        return Err(limit());
    }
    Ok(total)
}

/// A human-readable explanation of the first differing metadata field.
fn metadata_instability(before: &StableSnapshotMetadata, after: &StableSnapshotMetadata) -> String {
    if before.head != after.head {
        return format!("observed HEAD changed: {} -> {}", before.head, after.head);
    }
    if before.file_mode_policy != after.file_mode_policy {
        return "core.fileMode policy changed across observations".to_owned();
    }
    if before.changed_paths != after.changed_paths {
        return "changed path set changed across observations".to_owned();
    }
    if before.staged_paths != after.staged_paths {
        return "staged path set changed across observations".to_owned();
    }
    if before.untracked_paths != after.untracked_paths {
        return "untracked path set changed across observations".to_owned();
    }
    if before.index_entries != after.index_entries {
        return "index entries changed across observations (path, mode, or object ID)".to_owned();
    }
    "managed snapshot metadata changed across observations".to_owned()
}

/// A human-readable explanation of the first differing content identity, or
/// `None` when the two passes are identity-equal. Because both passes are
/// canonically ordered, positional comparison is a per-path comparison.
fn content_instability(
    first: &ObservedPassContent,
    second: &ObservedPassContent,
) -> Option<String> {
    if first.git_visible_paths.len() != second.git_visible_paths.len() {
        return Some(format!(
            "git-visible path count changed between passes ({} -> {})",
            first.git_visible_paths.len(),
            second.git_visible_paths.len()
        ));
    }
    for (before, after) in first
        .git_visible_paths
        .iter()
        .zip(second.git_visible_paths.iter())
    {
        if before != after {
            let side = if before.index_object != after.index_object {
                "index object"
            } else {
                "worktree object"
            };
            return Some(format!(
                "git-visible {side} changed between passes for {}",
                before.path.as_str()
            ));
        }
    }
    if first.writer_mutated_paths.len() != second.writer_mutated_paths.len() {
        return Some(format!(
            "writer-mutated path count changed between passes ({} -> {})",
            first.writer_mutated_paths.len(),
            second.writer_mutated_paths.len()
        ));
    }
    for (before, after) in first
        .writer_mutated_paths
        .iter()
        .zip(second.writer_mutated_paths.iter())
    {
        if before != after {
            return Some(format!(
                "writer-mutated object changed between passes for {}",
                before.path.as_str()
            ));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Index blob pass types
// ---------------------------------------------------------------------------

/// One unique index OID requested for a physical read within a single pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexBlobRequest {
    pub(crate) oid: String,
}

/// The mode-independent content of one index blob, keyed by OID. The digest is
/// over the raw object body and the size is the declared body length, so the
/// coherence layer reconstructs the per-path [`CandidateObjectIdentity`] from
/// that path's own index mode exactly as Slice 2C would label it. This is what
/// makes the same OID a physical single read yet semantic per represented path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexBlobContent {
    pub(crate) oid: String,
    pub(crate) content_sha256: String,
    pub(crate) size_bytes: u64,
}

fn index_content_parts(identity: &CandidateObjectIdentity) -> Option<(String, u64)> {
    match identity {
        CandidateObjectIdentity::RegularFile {
            content_sha256,
            size_bytes,
            ..
        } => Some((content_sha256.clone(), *size_bytes)),
        CandidateObjectIdentity::Symlink {
            target_sha256,
            target_size_bytes,
        } => Some((target_sha256.clone(), *target_size_bytes)),
        CandidateObjectIdentity::Absent => None,
    }
}

fn index_object_from_content(
    mode: SnapshotIndexMode,
    content: &IndexBlobContent,
) -> CandidateObjectIdentity {
    match mode {
        SnapshotIndexMode::Regular { executable } => CandidateObjectIdentity::RegularFile {
            content_sha256: content.content_sha256.clone(),
            size_bytes: content.size_bytes,
            executable,
        },
        SnapshotIndexMode::Symlink => CandidateObjectIdentity::Symlink {
            target_sha256: content.content_sha256.clone(),
            target_size_bytes: content.size_bytes,
        },
    }
}

// ---------------------------------------------------------------------------
// Injectable observer seams
// ---------------------------------------------------------------------------

/// Re-proves the managed execution root exactly, reusing
/// [`crate::managed_worktree_prepare::validated_execution_root`]. Called once at
/// AUTHORITY A and once at AUTHORITY C.
pub(crate) trait SnapshotAuthorityObserver {
    fn prove_execution_root(&self) -> Result<PathBuf, SnapshotCoherenceError>;
}

/// Observes closed candidate metadata, reusing Slice 2A's
/// [`observe_snapshot_metadata`].
pub(crate) trait SnapshotMetadataObserver {
    fn observe_metadata(&self, root: &Path) -> Result<SnapshotGitMetadata, SnapshotCoherenceError>;
}

/// Observes one actual worktree object, reusing Slice 2B's
/// [`observe_worktree_object`].
pub(crate) trait SnapshotWorktreeObjectObserver {
    fn observe_worktree_object(
        &self,
        root: &Path,
        path: &NormalizedWorkspacePath,
        index_entry: Option<&SnapshotIndexEntry>,
        file_mode_policy: GitFileModePolicy,
        budget: &mut SnapshotHashBudget,
    ) -> Result<CandidateObjectIdentity, SnapshotCoherenceError>;
}

/// Observes every requested unique index OID for one content pass, reusing Slice
/// 2C's [`SnapshotIndexBlobSession`]. The `requests` slice carries distinct OIDs
/// in a deterministic order; the observer reads and hashes each exactly once,
/// charging the shared pass budget once per unique OID.
pub(crate) trait SnapshotIndexBlobObserver {
    async fn observe_pass_blobs(
        &self,
        root: &Path,
        requests: &[IndexBlobRequest],
        budget: &mut SnapshotHashBudget,
    ) -> Result<Vec<IndexBlobContent>, SnapshotCoherenceError>;
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// Assemble a sampled-stable [`StableWorkspaceObservation`] through the fixed
/// `AUTHORITY A / META A / CONTENT A / META B / CONTENT B / META C / AUTHORITY C`
/// sequence with two independent content passes. No retry.
pub(crate) async fn cohere_stable_observation<A, M, W, I>(
    authority: &A,
    metadata: &M,
    worktree: &W,
    index: &I,
    writer_paths: &[NormalizedWorkspacePath],
) -> Result<StableWorkspaceObservation, SnapshotCoherenceError>
where
    A: SnapshotAuthorityObserver,
    M: SnapshotMetadataObserver,
    W: SnapshotWorktreeObjectObserver,
    I: SnapshotIndexBlobObserver,
{
    // Writer-mutated path bound and canonicalization before any content work.
    let writer_paths = canonicalize_writer_paths(writer_paths)?;

    // AUTHORITY A: an unprovable start is an authority/ownership error, never
    // instability.
    let root_a = authority.prove_execution_root()?;

    // META A and CONTENT A under one fresh physical pass budget.
    let meta_a = metadata.observe_metadata(&root_a)?;
    let projection_a = StableSnapshotMetadata::projected(&meta_a);
    let mut budget_a = SnapshotHashBudget::new();
    let content_a = observe_pass_content(
        &root_a,
        &meta_a,
        &writer_paths,
        worktree,
        index,
        &mut budget_a,
    )
    .await?;

    // META B; require META A == META B before doing any second-pass content work.
    let meta_b = metadata.observe_metadata(&root_a)?;
    let projection_b = StableSnapshotMetadata::projected(&meta_b);
    if projection_a != projection_b {
        return Err(SnapshotCoherenceError::SnapshotUnstable(
            metadata_instability(&projection_a, &projection_b),
        ));
    }

    // CONTENT B under a second fresh physical pass budget.
    let mut budget_b = SnapshotHashBudget::new();
    let content_b = observe_pass_content(
        &root_a,
        &meta_b,
        &writer_paths,
        worktree,
        index,
        &mut budget_b,
    )
    .await?;

    // META C; require META A == META B == META C.
    let meta_c = metadata.observe_metadata(&root_a)?;
    let projection_c = StableSnapshotMetadata::projected(&meta_c);
    if projection_a != projection_b || projection_b != projection_c {
        return Err(SnapshotCoherenceError::SnapshotUnstable(
            metadata_instability(&projection_a, &projection_c),
        ));
    }

    // AUTHORITY C: rerun the exact proof after all content/metadata work.
    let root_c = authority.prove_execution_root().map_err(|error| {
        SnapshotCoherenceError::SnapshotUnstable(format!(
            "managed execution root no longer proven after stable content observation: {error}"
        ))
    })?;
    if !same_path_identity(&root_a, &root_c) {
        return Err(SnapshotCoherenceError::SnapshotUnstable(
            "managed execution root changed across the coherence window".to_owned(),
        ));
    }

    // CONTENT A == CONTENT B at every identity.
    if let Some(detail) = content_instability(&content_a, &content_b) {
        return Err(SnapshotCoherenceError::SnapshotUnstable(detail));
    }

    // Semantic evidence bound: the only bound that limits the final manifest.
    semantic_evidence_bytes(
        &content_a.git_visible_paths,
        &content_a.writer_mutated_paths,
    )?;

    Ok(StableWorkspaceObservation {
        metadata: projection_a,
        git_visible_paths: content_a.git_visible_paths,
        writer_mutated_paths: content_a.writer_mutated_paths,
    })
}

/// Observe one content pass: the Git-visible union (`changed ∪ staged ∪
/// untracked`, one logical path once), then the worktree observation set
/// (Git-visible ∪ writer, one physical path once), reusing the worktree identity
/// for an overlapping Git-visible record and writer record.
async fn observe_pass_content<W, I>(
    root: &Path,
    metadata: &SnapshotGitMetadata,
    writer_paths: &[NormalizedWorkspacePath],
    worktree: &W,
    index: &I,
    budget: &mut SnapshotHashBudget,
) -> Result<ObservedPassContent, SnapshotCoherenceError>
where
    W: SnapshotWorktreeObjectObserver,
    I: SnapshotIndexBlobObserver,
{
    // Git-visible union: one logical path once, canonical path-byte order.
    let git_visible_union = canonical_unique_paths(
        metadata
            .changed_paths
            .iter()
            .chain(metadata.staged_paths.iter())
            .chain(metadata.untracked_paths.iter()),
    );

    let index_by_path: BTreeMap<&NormalizedWorkspacePath, &SnapshotIndexEntry> = metadata
        .index_entries
        .iter()
        .map(|entry| (&entry.path, entry))
        .collect();

    // Worktree observation set: Git-visible ∪ writer, one physical path once.
    let worktree_paths =
        canonical_unique_paths(git_visible_union.iter().chain(writer_paths.iter()));

    // Distinct index OIDs actually needed in this pass (dedup => physical once).
    let mut unique_oids: BTreeSet<String> = BTreeSet::new();
    for path in &git_visible_union {
        if let Some(entry) = index_by_path.get(path) {
            unique_oids.insert(entry.oid.clone());
        }
    }
    let requests = unique_oids
        .into_iter()
        .map(|oid| IndexBlobRequest { oid })
        .collect::<Vec<_>>();

    // INDEX pass: read + hash each unique OID once into this pass's cache.
    let contents = index.observe_pass_blobs(root, &requests, budget).await?;
    let content_by_oid: BTreeMap<&str, &IndexBlobContent> = contents
        .iter()
        .map(|content| (content.oid.as_str(), content))
        .collect();

    // WORKTREE pass: observe each unique physical worktree path once.
    let mut worktree_by_path: BTreeMap<NormalizedWorkspacePath, CandidateObjectIdentity> =
        BTreeMap::new();
    for path in &worktree_paths {
        let index_entry = index_by_path.get(path).copied();
        let object = worktree.observe_worktree_object(
            root,
            path,
            index_entry,
            metadata.file_mode_policy,
            budget,
        )?;
        worktree_by_path.insert(path.clone(), object);
    }

    // Build Git-visible records (index object + worktree object).
    let mut git_visible_paths = Vec::with_capacity(git_visible_union.len());
    for path in &git_visible_union {
        let index_object = match index_by_path.get(path).copied() {
            Some(entry) => {
                let content = content_by_oid.get(entry.oid.as_str()).ok_or_else(|| {
                    SnapshotCoherenceError::IndexBlobObservationUnavailable(format!(
                        "index object {oid} for {path} was not observed in this pass",
                        oid = entry.oid,
                        path = path.as_str()
                    ))
                })?;
                index_object_from_content(entry.mode, content)
            }
            None => CandidateObjectIdentity::Absent,
        };
        let worktree_object = worktree_by_path.get(path).cloned().ok_or_else(|| {
            SnapshotCoherenceError::FilesystemObservationUnavailable(format!(
                "worktree object for {path} was not observed in this pass",
                path = path.as_str()
            ))
        })?;
        git_visible_paths.push(StableGitVisiblePath {
            path: path.clone(),
            index_object,
            worktree_object,
        });
    }

    // Build writer records, reusing the single worktree observation (never a
    // second read) for any path that is also Git-visible.
    let mut writer_mutated_paths = Vec::with_capacity(writer_paths.len());
    for path in writer_paths {
        let object = worktree_by_path.get(path).cloned().ok_or_else(|| {
            SnapshotCoherenceError::FilesystemObservationUnavailable(format!(
                "worktree object for writer path {path} was not observed in this pass",
                path = path.as_str()
            ))
        })?;
        writer_mutated_paths.push(StableWriterPath {
            path: path.clone(),
            object,
        });
    }

    Ok(ObservedPassContent {
        git_visible_paths,
        writer_mutated_paths,
    })
}

// ---------------------------------------------------------------------------
// Production adapters
// ---------------------------------------------------------------------------

/// A constant placeholder path for index blob reads. Slice 2C keys its request
/// on the OID and mode only, so the logical path never reaches the process.
fn index_blob_placeholder_path() -> NormalizedWorkspacePath {
    NormalizedWorkspacePath::parse("index-blob-object").expect("constant placeholder path is valid")
}

/// Production authority adapter reusing the frozen
/// [`crate::managed_worktree_prepare::validated_execution_root`]. No authority
/// rule is duplicated. A proof failure is reported as
/// [`SnapshotCoherenceError::AuthorityUnavailable`]; the reused helper collapses
/// its own cause to a string and 2D does not re-derive it.
pub(crate) struct HostSnapshotAuthority<'a> {
    pub(crate) goal: &'a crate::goal::Goal,
    pub(crate) session: &'a crate::config::Session,
}

impl SnapshotAuthorityObserver for HostSnapshotAuthority<'_> {
    fn prove_execution_root(&self) -> Result<PathBuf, SnapshotCoherenceError> {
        crate::managed_worktree_prepare::validated_execution_root(self.goal, self.session)
            .map_err(SnapshotCoherenceError::AuthorityUnavailable)
    }
}

/// Production metadata adapter reusing Slice 2A
/// [`observe_snapshot_metadata`] through the hardened host seam.
pub(crate) struct HostSnapshotMetadata;

impl SnapshotMetadataObserver for HostSnapshotMetadata {
    fn observe_metadata(&self, root: &Path) -> Result<SnapshotGitMetadata, SnapshotCoherenceError> {
        observe_snapshot_metadata(&HostSnapshotGit, root).map_err(SnapshotCoherenceError::from)
    }
}

/// Production worktree-object adapter reusing Slice 2B
/// [`observe_worktree_object`].
pub(crate) struct HostSnapshotWorktreeObject;

impl SnapshotWorktreeObjectObserver for HostSnapshotWorktreeObject {
    fn observe_worktree_object(
        &self,
        root: &Path,
        path: &NormalizedWorkspacePath,
        index_entry: Option<&SnapshotIndexEntry>,
        file_mode_policy: GitFileModePolicy,
        budget: &mut SnapshotHashBudget,
    ) -> Result<CandidateObjectIdentity, SnapshotCoherenceError> {
        observe_worktree_object(root, path, index_entry, file_mode_policy, budget)
            .map_err(SnapshotCoherenceError::from)
    }
}

/// Production index-blob adapter reusing Slice 2C
/// [`SnapshotIndexBlobSession`]. A brand-new bounded, process-contained session
/// is spawned per pass and finished before returning, so no `cat-file` process
/// ever spans a metadata boundary. Each requested unique OID is read once; a
/// handled read error drops the session, whose `ProcessGroup` drop terminates
/// its tree.
pub(crate) struct HostSnapshotIndexBlob;

impl SnapshotIndexBlobObserver for HostSnapshotIndexBlob {
    async fn observe_pass_blobs(
        &self,
        root: &Path,
        requests: &[IndexBlobRequest],
        budget: &mut SnapshotHashBudget,
    ) -> Result<Vec<IndexBlobContent>, SnapshotCoherenceError> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let placeholder = index_blob_placeholder_path();
        let mut session = SnapshotIndexBlobSession::spawn(root)
            .await
            .map_err(SnapshotCoherenceError::from)?;
        let mut contents = Vec::with_capacity(requests.len());
        for request in requests {
            let entry = SnapshotIndexEntry {
                path: placeholder.clone(),
                mode: SnapshotIndexMode::Regular { executable: false },
                oid: request.oid.clone(),
            };
            let (next, identity) = session
                .read_entry(&entry, budget)
                .await
                .map_err(SnapshotCoherenceError::from)?;
            session = next;
            let (content_sha256, size_bytes) = index_content_parts(&identity).ok_or_else(|| {
                SnapshotCoherenceError::IndexBlobObservationUnavailable(format!(
                    "index object {} produced no content identity",
                    request.oid
                ))
            })?;
            contents.push(IndexBlobContent {
                oid: request.oid.clone(),
                content_sha256,
                size_bytes,
            });
        }
        session
            .finish()
            .await
            .map_err(SnapshotCoherenceError::from)?;
        Ok(contents)
    }
}

/// Production entry point: assemble a sampled-stable observation for an already
/// validated managed Goal/Session, wiring the host adapters. The finalizer /
/// manifest assembly (a later slice) is the only intended caller.
pub(crate) async fn observe_stable_managed_workspace(
    goal: &crate::goal::Goal,
    session: &crate::config::Session,
    writer_paths: &[NormalizedWorkspacePath],
) -> Result<StableWorkspaceObservation, SnapshotCoherenceError> {
    let authority = HostSnapshotAuthority { goal, session };
    let metadata = HostSnapshotMetadata;
    let worktree = HostSnapshotWorktreeObject;
    let index = HostSnapshotIndexBlob;
    cohere_stable_observation(&authority, &metadata, &worktree, &index, writer_paths).await
}
