//! Slice 2D tests: pure contract tests plus a deterministic, barrier-free race
//! matrix driven by injected fake observer sequences. Races are made
//! deterministic by scripting per-pass observer responses (metadata A/B/C,
//! worktree, index, authority), never by `sleep`. Real filesystem/Git is
//! exercised only where it adds semantic proof (the end-to-end happy path).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use sha2::Digest as _;

use crate::managed_worktree_snapshot_coherence::*;
use crate::managed_worktree_snapshot_index_blob::IndexBlobError;
use crate::managed_worktree_snapshot_object::SnapshotObjectError;
use crate::managed_worktree_snapshot_observe::{
    GitFileModePolicy, SnapshotGitMetadata, SnapshotIndexEntry, SnapshotIndexMode,
    SnapshotMetadataError,
};
use crate::workspace_snapshot::{
    CandidateObjectIdentity, MAX_SNAPSHOT_CONTENT_BYTES, NormalizedWorkspacePath,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const HEAD: &str = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0";
const MIB: u64 = 1024 * 1024;

fn path(value: &str) -> NormalizedWorkspacePath {
    NormalizedWorkspacePath::parse(value.to_owned()).unwrap()
}

fn digest(byte: u8) -> String {
    std::iter::repeat_n(byte as char, 64).collect()
}

fn oid(byte: u8) -> String {
    std::iter::repeat_n(byte as char, 40).collect()
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

const REG: SnapshotIndexMode = SnapshotIndexMode::Regular { executable: false };

fn index_entry(value: &str, mode: SnapshotIndexMode, object: u8) -> SnapshotIndexEntry {
    SnapshotIndexEntry {
        path: path(value),
        mode,
        oid: oid(object),
    }
}

fn metadata(
    head: &str,
    policy: GitFileModePolicy,
    changed: &[&str],
    staged: &[&str],
    untracked: &[&str],
    entries: Vec<SnapshotIndexEntry>,
) -> SnapshotGitMetadata {
    SnapshotGitMetadata {
        head: head.to_owned(),
        file_mode_policy: policy,
        changed_paths: changed.iter().map(|p| path(p)).collect(),
        staged_paths: staged.iter().map(|p| path(p)).collect(),
        untracked_paths: untracked.iter().map(|p| path(p)).collect(),
        index_entries: entries,
    }
}

fn content(object: u8, byte: u8, size: u64) -> IndexBlobContent {
    IndexBlobContent {
        oid: oid(object),
        content_sha256: digest(byte),
        size_bytes: size,
    }
}

fn worktree_map(
    entries: Vec<(&str, CandidateObjectIdentity)>,
) -> BTreeMap<String, CandidateObjectIdentity> {
    entries
        .into_iter()
        .map(|(p, object)| (p.to_owned(), object))
        .collect()
}

fn index_map(entries: Vec<IndexBlobContent>) -> BTreeMap<String, IndexBlobContent> {
    entries.into_iter().map(|c| (c.oid.clone(), c)).collect()
}

// ---------------------------------------------------------------------------
// Injectable fakes (call-indexed, deterministic; no sleeps, no threads)
// ---------------------------------------------------------------------------

struct FakeAuthority {
    outcomes: VecDeque<Result<PathBuf, SnapshotCoherenceError>>,
    calls: Cell<usize>,
}

impl FakeAuthority {
    fn always_ok(path: &str) -> Self {
        Self {
            outcomes: VecDeque::from(vec![Ok(PathBuf::from(path))]),
            calls: Cell::new(0),
        }
    }

    fn script(outcomes: Vec<Result<PathBuf, SnapshotCoherenceError>>) -> Self {
        Self {
            outcomes: outcomes.into(),
            calls: Cell::new(0),
        }
    }

    fn prove_at(&self, index: usize) -> Result<PathBuf, SnapshotCoherenceError> {
        let call = self.calls.get();
        self.calls.set(call + 1);
        // Clamp the last scripted outcome so a one-outcome script serves every
        // AUTHORITY A / C call identically.
        self.outcomes[index.min(self.outcomes.len().saturating_sub(1))].clone()
    }
}

impl SnapshotAuthorityObserver for FakeAuthority {
    fn prove_execution_root(&self) -> Result<PathBuf, SnapshotCoherenceError> {
        self.prove_at(self.calls.get())
    }
}

struct FakeMetadata {
    metas: VecDeque<SnapshotGitMetadata>,
    calls: Cell<usize>,
}

impl FakeMetadata {
    fn stable(meta: SnapshotGitMetadata) -> Self {
        Self {
            metas: VecDeque::from(vec![meta]),
            calls: Cell::new(0),
        }
    }

    fn script(metas: Vec<SnapshotGitMetadata>) -> Self {
        Self {
            metas: metas.into(),
            calls: Cell::new(0),
        }
    }
}

impl SnapshotMetadataObserver for FakeMetadata {
    fn observe_metadata(
        &self,
        _root: &Path,
    ) -> Result<SnapshotGitMetadata, SnapshotCoherenceError> {
        let call = self.calls.get();
        self.calls.set(call + 1);
        Ok(self.metas[call.min(self.metas.len().saturating_sub(1))].clone())
    }
}

struct FakeWorktree {
    passes: Vec<BTreeMap<String, CandidateObjectIdentity>>,
    // Per pass, per path: an error to return (simulating a 2B-detected
    // within-observation race, e.g. parent-directory swap).
    errors: Vec<BTreeMap<String, SnapshotCoherenceError>>,
    observed: RefCell<Vec<String>>,
    calls: Cell<usize>,
}

impl FakeWorktree {
    fn passive(passes: Vec<BTreeMap<String, CandidateObjectIdentity>>) -> Self {
        Self {
            passes,
            errors: Vec::new(),
            observed: RefCell::new(Vec::new()),
            calls: Cell::new(0),
        }
    }

    fn with_pass_error(
        passes: Vec<BTreeMap<String, CandidateObjectIdentity>>,
        pass: usize,
        failing_path: &str,
        error: SnapshotCoherenceError,
    ) -> Self {
        let mut errors = vec![BTreeMap::new(); passes.len()];
        errors[pass].insert(failing_path.to_owned(), error);
        Self {
            passes,
            errors,
            observed: RefCell::new(Vec::new()),
            calls: Cell::new(0),
        }
    }

    fn observed(&self, path: &str) -> usize {
        self.observed
            .borrow()
            .iter()
            .filter(|p| p.as_str() == path)
            .count()
    }
}

impl SnapshotWorktreeObjectObserver for FakeWorktree {
    fn observe_worktree_object(
        &self,
        _root: &Path,
        path: &NormalizedWorkspacePath,
        _index_entry: Option<&SnapshotIndexEntry>,
        _file_mode_policy: GitFileModePolicy,
        budget: &mut crate::managed_worktree_snapshot_object::SnapshotHashBudget,
    ) -> Result<CandidateObjectIdentity, SnapshotCoherenceError> {
        let pass = self.calls.get().min(self.passes.len().saturating_sub(1));
        self.calls.set(self.calls.get() + 1);
        self.observed.borrow_mut().push(path.as_str().to_owned());
        if let Some(error) = self.errors.get(pass).and_then(|m| m.get(path.as_str())) {
            return Err(error.clone());
        }
        let object = self.passes[pass]
            .get(path.as_str())
            .cloned()
            .unwrap_or(CandidateObjectIdentity::Absent);
        // Charge the physical pass budget exactly as Slice 2B would.
        budget
            .charge(usize::try_from(object.size_bytes()).unwrap_or(usize::MAX))
            .map_err(SnapshotCoherenceError::from)?;
        Ok(object)
    }
}

struct FakeIndex {
    passes: Vec<BTreeMap<String, IndexBlobContent>>,
    requests: RefCell<Vec<Vec<String>>>,
    calls: Cell<usize>,
}

impl FakeIndex {
    fn passive(passes: Vec<BTreeMap<String, IndexBlobContent>>) -> Self {
        Self {
            passes,
            requests: RefCell::new(Vec::new()),
            calls: Cell::new(0),
        }
    }

    fn per_pass_requests(&self, pass: usize) -> usize {
        self.requests.borrow().get(pass).map(Vec::len).unwrap_or(0)
    }
}

impl SnapshotIndexBlobObserver for FakeIndex {
    async fn observe_pass_blobs(
        &self,
        _root: &Path,
        requests: &[IndexBlobRequest],
        budget: &mut crate::managed_worktree_snapshot_object::SnapshotHashBudget,
    ) -> Result<Vec<IndexBlobContent>, SnapshotCoherenceError> {
        self.requests
            .borrow_mut()
            .push(requests.iter().map(|r| r.oid.clone()).collect());
        let pass = self.calls.get().min(self.passes.len().saturating_sub(1));
        self.calls.set(self.calls.get() + 1);
        let map = &self.passes[pass];
        let mut out = Vec::with_capacity(requests.len());
        for request in requests {
            let content = map.get(&request.oid).cloned().ok_or_else(|| {
                SnapshotCoherenceError::IndexBlobObservationUnavailable(format!(
                    "fake index has no content for {}",
                    request.oid
                ))
            })?;
            budget
                .charge(usize::try_from(content.size_bytes).unwrap_or(usize::MAX))
                .map_err(SnapshotCoherenceError::from)?;
            out.push(content);
        }
        Ok(out)
    }
}

// A canonical, fully-stable single-path baseline used by race tests.
fn stable_one_path_meta() -> SnapshotGitMetadata {
    metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p1"],
        &[],
        &[],
        vec![index_entry("p1", REG, b'e')],
    )
}

async fn cohere(
    authority: &impl SnapshotAuthorityObserver,
    metadata: &impl SnapshotMetadataObserver,
    worktree: &impl SnapshotWorktreeObjectObserver,
    index: &impl SnapshotIndexBlobObserver,
    writer: &[NormalizedWorkspacePath],
) -> Result<StableWorkspaceObservation, SnapshotCoherenceError> {
    cohere_stable_observation(authority, metadata, worktree, index, writer).await
}

fn one_stable_index() -> Vec<BTreeMap<String, IndexBlobContent>> {
    vec![index_map(vec![content(b'e', b'c', 4)])]
}

fn one_stable_worktree() -> Vec<BTreeMap<String, CandidateObjectIdentity>> {
    vec![worktree_map(vec![("p1", regular(b'd', 7, true))])]
}

// ---------------------------------------------------------------------------
// Pure: metadata canonical equality projection
// ---------------------------------------------------------------------------

#[test]
fn metadata_projection_is_order_independent_and_overlap_legal() {
    let base = StableSnapshotMetadata::projected(&metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p1", "p2"],
        &["p2"],
        &["u1"],
        vec![index_entry("p2", REG, b'e'), index_entry("p1", REG, b'f')],
    ));
    // Same content, deliberately shuffled insertion order everywhere.
    let shuffled = StableSnapshotMetadata::projected(&metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p2", "p1"],
        &["p2"],
        &["u1"],
        vec![index_entry("p1", REG, b'f'), index_entry("p2", REG, b'e')],
    ));
    assert_eq!(base, shuffled);
    // Legal staged+changed overlap: p2 appears in both changed and staged.
    assert!(base.changed_paths.contains(&path("p2")));
    assert!(base.staged_paths.contains(&path("p2")));
    // Index entries are path-sorted (p1 before p2) regardless of input order.
    assert_eq!(base.index_entries[0].path, path("p1"));
    assert_eq!(base.index_entries[1].path, path("p2"));
}

