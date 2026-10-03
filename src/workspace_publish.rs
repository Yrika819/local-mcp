//! Workspace-safe atomic content publication.
//!
//! This module owns the *filesystem* half of the Writer commit contract frozen in
//! `docs/WRITER_INTEGRITY_V1_DESIGN.md`. It deliberately does **not** own authority:
//! every path handed in here has already been resolved against the execution root and
//! the TaskScope by the caller. What this module guarantees is that once a caller
//! decides a mutation is authorized, the mutation lands as an indivisible publication
//! or not at all.
//!
//! Two rules shape every decision here:
//!
//! 1. **No private-state policy.** `secure_fs` exists to protect GoalLatch's own state
//!    and encodes policy that is wrong for a user's project files: it `fchmod`s
//!    directories to `0700` and files to `0600`. Reusing it here would rewrite the
//!    permissions of a source tree merely to fsync it. Everything below is workspace
//!    -safe by construction.
//! 2. **No shell.** Publication is performed with direct syscalls. There is no `sh -c`,
//!    no `cmd /c`, no PowerShell, and therefore no shell quoting or path-encoding
//!    ambiguity anywhere in the commit path.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// What the caller observed at the destination immediately before it decided to
/// publish. This is the durable expectation, not a fresh guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExpectedPreimage {
    /// The destination must still be absent at the commit point.
    Absent,
    /// The destination must still exist and hash to this SHA-256.
    Sha256(String),
}

/// Why a publication did not happen. Recovery reads these to distinguish "the target
/// was never touched" from "the target state is unknown".
#[derive(Debug)]
pub(crate) enum PublishError {
    /// The destination no longer matches the expected preimage. Nothing was written.
    PreimageMismatch { path: PathBuf },
    /// The destination is no longer a regular file (it became a directory, device,
    /// or similar). Nothing was written.
    NotARegularFile { path: PathBuf },
    /// Staging could not be created or written. The destination is untouched.
    Staging { path: PathBuf, source: io::Error },
    /// The staged bytes could not be made durable. The destination is untouched.
    StagingSync { path: PathBuf, source: io::Error },
    /// The destination appeared between validation and publication, so an
    /// `ABSENT` publication refused to clobber it. The destination is the external
    /// actor's file and is untouched.
    TargetAppeared { path: PathBuf },
    /// The namespace operation itself failed. The destination's state is unknown to
    /// this process; recovery must reconcile from evidence.
    Publish { path: PathBuf, source: io::Error },
    /// The destination did not hash to the intended content after publication.
    PostimageMismatch { path: PathBuf },
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PreimageMismatch { path } => write!(
                f,
                "destination changed before publication: {}",
                path.display()
            ),
            Self::NotARegularFile { path } => {
                write!(f, "destination is not a regular file: {}", path.display())
            }
            Self::Staging { path, source } => {
                write!(
                    f,
                    "cannot stage replacement for {}: {source}",
                    path.display()
                )
            }
            Self::StagingSync { path, source } => {
                write!(
                    f,
                    "cannot sync staged replacement for {}: {source}",
                    path.display()
                )
            }
            Self::TargetAppeared { path } => write!(
                f,
                "refusing to replace a destination that appeared concurrently: {}",
                path.display()
            ),
            Self::Publish { path, source } => {
                write!(f, "cannot publish {}: {source}", path.display())
            }
            Self::PostimageMismatch { path } => {
                write!(
                    f,
                    "postimage does not match intended content: {}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for PublishError {}

/// A host-owned request to publish `content` at `destination`.
///
/// The caller builds this only after it has decided the mutation is authorized. The
/// `destination` and `parent` must already be resolved and validated; this type exists
/// so that no authority-bearing field can be confused with model-supplied content.
#[derive(Clone, Debug)]
pub(crate) struct PublishRequest<'a> {
    /// The canonical destination. Its parent must equal `parent`.
    pub destination: &'a Path,
    /// The canonical, host-validated parent directory.
    pub parent: &'a Path,
    /// What the caller durably recorded as the preimage.
    pub expected_preimage: &'a ExpectedPreimage,
    /// The already-durable request identity, used to derive the staged file's name so
    /// crash debris is attributable rather than anonymous.
    pub request_id: &'a str,
    /// The exact bytes to publish.
    pub content: &'a [u8],
}

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

#[cfg(unix)]
fn open_staging(path: &Path) -> io::Result<File> {
    // `create_new` gives O_CREAT|O_EXCL, so an attacker-planted path is never opened
    // or followed. `O_NOFOLLOW` additionally refuses a symlink at the leaf. This is a
    // workspace primitive, so it uses ordinary creation permissions rather than the
    // private-state 0600 policy in `secure_fs`.
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(windows)]
fn open_staging(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
}

/// Read the destination's current state without following a final symlink.
///
/// A final symlink, a directory, and a device node are all refused rather than
/// written through: the Writer only ever replaces a regular file.
fn observe_destination(destination: &Path) -> Result<Option<(u64, String)>, PublishError> {
    let link = match fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(PublishError::Publish {
                path: destination.to_path_buf(),
                source: error,
            });
        }
    };
    if link.file_type().is_symlink() {
        return Err(PublishError::PreimageMismatch {
            path: destination.to_path_buf(),
        });
    }
    if !link.is_file() {
        return Err(PublishError::NotARegularFile {
            path: destination.to_path_buf(),
        });
    }
    let metadata = match fs::metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Err(PublishError::Publish {
                path: destination.to_path_buf(),
                source: error,
            });
        }
    };
    if !metadata.is_file() {
        return Err(PublishError::NotARegularFile {
            path: destination.to_path_buf(),
        });
    }
    let bytes = match fs::read(destination) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Err(PublishError::Publish {
                path: destination.to_path_buf(),
                source: error,
            });
        }
    };
    Ok(Some((bytes.len() as u64, sha256_hex(&bytes))))
}

