//! Pure, versioned canonical evidence for a future managed-worktree snapshot.
//!
//! This module deliberately performs no filesystem observation, Git execution,
//! Session lookup, or Goal persistence. Callers must supply already-observed
//! logical paths and content identities. Path validation is lexical; it does not
//! inspect or claim host filesystem semantics.

#![allow(
    dead_code,
    reason = "Phase 5 Slice 1 freezes the pure contract before later observer and finalizer wiring."
)]

use sha2::{Digest, Sha256};

use crate::managed_worktree::WorktreeId;

pub(crate) const WORKSPACE_SNAPSHOT_V1_DOMAIN: &[u8] = b"managed-worktree-workspace-snapshot-v1\0";

pub(crate) const MAX_CHANGED_PATHS: usize = 20_000;
pub(crate) const MAX_STAGED_PATHS: usize = 20_000;
pub(crate) const MAX_UNTRACKED_PATHS: usize = 20_000;
pub(crate) const MAX_WRITER_MUTATED_PATHS: usize = 20_000;
pub(crate) const MAX_GIT_VISIBLE_PATHS: usize = 60_000;
pub(crate) const MAX_NORMALIZED_PATH_BYTES: usize = 4 * 1024;
pub(crate) const MAX_CANONICAL_MANIFEST_BYTES: usize = 2 * 1024 * 1024;
/// Maximum combined **semantic content evidence** bytes represented by one
/// manifest, summed exactly as the constructor below sums it (per Git-visible
/// `index_object + worktree_object`, per writer-mutated `Present` object, with
/// logical duplication counted). This is a semantic bound, not a lifetime
/// physical read count: the Slice 2D coherence recheck performs two physical
/// passes, each independently capped at this value, and validates against this
/// single semantic bound. See `docs/MANAGED_WORKTREES_PHASE5_SNAPSHOT_EVIDENCE.md`.
pub(crate) const MAX_SNAPSHOT_CONTENT_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceSnapshotError {
    InvalidWorktreeId,
    InvalidObjectId(&'static str),
    InvalidPath,
    InvalidContentDigest,
    InvalidCandidateIdentityCoverage,
    DuplicatePath(&'static str),
    LimitExceeded(&'static str),
}

impl std::fmt::Display for WorkspaceSnapshotError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidWorktreeId => formatter.write_str("invalid worktree ID"),
            Self::InvalidObjectId(field) => write!(formatter, "invalid {field} object ID"),
            Self::InvalidPath => formatter.write_str("invalid normalized workspace path"),
            Self::InvalidContentDigest => formatter.write_str("invalid content SHA-256 digest"),
            Self::InvalidCandidateIdentityCoverage => {
                formatter.write_str("Git-visible paths must have exactly one candidate identity")
            }
            Self::DuplicatePath(set) => write!(formatter, "duplicate path in {set}"),
            Self::LimitExceeded(limit) => {
                write!(formatter, "workspace snapshot {limit} limit exceeded")
            }
        }
    }
}

impl std::error::Error for WorkspaceSnapshotError {}

/// A validated `/`-separated logical path relative to the worktree root.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NormalizedWorkspacePath(String);

impl NormalizedWorkspacePath {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, WorkspaceSnapshotError> {
        let value = value.into();
        if value.len() > MAX_NORMALIZED_PATH_BYTES {
            return Err(WorkspaceSnapshotError::LimitExceeded(
                "normalized path byte",
            ));
        }
        if value.is_empty()
            || value.starts_with('/')
            || value.contains('\\')
            || value.contains('\0')
        {
            return Err(WorkspaceSnapshotError::InvalidPath);
        }
        let mut components = value.split('/');
        let first = components
            .next()
            .ok_or(WorkspaceSnapshotError::InvalidPath)?;
        if first.is_empty()
            || first == "."
            || first == ".."
            || (first.len() >= 2
                && first.as_bytes()[0].is_ascii_alphabetic()
                && first.as_bytes()[1] == b':')
            || components
                .any(|component| component.is_empty() || component == "." || component == "..")
        {
            return Err(WorkspaceSnapshotError::InvalidPath);
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stable Git candidate object identity. Regular files bind Git's 100644/100755
/// mode distinction; symlinks bind mode 120000 and the link-target bytes. Other
/// OS metadata is excluded. Unsupported object kinds have no representable case,
/// so observers must fail closed rather than omit or flatten them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CandidateObjectIdentity {
    Absent,
    RegularFile {
        content_sha256: String,
        size_bytes: u64,
        executable: bool,
    },
    Symlink {
        target_sha256: String,
        target_size_bytes: u64,
    },
}

impl CandidateObjectIdentity {
    fn validate(&self) -> Result<(), WorkspaceSnapshotError> {
        let (digest, size) = match self {
            Self::Absent => return Ok(()),
            Self::RegularFile {
                content_sha256,
                size_bytes,
                ..
            } => (content_sha256, size_bytes),
            Self::Symlink {
                target_sha256,
                target_size_bytes,
            } => (target_sha256, target_size_bytes),
        };
        if !is_lowercase_sha256(digest) {
            return Err(WorkspaceSnapshotError::InvalidContentDigest);
        }
        if *size > MAX_SNAPSHOT_CONTENT_BYTES {
            return Err(WorkspaceSnapshotError::LimitExceeded(
                "snapshot content byte",
            ));
        }
        Ok(())
    }