#[test]
fn metadata_projection_detects_every_single_field_difference() {
    let base = || {
        StableSnapshotMetadata::projected(&metadata(
            HEAD,
            GitFileModePolicy::TrustExecutableBit,
            &["p1"],
            &[],
            &[],
            vec![index_entry("p1", REG, b'e')],
        ))
    };

    let different_head = StableSnapshotMetadata::projected(&metadata(
        "0f1e2d3c4b5a69788796a5b4c3d2e1f009182736",
        GitFileModePolicy::TrustExecutableBit,
        &["p1"],
        &[],
        &[],
        vec![index_entry("p1", REG, b'e')],
    ));
    assert_ne!(base(), different_head);

    let different_policy = StableSnapshotMetadata::projected(&metadata(
        HEAD,
        GitFileModePolicy::IgnoreExecutableBit,
        &["p1"],
        &[],
        &[],
        vec![index_entry("p1", REG, b'e')],
    ));
    assert_ne!(base(), different_policy);

    let different_changed = StableSnapshotMetadata::projected(&metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p1", "pX"],
        &[],
        &[],
        vec![index_entry("p1", REG, b'e')],
    ));
    assert_ne!(base(), different_changed);

    let different_staged = StableSnapshotMetadata::projected(&metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p1"],
        &["pX"],
        &[],
        vec![index_entry("p1", REG, b'e')],
    ));
    assert_ne!(base(), different_staged);

    let different_untracked = StableSnapshotMetadata::projected(&metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p1"],
        &[],
        &["uX"],
        vec![index_entry("p1", REG, b'e')],
    ));
    assert_ne!(base(), different_untracked);

    let different_oid = StableSnapshotMetadata::projected(&metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p1"],
        &[],
        &[],
        vec![index_entry("p1", REG, b'f')],
    ));
    assert_ne!(base(), different_oid);

    let different_mode = StableSnapshotMetadata::projected(&metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p1"],
        &[],
        &[],
        vec![index_entry("p1", SnapshotIndexMode::Symlink, b'e')],
    ));
    assert_ne!(base(), different_mode);
}