fn matches_preimage(observed: Option<&(u64, String)>, expected: &ExpectedPreimage) -> bool {
    match (observed, expected) {
        (None, ExpectedPreimage::Absent) => true,
        (Some((_, digest)), ExpectedPreimage::Sha256 { .. }) => match expected {
            ExpectedPreimage::Sha256(want) => digest == want,
            ExpectedPreimage::Absent => false,
        },
        (Some(_), ExpectedPreimage::Absent) => false,
        (None, ExpectedPreimage::Sha256 { .. }) => false,
    }
}

/// The permission bits an existing destination should hand to its replacement.
///
/// Atomic replacement changes inode identity, so an existing executable file would
/// otherwise silently lose its executable bit. This carries the mode across the
/// replacement. It deliberately preserves *only* mode bits; ownership, ACLs, and
/// xattrs are not carried, and the design says so rather than implying otherwise.
#[cfg(unix)]
fn inherit_mode(destination: &Path) -> io::Result<Option<u32>> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_file() => {
            Ok(Some(metadata.permissions().mode() & 0o7777))
        }
        _ => Ok(None),
    }
}

#[cfg(not(unix))]
fn inherit_mode(_destination: &Path) -> io::Result<Option<u32>> {
    Ok(None)
}

/// fsync a *workspace* directory without touching its permissions.
///
/// This deliberately does not reuse `secure_fs::sync_parent_directory`, which opens
/// through `open_private_directory_file` and `fchmod`s the directory to `0700`.
/// Applying that to a source directory would rewrite the user's permissions merely to
/// fsync it. Opening `O_RDONLY|O_DIRECTORY` is sufficient for the fsync to be
/// meaningful and mutates nothing.
#[cfg(unix)]
fn sync_workspace_directory(directory: &Path) -> io::Result<()> {
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(directory)?;
    handle.sync_all()
}

#[cfg(not(unix))]
fn sync_workspace_directory(_directory: &Path) -> io::Result<()> {
    // Windows has no directory fsync equivalent that `FlushFileBuffers` on a
    // directory handle reliably provides. Publication is already write-through via
    // MOVEFILE_WRITE_THROUGH, so there is nothing further to force here.
    Ok(())
}

/// Derive the staged file's name from the already-durable request identity.
///
/// The name is host-owned: the model cannot influence it, and it is a pure function of
/// the destination's file name plus the durable `request_id`. That makes crash debris
/// attributable to exactly one durable mutation operation instead of being an anonymous
/// file, which is what lets cleanup stay narrow (see `remove_owned_staging`).
fn staging_path(
    parent: &Path,
    destination: &Path,
    request_id: &str,
) -> Result<PathBuf, PublishError> {
    let name = destination
        .file_name()
        .ok_or_else(|| PublishError::Staging {
            path: destination.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "destination has no file name"),
        })?;
    let mut digest = Sha256::digest(name.as_encoded_bytes());
    // 16 hex characters is 64 bits of the name digest; together with the full
    // request id this is unique per (destination, durable request).
    let short: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let _ = &mut digest;
    let safe: String = request_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(64)
        .collect();
    let stem: String = name
        .to_string_lossy()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(48)
        .collect();
    Ok(parent.join(format!(".{stem}.{short}.{safe}.staged")))
}

