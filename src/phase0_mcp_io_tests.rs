include!("mcp.rs");

fn phase0_io_session() -> (config::Session, PathBuf) {
    let cwd = std::env::temp_dir().join(format!("local-mcp-phase0-io-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    (
        config::Session {
            id: format!("phase0-io-{}", Uuid::new_v4()),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd.clone()],
        },
        cwd,
    )
}

fn phase0_io_text(value: &Value) -> &str {
    value["content"][0]["text"].as_str().unwrap()
}

async fn phase0_io_cleanup_session(id: &str) {
    if let Ok(path) = config::session_path(id) {
        let _ = tokio::fs::remove_file(path).await;
    }
}

#[tokio::test]
async fn session_info_read_file_and_list_directory_are_frozen() {
    let (session, cwd) = phase0_io_session();
    phase0_io_cleanup_session(&session.id).await;
    config::save_session(&session).await.unwrap();

    tokio::fs::write(cwd.join("z.txt"), "zeta").await.unwrap();
    tokio::fs::write(cwd.join("a.txt"), "alpha").await.unwrap();
    tokio::fs::create_dir(cwd.join("b_dir")).await.unwrap();

    let info = call_tool(&json!({
        "name": "session_info",
        "arguments": {"session_id": session.id}
    }))
    .await
    .unwrap();
    let info: Value = serde_json::from_str(phase0_io_text(&info)).unwrap();
    assert_eq!(info["id"], session.id);
    assert_eq!(info["cwd"], session.cwd.to_string_lossy().as_ref());
    assert_eq!(info["permitted_directories"].as_array().unwrap().len(), 1);

    let read = call_tool(&json!({
        "name": "read_file",
        "arguments": {"session_id": session.id, "path": "a.txt"}
    }))
    .await
    .unwrap();
    assert_eq!(phase0_io_text(&read), "alpha");

    let listed = call_tool(&json!({
        "name": "list_directory",
        "arguments": {"session_id": session.id, "path": "."}
    }))
    .await
    .unwrap();
    assert_eq!(phase0_io_text(&listed), "a.txt\nb_dir/\nz.txt");

    phase0_io_cleanup_session(&session.id).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[cfg(unix)]
#[tokio::test]
async fn write_file_replaces_existing_utf8_content() {
    let (session, cwd) = phase0_io_session();
    let path = cwd.join("edit.txt");
    tokio::fs::write(&path, "old\n").await.unwrap();

    let result = write_file(
        &json!({"path": "edit.txt", "content": "new\nβeta\n"}),
        &session,
    )
    .await
    .unwrap();
    let output: Value = serde_json::from_str(phase0_io_text(&result)).unwrap();
    assert_eq!(output["exit_code"], 0);
    assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "new\nβeta\n");

    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn tool_argument_and_missing_session_errors_are_frozen() {
    let (session, cwd) = phase0_io_session();
    phase0_io_cleanup_session(&session.id).await;
    config::save_session(&session).await.unwrap();

    let missing_path = call_tool(&json!({
        "name": "read_file",
        "arguments": {"session_id": session.id}
    }))
    .await
    .unwrap_err();
    assert!(missing_path.to_string().contains("missing path"));

    let missing_id = format!("phase0-missing-{}", Uuid::new_v4());
    phase0_io_cleanup_session(&missing_id).await;
    let missing_session = call_tool(&json!({
        "name": "session_info",
        "arguments": {"session_id": missing_id}
    }))
    .await
    .unwrap_err();
    assert!(missing_session.to_string().contains("was not found"));

    phase0_io_cleanup_session(&session.id).await;
    let _ = tokio::fs::remove_dir_all(cwd).await;
}