// ---------------------------------------------------------------------------
// Pure: writer-path canonicalization bound + dedup
// ---------------------------------------------------------------------------

#[test]
fn writer_paths_are_bounded_and_de_duplicated() {
    let deduped = canonicalize_writer_paths(&[path("b"), path("a"), path("b")]).unwrap();
    assert_eq!(deduped, vec![path("a"), path("b")]);

    let too_many: Vec<NormalizedWorkspacePath> = (0
        ..crate::workspace_snapshot::MAX_WRITER_MUTATED_PATHS + 1)
        .map(|i| path(&format!("d/{i}")))
        .collect();
    assert!(matches!(
        canonicalize_writer_paths(&too_many),
        Err(SnapshotCoherenceError::LimitExceeded(
            "writer-mutated path count"
        ))
    ));
}

// ---------------------------------------------------------------------------
// Pure: semantic evidence budget (exact, +1, double count)
// ---------------------------------------------------------------------------

#[test]
fn semantic_evidence_counts_exact_and_rejects_one_over_logical_duplication() {
    // One git-visible record: index + worktree, summed with logical duplication.
    let record = |index_size: u64, worktree_size: u64| StableGitVisiblePath {
        path: path("p1"),
        index_object: regular(b'a', index_size, false),
        worktree_object: regular(b'b', worktree_size, false),
    };
    let half = MAX_SNAPSHOT_CONTENT_BYTES / 2;
    // Exactly at the bound: Ok.
    assert_eq!(
        semantic_evidence_bytes(&[record(half, MAX_SNAPSHOT_CONTENT_BYTES - half)], &[]).unwrap(),
        MAX_SNAPSHOT_CONTENT_BYTES
    );
    // One byte over the bound: LimitExceeded.
    assert!(matches!(
        semantic_evidence_bytes(&[record(half + 1, MAX_SNAPSHOT_CONTENT_BYTES - half)], &[]),
        Err(SnapshotCoherenceError::LimitExceeded(
            "snapshot content byte"
        ))
    ));
}

#[test]
fn semantic_evidence_double_counts_the_same_bytes_across_git_visible_and_writer() {
    // One 10-byte blob that is both the git-visible worktree object and the
    // writer-mutated object counts twice (physical read once, semantic twice).
    let shared = regular(b'c', 10, false);
    let git_visible = vec![StableGitVisiblePath {
        path: path("p1"),
        index_object: CandidateObjectIdentity::Absent,
        worktree_object: shared.clone(),
    }];
    let writer = vec![StableWriterPath {
        path: path("p1"),
        object: shared,
    }];
    assert_eq!(semantic_evidence_bytes(&git_visible, &writer).unwrap(), 20);
}

#[test]
fn semantic_evidence_counts_absent_writer_paths_as_zero() {
    let writer = vec![StableWriterPath {
        path: path("w1"),
        object: CandidateObjectIdentity::Absent,
    }];
    assert_eq!(semantic_evidence_bytes(&[], &writer).unwrap(), 0);
}

// ---------------------------------------------------------------------------
// Pure: typed error mapping (no source is stringified wholesale)
// ---------------------------------------------------------------------------

