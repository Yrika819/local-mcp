//! E5-A — private, durable, content-addressed stage for external Writer output.
//!
//! The existing `MutationIntent` journal stores what recovery needs to *reason*
//! about a mutation (paths, preimages, observed state, intended digests, request
//! ids) but deliberately does not store the content bytes. An external Writer
//! needs those bytes after a crash, and re-asking the model would both break
//! at-most-once and turn a client into a substitute for durable state. This
//! module is the missing piece: a bounded, private stage that holds **only** the
//! host-validated content, keyed by the same `operation_id` the journal records.
//!
//! Deliberate properties:
//!
//! * **No authority is duplicated.** The stage stores no paths and no preimages.
//!   Publication reads those from the durable `MutationIntent`, so a tampered
//!   stage manifest cannot redirect a write to another file, and a stale stage
//!   cannot resurrect a superseded scope.
//! * **Identity-bound.** The manifest records session/Goal/Task/Attempt, plan
//!   revision, `operation_id`, and `scope_identity`. A load requires all of them
//!   to match the durable intent, so a stage cannot be replayed across Goals,
//!   sessions, attempts, or plans.
//! * **Integrity-verified.** Every content file is re-hashed on load and compared
//!   with both the manifest and the journal's intended digest. A mismatch is a
//!   hard failure, never a reconstruction guess.
//! * **Private.** The directory is created with the crate's private-directory
//!   helper, files are written through `secure_fs` (0600 on Unix, owner-only ACL
//!   on Windows), and content is replaced atomically after `sync_all`.
//! * **Bounded.** Per-operation and total byte bounds come from the Writer
//!   validator; the stage re-checks them so a forged manifest cannot exceed them.
//!
//! The stage is not a workspace mutation: it lives under the host state root and
//! is never published to the Goal workspace by this module.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::writer::{MAX_TOTAL_WRITE_CONTENT_BYTES, MAX_WRITE_CONTENT_BYTES};

pub(crate) const STAGE_SCHEMA_VERSION: u32 = 1;

/// Bounded, redacted stage failure. Never carries content or a model value.
#[derive(Debug)]
pub(crate) enum StageError {
    /// The stage directory or one of its files could not be created/read.
    Io,
    /// The manifest is absent, malformed, or of an unsupported schema.
    ManifestInvalid,
    /// The manifest identity does not match the durable intent.
    IdentityMismatch,
    /// A content file's digest or size disagrees with the manifest/journal.
    IntegrityMismatch,
    /// The stage exceeds a host byte bound.
    Oversized,
}

impl StageError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Io => "WRITER_STAGE_IO_FAILED",
            Self::ManifestInvalid => "WRITER_STAGE_MANIFEST_INVALID",
            Self::IdentityMismatch => "WRITER_STAGE_IDENTITY_MISMATCH",
            Self::IntegrityMismatch => "WRITER_STAGE_INTEGRITY_MISMATCH",
            Self::Oversized => "WRITER_STAGE_OVERSIZED",
        }
    }
}

