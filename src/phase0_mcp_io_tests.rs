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
    assert_eq!(
        tokio::fs::read_to_string(&path).await.unwrap(),
        "new\nβeta\n"
    );

    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn filesystem_tools_reject_outside_paths_without_side_effects() {
    let (session, root) = phase0_io_session();
    let outside = root
        .parent()
        .unwrap()
        .join(format!("local-mcp-phase0-outside-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&outside).await.unwrap();
    tokio::fs::write(outside.join("secret.txt"), "secret")
        .await
        .unwrap();
    tokio::fs::write(
        outside.join("image.png"),
        b"\x89PNG\r\n\x1a\nphase0-test-image",
    )
    .await
    .unwrap();
    let outside_new_parent = outside.join("new-parent");
    tokio::fs::create_dir_all(&outside_new_parent)
        .await
        .unwrap();

    phase0_io_cleanup_session(&session.id).await;
    config::save_session(&session).await.unwrap();

    let read = call_tool(&json!({
        "name": "read_file",
        "arguments": {"session_id": session.id, "path": outside.join("secret.txt")}
    }))
    .await
    .unwrap_err();
    assert!(
        read.to_string()
            .contains("outside the session's permitted directories")
    );

    let image = call_tool(&json!({
        "name": "get_image",
        "arguments": {"session_id": session.id, "path": outside.join("image.png")}
    }))
    .await
    .unwrap_err();
    assert!(
        image
            .to_string()
            .contains("outside the session's permitted directories")
    );

    let listed = call_tool(&json!({
        "name": "list_directory",
        "arguments": {"session_id": session.id, "path": outside}
    }))
    .await
    .unwrap_err();
    assert!(
        listed
            .to_string()
            .contains("outside the session's permitted directories")
    );

    let write = write_file(
        &json!({
            "path": outside.join("created.txt"),
            "content": "must not be written"
        }),
        &session,
    )
    .await
    .unwrap_err();
    assert!(
        write
            .to_string()
            .contains("outside the session's permitted directories")
    );
    assert!(!outside.join("created.txt").exists());

    let new_parent_write = write_file(
        &json!({
            "path": outside_new_parent.join("created.txt"),
            "content": "must not be written"
        }),
        &session,
    )
    .await
    .unwrap_err();
    assert!(
        new_parent_write
            .to_string()
            .contains("outside the session's permitted directories")
    );
    assert!(!outside_new_parent.join("created.txt").exists());

    let cwd_error = cwd(&json!({"cwd": outside}), &session).unwrap_err();
    assert!(
        cwd_error
            .to_string()
            .contains("outside the session's permitted directories")
    );

    #[cfg(unix)]
    {
        let link = root.join("outside-link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let symlink_read = call_tool(&json!({
            "name": "read_file",
            "arguments": {"session_id": session.id, "path": link.join("secret.txt")}
        }))
        .await
        .unwrap_err();
        assert!(
            symlink_read
                .to_string()
                .contains("outside the session's permitted directories")
        );

        let symlink_write = write_file(
            &json!({
                "path": link.join("created.txt"),
                "content": "must not be written"
            }),
            &session,
        )
        .await
        .unwrap_err();
        assert!(
            symlink_write
                .to_string()
                .contains("outside the session's permitted directories")
        );
        assert!(!outside.join("created.txt").exists());
    }

    phase0_io_cleanup_session(&session.id).await;
    let _ = tokio::fs::remove_dir_all(root).await;
    let _ = tokio::fs::remove_dir_all(outside).await;
}

#[tokio::test]
async fn write_file_rejects_dangling_final_symlink_escape() {
    let (session, root) = phase0_io_session();
    let outside = root
        .parent()
        .unwrap()
        .join(format!("local-mcp-phase0-dangling-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&outside).await.unwrap();
    let outside_target = outside.join("escaped.txt");

    let link = root.join("dangling.txt");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside_target, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&outside_target, &link).unwrap();

    let result = write_file(
        &json!({
            "path": "dangling.txt",
            "content": "must not escape"
        }),
        &session,
    )
    .await;
    assert!(
        result.is_err(),
        "write through dangling final symlink must be rejected"
    );
    assert!(
        !outside_target.exists(),
        "dangling final symlink write must not create the outside target"
    );

    let _ = tokio::fs::remove_dir_all(root).await;
    let _ = tokio::fs::remove_dir_all(outside).await;
}

#[cfg(windows)]
#[tokio::test]
async fn write_file_rejects_ads_through_final_symlink() {
    let (session, root) = phase0_io_session();
    let outside = root
        .parent()
        .unwrap()
        .join(format!("local-mcp-phase0-ads-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&outside).await.unwrap();
    let outside_target = outside.join("ads-target.txt");
    tokio::fs::write(&outside_target, "base").await.unwrap();

    let link = root.join("ads.txt");
    std::os::windows::fs::symlink_file(&outside_target, &link).unwrap();

    let result = write_file(
        &json!({
            "path": "ads.txt:stream",
            "content": "must not escape via ADS"
        }),
        &session,
    )
    .await;
    assert!(
        result.is_err(),
        "ADS write through final symlink must be rejected"
    );

    let _ = tokio::fs::remove_dir_all(root).await;
    let _ = tokio::fs::remove_dir_all(outside).await;
}

#[cfg(windows)]
#[tokio::test]
async fn write_file_requires_live_host_native_approval_before_writing() {
    let (session, root) = phase0_io_session();
    let path = root.join("must-not-write.txt");
    let result = write_file(
        &json!({
            "path": "must-not-write.txt",
            "content": "requires approval"
        }),
        &session,
    )
    .await;
    assert!(
        result.is_err(),
        "host-native write_file must fail closed without live approval"
    );
    assert!(
        !path.exists(),
        "write_file must not create files before host-native approval"
    );
    let _ = tokio::fs::remove_dir_all(root).await;
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