#[test]
fn observer_errors_map_to_typed_coherence_errors() {
    use SnapshotCoherenceError::*;
    assert!(matches!(
        SnapshotCoherenceError::from(SnapshotMetadataError::UnsupportedGitState("gitlink".into())),
        UnsupportedGitState(_)
    ));
    assert!(matches!(
        SnapshotCoherenceError::from(SnapshotMetadataError::LimitExceeded("changed path count")),
        LimitExceeded("changed path count")
    ));
    assert!(matches!(
        SnapshotCoherenceError::from(SnapshotMetadataError::GitCommand {
            query: "rev-parse HEAD",
            detail: "exit status 1".into(),
        }),
        GitObservationUnavailable(_)
    ));
    assert!(matches!(
        SnapshotCoherenceError::from(SnapshotMetadataError::InvalidPath("bad".into())),
        GitObservationUnavailable(_)
    ));
    // 2B Unstable must map to SnapshotUnstable (design §18).
    assert!(matches!(
        SnapshotCoherenceError::from(SnapshotObjectError::Unstable),
        SnapshotUnstable(_)
    ));
    assert!(matches!(
        SnapshotCoherenceError::from(SnapshotObjectError::Unsupported("device")),
        UnsupportedObjectType(_)
    ));
    assert!(matches!(
        SnapshotCoherenceError::from(SnapshotObjectError::Io(std::io::Error::other("boom"))),
        FilesystemObservationUnavailable(detail) if detail.contains("boom")
    ));
    assert!(matches!(
        SnapshotCoherenceError::from(IndexBlobError::LimitExceeded("stderr bytes")),
        LimitExceeded("stderr bytes")
    ));
    assert!(matches!(
        SnapshotCoherenceError::from(IndexBlobError::Timeout),
        IndexBlobObservationUnavailable(_)
    ));
}

// ---------------------------------------------------------------------------
// Orchestration happy path + result shape
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stable_happy_path_returns_canonical_observation() {
    let authority = FakeAuthority::always_ok("/stubbed/managed/worktree");
    let metadata = FakeMetadata::stable(stable_one_path_meta());
    let worktree = FakeWorktree::passive(one_stable_worktree());
    let index = FakeIndex::passive(one_stable_index());
    let observation = cohere(&authority, &metadata, &worktree, &index, &[])
        .await
        .unwrap();

    assert_eq!(observation.metadata.head, HEAD);
    assert_eq!(
        observation.metadata.file_mode_policy,
        GitFileModePolicy::TrustExecutableBit
    );
    assert_eq!(observation.git_visible_paths.len(), 1);
    let record = &observation.git_visible_paths[0];
    assert_eq!(record.path, path("p1"));
    // Index object reconstructed from the OID content under the path's mode.
    assert_eq!(record.index_object, regular(b'c', 4, false));
    assert_eq!(record.worktree_object, regular(b'd', 7, true));
    assert!(observation.writer_mutated_paths.is_empty());
}

#[tokio::test]
async fn result_paths_are_deterministically_ordered() {
    let meta = metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["b.txt", "a.txt"],
        &[],
        &[],
        vec![
            index_entry("a.txt", REG, b'e'),
            index_entry("b.txt", REG, b'f'),
        ],
    );
    let authority = FakeAuthority::always_ok("/stubbed/managed/worktree");
    let metadata = FakeMetadata::stable(meta);
    let worktree = FakeWorktree::passive(vec![worktree_map(vec![
        ("a.txt", regular(b'a', 1, false)),
        ("b.txt", regular(b'b', 2, false)),
        ("m.txt", regular(b'm', 3, false)),
        ("z.txt", regular(b'z', 4, false)),
    ])]);
    let index = FakeIndex::passive(vec![index_map(vec![
        content(b'e', b'c', 4),
        content(b'f', b'd', 5),
    ])]);
    // Writer paths are supplied out of order and must be canonically sorted.
    let writer = vec![path("z.txt"), path("m.txt")];
    let observation = cohere(&authority, &metadata, &worktree, &index, &writer)
        .await
        .unwrap();

    let git_order: Vec<&str> = observation
        .git_visible_paths
        .iter()
        .map(|r| r.path.as_str())
        .collect();
    assert_eq!(git_order, vec!["a.txt", "b.txt"]);
    let writer_order: Vec<&str> = observation
        .writer_mutated_paths
        .iter()
        .map(|r| r.path.as_str())
        .collect();
    assert_eq!(writer_order, vec!["m.txt", "z.txt"]);
}

#[tokio::test]
async fn staged_and_changed_overlap_survives_union_dedup() {
    let meta = metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p1"],
        &["p1"],
        &[],
        vec![index_entry("p1", REG, b'e')],
    );
    let authority = FakeAuthority::always_ok("/stubbed/managed/worktree");
    let metadata = FakeMetadata::stable(meta);
    let worktree = FakeWorktree::passive(one_stable_worktree());
    let index = FakeIndex::passive(one_stable_index());
    let observation = cohere(&authority, &metadata, &worktree, &index, &[])
        .await
        .unwrap();
    // The overlapping staged+changed path appears exactly once in the union.
    assert_eq!(observation.git_visible_paths.len(), 1);
    assert_eq!(observation.git_visible_paths[0].path, path("p1"));
}

