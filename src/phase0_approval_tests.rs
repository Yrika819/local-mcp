include!("approvals.rs");

fn phase0_approval_session() -> (Session, PathBuf) {
    let cwd = std::env::temp_dir().join(format!("local-mcp-phase0-approval-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    (
        Session {
            id: format!("phase0-approval-{}", Uuid::new_v4()),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd.clone()],
        },
        cwd,
    )
}

fn phase0_pending(
    operation: &str,
    cwd: &Path,
) -> (tokio::io::DuplexStream, (Request, SessionStream)) {
    let (client, server) = tokio::io::duplex(1024);
    let request = Request {
        id: Uuid::new_v4(),
        operation: operation.to_owned(),
        detail: operation.to_owned(),
        cwd: cwd.to_owned(),
    };
    (client, (request, Box::new(server)))
}

#[tokio::test]
async fn pending_approval_order_and_allow_deny_are_frozen() {
    let (mut session, cwd) = phase0_approval_session();
    let mut yolo = false;
    let mut pending = VecDeque::new();
    let (first_client, first) = phase0_pending("first", &cwd);
    let (second_client, second) = phase0_pending("second", &cwd);
    pending.push_back(first);
    pending.push_back(second);

    handle_input("yes", &mut session, &mut yolo, &mut pending)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    let mut first_line = String::new();
    BufReader::new(first_client)
        .read_line(&mut first_line)
        .await
        .unwrap();
    assert_eq!(first_line.trim(), "allow");

    handle_input("no", &mut session, &mut yolo, &mut pending)
        .await
        .unwrap();
    assert!(pending.is_empty());
    let mut second_line = String::new();
    BufReader::new(second_client)
        .read_line(&mut second_line)
        .await
        .unwrap();
    assert_eq!(second_line.trim(), "deny");

    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn yolo_is_volatile_and_drains_pending_requests() {
    let (mut session, cwd) = phase0_approval_session();
    let mut yolo = false;
    let mut pending = VecDeque::new();
    let (first_client, first) = phase0_pending("first", &cwd);
    let (second_client, second) = phase0_pending("second", &cwd);
    pending.push_back(first);
    pending.push_back(second);

    assert!(!yolo);
    handle_input("/permission yolo", &mut session, &mut yolo, &mut pending)
        .await
        .unwrap();
    assert!(yolo);
    assert!(pending.is_empty());

    for client in [first_client, second_client] {
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line).await.unwrap();
        assert_eq!(line.trim(), "allow");
    }

    handle_input("/permission ask", &mut session, &mut yolo, &mut pending)
        .await
        .unwrap();
    assert!(!yolo);
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn sandbox_root_allow_revoke_persists_and_cwd_cannot_be_revoked() {
    let (mut session, cwd) = phase0_approval_session();
    let extra = std::env::temp_dir().join(format!("local-mcp-phase0-extra-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&extra).unwrap();
    if let Ok(path) = config::session_path(&session.id) {
        let _ = tokio::fs::remove_file(path).await;
    }
    config::save_session(&session).await.unwrap();

    let mut yolo = false;
    let mut pending = VecDeque::new();
    handle_input(
        &format!("/permission allow {}", extra.display()),
        &mut session,
        &mut yolo,
        &mut pending,
    )
    .await
    .unwrap();
    let canonical_extra = std::fs::canonicalize(&extra).unwrap();
    assert!(session.permitted_directories.contains(&canonical_extra));
    assert!(
        config::load_session(&session.id)
            .await
            .unwrap()
            .permitted_directories
            .contains(&canonical_extra)
    );

    handle_input(
        &format!("/permission revoke {}", cwd.display()),
        &mut session,
        &mut yolo,
        &mut pending,
    )
    .await
    .unwrap();
    assert!(session.permitted_directories.contains(&session.cwd));

    handle_input(
        &format!("/permission revoke {}", extra.display()),
        &mut session,
        &mut yolo,
        &mut pending,
    )
    .await
    .unwrap();
    assert!(!session.permitted_directories.contains(&canonical_extra));

    if let Ok(path) = config::session_path(&session.id) {
        let _ = tokio::fs::remove_file(path).await;
    }
    let _ = tokio::fs::remove_dir_all(cwd).await;
    let _ = tokio::fs::remove_dir_all(extra).await;
}