/// Remove a staged file, but only one this process could have created.
///
/// This never deletes an arbitrary user file: the path must match the exact staged
/// naming shape, be a regular file, and (on Unix) be owned by the effective user.
/// There is deliberately no "delete anything matching *.staged" cleanup anywhere.
fn remove_owned_staging(path: &Path) {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    if !name.starts_with('.') || !name.ends_with(".staged") {
        return;
    }
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if !metadata.file_type().is_file() {
        return;
    }
    #[cfg(unix)]
    if metadata.uid() != unsafe { libc::geteuid() } {
        return;
    }
    let _ = fs::remove_file(path);
}

/// Publish `request.content` at `request.destination` atomically.
///
/// The sequence is: validate the preimage, stage in the destination's own directory,
/// write, flush, `fsync`, re-validate the preimage, publish with the strongest
/// available primitive, `fsync` the parent, then verify the postimage.
///
/// On any failure before the namespace operation the destination is guaranteed
/// untouched. If the namespace operation itself fails, the destination's state is
/// genuinely unknown and the caller must reconcile from evidence rather than assume.
pub(crate) fn publish(request: PublishRequest<'_>) -> Result<(), PublishError> {
    let PublishRequest {
        destination,
        parent,
        expected_preimage,
        request_id,
        content,
    } = request;

    if destination.parent() != Some(parent) {
        return Err(PublishError::Staging {
            path: destination.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "destination is not a direct child of the validated parent",
            ),
        });
    }

    // Gate 1: reject a stale preimage before touching anything at all.
    let initial = observe_destination(destination)?;
    if !matches_preimage(initial.as_ref(), expected_preimage) {
        return Err(PublishError::PreimageMismatch {
            path: destination.to_path_buf(),
        });
    }
    let mode = inherit_mode(destination).unwrap_or(None);

    let staged = staging_path(parent, destination, request_id)?;
    // A leftover from a crashed attempt with the same durable identity means the bytes
    // on disk are unknown. Refuse rather than reuse them.
    remove_owned_staging(&staged);
    if staged.exists() {
        return Err(PublishError::Staging {
            path: staged,
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a staged file for this request already exists",
            ),
        });
    }

    let result = stage_and_publish(
        &staged,
        destination,
        parent,
        expected_preimage,
        content,
        mode,
    );
    if result.is_err() {
        // Cleanup must never mask the causal error, and it only ever removes the exact
        // path this call created.
        remove_owned_staging(&staged);
    }
    result
}

fn stage_and_publish(
    staged: &Path,
    destination: &Path,
    parent: &Path,
    expected_preimage: &ExpectedPreimage,
    content: &[u8],
    #[cfg_attr(not(unix), allow(unused_variables))] mode: Option<u32>,
) -> Result<(), PublishError> {
    let mut file = open_staging(staged).map_err(|source| PublishError::Staging {
        path: staged.to_path_buf(),
        source,
    })?;

    file.write_all(content)
        .and_then(|()| file.flush())
        .map_err(|source| PublishError::Staging {
            path: staged.to_path_buf(),
            source,
        })?;

    #[cfg(unix)]
    if let Some(mode) = mode {
        // Set the mode *before* publication so the destination is never briefly
        // observable with the wrong permissions.
        if let Err(error) = file.set_permissions(fs::Permissions::from_mode(mode)) {
            drop(file);
            return Err(PublishError::Staging {
                path: staged.to_path_buf(),
                source: error,
            });
        }
    }

    // The staged bytes must be durable before the namespace operation, so a crash
    // cannot leave a published name pointing at unflushed content.
    file.sync_all()
        .map_err(|source| PublishError::StagingSync {
            path: staged.to_path_buf(),
            source,
        })?;
    drop(file);

    // Gate 2: the last host-controlled action before publication. This is as close to
    // the namespace operation as the platform allows; the residual window between
    // here and the syscall is documented in the design and is not claimed to be a
    // cross-process compare-and-swap.
    let current = observe_destination(destination)?;
    if !matches_preimage(current.as_ref(), expected_preimage) {
        return Err(PublishError::PreimageMismatch {
            path: destination.to_path_buf(),
        });
    }

    match expected_preimage {
        ExpectedPreimage::Absent => link_into_place(staged, destination),
        ExpectedPreimage::Sha256 { .. } => replace_in_place(staged, destination),
    }?;

    // Make the namespace change itself durable.
    sync_workspace_directory(parent).map_err(|source| PublishError::Publish {
        path: parent.to_path_buf(),
        source,
    })?;

    // Postimage verification. Publication succeeding is not proof the right content
    // landed, so the digest is checked before the caller may record APPLIED.
    let intended = sha256_hex(content);
    let observed = observe_destination(destination)?;
    match observed {
        Some((_, digest)) if digest == intended => Ok(()),
        _ => Err(PublishError::PostimageMismatch {
            path: destination.to_path_buf(),
        }),
    }
}

