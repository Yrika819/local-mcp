use std::path::PathBuf;

use uuid::Uuid;

use crate::config;

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
        let _ = tokio::fs::remove_file(path).await;
    }
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

    let prefix = format!("{id}.json.");
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
    let corrupt = config::load_session(&corrupt_id)
        .await
        .err()
        .expect("corrupt session must fail");
    assert!(corrupt.to_string().contains("invalid local-mcp session"));
    cleanup_session(&corrupt_id).await;
}