    pub(crate) fn size_bytes(&self) -> u64 {
        match self {
            Self::Absent => 0,
            Self::RegularFile { size_bytes, .. } => *size_bytes,
            Self::Symlink {
                target_size_bytes, ..
            } => *target_size_bytes,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitVisiblePathIdentity {
    path: NormalizedWorkspacePath,
    index_object: CandidateObjectIdentity,
    worktree_object: CandidateObjectIdentity,
}

impl GitVisiblePathIdentity {
    pub(crate) fn new(
        path: NormalizedWorkspacePath,
        index_object: CandidateObjectIdentity,
        worktree_object: CandidateObjectIdentity,
    ) -> Result<Self, WorkspaceSnapshotError> {
        index_object.validate()?;
        worktree_object.validate()?;
        Ok(Self {
            path,
            index_object,
            worktree_object,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WriterMutatedPathIdentity {
    Present {
        path: NormalizedWorkspacePath,
        object: CandidateObjectIdentity,
    },
    Absent {
        path: NormalizedWorkspacePath,
    },
}

impl WriterMutatedPathIdentity {
    pub(crate) fn present(
        path: NormalizedWorkspacePath,
        content_sha256: impl Into<String>,
        size_bytes: u64,
    ) -> Result<Self, WorkspaceSnapshotError> {
        let content_sha256 = content_sha256.into();
        if !is_lowercase_sha256(&content_sha256) {
            return Err(WorkspaceSnapshotError::InvalidContentDigest);
        }
        if size_bytes > MAX_SNAPSHOT_CONTENT_BYTES {
            return Err(WorkspaceSnapshotError::LimitExceeded(
                "snapshot content byte",
            ));
        }
        let object = CandidateObjectIdentity::RegularFile {
            content_sha256,
            size_bytes,
            executable: false,
        };
        object.validate()?;
        Ok(Self::Present { path, object })
    }

    pub(crate) fn absent(path: NormalizedWorkspacePath) -> Self {
        Self::Absent { path }
    }

    fn path(&self) -> &NormalizedWorkspacePath {
        match self {
            Self::Present { path, .. } | Self::Absent { path } => path,
        }
    }
}

/// A canonical, validated, resource-bounded v1 snapshot. Construction sorts set
/// values, rejects duplicates, and stores the exact bounded canonical bytes so
/// `canonical_bytes()` and `digest()` are total operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceSnapshotManifestV1 {
    worktree_id: WorktreeId,
    base_commit: String,
    observed_head: String,
    changed_paths: Vec<NormalizedWorkspacePath>,
    staged_paths: Vec<NormalizedWorkspacePath>,
    untracked_paths: Vec<NormalizedWorkspacePath>,
    git_visible_paths: Vec<GitVisiblePathIdentity>,
    writer_mutated_paths: Vec<WriterMutatedPathIdentity>,
    canonical_bytes: Vec<u8>,
}

impl WorkspaceSnapshotManifestV1 {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        worktree_id: WorktreeId,
        base_commit: impl Into<String>,
        observed_head: impl Into<String>,
        changed_paths: Vec<NormalizedWorkspacePath>,
        staged_paths: Vec<NormalizedWorkspacePath>,
        untracked_paths: Vec<NormalizedWorkspacePath>,
        git_visible_paths: Vec<GitVisiblePathIdentity>,
        writer_mutated_paths: Vec<WriterMutatedPathIdentity>,
    ) -> Result<Self, WorkspaceSnapshotError> {
        worktree_id
            .validate()
            .map_err(|_| WorkspaceSnapshotError::InvalidWorktreeId)?;
        let base_commit = base_commit.into();
        let observed_head = observed_head.into();
        validate_object_id(&base_commit, "base_commit")?;
        validate_object_id(&observed_head, "observed_head")?;

        let changed_paths = canonicalize_set(changed_paths, MAX_CHANGED_PATHS, "changed paths")?;
        let staged_paths = canonicalize_set(staged_paths, MAX_STAGED_PATHS, "staged paths")?;
        let untracked_paths =
            canonicalize_set(untracked_paths, MAX_UNTRACKED_PATHS, "untracked paths")?;
        if writer_mutated_paths.len() > MAX_WRITER_MUTATED_PATHS {
            return Err(WorkspaceSnapshotError::LimitExceeded(
                "writer-mutated path count",
            ));
        }
        let mut git_visible_paths = git_visible_paths;
        if git_visible_paths.len() > MAX_GIT_VISIBLE_PATHS {
            return Err(WorkspaceSnapshotError::LimitExceeded(
                "Git-visible path count",
            ));
        }
        git_visible_paths
            .sort_by(|left, right| left.path.0.as_bytes().cmp(right.path.0.as_bytes()));
        reject_duplicate_git_visible_paths(&git_visible_paths)?;
        validate_git_visible_coverage(
            &changed_paths,
            &staged_paths,
            &untracked_paths,
            &git_visible_paths,
        )?;

        let mut writer_mutated_paths = writer_mutated_paths;
        writer_mutated_paths
            .sort_by(|left, right| left.path().0.as_bytes().cmp(right.path().0.as_bytes()));
        reject_duplicate_identity_paths(&writer_mutated_paths)?;
        for identity in &writer_mutated_paths {
            if let WriterMutatedPathIdentity::Present { object, .. } = identity {
                object.validate()?;
                if object == &CandidateObjectIdentity::Absent {
                    return Err(WorkspaceSnapshotError::InvalidCandidateIdentityCoverage);
                }
            }
        }
        validate_writer_identity_consistency(&git_visible_paths, &writer_mutated_paths)?;
        let hashed_bytes = git_visible_paths
            .iter()
            .try_fold(0u64, |total, item| {
                total
                    .checked_add(item.index_object.size_bytes())
                    .and_then(|value| value.checked_add(item.worktree_object.size_bytes()))
            })
            .ok_or(WorkspaceSnapshotError::LimitExceeded(
                "snapshot content byte",
            ))?;
        let hashed_bytes = writer_mutated_paths
            .iter()
            .try_fold(hashed_bytes, |total, item| {
                let size = match item {
                    WriterMutatedPathIdentity::Present { object, .. } => object.size_bytes(),
                    WriterMutatedPathIdentity::Absent { .. } => 0,
                };
                total.checked_add(size)
            })
            .ok_or(WorkspaceSnapshotError::LimitExceeded(
                "snapshot content byte",
            ))?;
        if hashed_bytes > MAX_SNAPSHOT_CONTENT_BYTES {
            return Err(WorkspaceSnapshotError::LimitExceeded(
                "snapshot content byte",
            ));
        }

        let mut manifest = Self {
            worktree_id,
            base_commit,
            observed_head,
            changed_paths,
            staged_paths,
            untracked_paths,
            git_visible_paths,
            writer_mutated_paths,
            canonical_bytes: Vec::new(),
        };
        let encoded_len = manifest.encoded_len()?;
        manifest.canonical_bytes = manifest.encode_canonical(encoded_len);
        Ok(manifest)
    }

    pub(crate) fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    pub(crate) fn digest(&self) -> String {
        format!("{:x}", Sha256::digest(&self.canonical_bytes))
    }

    fn encoded_len(&self) -> Result<usize, WorkspaceSnapshotError> {
        let mut size = WORKSPACE_SNAPSHOT_V1_DOMAIN.len();
        add_len(
            &mut size,
            field_encoded_len(self.worktree_id.as_str().len())?,
        )?;
        add_len(&mut size, field_encoded_len(self.base_commit.len())?)?;
        add_len(&mut size, field_encoded_len(self.observed_head.len())?)?;
        for paths in [
            &self.changed_paths,
            &self.staged_paths,
            &self.untracked_paths,
        ] {
            add_len(&mut size, 1 + 8)?;
            for path in paths {
                add_len(&mut size, field_encoded_len(path.0.len())?)?;
            }
        }
        add_len(&mut size, 1 + 8)?;
        for identity in &self.git_visible_paths {
            add_len(&mut size, field_encoded_len(identity.path.0.len())?)?;
            add_len(
                &mut size,
                object_identity_encoded_len(&identity.index_object)?,
            )?;
            add_len(
                &mut size,
                object_identity_encoded_len(&identity.worktree_object)?,
            )?;
        }
        add_len(&mut size, 1 + 8)?;
        for identity in &self.writer_mutated_paths {
            match identity {
                WriterMutatedPathIdentity::Present { path, object } => {
                    add_len(&mut size, 1)?;
                    add_len(&mut size, field_encoded_len(path.0.len())?)?;
                    add_len(&mut size, object_identity_encoded_len(object)?)?;
                }
                WriterMutatedPathIdentity::Absent { path } => {
                    add_len(&mut size, 1)?;
                    add_len(&mut size, field_encoded_len(path.0.len())?)?;
                }
            }
        }
        if size > MAX_CANONICAL_MANIFEST_BYTES {
            return Err(WorkspaceSnapshotError::LimitExceeded("canonical byte"));
        }
        Ok(size)
    }

    fn encode_canonical(&self, capacity: usize) -> Vec<u8> {
        // Length-prefixed fields avoid delimiter ambiguity for arbitrary valid path bytes.
        let mut bytes = Vec::with_capacity(capacity);
        bytes.extend_from_slice(WORKSPACE_SNAPSHOT_V1_DOMAIN);
        push_field(&mut bytes, 0x01, self.worktree_id.as_str().as_bytes());
        push_field(&mut bytes, 0x02, self.base_commit.as_bytes());
        push_field(&mut bytes, 0x03, self.observed_head.as_bytes());
        push_path_set(&mut bytes, 0x10, &self.changed_paths);
        push_path_set(&mut bytes, 0x11, &self.staged_paths);
        push_path_set(&mut bytes, 0x12, &self.untracked_paths);
        bytes.push(0x13);
        push_count(&mut bytes, self.git_visible_paths.len());
        for identity in &self.git_visible_paths {
            push_field(&mut bytes, 0x01, identity.path.0.as_bytes());
            push_object_identity(&mut bytes, &identity.index_object);
            push_object_identity(&mut bytes, &identity.worktree_object);
        }
        bytes.push(0x14);
        push_count(&mut bytes, self.writer_mutated_paths.len());
        for identity in &self.writer_mutated_paths {
            match identity {
                WriterMutatedPathIdentity::Present { path, object } => {
                    bytes.push(0x01);
                    push_field(&mut bytes, 0x01, path.0.as_bytes());
                    push_object_identity(&mut bytes, object);
                }
                WriterMutatedPathIdentity::Absent { path } => {
                    bytes.push(0x02);
                    push_field(&mut bytes, 0x01, path.0.as_bytes());
                }
            }
        }
        bytes
    }
}

fn field_encoded_len(value_len: usize) -> Result<usize, WorkspaceSnapshotError> {
    value_len
        .checked_add(9)
        .ok_or(WorkspaceSnapshotError::LimitExceeded("canonical byte"))
}

fn add_len(total: &mut usize, amount: usize) -> Result<(), WorkspaceSnapshotError> {
    *total = total
        .checked_add(amount)
        .ok_or(WorkspaceSnapshotError::LimitExceeded("canonical byte"))?;
    Ok(())
}

fn canonicalize_set(
    mut paths: Vec<NormalizedWorkspacePath>,
    limit: usize,
    label: &'static str,
) -> Result<Vec<NormalizedWorkspacePath>, WorkspaceSnapshotError> {
    if paths.len() > limit {
        return Err(WorkspaceSnapshotError::LimitExceeded(label));
    }
    paths.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    if paths.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(WorkspaceSnapshotError::DuplicatePath(label));
    }
    Ok(paths)
}

fn object_identity_encoded_len(
    object: &CandidateObjectIdentity,
) -> Result<usize, WorkspaceSnapshotError> {
    let mut size = 1usize;
    match object {
        CandidateObjectIdentity::Absent => {}
        CandidateObjectIdentity::RegularFile { content_sha256, .. } => {
            add_len(&mut size, field_encoded_len(content_sha256.len())?)?;
            add_len(&mut size, 8 + 1)?;
        }
        CandidateObjectIdentity::Symlink { target_sha256, .. } => {
            add_len(&mut size, field_encoded_len(target_sha256.len())?)?;
            add_len(&mut size, 8)?;
        }
    }
    Ok(size)
}

fn push_object_identity(bytes: &mut Vec<u8>, object: &CandidateObjectIdentity) {
    match object {
        CandidateObjectIdentity::Absent => bytes.push(0x03),
        CandidateObjectIdentity::RegularFile {
            content_sha256,
            size_bytes,
            executable,
        } => {
            bytes.push(0x01);
            push_field(bytes, 0x02, content_sha256.as_bytes());
            bytes.extend_from_slice(&size_bytes.to_be_bytes());
            bytes.push(u8::from(*executable));
        }
        CandidateObjectIdentity::Symlink {
            target_sha256,
            target_size_bytes,
        } => {
            bytes.push(0x02);
            push_field(bytes, 0x02, target_sha256.as_bytes());
            bytes.extend_from_slice(&target_size_bytes.to_be_bytes());
        }
    }
}

fn validate_git_visible_coverage(
    changed: &[NormalizedWorkspacePath],
    staged: &[NormalizedWorkspacePath],
    untracked: &[NormalizedWorkspacePath],
    identities: &[GitVisiblePathIdentity],
) -> Result<(), WorkspaceSnapshotError> {
    let mut expected = changed
        .iter()
        .chain(staged)
        .chain(untracked)
        .collect::<Vec<_>>();
    expected.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    expected.dedup();
    if expected.len() != identities.len()
        || expected
            .iter()
            .zip(identities)
            .any(|(path, identity)| *path != &identity.path)
    {
        return Err(WorkspaceSnapshotError::InvalidCandidateIdentityCoverage);
    }
    Ok(())
}

fn validate_writer_identity_consistency(
    git_visible: &[GitVisiblePathIdentity],
    writer_mutated: &[WriterMutatedPathIdentity],
) -> Result<(), WorkspaceSnapshotError> {
    for writer in writer_mutated {
        let (path, writer_object) = match writer {
            WriterMutatedPathIdentity::Present { path, object } => (path, Some(object)),
            WriterMutatedPathIdentity::Absent { path } => (path, None),
        };
        if let Ok(index) = git_visible
            .binary_search_by(|candidate| candidate.path.0.as_bytes().cmp(path.0.as_bytes()))
        {
            let candidate_object = &git_visible[index].worktree_object;
            let matches = match writer_object {
                Some(object) => object == candidate_object,
                None => candidate_object == &CandidateObjectIdentity::Absent,
            };
            if !matches {
                return Err(WorkspaceSnapshotError::InvalidCandidateIdentityCoverage);
            }
        }
    }
    Ok(())
}

fn reject_duplicate_git_visible_paths(
    paths: &[GitVisiblePathIdentity],
) -> Result<(), WorkspaceSnapshotError> {
    if paths.windows(2).any(|pair| pair[0].path == pair[1].path) {
        return Err(WorkspaceSnapshotError::DuplicatePath("Git-visible paths"));
    }
    Ok(())
}

fn reject_duplicate_identity_paths(
    paths: &[WriterMutatedPathIdentity],
) -> Result<(), WorkspaceSnapshotError> {
    if paths
        .windows(2)
        .any(|pair| pair[0].path() == pair[1].path())
    {
        return Err(WorkspaceSnapshotError::DuplicatePath(
            "writer-mutated paths",
        ));
    }
    Ok(())
}

fn validate_object_id(value: &str, field: &'static str) -> Result<(), WorkspaceSnapshotError> {
    crate::managed_worktree::validate_object_id(value, field)
        .map_err(|_| WorkspaceSnapshotError::InvalidObjectId(field))
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn push_path_set(bytes: &mut Vec<u8>, tag: u8, paths: &[NormalizedWorkspacePath]) {
    bytes.push(tag);
    push_count(bytes, paths.len());
    for path in paths {
        push_field(bytes, 0x01, path.0.as_bytes());
    }
}

fn push_field(bytes: &mut Vec<u8>, tag: u8, value: &[u8]) {
    bytes.push(tag);
    push_count(bytes, value.len());
    bytes.extend_from_slice(value);
}

fn push_count(bytes: &mut Vec<u8>, count: usize) {
    // Every count is checked against a V1 collection limit before encoding; those
    // limits are far below u64::MAX on all supported targets.
    bytes.extend_from_slice(
        &u64::try_from(count)
            .expect("bounded count fits u64")
            .to_be_bytes(),
    );
}