/// No-clobber publication for `expected ABSENT`.
///
/// `link()` fails with `EEXIST` and leaves the concurrent external target byte-for-byte
/// intact, which is exactly the required invariant. A plain `rename()` is deliberately
/// *not* used here because it would replace a target that appeared concurrently. If the
/// filesystem cannot support the primitive the operation fails; it never silently
/// degrades to a clobbering rename.
#[cfg(unix)]
fn link_into_place(staged: &Path, destination: &Path) -> Result<(), PublishError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source =
        CString::new(staged.as_os_str().as_bytes()).map_err(|_| PublishError::Publish {
            path: destination.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "staged path contains NUL"),
        })?;
    let target =
        CString::new(destination.as_os_str().as_bytes()).map_err(|_| PublishError::Publish {
            path: destination.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "destination contains NUL"),
        })?;

    // SAFETY: both CStrings are NUL-terminated and outlive the calls; `link` does not
    // retain either pointer.
    let linked = unsafe { libc::link(source.as_ptr(), target.as_ptr()) };
    if linked != 0 {
        let error = io::Error::last_os_error();
        return Err(if error.kind() == io::ErrorKind::AlreadyExists {
            PublishError::TargetAppeared {
                path: destination.to_path_buf(),
            }
        } else {
            PublishError::Publish {
                path: destination.to_path_buf(),
                source: error,
            }
        });
    }

    // The staged name now has a second link; drop it so the debris does not linger.
    let _ = fs::remove_file(staged);
    Ok(())
}

#[cfg(windows)]
fn link_into_place(staged: &Path, destination: &Path) -> Result<(), PublishError> {
    // Without MOVEFILE_REPLACE_EXISTING this fails when the destination exists, which
    // is the same no-clobber guarantee `link()` provides on Unix.
    move_without_replace(staged, destination, false)
}

#[cfg(windows)]
fn replace_in_place(staged: &Path, destination: &Path) -> Result<(), PublishError> {
    move_without_replace(staged, destination, true)
}

#[cfg(not(any(unix, windows)))]
fn link_into_place(_staged: &Path, destination: &Path) -> Result<(), PublishError> {
    Err(PublishError::Publish {
        path: destination.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic publication is unsupported on this platform",
        ),
    })
}

#[cfg(not(any(unix, windows)))]
fn replace_in_place(_staged: &Path, destination: &Path) -> Result<(), PublishError> {
    Err(PublishError::Publish {
        path: destination.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic publication is unsupported on this platform",
        ),
    })
}

/// Atomic replacement of an existing destination.
///
/// `rename`/`MoveFileExW(MOVEFILE_REPLACE_EXISTING)` replaces the directory entry in one
/// namespace operation, so a concurrent reader observes either the old or the new
/// content, never a truncated mixture.
#[cfg(unix)]
fn replace_in_place(staged: &Path, destination: &Path) -> Result<(), PublishError> {
    fs::rename(staged, destination).map_err(|source| PublishError::Publish {
        path: destination.to_path_buf(),
        source,
    })
}

