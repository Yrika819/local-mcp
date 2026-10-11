//! Managed Worktrees V1 Phase 5 Slice 2E — stable observation → canonical manifest.
//!
//! This slice is intentionally **pure and narrow**. It consumes the already
//! sampled-stable [`StableWorkspaceObservation`] produced by Slice 2D plus the
//! durable [`ManagedWorktreeRecord`] identity, and produces a canonical
//! [`WorkspaceSnapshotManifestV1`]. It performs **no** Git observation, **no**
//! filesystem read, **no** `TaskStore` query, no Goal state transition, no
//! finalizer wiring, and no persistence.
//!
//! ## Authority split (why 2E adds none)
//!
//! * **Durable identity** (`worktree_id`, `base_commit`) is owned by
//!   [`ManagedWorktreeRecord`]. A caller cannot substitute an arbitrary base SHA
//!   or worktree ID; both are read from the record.
//! * **Observed state** (`observed_head`, `changed`, `staged`, `untracked`,
//!   Git-visible identities, writer-mutated identities) is owned by the 2D
//!   [`StableWorkspaceObservation`]. 2D already proved mechanical stability
//!   across `AUTHORITY A / META A / CONTENT A / META B / CONTENT B / META C /
//!   AUTHORITY C`, so 2E copies its identities verbatim — no rehash, no digest
//!   normalization, no mode inference.
//! * **Validation** remains owned by
//!   [`WorkspaceSnapshotManifestV1::new`], which stays the final authority for
//!   worktree-ID shape, object-ID shape, duplicate paths, exact Git-visible
//!   coverage, writer consistency, the semantic content budget, and the canonical
//!   2 MiB bound. 2E never reimplements those rules.
//!
//! ## Canonical digest
//!
//! 2E exposes the existing [`WorkspaceSnapshotManifestV1::digest`] and introduces
//! **no** second snapshot digest algorithm. The domain stays
//! `managed-worktree-workspace-snapshot-v1\0`, so the frozen golden digest is
//! unchanged.
//!
//! ## `base_commit` vs `observed_head`
//!
//! These are different values with different owners and are never conflated:
//! `base_commit` is the durable commit the managed candidate was created from,
//! while `observed_head` is the candidate's currently observed HEAD. 2E never
//! substitutes one for the other and never consults the primary workspace HEAD
//! (that is `BASE_ADVANCED` review evidence, owned by a later slice).

#![expect(
    dead_code,
    reason = "Slice 2E freezes the pure stable-observation-to-manifest adapter before the finalizer re-observation seam is authorized to call it. Its error type and helpers are the exact surface that later slice consumes."
)]

use crate::managed_worktree::{ManagedWorktreeLifecycle, ManagedWorktreeRecord};
use crate::managed_worktree_snapshot_coherence::{
    StableGitVisiblePath, StableWorkspaceObservation, StableWriterPath,
};
use crate::orchestrator_error::OrchestratorError;
use crate::workspace_snapshot::{
    CandidateObjectIdentity, GitVisiblePathIdentity, NormalizedWorkspacePath,
    WorkspaceSnapshotError, WorkspaceSnapshotManifestV1, WriterMutatedPathIdentity,
};

/// A narrow Slice 2E assembly error. Durable-record failures stay distinct from
/// pure manifest-construction failures so a caller can tell "the record is not
/// in a state that can hold stable managed evidence" from "the observation is not
/// internally consistent".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotManifestAssemblyError {
    /// The durable managed-worktree record itself is malformed or carries an
    /// invalid durable identity (`base_commit`, worktree ID, path overlap, ...).
    DurableRecordInvalid(String),
    /// The record is well-formed but is not in a lifecycle that can hold stable
    /// managed evidence. Only [`ManagedWorktreeLifecycle::Active`] qualifies,
    /// matching the lifecycle requirement `managed_worktree_prepare` enforces
    /// before a managed root may be executed against.
    LifecycleNotStable(ManagedWorktreeLifecycle),
    /// The canonical manifest constructor — the final pure validator — rejected
    /// the assembled candidate.
    Manifest(WorkspaceSnapshotError),
}

impl std::fmt::Display for SnapshotManifestAssemblyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DurableRecordInvalid(detail) => {
                write!(formatter, "managed worktree record is invalid: {detail}")
            }
            Self::LifecycleNotStable(lifecycle) => write!(
                formatter,
                "managed worktree lifecycle {lifecycle:?} cannot hold a stable workspace snapshot"
            ),
            Self::Manifest(error) => write!(formatter, "workspace snapshot manifest: {error}"),
        }
    }
}

impl std::error::Error for SnapshotManifestAssemblyError {}

impl From<WorkspaceSnapshotError> for SnapshotManifestAssemblyError {
    fn from(error: WorkspaceSnapshotError) -> Self {
        Self::Manifest(error)
    }
}

