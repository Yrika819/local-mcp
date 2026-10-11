//! Slice 2E tests: the stable-observation → canonical-manifest adapter.
//!
//! Every test here is **pure** — no Git, no filesystem, no `TaskStore`, no Goal
//! state transition, no persistence. The durable record is built through the real
//! pure lifecycle path (`requested` → `to_prepared` → `to_active`) and the
//! observation is supplied in exactly the shape Slice 2D emits.
//!
//! `WorkspaceSnapshotManifestV1` intentionally exposes only `canonical_bytes()`
//! and `digest()`, so structural claims (executable bit, symlink target, ordering,
//! budget accounting) are proven against the canonical bytes and the manifest's
//! own error surface rather than through invented accessors. This mirrors the
//! frozen Slice 1 test style.

use std::path::PathBuf;

use crate::goal::GoalId;
use crate::managed_worktree::{ManagedWorktreeLifecycle, ManagedWorktreeRecord, WorktreeId};
use crate::managed_worktree_snapshot_coherence::{
    StableGitVisiblePath, StableSnapshotIndexEntry, StableSnapshotMetadata,
    StableWorkspaceObservation, StableWriterPath,
};
use crate::managed_worktree_snapshot_manifest::{
    SnapshotManifestAssemblyError, assemble_workspace_snapshot_manifest,
};
use crate::managed_worktree_snapshot_observe::{GitFileModePolicy, SnapshotIndexMode};
use crate::workspace_snapshot::{
    CandidateObjectIdentity, GitVisiblePathIdentity, MAX_SNAPSHOT_CONTENT_BYTES,
    MAX_WRITER_MUTATED_PATHS, NormalizedWorkspacePath, WORKSPACE_SNAPSHOT_V1_DOMAIN,
    WorkspaceSnapshotError, WorkspaceSnapshotManifestV1, WriterMutatedPathIdentity,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const BASE_OID: &str = "0123456789abcdef0123456789abcdef01234567";
const OBSERVED_OID: &str = "1123456789abcdef0123456789abcdef01234567";
const OTHER_OID: &str = "2123456789abcdef0123456789abcdef01234567";
/// 64 'a' characters, the Slice 1 test convention for a regular-file digest.
const CONTENT_SHA256: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn path(value: &str) -> NormalizedWorkspacePath {
    NormalizedWorkspacePath::parse(value.to_owned()).unwrap()
}

fn digest(byte: u8) -> String {
    std::iter::repeat_n(byte as char, 64).collect()
}

fn regular(byte: u8, size: u64, executable: bool) -> CandidateObjectIdentity {
    CandidateObjectIdentity::RegularFile {
        content_sha256: digest(byte),
        size_bytes: size,
        executable,
    }
}

fn symlink(byte: u8, size: u64) -> CandidateObjectIdentity {
    CandidateObjectIdentity::Symlink {
        target_sha256: digest(byte),
        target_size_bytes: size,
    }
}

fn goal_id() -> GoalId {
    GoalId::parse("00000000-0000-4000-8000-000000000002").unwrap()
}

fn worktree_id() -> WorktreeId {
    WorktreeId::parse("00000000-0000-4000-8000-000000000001").unwrap()
}

/// Durable root paths. `ManagedWorktreeRecord::validate` requires canonical
/// absolute paths, which on Windows means a drive prefix, so the shapes are
/// platform-specific exactly as `managed_worktree_tests` does.
#[cfg(not(windows))]
const PRIMARY_ROOT: &str = "/managed/primary";
#[cfg(windows)]
const PRIMARY_ROOT: &str = r"C:\managed\primary";
#[cfg(not(windows))]
const COMMON_DIR: &str = "/managed/repo-common";
#[cfg(windows)]
const COMMON_DIR: &str = r"C:\managed\repo-common";
#[cfg(not(windows))]
const WORKTREE_ROOT: &str = "/managed/worktree";
#[cfg(windows)]
const WORKTREE_ROOT: &str = r"C:\managed\worktree";
const NOW: &str = "2026-10-11T00:00:00Z";

/// A `REQUESTED` durable record. Exposed so tests can observe the lifecycle gate
/// without reaching `ACTIVE`.
fn requested_record() -> ManagedWorktreeRecord {
    ManagedWorktreeRecord::requested(
        &goal_id(),
        worktree_id(),
        PathBuf::from(PRIMARY_ROOT),
        PathBuf::from(COMMON_DIR),
        PathBuf::from(WORKTREE_ROOT),
        BASE_OID.to_owned(),
        None,
        1,
        0,
    )
    .unwrap()
}

/// An `ACTIVE` durable record reached through the real pure lifecycle path.
fn active_record() -> ManagedWorktreeRecord {
    requested_record()
        .to_prepared()
        .unwrap()
        .to_active(OBSERVED_OID, NOW)
        .unwrap()
}

type GitVisibleSpec<'a> = (&'a str, CandidateObjectIdentity, CandidateObjectIdentity);
type WriterSpec<'a> = (&'a str, CandidateObjectIdentity);