// ---------------------------------------------------------------------------
// Writer handling: overlap reuse, writer-only, ignored/absent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn writer_path_overlapping_git_visible_reuses_one_worktree_identity() {
    let meta = metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p1"],
        &[],
        &[],
        vec![index_entry("p1", REG, b'e')],
    );
    let authority = FakeAuthority::always_ok("/stubbed/managed/worktree");
    let metadata = FakeMetadata::stable(meta);
    let worktree = FakeWorktree::passive(vec![worktree_map(vec![("p1", regular(b'd', 7, false))])]);
    let index = FakeIndex::passive(one_stable_index());
    let writer = vec![path("p1")];
    let observation = cohere(&authority, &metadata, &worktree, &index, &writer)
        .await
        .unwrap();

    // The shared worktree identity is reused, not re-observed per record.
    assert_eq!(
        observation.git_visible_paths[0].worktree_object,
        observation.writer_mutated_paths[0].object
    );
    // Observed once per pass (two passes => two total observations), never once
    // per represented record.
    assert_eq!(worktree.observed("p1"), 2);
    // Both passes reuse the single passive map.
}

#[tokio::test]
async fn writer_only_path_is_observed_and_recorded() {
    let meta = metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &[],
        &[],
        &[],
        vec![],
    );
    let authority = FakeAuthority::always_ok("/stubbed/managed/worktree");
    let metadata = FakeMetadata::stable(meta);
    let worktree = FakeWorktree::passive(vec![worktree_map(vec![("w1", regular(b'a', 5, false))])]);
    let index = FakeIndex::passive(vec![index_map(vec![])]);
    let writer = vec![path("w1")];
    let observation = cohere(&authority, &metadata, &worktree, &index, &writer)
        .await
        .unwrap();

    assert!(observation.git_visible_paths.is_empty());
    assert_eq!(observation.writer_mutated_paths.len(), 1);
    assert_eq!(observation.writer_mutated_paths[0].path, path("w1"));
    assert_eq!(
        observation.writer_mutated_paths[0].object,
        regular(b'a', 5, false)
    );
}

#[tokio::test]
async fn writer_only_absent_path_is_a_valid_record() {
    let meta = metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &[],
        &[],
        &[],
        vec![],
    );
    let authority = FakeAuthority::always_ok("/stubbed/managed/worktree");
    let metadata = FakeMetadata::stable(meta);
    // The writer path is ignored by Git and currently missing on disk.
    let worktree = FakeWorktree::passive(vec![worktree_map(vec![])]);
    let index = FakeIndex::passive(vec![index_map(vec![])]);
    let writer = vec![path("w2")];
    let observation = cohere(&authority, &metadata, &worktree, &index, &writer)
        .await
        .unwrap();
    assert_eq!(observation.writer_mutated_paths.len(), 1);
    assert_eq!(
        observation.writer_mutated_paths[0].object,
        CandidateObjectIdentity::Absent
    );
    assert_eq!(
        semantic_evidence_bytes(&[], &observation.writer_mutated_paths).unwrap(),
        0
    );
}

// ---------------------------------------------------------------------------
// Caching: same OID read once physically, counted twice semantically
// ---------------------------------------------------------------------------

#[tokio::test]
async fn same_oid_two_paths_reads_physically_once_semantic_twice() {
    let meta = metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p1", "p2"],
        &[],
        &[],
        vec![index_entry("p1", REG, b'e'), index_entry("p2", REG, b'e')],
    );
    let authority = FakeAuthority::always_ok("/stubbed/managed/worktree");
    let metadata = FakeMetadata::stable(meta);
    let worktree = FakeWorktree::passive(vec![worktree_map(vec![])]);
    let index = FakeIndex::passive(vec![index_map(vec![content(b'e', b'c', 10)])]);
    let observation = cohere(&authority, &metadata, &worktree, &index, &[])
        .await
        .unwrap();

    // Physical read happened once per pass for the shared OID.
    assert_eq!(index.per_pass_requests(0), 1);
    assert_eq!(index.per_pass_requests(1), 1);
    // Both logical paths carry the reconstructed index identity.
    assert_eq!(
        observation.git_visible_paths[0].index_object,
        regular(b'c', 10, false)
    );
    assert_eq!(
        observation.git_visible_paths[1].index_object,
        regular(b'c', 10, false)
    );
    // Semantic evidence counts the blob once per represented path.
    assert_eq!(
        semantic_evidence_bytes(&observation.git_visible_paths, &[]).unwrap(),
        20
    );
}

#[tokio::test]
async fn semantic_over_physical_divergence_fails_closed_on_the_manifest_bound() {
    // Two index paths share a blob larger than half the semantic bound: the
    // physical pass reads it once (well under the physical bound), but the
    // semantic evidence (counted per path) exceeds MAX_SNAPSHOT_CONTENT_BYTES.
    let blob = 140 * MIB;
    assert!(blob < MAX_SNAPSHOT_CONTENT_BYTES);
    assert!(blob.saturating_mul(2) > MAX_SNAPSHOT_CONTENT_BYTES);
    let meta = metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p1", "p2"],
        &[],
        &[],
        vec![index_entry("p1", REG, b'e'), index_entry("p2", REG, b'e')],
    );
    let authority = FakeAuthority::always_ok("/stubbed/managed/worktree");
    let metadata = FakeMetadata::stable(meta);
    let worktree = FakeWorktree::passive(vec![worktree_map(vec![])]);
    let index = FakeIndex::passive(vec![index_map(vec![content(b'e', b'c', blob)])]);
    let error = cohere(&authority, &metadata, &worktree, &index, &[])
        .await
        .unwrap_err();
    // Read once per pass (physical under bound); failure is the semantic bound.
    assert_eq!(index.per_pass_requests(0), 1);
    assert!(matches!(
        error,
        SnapshotCoherenceError::LimitExceeded("snapshot content byte")
    ));
}

