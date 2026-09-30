use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::secure_fs;

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub permitted_directories: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathIntent {
    ReadExisting,
    ListDirectory,
    WriteExisting,
    CreateFile,
    ExecutionCwd,
}

/// Resolve a caller-provided path without allowing the caller to expand the
/// session's authority. Existing targets are canonicalized themselves; new
/// targets are validated through their canonicalized parent. This also blocks
/// symlink escapes from every permitted root.
pub fn validate_path_authority(
    session: &Session,
    requested: &Path,
    intent: PathIntent,
) -> Result<PathBuf> {
    let roots = session
        .permitted_directories
        .iter()
        .map(|root| {
            std::fs::canonicalize(root)
                .with_context(|| format!("cannot resolve permitted root {}", root.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(!roots.is_empty(), "session has no permitted directories");

    let candidate = if requested.is_absolute() {
        requested.to_owned()
    } else {
        session.cwd.join(requested)
    };

    // Windows leaf names cannot contain ':' except as ADS syntax (`name:stream`)
    // or drive-relative prefixes. ADS writes follow the host file's reparse
    // point and can land outside the permitted roots even when `name:stream`
    // itself is NotFound. Reject that shape outright. Drive-relative names
    // (`C:foo`) resolve against the process cwd on that drive, not the session.
    #[cfg(windows)]
    {
        use std::path::Component;
        let drive_relative = !candidate.is_absolute()
            && candidate
                .components()
                .next()
                .is_some_and(|component| matches!(component, Component::Prefix(_)));
        anyhow::ensure!(
            !drive_relative,
            "drive-relative paths are not allowed: {}",
            requested.display()
        );
        for component in candidate.components() {
            if let Component::Normal(part) = component
                && part.to_string_lossy().contains(':')
            {
                anyhow::bail!(
                    "path contains Windows reserved ':' stream/drive syntax: {}",
                    requested.display()
                );
            }
        }
    }

    let existing = matches!(
        intent,
        PathIntent::ReadExisting
            | PathIntent::ListDirectory
            | PathIntent::WriteExisting
            | PathIntent::ExecutionCwd
    );
    let resolved = if existing {
        std::fs::canonicalize(&candidate)
            .with_context(|| format!("cannot resolve {}", candidate.display()))?
    } else {
        // CreateFile must not treat a dangling final symlink (or any other
        // reparse point) as a creatable leaf name: open/write would follow it
        // outside the permitted roots. If anything already occupies the leaf,
        // resolve it fully and reject broken links / out-of-bounds targets.
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => std::fs::canonicalize(&candidate).with_context(|| {
                format!(
                    "existing writer target cannot be canonicalized; broken symlinks are rejected: {}",
                    candidate.display()
                )
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = candidate.parent().context("path has no parent directory")?;
                let parent = std::fs::canonicalize(parent)
                    .with_context(|| format!("cannot resolve parent {}", parent.display()))?;
                parent.join(candidate.file_name().context("path has no file name")?)
            }
            Err(error) => {
                return Err(anyhow::Error::new(error)
                    .context(format!("cannot inspect {}", candidate.display())));
            }
        }
    };

    anyhow::ensure!(
        roots.iter().any(|root| resolved.starts_with(root)),
        "path is outside the session's permitted directories: {}",
        requested.display()
    );
    if matches!(intent, PathIntent::ExecutionCwd | PathIntent::ListDirectory) {
        anyhow::ensure!(
            resolved.is_dir(),
            "path is not a directory: {}",
            resolved.display()
        );
    }
    Ok(resolved)
}

/// Maximum number of not-yet-existing leading components resolved when checking
/// a future path against Session authority. Bounded so a pathological path
/// cannot spin.
const MAX_UNRESOLVED_PATH_COMPONENTS: usize = 64;

/// Resolve a host-derived path that does not exist yet to a canonical absolute
/// path, by canonicalizing the deepest existing ancestor and re-appending the
/// missing components.
///
/// This grants no authority and touches no session state. It exists so a
/// host-derived managed target is canonical before it is persisted, and so the
/// Session authority check below can compare the live resolution against the
/// durable record.
pub fn canonical_future_path(requested: &Path) -> Result<PathBuf> {
    let candidate = requested.to_owned();
    anyhow::ensure!(
        candidate.is_absolute(),
        "path is not absolute: {}",
        requested.display()
    );

    let mut unresolved: Vec<OsString> = Vec::new();
    let mut existing = candidate.clone();
    loop {
        anyhow::ensure!(
            unresolved.len() < MAX_UNRESOLVED_PATH_COMPONENTS,
            "path has too many unresolved components: {}",
            requested.display()
        );
        match std::fs::symlink_metadata(&existing) {
            // The leaf may be a symlink or reparse point; `canonicalize` below
            // resolves it. A dangling link fails to canonicalize and is rejected.
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing
                    .file_name()
                    .with_context(|| format!("path has no file name: {}", existing.display()))?
                    .to_os_string();
                anyhow::ensure!(
                    name != "..",
                    "path may not contain '..' components: {}",
                    requested.display()
                );
                unresolved.push(name);
                let parent = existing.parent().map(Path::to_path_buf).with_context(|| {
                    format!("path has no existing ancestor: {}", requested.display())
                })?;
                existing = parent;
            }
            Err(error) => {
                return Err(anyhow::Error::new(error)
                    .context(format!("cannot inspect {}", existing.display())));
            }
        }
    }

    let mut resolved = std::fs::canonicalize(&existing)
        .with_context(|| format!("cannot resolve existing ancestor {}", existing.display()))?;
    for name in unresolved.iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

/// Resolve a host-derived path that does not exist yet, and require that the
/// result is **already** covered by current Session filesystem authority.
///
/// This is the Managed Worktrees V1 Session path-authority gate
/// (`docs/MANAGED_WORKTREES_V1_DESIGN.md` section 7). It deliberately:
///
/// - never mutates `session.permitted_directories`;
/// - never treats durable Goal state as authorization;
/// - never invents a second permission system. If the exact derived target is
///   not covered, the caller must stop and the operator must use the existing
///   explicit `/permission allow <directory>` path.
///
/// Containment is decided by [`canonical_future_path`], which resolves every
/// symlink and, on Windows, every reparse point or junction that already exists
/// on the path before the containment check, exactly as `validate_path_authority`
/// does for a single missing component. Components that do not exist yet cannot
/// be reparse points, so nothing escapes the check.
pub fn resolve_path_covered_by_session_authority(
    session: &Session,
    requested: &Path,
) -> Result<PathBuf> {
    let roots = session
        .permitted_directories
        .iter()
        .map(|root| {
            std::fs::canonicalize(root)
                .with_context(|| format!("cannot resolve permitted root {}", root.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(!roots.is_empty(), "session has no permitted directories");

    let candidate = if requested.is_absolute() {
        requested.to_owned()
    } else {
        session.cwd.join(requested)
    };
    let resolved = canonical_future_path(&candidate)?;
    anyhow::ensure!(
        roots.iter().any(|root| resolved.starts_with(root)),
        "path is outside the session's permitted directories: {}",
        requested.display()
    );
    Ok(resolved)
}

/// Host-owned environment override for the Managed Worktrees root.
///
/// This is host/operator configuration, never MCP input, model prose, or a
/// value derived from an existing arbitrary worktree.
pub const MANAGED_WORKTREE_ROOT_ENV: &str = "LOCAL_MCP_MANAGED_WORKTREE_ROOT";

/// The host-owned root under which managed linked worktrees are created
/// (`docs/MANAGED_WORKTREES_V1_DESIGN.md` section 7).
///
/// The exact root is host configuration/state. It is not Goal authority, and it
/// is never supplied through `goal_start`. The default keeps managed worktrees
/// beside the other host-owned `local-mcp` state, outside any user repository.
pub fn managed_worktree_root() -> Result<PathBuf> {
    if let Some(raw) = std::env::var_os(MANAGED_WORKTREE_ROOT_ENV) {
        let path = PathBuf::from(raw);
        anyhow::ensure!(
            path.is_absolute(),
            "{MANAGED_WORKTREE_ROOT_ENV} must be an absolute path: {}",
            path.display()
        );
        return Ok(path);
    }
    Ok(state_dir()?.join("managed-worktrees"))
}

pub fn state_dir() -> Result<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|path| path.join("local-mcp"))
        .context("could not determine a local state directory")
}

fn ensure_session_directories() -> Result<PathBuf> {
    let state_root = state_dir()?;
    let sessions_dir = state_root.join("sessions");
    secure_fs::ensure_private_directory(&state_root, None)
        .with_context(|| format!("cannot secure state directory {}", state_root.display()))?;
    secure_fs::ensure_private_directory(&sessions_dir, Some(&state_root)).with_context(|| {
        format!(
            "cannot secure sessions directory {}",
            sessions_dir.display()
        )
    })?;
    Ok(sessions_dir)
}

pub fn session_path(id: &str) -> Result<PathBuf> {
    validate_session_id(id)?;
    Ok(state_dir()?.join("sessions").join(format!("{id}.json")))
}

pub fn socket_path(id: &str) -> Result<PathBuf> {
    validate_session_id(id)?;
    Ok(socket_path_for_id(id))
}

/// Returns a short, per-user directory for Unix-domain session sockets.
///
/// Socket paths have a platform-specific length limit (104 bytes on macOS),
/// so they cannot live below the regular state directory, which may include
/// a long home-directory path. Session metadata remains in `state_dir()`.
#[cfg(unix)]
fn socket_dir() -> PathBuf {
    // `TMPDIR` on macOS can itself be long, so use the conventional short
    // system temporary directory rather than `std::env::temp_dir()`.
    let uid = unsafe { libc::geteuid() };
    PathBuf::from("/tmp").join(format!("local-mcp-{uid}"))
}

#[cfg(unix)]
fn socket_path_for_id(id: &str) -> PathBuf {
    socket_dir().join(format!("{id}.sock"))
}

#[cfg(windows)]
fn socket_path_for_id(id: &str) -> PathBuf {
    PathBuf::from(format!(r"\\.\pipe\local-mcp-{id}"))
}

pub async fn create_session(cwd: &Path, id: Option<&str>) -> Result<Session> {
    let cwd = cwd.to_owned();
    let id = id.map(str::to_owned);
    tokio::task::spawn_blocking(move || -> Result<Session> {
        let cwd = canonical_directory(&cwd)?;
        let id = id.unwrap_or_else(|| Uuid::new_v4().to_string());
        validate_session_id(&id)?;
        let session = Session {
            id,
            cwd: cwd.clone(),
            permitted_directories: vec![cwd],
        };
        let bytes = serde_json::to_vec_pretty(&session)?;
        save_session_blocking(&session.id, bytes)?;
        Ok(session)
    })
    .await
    .map_err(anyhow::Error::from)?
}

fn load_session_blocking(id: &str) -> Result<Vec<u8>> {
    let path = session_path(id)?;
    ensure_session_directories()?;
    let mut file = secure_fs::open_private_existing(&path, false)
        .with_context(|| format!("session {id} was not found; run `local-mcp start` first"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("session {id} was not found; run `local-mcp start` first"))?;
    Ok(bytes)
}

fn save_session_blocking(id: &str, bytes: Vec<u8>) -> Result<()> {
    let path = session_path(id)?;
    let directory = ensure_session_directories()?;
    match secure_fs::open_private_existing(&path, false) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot secure existing session file"),
    }
    let (temporary, mut file) = secure_fs::create_unique_temp(&path)?;
    let write_result = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = secure_fs::remove_private_temp(&temporary);
        return Err(error).context("failed to write session state");
    }
    if let Err(error) = secure_fs::atomic_replace(&temporary, &path) {
        let _ = secure_fs::remove_private_temp(&temporary);
        return Err(error).context("failed to replace session state");
    }
    if let Err(error) = secure_fs::sync_parent_directory(&directory) {
        let _ = secure_fs::remove_private_temp(&temporary);
        return Err(error).context("failed to sync session state directory");
    }
    Ok(())
}

pub async fn load_session(id: &str) -> Result<Session> {
    let id = id.to_owned();
    let bytes = tokio::task::spawn_blocking(move || load_session_blocking(&id)).await??;
    serde_json::from_slice(&bytes).context("invalid local-mcp session")
}

pub async fn save_session(session: &Session) -> Result<()> {
    let id = session.id.clone();
    let bytes = serde_json::to_vec_pretty(session)?;
    tokio::task::spawn_blocking(move || save_session_blocking(&id, bytes)).await??;
    Ok(())
}

pub fn validate_session_id(id: &str) -> Result<()> {
    anyhow::ensure!(!id.is_empty(), "session ID must not be empty");
    anyhow::ensure!(id.len() <= 64, "session ID must be at most 64 bytes");
    anyhow::ensure!(
        id.chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character)),
        "session ID may contain only ASCII letters, numbers, '-', '_', and '.'"
    );
    anyhow::ensure!(id != "." && id != "..", "invalid session ID");
    Ok(())
}

pub fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let path = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve {}", path.display()))?;
    anyhow::ensure!(path.is_dir(), "{} is not a directory", path.display());
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_custom_session_ids() {
        assert!(validate_session_id("my-project_1.dev").is_ok());
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id("../escape").is_err());
        assert!(validate_session_id("contains spaces").is_err());
        assert!(validate_session_id(&"x".repeat(65)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn puts_sockets_in_a_short_per_user_directory() {
        let path = socket_path("7418eda5-fd07-4e00-ace5-c1ece2f68a02").unwrap();
        assert_eq!(path.parent(), Some(socket_dir().as_path()));
        assert!(path.as_os_str().len() < 104);
    }

    #[test]
    fn path_authority_rejects_outside_and_symlink_escapes() {
        let base = std::env::temp_dir().join(format!("local-mcp-authority-{}", Uuid::new_v4()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        let session = Session {
            id: "authority-test".into(),
            cwd: root.clone(),
            permitted_directories: vec![root.clone()],
        };

        assert!(
            validate_path_authority(
                &session,
                &outside.join("secret.txt"),
                PathIntent::ReadExisting
            )
            .is_err()
        );
        assert!(
            validate_path_authority(
                &session,
                &root.join("../outside"),
                PathIntent::ListDirectory
            )
            .is_err()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
            assert!(
                validate_path_authority(
                    &session,
                    &root.join("link/secret.txt"),
                    PathIntent::ReadExisting
                )
                .is_err()
            );
            assert!(
                validate_path_authority(
                    &session,
                    &root.join("link/new.txt"),
                    PathIntent::CreateFile
                )
                .is_err()
            );
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn path_authority_accepts_colon_in_unix_filenames() {
        let base = std::env::temp_dir().join(format!("local-mcp-colon-{}", Uuid::new_v4()));
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let session = Session {
            id: "colon-test".into(),
            cwd: root.clone(),
            permitted_directories: vec![root.clone()],
        };

        let existing = root.join("report:final.txt");
        std::fs::write(&existing, "allowed").unwrap();
        assert_eq!(
            validate_path_authority(&session, &existing, PathIntent::ReadExisting).unwrap(),
            std::fs::canonicalize(&existing).unwrap()
        );

        let new_file = root.join("report:draft.txt");
        assert_eq!(
            validate_path_authority(&session, &new_file, PathIntent::CreateFile).unwrap(),
            std::fs::canonicalize(&root)
                .unwrap()
                .join("report:draft.txt")
        );

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn path_authority_rejects_dangling_final_symlink_targets() {
        let base = std::env::temp_dir().join(format!("local-mcp-dangling-{}", Uuid::new_v4()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let session = Session {
            id: "dangling-test".into(),
            cwd: root.clone(),
            permitted_directories: vec![root.clone()],
        };

        let link = root.join("dangling.txt");
        let outside_target = outside.join("escaped.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&outside_target, &link).unwrap();

        for intent in [
            PathIntent::CreateFile,
            PathIntent::WriteExisting,
            PathIntent::ReadExisting,
        ] {
            assert!(
                validate_path_authority(&session, &link, intent).is_err(),
                "dangling final symlink must be rejected for {intent:?}"
            );
        }
        assert!(!outside_target.exists());

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn path_authority_rejects_final_symlink_escape_for_create() {
        let base = std::env::temp_dir().join(format!("local-mcp-final-link-{}", Uuid::new_v4()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        let session = Session {
            id: "final-link-test".into(),
            cwd: root.clone(),
            permitted_directories: vec![root.clone()],
        };

        let link = root.join("leaf.txt");
        let outside_file = outside.join("secret.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_file, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&outside_file, &link).unwrap();

        for intent in [
            PathIntent::CreateFile,
            PathIntent::WriteExisting,
            PathIntent::ReadExisting,
        ] {
            assert!(
                validate_path_authority(&session, &link, intent).is_err(),
                "final-component symlink escape must be rejected for {intent:?}"
            );
        }

        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(windows)]
    fn create_junction(link: &std::path::Path, target: &std::path::Path) {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status()
            .expect("failed to spawn mklink /J");
        assert!(status.success(), "mklink /J failed for {}", link.display());
    }

    #[cfg(windows)]
    #[test]
    fn path_authority_rejects_junction_and_reparse_escapes() {
        let base = std::env::temp_dir().join(format!("local-mcp-junction-{}", Uuid::new_v4()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        let session = Session {
            id: "junction-test".into(),
            cwd: root.clone(),
            permitted_directories: vec![root.clone()],
        };

        // Intermediate directory junction escaping the permitted root.
        let junc = root.join("junc");
        create_junction(&junc, &outside);
        for intent in [
            PathIntent::ReadExisting,
            PathIntent::CreateFile,
            PathIntent::ListDirectory,
        ] {
            let requested = match intent {
                PathIntent::CreateFile => junc.join("created.txt"),
                PathIntent::ReadExisting => junc.join("secret.txt"),
                PathIntent::ListDirectory => junc.clone(),
                PathIntent::WriteExisting => junc.join("secret.txt"),
                PathIntent::ExecutionCwd => junc.clone(),
            };
            assert!(
                validate_path_authority(&session, &requested, intent).is_err(),
                "junction escape must be rejected for {intent:?} on {}",
                requested.display()
            );
        }
        assert!(
            validate_path_authority(&session, &junc, PathIntent::ExecutionCwd).is_err(),
            "execution cwd must not resolve through an escaping junction"
        );

        // Intermediate directory symlink escaping the permitted root.
        let dir_link = root.join("dir-link");
        std::os::windows::fs::symlink_dir(&outside, &dir_link).unwrap();
        assert!(
            validate_path_authority(
                &session,
                &dir_link.join("secret.txt"),
                PathIntent::ReadExisting
            )
            .is_err()
        );
        assert!(
            validate_path_authority(
                &session,
                &dir_link.join("created.txt"),
                PathIntent::CreateFile
            )
            .is_err()
        );

        // Final-component directory junction pointing outside.
        let final_junc = root.join("final-junc");
        create_junction(&final_junc, &outside);
        for intent in [
            PathIntent::CreateFile,
            PathIntent::ListDirectory,
            PathIntent::ExecutionCwd,
            PathIntent::ReadExisting,
        ] {
            assert!(
                validate_path_authority(&session, &final_junc, intent).is_err(),
                "final junction escape must be rejected for {intent:?}"
            );
        }

        // Dangling final directory junction must not be treated as creatable.
        let dangling = root.join("dangling-junc");
        create_junction(&dangling, &outside.join("missing-dir"));
        assert!(
            validate_path_authority(&session, &dangling, PathIntent::CreateFile).is_err(),
            "dangling final junction must be rejected for CreateFile"
        );

        // In-bounds junction must remain usable.
        let inside = root.join("inside");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(inside.join("ok.txt"), "ok").unwrap();
        let ok_junc = root.join("ok-junc");
        create_junction(&ok_junc, &inside);
        let resolved =
            validate_path_authority(&session, &ok_junc.join("ok.txt"), PathIntent::ReadExisting)
                .unwrap();
        assert!(resolved.starts_with(std::fs::canonicalize(&root).unwrap().as_path()));

        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn path_authority_rejects_windows_path_semantics_escapes() {
        let base = std::env::temp_dir().join(format!("local-mcp-winpath-{}", Uuid::new_v4()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        let session = Session {
            id: "winpath-test".into(),
            cwd: root.clone(),
            permitted_directories: vec![root.clone()],
        };

        // Drive-relative leaf names must be rejected (process-cwd ambiguity).
        let drive = root
            .ancestors()
            .find_map(|path| {
                let text = path.to_string_lossy();
                let mut chars = text.chars();
                match (chars.next(), chars.next(), chars.next()) {
                    (Some(letter), Some(':'), Some('\\')) if letter.is_ascii_alphabetic() => {
                        Some(format!("{letter}:"))
                    }
                    _ => None,
                }
            })
            .expect("temp root should be drive-qualified");
        let drive_relative = PathBuf::from(format!(r"{drive}..\..\outside\escaped.txt"));
        assert!(
            validate_path_authority(&session, &drive_relative, PathIntent::CreateFile).is_err(),
            "drive-relative paths must be rejected"
        );
        let drive_leaf = PathBuf::from(format!(r"{drive}escaped-leaf.txt"));
        assert!(
            validate_path_authority(&session, &drive_leaf, PathIntent::CreateFile).is_err(),
            "drive-relative leaf names must be rejected"
        );

        // Verbatim absolute paths outside the root must be rejected.
        let outside_canonical = std::fs::canonicalize(outside.join("secret.txt")).unwrap();
        assert!(
            validate_path_authority(&session, &outside_canonical, PathIntent::ReadExisting)
                .is_err()
        );

        // Alternate data stream on a final symlink must not write through the link.
        let outside_stream_target = outside.join("ads-target.txt");
        std::fs::write(&outside_stream_target, "base").unwrap();
        let ads_link = root.join("ads.txt");
        std::os::windows::fs::symlink_file(&outside_stream_target, &ads_link).unwrap();
        let ads_path = root.join("ads.txt:stream");
        for intent in [
            PathIntent::CreateFile,
            PathIntent::WriteExisting,
            PathIntent::ReadExisting,
        ] {
            assert!(
                validate_path_authority(&session, &ads_path, intent).is_err(),
                "ADS path must be rejected for {intent:?}"
            );
        }

        // Trailing-dot alias of an outside file must not bypass authority.
        let alias = root.join("secret.txt.");
        std::os::windows::fs::symlink_file(outside.join("secret.txt"), &alias).unwrap();
        assert!(validate_path_authority(&session, &alias, PathIntent::ReadExisting).is_err());

        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn uses_a_named_pipe_for_session_ipc() {
        let path = socket_path("7418eda5-fd07-4e00-ace5-c1ece2f68a02").unwrap();
        assert_eq!(
            path,
            PathBuf::from(r"\\.\pipe\local-mcp-7418eda5-fd07-4e00-ace5-c1ece2f68a02")
        );
    }
}