/// The identity a stage must match to be usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StageIdentity {
    pub(crate) session_id: String,
    pub(crate) goal_id: String,
    pub(crate) task_id: String,
    pub(crate) attempt_id: String,
    pub(crate) plan_revision: u32,
    pub(crate) operation_id: String,
    pub(crate) scope_identity: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StageManifest {
    schema_version: u32,
    session_id: String,
    goal_id: String,
    task_id: String,
    attempt_id: String,
    plan_revision: u32,
    operation_id: String,
    scope_identity: String,
    operations: Vec<StageOperation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StageOperation {
    index: u32,
    content_sha256: String,
    size: u64,
}

/// A sealed stage handle. Holding one means the identity and manifest were
/// verified when it was created or loaded.
#[derive(Clone, Debug)]
pub(crate) struct WriteStage {
    root: PathBuf,
    manifest: StageManifest,
}

impl WriteStage {
    pub(crate) fn operation_id(&self) -> &str {
        &self.manifest.operation_id
    }

    pub(crate) fn operation_count(&self) -> usize {
        self.manifest.operations.len()
    }

    #[cfg(test)]
    pub(crate) fn total_bytes(&self) -> u64 {
        self.manifest.operations.iter().map(|op| op.size).sum()
    }

    /// Read one operation's content, verifying it against the manifest and the
    /// caller's durable expectation.
    pub(crate) fn read_content(
        &self,
        index: u32,
        expected_sha256: &str,
    ) -> Result<String, StageError> {
        let entry = self
            .manifest
            .operations
            .iter()
            .find(|operation| operation.index == index)
            .ok_or(StageError::IdentityMismatch)?;
        if entry.content_sha256 != expected_sha256 {
            return Err(StageError::IntegrityMismatch);
        }
        let bytes = fs::read(self.content_path(index)).map_err(|_| StageError::Io)?;
        if bytes.len() as u64 != entry.size {
            return Err(StageError::IntegrityMismatch);
        }
        if sha256_hex(&bytes) != entry.content_sha256 {
            return Err(StageError::IntegrityMismatch);
        }
        String::from_utf8(bytes).map_err(|_| StageError::IntegrityMismatch)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn stages_root(state_root: &Path, session_id: &str, goal_id: &str) -> PathBuf {
    state_root
        .join("stages")
        .join(sanitize_component(session_id))
        .join(sanitize_component(goal_id))
}

/// Directory for one `operation_id`.
///
/// The operation id is host-derived but may carry characters that are illegal in
/// a Windows path component (for example `:`). The directory name is therefore a
/// digest of the id; the manifest keeps the real id and loading verifies equality,
/// so the mapping is one-way and collision-checked by the identity comparison.
pub(crate) fn stage_directory(
    state_root: &Path,
    session_id: &str,
    goal_id: &str,
    operation_id: &str,
) -> PathBuf {
    let digest = sha256_hex(operation_id.as_bytes());
    stages_root(state_root, session_id, goal_id).join(&digest[..32])
}

/// Session and Goal ids are validated identifiers, but the stage path is built
/// from them, so a separator or traversal is refused rather than normalized.
fn sanitize_component(value: &str) -> &str {
    assert!(
        !value.is_empty()
            && !value.contains('/')
            && !value.contains('\\')
            && value != "."
            && value != "..",
        "stage component must be a plain identifier"
    );
    value
}

impl WriteStage {
    fn content_path(&self, index: u32) -> PathBuf {
        self.root.join(format!("op-{index:04}.content"))
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }
}

/// Persist one validated Writer operation set as a private stage.
///
/// `operations` carries `(index, content)` pairs produced by the host validator;
/// their digests are recomputed here rather than trusted. The caller then binds
/// this stage to the durable journal by `operation_id`/`scope_identity` in the
/// same commit that writes the `MutationIntent`.
pub(crate) fn write_stage(
    state_root: &Path,
    identity: &StageIdentity,
    operations: &[(u32, String)],
) -> Result<WriteStage, StageError> {
    if operations.is_empty() || operations.len() > crate::writer::MAX_WRITER_OPERATIONS {
        return Err(StageError::Oversized);
    }
    let mut total = 0usize;
    let mut manifest_operations = Vec::with_capacity(operations.len());
    for (index, content) in operations {
        if content.len() > MAX_WRITE_CONTENT_BYTES {
            return Err(StageError::Oversized);
        }
        total = total
            .checked_add(content.len())
            .ok_or(StageError::Oversized)?;
        manifest_operations.push(StageOperation {
            index: *index,
            content_sha256: sha256_hex(content.as_bytes()),
            size: content.len() as u64,
        });
    }
    if total > MAX_TOTAL_WRITE_CONTENT_BYTES {
        return Err(StageError::Oversized);
    }

    let root = stage_directory(
        state_root,
        &identity.session_id,
        &identity.goal_id,
        &identity.operation_id,
    );
    let parent = root.parent().ok_or(StageError::Io)?.to_path_buf();
    crate::secure_fs::ensure_private_directory(&parent, None).map_err(|_| StageError::Io)?;
    crate::secure_fs::ensure_private_directory(&root, Some(&parent)).map_err(|_| {
        // A stage directory that already exists is reused only when its manifest
        // is rewritten below; a foreign object at that path is a hard failure.
        StageError::Io
    })?;

    let manifest = StageManifest {
        schema_version: STAGE_SCHEMA_VERSION,
        session_id: identity.session_id.clone(),
        goal_id: identity.goal_id.clone(),
        task_id: identity.task_id.clone(),
        attempt_id: identity.attempt_id.clone(),
        plan_revision: identity.plan_revision,
        operation_id: identity.operation_id.clone(),
        scope_identity: identity.scope_identity.clone(),
        operations: manifest_operations,
    };
    let stage = WriteStage {
        root: root.clone(),
        manifest: manifest.clone(),
    };

    for (index, content) in operations {
        let destination = stage.content_path(*index);
        write_private_atomic(&destination, content.as_bytes())?;
    }
    let encoded = serde_json::to_vec_pretty(&manifest).map_err(|_| StageError::ManifestInvalid)?;
    write_private_atomic(&stage.manifest_path(), &encoded)?;
    crate::secure_fs::sync_parent_directory(&root).map_err(|_| StageError::Io)?;
    Ok(stage)
}

/// Load and verify a stage against the durable intent's identity.
pub(crate) fn load_stage(
    state_root: &Path,
    identity: &StageIdentity,
) -> Result<WriteStage, StageError> {
    let root = stage_directory(
        state_root,
        &identity.session_id,
        &identity.goal_id,
        &identity.operation_id,
    );
    let encoded = fs::read(root.join("manifest.json")).map_err(|_| StageError::ManifestInvalid)?;
    if encoded.len() > 64 * 1024 {
        return Err(StageError::ManifestInvalid);
    }
    let manifest: StageManifest =
        serde_json::from_slice(&encoded).map_err(|_| StageError::ManifestInvalid)?;
    if manifest.schema_version != STAGE_SCHEMA_VERSION {
        return Err(StageError::ManifestInvalid);
    }
    let matches = manifest.session_id == identity.session_id
        && manifest.goal_id == identity.goal_id
        && manifest.task_id == identity.task_id
        && manifest.attempt_id == identity.attempt_id
        && manifest.plan_revision == identity.plan_revision
        && manifest.operation_id == identity.operation_id
        && manifest.scope_identity == identity.scope_identity;
    if !matches {
        return Err(StageError::IdentityMismatch);
    }
    if manifest.operations.is_empty()
        || manifest.operations.len() > crate::writer::MAX_WRITER_OPERATIONS
    {
        return Err(StageError::Oversized);
    }
    let mut total = 0u64;
    for operation in &manifest.operations {
        if operation.size > MAX_WRITE_CONTENT_BYTES as u64 {
            return Err(StageError::Oversized);
        }
        total = total
            .checked_add(operation.size)
            .ok_or(StageError::Oversized)?;
        if operation.content_sha256.len() != 64
            || !operation
                .content_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(StageError::ManifestInvalid);
        }
    }
    if total > MAX_TOTAL_WRITE_CONTENT_BYTES as u64 {
        return Err(StageError::Oversized);
    }
    Ok(WriteStage { root, manifest })
}

/// Remove a stage whose journal proves no mutation was ever begun.
///
/// Callers must only reach this from a state in which the durable
/// `MutationIntent` has no begun operation (or no intent at all). Deleting a
/// stage after `BeginOperation` would destroy recovery evidence.
pub(crate) fn remove_never_begun_stage(
    state_root: &Path,
    session_id: &str,
    goal_id: &str,
    operation_id: &str,
) -> Result<(), StageError> {
    let root = stage_directory(state_root, session_id, goal_id, operation_id);
    match fs::remove_dir_all(&root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(StageError::Io),
    }
}

fn write_private_atomic(destination: &Path, bytes: &[u8]) -> Result<(), StageError> {
    let (temporary, mut file) =
        crate::secure_fs::create_unique_temp(destination).map_err(|_| StageError::Io)?;
    let result = (|| {
        file.write_all(bytes).map_err(|_| StageError::Io)?;
        file.sync_all().map_err(|_| StageError::Io)?;
        Ok::<(), StageError>(())
    })();
    if let Err(error) = result {
        drop(file);
        let _ = crate::secure_fs::remove_private_temp(&temporary);
        return Err(error);
    }
    drop(file);
    crate::secure_fs::atomic_replace(&temporary, destination).map_err(|_| {
        let _ = crate::secure_fs::remove_private_temp(&temporary);
        StageError::Io
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn identity(operation_id: &str) -> StageIdentity {
        StageIdentity {
            session_id: "stage-session".to_owned(),
            goal_id: Uuid::new_v4().to_string(),
            task_id: Uuid::new_v4().to_string(),
            attempt_id: Uuid::new_v4().to_string(),
            plan_revision: 1,
            operation_id: operation_id.to_owned(),
            scope_identity: "scope-1".to_owned(),
        }
    }

    fn root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("local-mcp-e5-stage-{label}-{}", Uuid::new_v4()))
    }

    #[test]
    fn a_stage_round_trips_and_verifies_content() {
        let root = root("roundtrip");
        let identity = identity("op-1");
        let stage = write_stage(
            &root,
            &identity,
            &[(0, "hello".to_owned()), (1, "world".to_owned())],
        )
        .unwrap();
        assert_eq!(stage.operation_count(), 2);
        assert_eq!(
            stage.read_content(0, &sha256_hex(b"hello")).unwrap(),
            "hello"
        );
        let loaded = load_stage(&root, &identity).unwrap();
        assert_eq!(loaded.total_bytes(), 10);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_tampered_content_file_is_refused_not_reconstructed() {
        let root = root("tamper");
        let identity = identity("op-2");
        let stage = write_stage(&root, &identity, &[(0, "original".to_owned())]).unwrap();
        fs::write(stage.content_path(0), b"tampered").unwrap();
        let error = stage.read_content(0, &sha256_hex(b"original")).unwrap_err();
        assert_eq!(error.code(), "WRITER_STAGE_INTEGRITY_MISMATCH");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_deleted_content_file_is_refused() {
        let root = root("deleted");
        let identity = identity("op-3");
        let stage = write_stage(&root, &identity, &[(0, "gone".to_owned())]).unwrap();
        fs::remove_file(stage.content_path(0)).unwrap();
        assert_eq!(
            stage
                .read_content(0, &sha256_hex(b"gone"))
                .unwrap_err()
                .code(),
            "WRITER_STAGE_IO_FAILED"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stage_cannot_be_loaded_for_another_identity() {
        let root = root("identity");
        let identity = identity("op-4");
        write_stage(&root, &identity, &[(0, "x".to_owned())]).unwrap();
        let mut other = identity.clone();
        other.attempt_id = Uuid::new_v4().to_string();
        assert_eq!(
            load_stage(&root, &other).unwrap_err().code(),
            "WRITER_STAGE_IDENTITY_MISMATCH"
        );
        let mut other = identity.clone();
        other.scope_identity = "scope-2".to_owned();
        assert_eq!(
            load_stage(&root, &other).unwrap_err().code(),
            "WRITER_STAGE_IDENTITY_MISMATCH"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_oversized_operation_set_is_refused() {
        let root = root("oversized");
        let identity = identity("op-5");
        let big = "x".repeat(MAX_WRITE_CONTENT_BYTES + 1);
        assert_eq!(
            write_stage(&root, &identity, &[(0, big)])
                .unwrap_err()
                .code(),
            "WRITER_STAGE_OVERSIZED"
        );
        assert_eq!(
            write_stage(&root, &identity, &[]).unwrap_err().code(),
            "WRITER_STAGE_OVERSIZED"
        );
    }
}