#[tokio::test]
async fn overlap_is_physically_cached_but_semantically_double_counted() {
    let meta = metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p1"],
        &[],
        &[],
        vec![index_entry("p1", REG, b'e')],
    );
    let authority = FakeAuthority::always_ok("/stubbed/managed/worktree");
    let metadata = FakeMetadata::stable(meta);
    let worktree =
        FakeWorktree::passive(vec![worktree_map(vec![("p1", regular(b'd', 20, false))])]);
    let index = FakeIndex::passive(vec![index_map(vec![content(b'e', b'c', 10)])]);
    let writer = vec![path("p1")];
    let observation = cohere(&authority, &metadata, &worktree, &index, &writer)
        .await
        .unwrap();
    // Physical: the shared 20-byte worktree file is read once per pass.
    assert_eq!(worktree.observed("p1"), 2);
    // Semantic: index(10) + worktree(20) [git-visible] + writer object(20) = 50.
    assert_eq!(
        semantic_evidence_bytes(
            &observation.git_visible_paths,
            &observation.writer_mutated_paths
        )
        .unwrap(),
        50
    );
}

// ---------------------------------------------------------------------------
// Deterministic race matrix (design §22). Every race is scripted through the
// injected observers; there are no sleeps, threads, or wall-clock barriers.
// ---------------------------------------------------------------------------

const HEAD_OTHER: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f009182736";

fn authority_ok() -> FakeAuthority {
    FakeAuthority::always_ok("/stubbed/managed/worktree")
}

fn meta_changed_oid(object: u8) -> SnapshotGitMetadata {
    metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p1"],
        &[],
        &[],
        vec![index_entry("p1", REG, object)],
    )
}

fn assert_unstable(
    result: Result<StableWorkspaceObservation, SnapshotCoherenceError>,
    needle: &str,
) {
    match result {
        Err(SnapshotCoherenceError::SnapshotUnstable(detail)) => {
            assert!(
                detail.contains(needle),
                "expected instability detail to mention {needle:?}, got {detail:?}"
            );
        }
        other => panic!("expected SnapshotUnstable, got {other:?}"),
    }
}

#[tokio::test]
async fn race_head_changes_after_meta_a() {
    let meta_b = metadata(
        HEAD_OTHER,
        GitFileModePolicy::TrustExecutableBit,
        &["p1"],
        &[],
        &[],
        vec![index_entry("p1", REG, b'e')],
    );
    let result = cohere(
        &authority_ok(),
        &FakeMetadata::script(vec![stable_one_path_meta(), meta_b.clone(), meta_b]),
        &FakeWorktree::passive(one_stable_worktree()),
        &FakeIndex::passive(one_stable_index()),
        &[],
    )
    .await;
    assert_unstable(result, "HEAD");
}

#[tokio::test]
async fn race_staged_oid_changes_after_content_a() {
    let result = cohere(
        &authority_ok(),
        &FakeMetadata::script(vec![
            meta_changed_oid(b'e'),
            meta_changed_oid(b'f'),
            meta_changed_oid(b'f'),
        ]),
        &FakeWorktree::passive(one_stable_worktree()),
        &FakeIndex::passive(one_stable_index()),
        &[],
    )
    .await;
    assert_unstable(result, "index entries changed");
}

#[tokio::test]
async fn race_index_mode_changes() {
    let meta_a = stable_one_path_meta();
    let meta_b = metadata(
        HEAD,
        GitFileModePolicy::TrustExecutableBit,
        &["p1"],
        &[],
        &[],
        vec![index_entry("p1", SnapshotIndexMode::Symlink, b'e')],
    );
    let result = cohere(
        &authority_ok(),
        &FakeMetadata::script(vec![meta_a, meta_b.clone(), meta_b]),
        &FakeWorktree::passive(one_stable_worktree()),
        &FakeIndex::passive(one_stable_index()),
        &[],
    )
    .await;
    assert_unstable(result, "index entries changed");
}

#[tokio::test]
async fn race_core_filemode_changes() {
    let meta_b = metadata(
        HEAD,
        GitFileModePolicy::IgnoreExecutableBit,
        &["p1"],
        &[],
        &[],
        vec![index_entry("p1", REG, b'e')],
    );
    let result = cohere(
        &authority_ok(),
        &FakeMetadata::script(vec![stable_one_path_meta(), meta_b.clone(), meta_b]),
        &FakeWorktree::passive(one_stable_worktree()),
        &FakeIndex::passive(one_stable_index()),
        &[],
    )
    .await;
    assert_unstable(result, "core.fileMode policy changed");
}

macro_rules! worktree_content_race {
    ($name:ident, $a:expr, $b:expr, $needle:expr) => {
        #[tokio::test]
        async fn $name() {
            let result = cohere(
                &authority_ok(),
                &FakeMetadata::stable(stable_one_path_meta()),
                &FakeWorktree::passive(vec![$a, $b]),
                &FakeIndex::passive(one_stable_index()),
                &[],
            )
            .await;
            assert_unstable(result, $needle);
        }
    };
}

