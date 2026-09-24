#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::config;
use crate::secure_fs;

fn unique_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

fn temp_directory(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(unique_id(prefix));
    std::fs::create_dir_all(&path).unwrap();
    path
}

async fn cleanup_session(id: &str) {
    if let Ok(path) = config::session_path(id) {
        let _ = tokio::fs::remove_file(&path).await;
        remove_session_temps(&path);
    }
}

fn remove_session_temps(path: &Path) {
    let Ok(prefix) = secure_fs::temp_prefix(path) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(path.parent().unwrap()) else {
        return;
    };
    for entry in entries.flatten() {
        let entry_path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with(&prefix) && name.to_string_lossy().ends_with(".tmp") {
            let _ = secure_fs::remove_private_temp(&entry_path);
        }
    }
}

#[cfg(unix)]
struct SessionCleanup {
    id: String,
    cwd: PathBuf,
}

#[cfg(unix)]
impl Drop for SessionCleanup {
    fn drop(&mut self) {
        if let Ok(path) = config::session_path(&self.id) {
            let _ = std::fs::remove_file(&path);
            remove_session_temps(&path);
        }
        let _ = std::fs::remove_dir_all(&self.cwd);
    }
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
fn assert_modes(phase: &str, checks: &[(&str, &Path, u32, u32)]) {
    let failures = checks
        .iter()
        .filter(|(_, _, expected, actual)| expected != actual)
        .map(|(label, path, expected, actual)| {
            format!(
                "{phase} {label} {}: actual {:04o}, expected {:04o}",
                path.display(),
                actual,
                expected
            )
        })
        .collect::<Vec<_>>();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[cfg(unix)]
fn assert_session_modes(phase: &str, path: &Path, state_root: &Path, sessions_dir: &Path) {
    assert_modes(
        phase,
        &[
            ("final JSON", path, 0o600, mode(path)),
            ("state root", state_root, 0o700, mode(state_root)),
            (
                "sessions directory",
                sessions_dir,
                0o700,
                mode(sessions_dir),
            ),
        ],
    );
}

#[tokio::test]
async fn session_create_load_update_and_atomic_replace_are_frozen() {
    let cwd = temp_directory("local-mcp-phase0-cwd");
    let extra = temp_directory("local-mcp-phase0-extra");
    let id = unique_id("phase0-session");
    cleanup_session(&id).await;

    let mut session = config::create_session(&cwd, Some(&id)).await.unwrap();
    assert_eq!(session.id, id);
    assert_eq!(session.cwd, std::fs::canonicalize(&cwd).unwrap());
    assert_eq!(session.permitted_directories, vec![session.cwd.clone()]);

    session
        .permitted_directories
        .push(std::fs::canonicalize(&extra).unwrap());
    session.permitted_directories.sort();
    config::save_session(&session).await.unwrap();

    let loaded = config::load_session(&id).await.unwrap();
    assert_eq!(loaded.id, session.id);
    assert_eq!(loaded.cwd, session.cwd);
    assert_eq!(loaded.permitted_directories, session.permitted_directories);

    let path = config::session_path(&id).unwrap();
    assert_eq!(
        path.parent().unwrap(),
        config::state_dir().unwrap().join("sessions").as_path()
    );
    assert!(path.is_file());

    let prefix = secure_fs::temp_prefix(&path).unwrap();
    let mut entries = tokio::fs::read_dir(path.parent().unwrap()).await.unwrap();
    while let Some(entry) = entries.next_entry().await.unwrap() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(
            !(name.starts_with(&prefix) && name.ends_with(".tmp")),
            "stale atomic-write temp file: {name}"
        );
    }

    cleanup_session(&id).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
    let _ = tokio::fs::remove_dir_all(extra).await;
}

#[tokio::test]
async fn missing_and_corrupt_session_errors_are_frozen() {
    let missing_id = unique_id("phase0-missing");
    cleanup_session(&missing_id).await;
    let missing = config::load_session(&missing_id)
        .await
        .err()
        .expect("missing session must fail");
    assert!(missing.to_string().contains("was not found"));

    let corrupt_id = unique_id("phase0-corrupt");
    let path = config::session_path(&corrupt_id).unwrap();
    tokio::fs::create_dir_all(path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&path, b"{not-json").await.unwrap();
    let before = std::fs::read(&path).unwrap();
    let corrupt = config::load_session(&corrupt_id)
        .await
        .err()
        .expect("corrupt session must fail");
    assert!(corrupt.to_string().contains("invalid local-mcp session"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    #[cfg(unix)]
    assert_eq!(mode(&path), 0o600);
    cleanup_session(&corrupt_id).await;
}

#[cfg(unix)]
#[tokio::test]
async fn saved_session_final_and_state_modes_are_private() {
    let cwd = temp_directory("local-mcp-phase0-saved-permissions-cwd");
    let id = unique_id("phase0-saved-permissions");
    let _cleanup = SessionCleanup {
        id: id.clone(),
        cwd: cwd.clone(),
    };

    config::create_session(&cwd, Some(&id)).await.unwrap();
    let path = config::session_path(&id).unwrap();
    let state_root = config::state_dir().unwrap();
    let sessions_dir = state_root.join("sessions");
    assert_session_modes("saved session", &path, &state_root, &sessions_dir);
}

#[cfg(unix)]
#[tokio::test]
async fn session_save_replaces_permissive_final_with_private_mode() {
    let cwd = temp_directory("local-mcp-phase0-save-permissions-cwd");
    let id = unique_id("phase0-save-permissions");
    let _cleanup = SessionCleanup {
        id: id.clone(),
        cwd: cwd.clone(),
    };

    let session = config::create_session(&cwd, Some(&id)).await.unwrap();
    let path = config::session_path(&id).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    config::save_session(&session).await.unwrap();
    let state_root = config::state_dir().unwrap();
    let sessions_dir = state_root.join("sessions");
    assert_session_modes("save replacement", &path, &state_root, &sessions_dir);
}

#[cfg(unix)]
#[tokio::test]
async fn session_load_repairs_permissive_final_with_private_mode() {
    let cwd = temp_directory("local-mcp-phase0-load-permissions-cwd");
    let id = unique_id("phase0-load-permissions");
    let _cleanup = SessionCleanup {
        id: id.clone(),
        cwd: cwd.clone(),
    };

    config::create_session(&cwd, Some(&id)).await.unwrap();
    let path = config::session_path(&id).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    let loaded = config::load_session(&id).await.unwrap();
    assert_eq!(loaded.id, id);
    let state_root = config::state_dir().unwrap();
    let sessions_dir = state_root.join("sessions");
    assert_session_modes("load repair", &path, &state_root, &sessions_dir);
}