/// Validate the durable managed-worktree record as a *pure* prerequisite for
/// holding stable managed evidence.
///
/// This reuses [`ManagedWorktreeRecord::validate`] (the durable-integrity
/// validator, which touches neither the filesystem nor Git) and adds exactly one
/// 2E-specific rule: the lifecycle must be
/// [`ManagedWorktreeLifecycle::Active`]. That is the same lifecycle 2D's
/// authority seam requires before a managed execution root is considered
/// executable, so 2E refuses to assemble evidence for a workspace that could
/// never have been observed in the first place.
///
/// 2E deliberately performs **no** fresh filesystem/Git reconciliation here: 2D
/// owns mechanical observation and this slice does not broaden that authority.
fn validate_stable_record(
    record: &ManagedWorktreeRecord,
) -> Result<(), SnapshotManifestAssemblyError> {
    record.validate().map_err(|error| match error {
        OrchestratorError::CorruptGoal(detail) => {
            SnapshotManifestAssemblyError::DurableRecordInvalid(detail)
        }
        other => SnapshotManifestAssemblyError::DurableRecordInvalid(other.to_string()),
    })?;
    let lifecycle = record.lifecycle();
    if lifecycle != ManagedWorktreeLifecycle::Active {
        return Err(SnapshotManifestAssemblyError::LifecycleNotStable(lifecycle));
    }
    Ok(())
}

/// Adapt one Slice 2D Git-visible record into the canonical identity.
///
/// The exact stable objects are moved through unchanged: **no rehash, no digest
/// normalization, no mode inference**. `index_object` is `Absent` for a union
/// path that has no index entry (an untracked path), which the manifest
/// constructor already treats as a valid candidate identity.
fn git_visible_identity(
    record: &StableGitVisiblePath,
) -> Result<GitVisiblePathIdentity, SnapshotManifestAssemblyError> {
    Ok(GitVisiblePathIdentity::new(
        record.path.clone(),
        record.index_object.clone(),
        record.worktree_object.clone(),
    )?)
}

/// Adapt one Slice 2D writer-mutated record into the canonical identity.
///
/// An `Absent` object becomes [`WriterMutatedPathIdentity::absent`]. A present
/// object routes through [`WriterMutatedPathIdentity::present_with_object`], which
/// preserves the executable bit of a regular file and the raw target identity of
/// a symlink and validates digest/size through the same
/// [`CandidateObjectIdentity`] validation the manifest constructor uses.
///
/// Overlap with a Git-visible path is legal and preserved; Slice 1's writer
/// consistency rule (re-checked here by the manifest constructor) remains
/// authoritative rather than being restated differently.
fn writer_identity(
    record: &StableWriterPath,
) -> Result<WriterMutatedPathIdentity, SnapshotManifestAssemblyError> {
    match &record.object {
        CandidateObjectIdentity::Absent => {
            Ok(WriterMutatedPathIdentity::absent(record.path.clone()))
        }
        object => Ok(WriterMutatedPathIdentity::present_with_object(
            record.path.clone(),
            object.clone(),
        )?),
    }
}

/// Assemble a canonical [`WorkspaceSnapshotManifestV1`] from the durable managed
/// identity plus the already sampled-stable Slice 2D observation.
///
/// Identity provenance is fixed by construction:
///
/// * `worktree_id` comes from [`ManagedWorktreeRecord::worktree_id`];
/// * `base_commit` comes from [`ManagedWorktreeRecord::base_commit`];
/// * `observed_head` comes from [`StableSnapshotMetadata::head`];
/// * `changed` / `staged` / `untracked` come from the stable metadata sets;
/// * Git-visible and writer-mutated identities come from the stable content
///   passes, one logical path once each.
///
/// No caller-supplied arbitrary base SHA or worktree ID is accepted, because the
/// record already owns those values. Observed HEAD is never substituted for the
/// base commit, and the primary workspace HEAD is never consulted.
pub(crate) fn assemble_workspace_snapshot_manifest(
    record: &ManagedWorktreeRecord,
    observation: &StableWorkspaceObservation,
) -> Result<WorkspaceSnapshotManifestV1, SnapshotManifestAssemblyError> {
    // Durable identity first: a record that cannot own stable evidence must fail
    // before any content is trusted.
    validate_stable_record(record)?;

    let metadata = &observation.metadata;

    // Metadata sets: exact stable sets, no re-derivation.
    let changed_paths: Vec<NormalizedWorkspacePath> = metadata.changed_paths.clone();
    let staged_paths: Vec<NormalizedWorkspacePath> = metadata.staged_paths.clone();
    let untracked_paths: Vec<NormalizedWorkspacePath> = metadata.untracked_paths.clone();

    // Git-visible identities: verbatim stable objects.
    let git_visible_paths = observation
        .git_visible_paths
        .iter()
        .map(git_visible_identity)
        .collect::<Result<Vec<_>, _>>()?;

    // Writer-mutated identities: verbatim stable objects, kind preserved.
    let writer_mutated_paths = observation
        .writer_mutated_paths
        .iter()
        .map(writer_identity)
        .collect::<Result<Vec<_>, _>>()?;

    // WorkspaceSnapshotManifestV1::new remains the final pure validator. It
    // re-checks worktree-ID shape, object-ID shapes, duplicate paths, exact
    // Git-visible coverage against the union of the three metadata sets, writer
    // consistency, the semantic content budget, and the canonical 2 MiB bound.
    // 2E does not restate any of those rules.
    let manifest = WorkspaceSnapshotManifestV1::new(
        record.worktree_id().clone(),
        record.base_commit(),
        metadata.head.clone(),
        changed_paths,
        staged_paths,
        untracked_paths,
        git_visible_paths,
        writer_mutated_paths,
    )?;

    Ok(manifest)
}