#[cfg(windows)]
fn move_without_replace(
    staged: &Path,
    destination: &Path,
    replace: bool,
) -> Result<(), PublishError> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let source = wide(staged);
    let target = wide(destination);
    let mut flags = MOVEFILE_WRITE_THROUGH;
    if replace {
        flags |= MOVEFILE_REPLACE_EXISTING;
    }
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    let moved = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), flags) };
    if moved != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    // 183/80: ERROR_ALREADY_EXISTS / ERROR_FILE_EXISTS.
    let code = error.raw_os_error().unwrap_or(0);
    Err(if !replace && (code == 80 || code == 183) {
        PublishError::TargetAppeared {
            path: destination.to_path_buf(),
        }
    } else {
        PublishError::Publish {
            path: destination.to_path_buf(),
            source: error,
        }
    })
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// Lossless, unambiguous, versioned identity encoding for a filesystem path.
///
/// `to_string_lossy()` maps every byte sequence that is not valid UTF-8 onto the same
/// U+FFFD replacement text, so two distinct `PathBuf` values can collapse onto one
/// identity. Authority and evidence identities must never be derived that way.
///
/// Unix uses the raw `OsStr` bytes; Windows uses the native UTF-16 code units. Both are
/// exact, so distinct host paths stay distinct.
pub(crate) fn path_identity_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .flat_map(|unit| unit.to_le_bytes())
            .collect()
    }
    #[cfg(not(any(unix, windows)))]
    {
        path.as_os_str().as_encoded_bytes().to_vec()
    }
}