worktree_content_race!(
    race_worktree_bytes_change_same_status,
    worktree_map(vec![("p1", regular(b'd', 7, false))]),
    worktree_map(vec![("p1", regular(b'f', 7, false))]),
    "worktree object changed"
);
worktree_content_race!(
    race_same_size_bytes_change,
    worktree_map(vec![("p1", regular(b'd', 7, false))]),
    worktree_map(vec![("p1", regular(b'f', 7, false))]),
    "worktree object changed"
);
worktree_content_race!(
    race_delete_recreate,
    worktree_map(vec![("p1", regular(b'd', 7, false))]),
    worktree_map(vec![("p1", CandidateObjectIdentity::Absent)]),
    "worktree object changed"
);
worktree_content_race!(
    race_regular_to_symlink,
    worktree_map(vec![("p1", regular(b'd', 7, false))]),
    worktree_map(vec![("p1", symlink(b't', 3))]),
    "worktree object changed"
);
worktree_content_race!(
    race_symlink_to_regular,
    worktree_map(vec![("p1", symlink(b't', 3))]),
    worktree_map(vec![("p1", regular(b'd', 7, false))]),
    "worktree object changed"
);
worktree_content_race!(
    race_symlink_target_change,
    worktree_map(vec![("p1", symlink(b't', 3))]),
    worktree_map(vec![("p1", symlink(b'u', 3))]),
    "worktree object changed"
);
worktree_content_race!(
    race_executable_only_change,
    worktree_map(vec![("p1", regular(b'd', 7, false))]),
    worktree_map(vec![("p1", regular(b'd', 7, true))]),
    "worktree object changed"
);

#[tokio::test]
async fn race_parent_directory_swap_is_surfaced_within_a_pass() {
    // Slice 2B detects a mid-observation parent swap and returns Unstable; 2D
    // maps it to SnapshotUnstable (no retry).
    let worktree = FakeWorktree::with_pass_error(
        one_stable_worktree(),
        0,
        "p1",
        SnapshotCoherenceError::SnapshotUnstable(
            "worktree object changed while being observed".to_owned(),
        ),
    );
    let result = cohere(
        &authority_ok(),
        &FakeMetadata::stable(stable_one_path_meta()),
        &worktree,
        &FakeIndex::passive(one_stable_index()),
        &[],
    )
    .await;
    assert_unstable(result, "worktree object changed");
}

macro_rules! writer_content_race {
    ($name:ident, $a:expr, $b:expr) => {
        #[tokio::test]
        async fn $name() {
            let empty_meta = metadata(
                HEAD,
                GitFileModePolicy::TrustExecutableBit,
                &[],
                &[],
                &[],
                vec![],
            );
            let result = cohere(
                &authority_ok(),
                &FakeMetadata::stable(empty_meta),
                &FakeWorktree::passive(vec![$a, $b]),
                &FakeIndex::passive(vec![index_map(vec![])]),
                &[path("w1")],
            )
            .await;
            assert_unstable(result, "writer-mutated object changed");
        }
    };
}

writer_content_race!(
    race_writer_only_ignored_file_changes,
    worktree_map(vec![("w1", regular(b'a', 5, false))]),
    worktree_map(vec![("w1", regular(b'b', 6, false))])
);
writer_content_race!(
    race_writer_path_becomes_absent,
    worktree_map(vec![("w1", regular(b'a', 5, false))]),
    worktree_map(vec![("w1", CandidateObjectIdentity::Absent)])
);

#[tokio::test]
async fn race_index_object_bytes_differ_between_passes() {
    // Same index OID, but its raw bytes differ between CONTENT A and CONTENT B
    // (out-of-band object-store mutation / corruption under cat-file). An
    // unchanged OID alone is insufficient; the re-read surfaces it.
    let index_a = index_map(vec![content(b'e', b'c', 4)]);
    let index_b = index_map(vec![content(b'e', b'f', 4)]);
    let result = cohere(
        &authority_ok(),
        &FakeMetadata::stable(stable_one_path_meta()),
        &FakeWorktree::passive(one_stable_worktree()),
        &FakeIndex::passive(vec![index_a, index_b]),
        &[],
    )
    .await;
    assert_unstable(result, "index object changed");
}

#[tokio::test]
async fn race_authority_revoked_before_start_is_not_instability() {
    let result = cohere(
        &FakeAuthority::script(vec![Err(SnapshotCoherenceError::AuthorityUnavailable(
            "managed root is not covered by current Session authority".to_owned(),
        ))]),
        &FakeMetadata::stable(stable_one_path_meta()),
        &FakeWorktree::passive(one_stable_worktree()),
        &FakeIndex::passive(one_stable_index()),
        &[],
    )
    .await;
    assert!(matches!(
        result,
        Err(SnapshotCoherenceError::AuthorityUnavailable(_))
    ));
}

#[tokio::test]
async fn race_ownership_mismatch_before_start_is_not_instability() {
    let result = cohere(
        &FakeAuthority::script(vec![Err(SnapshotCoherenceError::OwnershipMismatch(
            "Goal does not belong to this session".to_owned(),
        ))]),
        &FakeMetadata::stable(stable_one_path_meta()),
        &FakeWorktree::passive(one_stable_worktree()),
        &FakeIndex::passive(one_stable_index()),
        &[],
    )
    .await;
    assert!(matches!(
        result,
        Err(SnapshotCoherenceError::OwnershipMismatch(_))
    ));
}

#[tokio::test]
async fn race_branch_common_dir_lock_mismatch_before_start() {
    // A reconciliation failure (branch/common-dir/lock) is an authority proof
    // failure at AUTHORITY A, not an instability.
    let result = cohere(
        &FakeAuthority::script(vec![Err(SnapshotCoherenceError::AuthorityUnavailable(
            "managed workspace is not exactly owned and active: BranchMismatch".to_owned(),
        ))]),
        &FakeMetadata::stable(stable_one_path_meta()),
        &FakeWorktree::passive(one_stable_worktree()),
        &FakeIndex::passive(one_stable_index()),
        &[],
    )
    .await;
    assert!(matches!(
        result,
        Err(SnapshotCoherenceError::AuthorityUnavailable(_))
    ));
}