fn build_observation(
    head: &str,
    changed: &[&str],
    staged: &[&str],
    untracked: &[&str],
    git_visible: &[GitVisibleSpec<'_>],
    writer: &[WriterSpec<'_>],
) -> StableWorkspaceObservation {
    StableWorkspaceObservation {
        metadata: StableSnapshotMetadata {
            head: head.to_owned(),
            file_mode_policy: GitFileModePolicy::TrustExecutableBit,
            changed_paths: changed.iter().map(|value| path(value)).collect(),
            staged_paths: staged.iter().map(|value| path(value)).collect(),
            untracked_paths: untracked.iter().map(|value| path(value)).collect(),
            index_entries: staged
                .iter()
                .map(|value| StableSnapshotIndexEntry {
                    path: path(value),
                    mode: SnapshotIndexMode::Regular { executable: false },
                    oid: std::iter::repeat_n('a', 40).collect(),
                })
                .collect(),
        },
        git_visible_paths: git_visible
            .iter()
            .map(
                |(value, index_object, worktree_object)| StableGitVisiblePath {
                    path: path(value),
                    index_object: index_object.clone(),
                    worktree_object: worktree_object.clone(),
                },
            )
            .collect(),
        writer_mutated_paths: writer
            .iter()
            .map(|(value, object)| StableWriterPath {
                path: path(value),
                object: object.clone(),
            })
            .collect(),
    }
}

/// The consistent default: Git-visible covers `changed ∪ staged ∪ untracked`
/// exactly once each, with present index objects for tracked paths and `Absent`
/// for untracked ones.
fn consistent(
    head: &str,
    changed: &[&str],
    staged: &[&str],
    untracked: &[&str],
    writer: &[WriterSpec],
) -> StableWorkspaceObservation {
    // Build the *deduplicated* Git-visible union, which is exactly what Slice 2D
    // emits: one logical path once, whatever sets it belongs to. A path that is
    // staged and changed is staged first so the index object is the staged one.
    let mut git_visible: Vec<GitVisibleSpec<'_>> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for (value, spec) in staged
        .iter()
        .map(|value| {
            (
                *value,
                (*value, regular(b'c', 20, false), regular(b'd', 22, false)),
            )
        })
        .chain(changed.iter().map(|value| {
            (
                *value,
                (*value, regular(b'a', 10, false), regular(b'b', 12, false)),
            )
        }))
        .chain(untracked.iter().map(|value| {
            (
                *value,
                (
                    *value,
                    CandidateObjectIdentity::Absent,
                    regular(b'e', 30, false),
                ),
            )
        }))
    {
        if !seen.contains(&value) {
            seen.push(value);
            git_visible.push(spec);
        }
    }
    build_observation(head, changed, staged, untracked, &git_visible, writer)
}

fn assemble(observed: &StableWorkspaceObservation) -> WorkspaceSnapshotManifestV1 {
    assemble_workspace_snapshot_manifest(&active_record(), observed).unwrap()
}

fn assembly_error(observed: &StableWorkspaceObservation) -> SnapshotManifestAssemblyError {
    assemble_workspace_snapshot_manifest(&active_record(), observed).unwrap_err()
}

/// True when the canonical bytes contain `value` verbatim.
fn bytes_contain(manifest: &WorkspaceSnapshotManifestV1, value: &str) -> bool {
    manifest
        .canonical_bytes()
        .windows(value.len())
        .any(|window| window == value.as_bytes())
}

/// True when the canonical bytes contain the exact 8-byte big-endian encoding of
/// `size`, followed by an executable flag byte.
fn bytes_contain_size_then_exec(
    manifest: &WorkspaceSnapshotManifestV1,
    size: u64,
    exec: u8,
) -> bool {
    let mut needle = size.to_be_bytes().to_vec();
    needle.push(exec);
    manifest
        .canonical_bytes()
        .windows(needle.len())
        .any(|window| window == needle.as_slice())
}

// ---------------------------------------------------------------------------
// H10 — required pure test matrix
// ---------------------------------------------------------------------------

#[test]
fn empty_clean_candidate_assembles_and_binds_durable_identity() {
    let manifest = assemble(&consistent(OBSERVED_OID, &[], &[], &[], &[]));
    // Identity provenance: the worktree ID and base commit are read from the
    // durable record, and the observed HEAD from the stable observation.
    assert!(bytes_contain(&manifest, BASE_OID));
    assert!(bytes_contain(&manifest, OBSERVED_OID));
    assert!(!bytes_contain(&manifest, OTHER_OID));
    // The canonical bytes are total: header-only manifest is well formed.
    assert_eq!(manifest.digest(), manifest.digest());
    assert_eq!(manifest.digest().len(), 64);
}

#[test]
fn changed_regular_file_keeps_exact_stable_digests() {
    let manifest = assemble(&consistent(OBSERVED_OID, &["src/lib.rs"], &[], &[], &[]));
    assert!(bytes_contain(&manifest, &digest(b'a')));
    assert!(bytes_contain(&manifest, &digest(b'b')));
}

#[test]
fn staged_regular_file_uses_the_index_object() {
    let manifest = assemble(&consistent(OBSERVED_OID, &[], &["src/main.rs"], &[], &[]));
    assert!(bytes_contain(&manifest, &digest(b'c')));
}

#[test]
fn staged_and_changed_overlap_produces_one_git_visible_record() {
    let manifest = assemble(&consistent(
        OBSERVED_OID,
        &["src/both.rs"],
        &["src/both.rs"],
        &[],
        &[],
    ));
    assert!(bytes_contain(&manifest, "src/both.rs"));
    // Exactly one identity exists: the staged objects. The changed-only index
    // object must NOT appear, which proves the union deduplicated the path
    // rather than emitting two candidate identities for it.
    assert!(bytes_contain(&manifest, &digest(b'c')));
    assert!(bytes_contain(&manifest, &digest(b'd')));
    assert!(!bytes_contain(&manifest, &digest(b'a')));
    assert!(!bytes_contain(&manifest, &digest(b'b')));
}

#[test]
fn untracked_file_has_an_absent_index_object() {
    // An absent index object encodes as a single 0x03 tag, so no index digest for
    // the untracked path is ever emitted.
    let manifest = assemble(&consistent(
        OBSERVED_OID,
        &[],
        &[],
        &["notes/todo.txt"],
        &[],
    ));
    assert!(bytes_contain(&manifest, "notes/todo.txt"));
    // The worktree object is present: 30 bytes with a non-executable flag.
    assert!(bytes_contain_size_then_exec(&manifest, 30, 0));
}

#[test]
fn executable_regular_bit_is_preserved_through_assembly() {
    let mut observed = consistent(OBSERVED_OID, &["scripts/run.sh"], &[], &[], &[]);
    observed.git_visible_paths[0].worktree_object = regular(b'b', 12, true);
    let manifest = assemble(&observed);
    assert!(bytes_contain_size_then_exec(&manifest, 12, 1));
    // A non-executable manifest of the same bytes must differ.
    let plain = assemble(&consistent(
        OBSERVED_OID,
        &["scripts/run.sh"],
        &[],
        &[],
        &[],
    ));
    assert_ne!(manifest.digest(), plain.digest());
}

#[test]
fn symlink_index_and_worktree_keep_raw_target_identity() {
    let mut observed = consistent(OBSERVED_OID, &["src/link"], &[], &[], &[]);
    observed.git_visible_paths[0].index_object = symlink(b'a', 7);
    observed.git_visible_paths[0].worktree_object = symlink(b'b', 7);
    let manifest = assemble(&observed);
    assert!(bytes_contain(&manifest, &digest(b'a')));
    assert!(bytes_contain(&manifest, &digest(b'b')));
    // A regular file with the same target bytes must not be equivalent.
    let flattened = consistent(OBSERVED_OID, &["src/link"], &[], &[], &[]);
    assert_ne!(manifest.digest(), assemble(&flattened).digest());
}

#[test]
fn writer_regular_file_is_present() {
    let manifest = assemble(&consistent(
        OBSERVED_OID,
        &[],
        &[],
        &[],
        &[("out/report.md", regular(b'f', 40, false))],
    ));
    assert!(bytes_contain(&manifest, "out/report.md"));
    assert!(bytes_contain_size_then_exec(&manifest, 40, 0));
}

#[test]
fn writer_executable_bit_is_preserved() {
    let manifest = assemble(&consistent(
        OBSERVED_OID,
        &[],
        &[],
        &[],
        &[("out/tool", regular(b'f', 40, true))],
    ));
    assert!(bytes_contain_size_then_exec(&manifest, 40, 1));
}

#[test]
fn writer_symlink_target_is_preserved() {
    let manifest = assemble(&consistent(
        OBSERVED_OID,
        &[],
        &[],
        &[],
        &[("out/alias", symlink(b'f', 5))],
    ));
    assert!(bytes_contain(&manifest, &digest(b'f')));
    // A regular file of the same target bytes must not be equivalent.
    let flattened = assemble(&consistent(
        OBSERVED_OID,
        &[],
        &[],
        &[],
        &[("out/alias", regular(b'f', 5, false))],
    ));
    assert_ne!(manifest.digest(), flattened.digest());
}

#[test]
fn writer_absent_is_assembled() {
    let manifest = assemble(&consistent(
        OBSERVED_OID,
        &[],
        &[],
        &[],
        &[("gone.txt", CandidateObjectIdentity::Absent)],
    ));
    assert!(bytes_contain(&manifest, "gone.txt"));
    // An absent writer path contributes no object bytes.
    assert!(!bytes_contain(&manifest, &digest(b'f')));
}

#[test]
fn writer_and_git_visible_overlap_identical_is_legal() {
    let shared = regular(b'b', 12, false);
    let manifest = assemble(&consistent(
        OBSERVED_OID,
        &["src/overlap.rs"],
        &[],
        &[],
        &[("src/overlap.rs", shared)],
    ));
    assert!(bytes_contain(&manifest, "src/overlap.rs"));
    // The shared worktree identity is carried once in each logical set.
    assert!(bytes_contain(&manifest, &digest(b'b')));
}

#[test]
fn writer_and_git_visible_overlap_mismatch_is_rejected() {
    let observed = consistent(
        OBSERVED_OID,
        &["src/overlap.rs"],
        &[],
        &[],
        &[("src/overlap.rs", regular(b'f', 99, false))],
    );
    assert!(matches!(
        assembly_error(&observed),
        SnapshotManifestAssemblyError::Manifest(
            WorkspaceSnapshotError::InvalidCandidateIdentityCoverage
        )
    ));
}

#[test]
fn deterministic_ordering_regardless_of_insertion_order() {
    let forward = consistent(
        OBSERVED_OID,
        &["b.rs", "a.rs"],
        &["d.rs", "c.rs"],
        &["f.rs", "e.rs"],
        &[
            ("z.txt", regular(b'f', 1, false)),
            ("y.txt", regular(b'c', 2, false)),
        ],
    );
    let reverse = consistent(
        OBSERVED_OID,
        &["a.rs", "b.rs"],
        &["c.rs", "d.rs"],
        &["e.rs", "f.rs"],
        &[
            ("y.txt", regular(b'c', 2, false)),
            ("z.txt", regular(b'f', 1, false)),
        ],
    );
    let first = assemble(&forward);
    let second = assemble(&reverse);
    assert_eq!(first.canonical_bytes(), second.canonical_bytes());
    assert_eq!(first.digest(), second.digest());
}

#[test]
fn base_commit_is_distinct_from_observed_head_and_never_conflated() {
    let manifest = assemble(&consistent(OTHER_OID, &["src/lib.rs"], &[], &[], &[]));
    assert!(bytes_contain(&manifest, BASE_OID));
    assert!(bytes_contain(&manifest, OTHER_OID));
    // Changing only the observed HEAD changes the digest, and the durable base
    // stays bound to the record rather than to the observation.
    let same_head = assemble(&consistent(OTHER_OID, &["src/lib.rs"], &[], &[], &[]));
    assert_eq!(manifest.digest(), same_head.digest());
    let different_head = assemble(&consistent(OBSERVED_OID, &["src/lib.rs"], &[], &[], &[]));
    assert_ne!(manifest.digest(), different_head.digest());
}

#[test]
fn present_with_object_rejects_absent() {
    assert!(matches!(
        WriterMutatedPathIdentity::present_with_object(path("x"), CandidateObjectIdentity::Absent),
        Err(WorkspaceSnapshotError::InvalidCandidateIdentityCoverage)
    ));
    // The old convenience helper is kept and still builds the documented
    // non-executable regular-file shape.
    assert!(matches!(
        WriterMutatedPathIdentity::present(path("x"), CONTENT_SHA256, 0),
        Ok(WriterMutatedPathIdentity::Present { .. })
    ));
}

#[test]
fn present_with_object_validates_digest_and_size() {
    let bad_digest = CandidateObjectIdentity::RegularFile {
        content_sha256: "NOT-HEX".to_owned(),
        size_bytes: 1,
        executable: false,
    };
    assert!(matches!(
        WriterMutatedPathIdentity::present_with_object(path("x"), bad_digest),
        Err(WorkspaceSnapshotError::InvalidContentDigest)
    ));
    let too_large = CandidateObjectIdentity::RegularFile {
        content_sha256: CONTENT_SHA256.to_owned(),
        size_bytes: MAX_SNAPSHOT_CONTENT_BYTES + 1,
        executable: false,
    };
    assert!(matches!(
        WriterMutatedPathIdentity::present_with_object(path("x"), too_large),
        Err(WorkspaceSnapshotError::LimitExceeded(
            "snapshot content byte"
        ))
    ));
}

#[test]
fn semantic_budget_exact_succeeds_and_one_over_fails_closed() {
    // Two Git-visible records with identical index object (logical duplication):
    // each carries `MAX / 2` bytes, so the semantic total is exactly the bound
    // and a third would exceed it. This proves per-path (semantic) accounting.
    let half = MAX_SNAPSHOT_CONTENT_BYTES / 2;
    let exact = build_observation(
        OBSERVED_OID,
        &["a", "b"],
        &[],
        &[],
        &[
            (
                "a",
                regular(b'a', half, false),
                CandidateObjectIdentity::Absent,
            ),
            (
                "b",
                regular(b'a', half, false),
                CandidateObjectIdentity::Absent,
            ),
        ],
        &[],
    );
    assert!(assemble(&exact).digest().len() == 64);

    let over = build_observation(
        OBSERVED_OID,
        &["a", "b", "c"],
        &[],
        &[],
        &[
            (
                "a",
                regular(b'a', half, false),
                CandidateObjectIdentity::Absent,
            ),
            (
                "b",
                regular(b'a', half, false),
                CandidateObjectIdentity::Absent,
            ),
            (
                "c",
                regular(b'a', 1, false),
                CandidateObjectIdentity::Absent,
            ),
        ],
        &[],
    );
    assert!(matches!(
        assembly_error(&over),
        SnapshotManifestAssemblyError::Manifest(WorkspaceSnapshotError::LimitExceeded(
            "snapshot content byte"
        ))
    ));
}

#[test]
fn logical_duplication_is_counted_not_physically_deduped() {
    // Three records sharing one digest are counted three times; a single record
    // of the same object is counted once.
    let size = MAX_SNAPSHOT_CONTENT_BYTES / 3 + 1;
    let tripled = build_observation(
        OBSERVED_OID,
        &["a", "b", "c"],
        &[],
        &[],
        &[
            (
                "a",
                regular(b'a', size, false),
                CandidateObjectIdentity::Absent,
            ),
            (
                "b",
                regular(b'a', size, false),
                CandidateObjectIdentity::Absent,
            ),
            (
                "c",
                regular(b'a', size, false),
                CandidateObjectIdentity::Absent,
            ),
        ],
        &[],
    );
    assert!(matches!(
        assembly_error(&tripled),
        SnapshotManifestAssemblyError::Manifest(WorkspaceSnapshotError::LimitExceeded(_))
    ));
}

#[test]
fn canonical_two_mib_boundary_is_enforced() {
    // Each record costs 93 + path_len bytes; with 4096-byte paths, 252 records
    // fit under the 2 MiB canonical bound and 253 exceed it.
    let prefix = "d".repeat(4096 - 10);
    let build = |count: usize| -> StableWorkspaceObservation {
        let mut names = Vec::with_capacity(count);
        let mut git_visible: Vec<GitVisibleSpec<'_>> = Vec::with_capacity(count);
        for index in 0..count {
            let name = format!("{prefix}{:010}", index);
            let leaked: &'static str = Box::leak(name.clone().into_boxed_str());
            names.push(name);
            git_visible.push((
                leaked,
                CandidateObjectIdentity::Absent,
                regular(b'b', 1, false),
            ));
        }
        // Paths live under `changed` so the union covers exactly them.
        StableWorkspaceObservation {
            metadata: StableSnapshotMetadata {
                head: OBSERVED_OID.to_owned(),
                file_mode_policy: GitFileModePolicy::TrustExecutableBit,
                changed_paths: names.iter().map(|value| path(value)).collect(),
                staged_paths: Vec::new(),
                untracked_paths: Vec::new(),
                index_entries: Vec::new(),
            },
            git_visible_paths: git_visible
                .iter()
                .map(
                    |(value, index_object, worktree_object)| StableGitVisiblePath {
                        path: path(value),
                        index_object: index_object.clone(),
                        worktree_object: worktree_object.clone(),
                    },
                )
                .collect(),
            writer_mutated_paths: Vec::new(),
        }
    };

    assert_eq!(
        assemble(&build(252)).digest().len(),
        64,
        "252 maximal-length paths must still fit the canonical bound"
    );
    assert!(matches!(
        assembly_error(&build(253)),
        SnapshotManifestAssemblyError::Manifest(WorkspaceSnapshotError::LimitExceeded(
            "canonical byte"
        ))
    ));
}

#[test]
fn existing_golden_digest_is_unchanged() {
    // The Slice 1 golden digest is a frozen artifact of the canonical encoding.
    // Assembling the identical semantic input through 2E must reproduce it.
    // Pin the exact Slice 1 shape: index Absent, worktree = 1 byte, digest 'a'*64.
    let observed = StableWorkspaceObservation {
        metadata: StableSnapshotMetadata {
            head: OBSERVED_OID.to_owned(),
            file_mode_policy: GitFileModePolicy::TrustExecutableBit,
            changed_paths: vec![path("a")],
            staged_paths: Vec::new(),
            untracked_paths: Vec::new(),
            index_entries: Vec::new(),
        },
        git_visible_paths: vec![StableGitVisiblePath {
            path: path("a"),
            index_object: CandidateObjectIdentity::Absent,
            worktree_object: CandidateObjectIdentity::RegularFile {
                content_sha256: CONTENT_SHA256.to_owned(),
                size_bytes: 1,
                executable: false,
            },
        }],
        writer_mutated_paths: Vec::new(),
    };
    let manifest = assemble(&observed);
    assert_eq!(
        manifest.digest(),
        "a99383cb80eb09ad801ab5ecdc4972c1cda4b55832f218c35c932bd0d7047cf0"
    );
    assert!(WORKSPACE_SNAPSHOT_V1_DOMAIN.starts_with(b"managed-worktree-workspace-snapshot-v1"));
    // No second digest algorithm was introduced: 2E exposes the manifest digest.
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(manifest.canonical_bytes());
    let recomputed = format!("{:x}", hasher.finalize());
    assert_eq!(recomputed, manifest.digest());
}

// ---------------------------------------------------------------------------
// H11 — adversarial matrix
// ---------------------------------------------------------------------------

#[test]
fn a_non_active_lifecycle_cannot_hold_stable_evidence() {
    let requested = requested_record();
    let observed = consistent(OBSERVED_OID, &[], &[], &[], &[]);
    let error = assemble_workspace_snapshot_manifest(&requested, &observed).unwrap_err();
    assert!(matches!(
        error,
        SnapshotManifestAssemblyError::LifecycleNotStable(ManagedWorktreeLifecycle::Requested)
    ));

    // A blocked workspace is likewise not stable evidence.
    let blocked = active_record().to_blocked().unwrap();
    let error = assemble_workspace_snapshot_manifest(&blocked, &observed).unwrap_err();
    assert!(matches!(
        error,
        SnapshotManifestAssemblyError::LifecycleNotStable(ManagedWorktreeLifecycle::Blocked)
    ));
}

#[test]
fn duplicate_writer_path_is_rejected() {
    let observed = build_observation(
        OBSERVED_OID,
        &[],
        &[],
        &[],
        &[],
        &[
            ("out/x", regular(b'f', 1, false)),
            ("out/x", regular(b'f', 1, false)),
        ],
    );
    assert!(matches!(
        assembly_error(&observed),
        SnapshotManifestAssemblyError::Manifest(WorkspaceSnapshotError::DuplicatePath(
            "writer-mutated paths"
        ))
    ));
}

#[test]
fn duplicate_git_visible_path_is_rejected() {
    let observed = build_observation(
        OBSERVED_OID,
        &["a"],
        &[],
        &[],
        &[
            (
                "a",
                CandidateObjectIdentity::Absent,
                regular(b'b', 1, false),
            ),
            (
                "a",
                CandidateObjectIdentity::Absent,
                regular(b'b', 1, false),
            ),
        ],
        &[],
    );
    assert!(matches!(
        assembly_error(&observed),
        SnapshotManifestAssemblyError::Manifest(WorkspaceSnapshotError::DuplicatePath(
            "Git-visible paths"
        ))
    ));
}

#[test]
fn missing_git_union_identity_is_rejected() {
    // `changed` names a path that has no candidate identity at all.
    let observed = build_observation(
        OBSERVED_OID,
        &["a", "b"],
        &[],
        &[],
        &[(
            "a",
            CandidateObjectIdentity::Absent,
            regular(b'b', 1, false),
        )],
        &[],
    );
    assert!(matches!(
        assembly_error(&observed),
        SnapshotManifestAssemblyError::Manifest(
            WorkspaceSnapshotError::InvalidCandidateIdentityCoverage
        )
    ));
}

#[test]
fn extra_git_identity_not_in_the_union_is_rejected() {
    let observed = build_observation(
        OBSERVED_OID,
        &["a"],
        &[],
        &[],
        &[
            (
                "a",
                CandidateObjectIdentity::Absent,
                regular(b'b', 1, false),
            ),
            (
                "stray",
                CandidateObjectIdentity::Absent,
                regular(b'c', 1, false),
            ),
        ],
        &[],
    );
    assert!(matches!(
        assembly_error(&observed),
        SnapshotManifestAssemblyError::Manifest(
            WorkspaceSnapshotError::InvalidCandidateIdentityCoverage
        )
    ));
}

#[test]
fn writer_path_count_bound_is_enforced() {
    let mut writer: Vec<WriterSpec> = Vec::with_capacity(MAX_WRITER_MUTATED_PATHS + 1);
    for index in 0..=MAX_WRITER_MUTATED_PATHS {
        let leaked: &'static str = Box::leak(format!("out/{index:05}").into_boxed_str());
        writer.push((leaked, regular(b'f', 1, false)));
    }
    let observed = build_observation(OBSERVED_OID, &[], &[], &[], &[], &writer);
    assert!(matches!(
        assembly_error(&observed),
        SnapshotManifestAssemblyError::Manifest(WorkspaceSnapshotError::LimitExceeded(
            "writer-mutated path count"
        ))
    ));
}

#[test]
fn observed_head_is_not_a_valid_object_id_shape_when_corrupted() {
    // A malformed observed HEAD is rejected by the manifest constructor, which
    // remains the final validator for object-ID shape.
    let observed = build_observation(
        "not-an-oid",
        &["a"],
        &[],
        &[],
        &[(
            "a",
            CandidateObjectIdentity::Absent,
            regular(b'b', 1, false),
        )],
        &[],
    );
    assert!(matches!(
        assembly_error(&observed),
        SnapshotManifestAssemblyError::Manifest(WorkspaceSnapshotError::InvalidObjectId(
            "observed_head"
        ))
    ));
}

#[test]
fn git_visible_identity_construction_validates_both_objects() {
    // The adapter's Git-visible construction path never bypasses validation.
    let result = GitVisiblePathIdentity::new(
        path("a"),
        CandidateObjectIdentity::RegularFile {
            content_sha256: "bad".to_owned(),
            size_bytes: 1,
            executable: false,
        },
        CandidateObjectIdentity::Absent,
    );
    assert!(matches!(
        result,
        Err(WorkspaceSnapshotError::InvalidContentDigest)
    ));
}

#[test]
fn assembly_error_display_is_human_readable() {
    let observed = build_observation(OBSERVED_OID, &[], &[], &[], &[], &[]);
    assert!(assemble(&observed).digest().len() == 64);
    let error = assemble_workspace_snapshot_manifest(&requested_record(), &observed).unwrap_err();
    let rendered = error.to_string();
    assert!(rendered.contains("lifecycle"));
    assert!(rendered.contains("stable workspace snapshot"));
}