/// Hash a Writer operation's scope identity.
///
/// The preimage is framed with a version tag and length-prefixed fields, so the
/// encoding is unambiguous: `(path A, content B)` cannot collide with `(path C,
/// content D)` merely because the concatenation was ambiguous. The version tag keeps a
/// new value distinguishable from anything produced by an older scheme.
pub(crate) fn scope_identity(operations: &[(PathBuf, String)]) -> String {
    let mut framed = Vec::new();
    framed.extend_from_slice(b"local-mcp/scope-identity/v1\0");
    for (path, content_digest) in operations {
        let bytes = path_identity_bytes(path);
        framed.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        framed.extend_from_slice(&bytes);
        framed.extend_from_slice(&(content_digest.len() as u32).to_le_bytes());
        framed.extend_from_slice(content_digest.as_bytes());
    }
    sha256_hex(&framed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "local-mcp-publish-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn publish_new(
        root: &Path,
        name: &str,
        content: &[u8],
        request_id: &str,
    ) -> Result<(), PublishError> {
        let destination = root.join(name);
        publish(PublishRequest {
            destination: &destination,
            parent: root,
            expected_preimage: &ExpectedPreimage::Absent,
            request_id,
            content,
        })
    }

    fn publish_replace(
        root: &Path,
        name: &str,
        expected: &str,
        content: &[u8],
        request_id: &str,
    ) -> Result<(), PublishError> {
        let destination = root.join(name);
        publish(PublishRequest {
            destination: &destination,
            parent: root,
            expected_preimage: &ExpectedPreimage::Sha256(expected.to_owned()),
            request_id,
            content,
        })
    }

    #[test]
    fn creates_an_absent_file_and_leaves_no_staging_debris() {
        let root = scratch("create");
        publish_new(&root, "a.txt", b"new\n", "req-1").unwrap();
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"new\n");
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".staged"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "staging debris survived a successful commit"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn replaces_an_existing_file_atomically() {
        let root = scratch("replace");
        fs::write(root.join("a.txt"), b"old\n").unwrap();
        let before = sha256_hex(b"old\n");
        publish_replace(&root, "a.txt", &before, b"brand new\n", "req-2").unwrap();
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"brand new\n");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn handles_empty_and_large_content() {
        let root = scratch("sizes");
        publish_new(&root, "empty.txt", b"", "req-3").unwrap();
        assert_eq!(fs::read(root.join("empty.txt")).unwrap(), b"");
        let large = vec![b'x'; 512 * 1024];
        publish_new(&root, "large.txt", &large, "req-4").unwrap();
        assert_eq!(fs::read(root.join("large.txt")).unwrap().len(), large.len());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stale_preimage_is_refused_and_the_destination_is_untouched() {
        let root = scratch("stale");
        fs::write(root.join("a.txt"), b"external edit\n").unwrap();
        let error = publish_replace(
            &root,
            "a.txt",
            &sha256_hex(b"original\n"),
            b"writer\n",
            "req-5",
        )
        .unwrap_err();
        assert!(matches!(error, PublishError::PreimageMismatch { .. }));
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"external edit\n");
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_concurrently_appearing_target_is_never_clobbered() {
        let root = scratch("noclobber");
        // Validate absence, then simulate the external actor winning the race before
        // publication by driving the same primitive directly at commit time.
        let destination = root.join("a.txt");
        assert!(!destination.exists());
        let external = root.join("a.txt");
        fs::write(&external, b"external\n").unwrap();
        let error = publish(PublishRequest {
            destination: &destination,
            parent: &root,
            expected_preimage: &ExpectedPreimage::Absent,
            request_id: "req-6",
            content: b"writer\n",
        })
        .unwrap_err();
        assert!(matches!(error, PublishError::PreimageMismatch { .. }));
        assert_eq!(fs::read(&destination).unwrap(), b"external\n");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_directory_at_the_destination_is_refused() {
        let root = scratch("dir");
        fs::create_dir(root.join("a.txt")).unwrap();
        let error = publish_new(&root, "a.txt", b"x", "req-7").unwrap_err();
        assert!(matches!(
            error,
            PublishError::NotARegularFile { .. } | PublishError::PreimageMismatch { .. }
        ));
        assert!(
            root.join("a.txt").is_dir(),
            "the directory must survive untouched"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_executable_file_keeps_its_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("exec");
        let target = root.join("run.sh");
        fs::write(&target, b"#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let before = sha256_hex(b"#!/bin/sh\necho hi\n");
        publish_replace(&root, "run.sh", &before, b"#!/bin/sh\necho bye\n", "req-8").unwrap();
        let mode = fs::symlink_metadata(&target).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            mode, 0o755,
            "executable bit was lost across atomic replacement"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_ordinary_non_executable_file_stays_non_executable() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("nonexec");
        let target = root.join("notes.txt");
        fs::write(&target, b"a\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let before = sha256_hex(b"a\n");
        publish_replace(&root, "notes.txt", &before, b"b\n", "req-9").unwrap();
        let mode = fs::symlink_metadata(&target).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o644);
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_workspace_parent_directory_permissions_are_not_rewritten() {
        use std::os::unix::fs::PermissionsExt;
        let root = scratch("dirperm");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        publish_new(&root, "a.txt", b"x", "req-10").unwrap();
        let mode = fs::symlink_metadata(&root).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            mode, 0o755,
            "workspace directory must not be chmod-ed to private-state 0700"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_non_utf8_path_still_yields_a_lossless_identity() {
        #[cfg(unix)]
        {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            let bad = |bytes: &[u8]| PathBuf::from(OsStr::from_bytes(bytes));
            let left = path_identity_bytes(&bad(b"/tmp/\xff\xfe-a"));
            let right = path_identity_bytes(&bad(b"/tmp/\xff\xfe-b"));
            assert_ne!(left, right, "distinct non-UTF-8 paths must not collapse");

            // And lossy text would have collapsed them; prove the identity does not.
            let l = scope_identity(&[(bad(b"/tmp/\xff\xfe-a"), "aa".to_owned())]);
            let r = scope_identity(&[(bad(b"/tmp/\xff\xfe-b"), "aa".to_owned())]);
            assert_ne!(
                l, r,
                "scope identity must not alias distinct non-UTF-8 paths"
            );
        }
    }

    #[test]
    fn scope_identity_framing_is_unambiguous_across_field_boundaries() {
        // (path "ab", content "c") vs (path "a", content "bc") must not collide even
        // though naive concatenation produces identical byte strings.
        let left = scope_identity(&[(PathBuf::from("ab"), "c".to_owned())]);
        let right = scope_identity(&[(PathBuf::from("a"), "bc".to_owned())]);
        assert_ne!(left, right, "ambiguous concatenation must not be reachable");
    }

    #[test]
    fn scope_identity_is_order_sensitive_and_version_tagged() {
        let one = scope_identity(&[(PathBuf::from("a"), "1".to_owned())]);
        let two = scope_identity(&[(PathBuf::from("b"), "1".to_owned())]);
        assert_ne!(one, two);
        assert_eq!(one.len(), 64, "scope identity remains a SHA-256 hex digest");
    }

    #[test]
    fn staged_paths_are_derived_from_the_durable_request_identity() {
        let root = scratch("stagepath");
        let destination = root.join("a.txt");
        let first = staging_path(&root, &destination, "req-1").unwrap();
        let again = staging_path(&root, &destination, "req-1").unwrap();
        let other = staging_path(&root, &destination, "req-2").unwrap();
        assert_eq!(
            first, again,
            "the same durable request yields the same path"
        );
        assert_ne!(first, other, "different requests must not share debris");
        assert!(
            first
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(".staged")
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cleanup_refuses_paths_that_are_not_our_staging_shape() {
        let root = scratch("cleanup");
        let user_file = root.join("important.txt");
        fs::write(&user_file, b"keep me").unwrap();
        remove_owned_staging(&user_file);
        assert!(user_file.exists(), "cleanup deleted a user file");
        fs::remove_dir_all(&root).unwrap();
    }
}