#[tokio::test]
async fn race_authority_revoked_after_content_is_instability() {
    let result = cohere(
        &FakeAuthority::script(vec![
            Ok(PathBuf::from("/stubbed/managed/worktree")),
            Err(SnapshotCoherenceError::AuthorityUnavailable(
                "managed workspace no longer reconciles exactly".to_owned(),
            )),
        ]),
        &FakeMetadata::stable(stable_one_path_meta()),
        &FakeWorktree::passive(one_stable_worktree()),
        &FakeIndex::passive(one_stable_index()),
        &[],
    )
    .await;
    assert_unstable(result, "no longer proven");
}

#[tokio::test]
async fn race_authority_root_changed_after_content_is_instability() {
    let result = cohere(
        &FakeAuthority::script(vec![
            Ok(PathBuf::from("/worktrees/a")),
            Ok(PathBuf::from("/worktrees/b")),
        ]),
        &FakeMetadata::stable(stable_one_path_meta()),
        &FakeWorktree::passive(one_stable_worktree()),
        &FakeIndex::passive(one_stable_index()),
        &[],
    )
    .await;
    assert_unstable(result, "changed across the coherence window");
}

#[tokio::test]
async fn aba_a_b_a_between_samples_is_an_accepted_residual_not_a_failure() {
    // NON_TRANSACTIONAL_ABA_RESIDUAL: a transient A -> B -> A change that leaves
    // every sampled state identical is invisible to 2D. When all sampled
    // metadata/content/authority states match, 2D returns success; it does not
    // and cannot claim temporal-history integrity.
    let result = cohere(
        &authority_ok(),
        &FakeMetadata::stable(stable_one_path_meta()),
        &FakeWorktree::passive(one_stable_worktree()),
        &FakeIndex::passive(one_stable_index()),
        &[],
    )
    .await;
    let observation = result.expect("identical sampled states succeed (ABA residual accepted)");
    assert_eq!(observation.git_visible_paths.len(), 1);
}

// ---------------------------------------------------------------------------
// Real-Git + real filesystem integration: the production adapters observe one
// real repository end-to-end and both content passes agree (adds semantic proof
// of the placeholder-path index read, the worktree hash, and their agreement).
// ---------------------------------------------------------------------------

fn init_repo(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "local-mcp-snapshot-2d-{label}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&path).unwrap();
    std::fs::canonicalize(path).unwrap()
}

fn git(repo: &Path, args: &[&str]) {
    let output = std::process::Command::new(crate::execution::host_git_path().unwrap())
        .args(args)
        .current_dir(repo)
        .output()
        .expect("host Git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn production_adapters_observe_one_real_repository() {
    let repo = init_repo("real");
    git(&repo, &["init", "--quiet"]);
    // Deterministic cross-platform: pin core.fileMode so Slice 2B never takes
    // the Windows-unsupported executable path and executable is always false.
    git(&repo, &["config", "core.fileMode", "false"]);
    std::fs::write(repo.join("tracked.txt"), b"HEAD-content").unwrap();
    git(&repo, &["add", "tracked.txt"]);
    git(
        &repo,
        &[
            "-c",
            "user.email=2d@example.invalid",
            "-c",
            "user.name=2d",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
    );
    // tracked.txt is now changed in the worktree (index still holds the base
    // blob); untracked.txt is a new untracked file.
    std::fs::write(repo.join("tracked.txt"), b"worktree-modified-content").unwrap();
    std::fs::write(repo.join("untracked.txt"), b"u-content").unwrap();

    let authority = FakeAuthority::always_ok(repo.to_str().unwrap());
    let metadata = HostSnapshotMetadata;
    let worktree = HostSnapshotWorktreeObject;
    let index = HostSnapshotIndexBlob;
    let observation = cohere(&authority, &metadata, &worktree, &index, &[])
        .await
        .expect("an unchanging real repository is internally stable");

    // Git-visible union: {tracked.txt (changed), untracked.txt (untracked)}.
    assert_eq!(observation.git_visible_paths.len(), 2);
    let tracked = observation
        .git_visible_paths
        .iter()
        .find(|record| record.path.as_str() == "tracked.txt")
        .expect("tracked.txt present");
    let untracked = observation
        .git_visible_paths
        .iter()
        .find(|record| record.path.as_str() == "untracked.txt")
        .expect("untracked.txt present");

    // tracked.txt: index object is the committed base blob (read via the
    // placeholder-path cat-file session), worktree object is the modified file.
    assert_eq!(
        tracked.index_object,
        CandidateObjectIdentity::RegularFile {
            content_sha256: format!("{:x}", sha2::Sha256::digest(b"HEAD-content")),
            size_bytes: u64::try_from("HEAD-content".len()).unwrap(),
            executable: false,
        }
    );
    assert_eq!(
        tracked.worktree_object,
        CandidateObjectIdentity::RegularFile {
            content_sha256: format!("{:x}", sha2::Sha256::digest(b"worktree-modified-content")),
            size_bytes: u64::try_from("worktree-modified-content".len()).unwrap(),
            executable: false,
        }
    );

    // untracked.txt: no index entry (Absent), worktree object is the file.
    assert_eq!(untracked.index_object, CandidateObjectIdentity::Absent);
    assert_eq!(
        untracked.worktree_object,
        CandidateObjectIdentity::RegularFile {
            content_sha256: format!("{:x}", sha2::Sha256::digest(b"u-content")),
            size_bytes: u64::try_from("u-content".len()).unwrap(),
            executable: false,
        }
    );

    // The two passes agreed, so the semantic evidence is a plain sum here.
    let observed_head = observation.metadata.head.clone();
    assert_eq!(observed_head.len(), 40);

    std::fs::remove_dir_all(repo).unwrap();
}
