include!("mcp.rs");

fn phase0_session() -> (config::Session, PathBuf) {
    let cwd = std::env::temp_dir().join(format!("local-mcp-phase0-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    (
        config::Session {
            id: format!("phase0-{}", Uuid::new_v4()),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd.clone()],
        },
        cwd,
    )
}

fn text(value: &Value) -> &str {
    value["content"][0]["text"].as_str().unwrap()
}

fn job_id(value: &Value) -> Uuid {
    let payload: Value = serde_json::from_str(text(value)).unwrap();
    Uuid::parse_str(payload["job_id"].as_str().unwrap()).unwrap()
}

const LEGACY_TOOL_NAMES: [&str; 11] = [
    "session_info",
    "read_file",
    "get_image",
    "list_directory",
    "write_file",
    "execute",
    "start_command",
    "poll_job",
    "stop_job",
    "codex_fallback",
    "without_sandbox",
];

const GOAL_TOOL_NAMES: [&str; 7] = [
    "goal_start",
    "goal_status",
    "goal_pause",
    "goal_resume",
    "goal_cancel",
    "goal_result",
    "goal_run",
];

#[test]
fn legacy_tool_catalog_is_frozen() {
    let value = tools();
    let names = value
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(&names[..LEGACY_TOOL_NAMES.len()], LEGACY_TOOL_NAMES);
}

#[test]
fn goal_tool_catalog_is_exactly_additive() {
    let value = tools();
    let tools = value.as_array().unwrap();
    let names = tools
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    let expected = LEGACY_TOOL_NAMES
        .iter()
        .chain(GOAL_TOOL_NAMES.iter())
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(names, expected);
    assert_eq!(tools.len(), 18);

    for forbidden in [
        "task_create",
        "task_update",
        "task_transition",
        "task_retry",
        "task_complete",
        "goal_mutate",
        "goal_add_task",
        "goal_replan",
        "goal_execute",
        "goal_tick",
    ] {
        assert!(!names.contains(&forbidden));
    }

    let goal_start = tools
        .iter()
        .find(|tool| tool["name"] == "goal_start")
        .unwrap();
    assert_eq!(goal_start["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        goal_start["inputSchema"]["required"],
        json!(["session_id", "objective"])
    );
    assert_eq!(
        goal_start["inputSchema"]["properties"]["objective"]["maxLength"],
        131072
    );
    assert_eq!(
        goal_start["inputSchema"]["properties"]["constraints"]["maxItems"],
        64
    );
    assert_eq!(
        goal_start["inputSchema"]["properties"]["completion_criteria"]["maxItems"],
        64
    );
    assert_eq!(
        goal_start["inputSchema"]["properties"]["idempotency_key"]["maxLength"],
        128
    );

    for name in ["goal_status", "goal_pause", "goal_resume", "goal_cancel"] {
        let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        assert_eq!(tool["inputSchema"]["required"], json!(["session_id"]));
    }
    let resume = tools
        .iter()
        .find(|tool| tool["name"] == "goal_resume")
        .unwrap();
    let rejection = &resume["inputSchema"]["properties"]["pre_execution_plan_rejection"];
    assert_eq!(rejection["type"], "object");
    assert_eq!(rejection["additionalProperties"], false);
    assert_eq!(
        rejection["required"],
        json!([
            "request_id",
            "expected_goal_revision",
            "expected_plan_revision",
            "trigger_task_id",
            "reason"
        ])
    );
    assert_eq!(rejection["properties"]["request_id"]["maxLength"], 128);
    assert_eq!(rejection["properties"]["trigger_task_id"]["maxLength"], 36);
    assert_eq!(rejection["properties"]["reason"]["maxLength"], 8192);
    let result = tools
        .iter()
        .find(|tool| tool["name"] == "goal_result")
        .unwrap();
    assert_eq!(result["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        result["inputSchema"]["required"],
        json!(["session_id", "goal_id"])
    );

    let run = tools.iter().find(|tool| tool["name"] == "goal_run").unwrap();
    assert_eq!(run["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        run["inputSchema"]["required"],
        json!(["session_id", "goal_id", "max_steps"])
    );
}

#[tokio::test]
async fn initialize_contract_is_frozen() {
    let value = dispatch(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    }))
    .await
    .unwrap();
    assert_eq!(value["protocolVersion"], "2025-06-18");
    assert_eq!(value["capabilities"]["tools"]["listChanged"], false);
    assert_eq!(value["serverInfo"]["name"], "local-mcp");
}

#[tokio::test]
async fn job_running_completion_and_completed_poll_are_frozen() {
    let (session, cwd) = phase0_session();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let handle = tokio::task::spawn_blocking(move || {
        release_receiver
            .recv()
            .expect("test must release the background job");
        Ok("{\"exit_code\":0,\"stdout\":\"done\",\"stderr\":\"\"}".to_owned())
    });
    let started = store_job(&session, "phase0-job".into(), handle, "Started")
        .await
        .unwrap();
    let id = job_id(&started);
    let args = json!({"job_id": id.to_string()});

    let running = poll_job(&args, &session).await.unwrap();
    let running: Value = serde_json::from_str(text(&running)).unwrap();
    assert_eq!(running["status"], "running");

    release_sender.send(()).unwrap();
    let mut completed = None;
    for _ in 0..100 {
        let polled = poll_job(&args, &session).await.unwrap();
        let value: Value = serde_json::from_str(text(&polled)).unwrap();
        if value["status"] == "running" {
            tokio::task::yield_now().await;
            continue;
        }
        completed = Some(value);
        break;
    }
    let completed = completed.expect("released background job must complete within the bounded poll");
    assert_eq!(completed["exit_code"], 0);
    assert_eq!(completed["stdout"], "done");
    assert!(
        poll_job(&args, &session)
            .await
            .unwrap_err()
            .to_string()
            .contains("unknown job_id")
    );

    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn job_stop_unknown_owner_and_failure_are_frozen() {
    let (session, cwd) = phase0_session();
    let other = config::Session {
        id: format!("phase0-other-{}", Uuid::new_v4()),
        cwd: session.cwd.clone(),
        permitted_directories: session.permitted_directories.clone(),
    };

    let handle = tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        Ok("never".to_owned())
    });
    let started = store_job(&session, "phase0-stop".into(), handle, "Started")
        .await
        .unwrap();
    let id = job_id(&started);
    let args = json!({"job_id": id.to_string()});

    assert!(
        poll_job(&args, &other)
            .await
            .unwrap_err()
            .to_string()
            .contains("does not belong to this session")
    );
    let stopped = stop_job(&args, &session).await.unwrap();
    let stopped: Value = serde_json::from_str(text(&stopped)).unwrap();
    assert_eq!(stopped["status"], "stopped");
    assert!(
        poll_job(&args, &session)
            .await
            .unwrap_err()
            .to_string()
            .contains("unknown job_id")
    );

    let failed_handle = tokio::spawn(async { anyhow::bail!("phase0 background failure") });
    let failed = store_job(&session, "phase0-fail".into(), failed_handle, "Started")
        .await
        .unwrap();
    let failed_id = job_id(&failed);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let err = poll_job(&json!({"job_id": failed_id.to_string()}), &session)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("phase0 background failure"));

    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn goal_lifecycle_operations_leave_legacy_job_authority_untouched() {
    use crate::goal::{Goal, GoalStatus};
    use crate::task::{ReplaySafety, TaskOperationKind, TaskScope, WorkerKind};
    use crate::task_store::TaskStore;

    let (session, cwd) = phase0_session();
    let state_root = std::env::temp_dir().join(format!("local-mcp-phase3-job-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&state_root).unwrap();
    let store = TaskStore::with_state_root(state_root.clone());

    let handle = tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok("legacy job completed".to_owned())
    });
    let started = store_job(&session, "legacy-independent-job".into(), handle, "Started")
        .await
        .unwrap();
    let legacy_job_id = job_id(&started);

    let mut goal = Goal::new(
        session.id.clone(),
        session.cwd.clone(),
        "durable lifecycle only",
        None,
        vec![],
        vec![],
        "2026-01-01T00:00:00Z",
    )
    .unwrap();
    goal.add_task(
        "pending task",
        "not executed in Phase 3",
        true,
        WorkerKind::CodexReadonly,
        TaskScope::new(
            vec![PathBuf::from("src")],
            vec![],
            TaskOperationKind::ReadOnly,
            ReplaySafety::SafeReadOnly,
        ),
        vec![],
        1,
        "2026-01-01T00:00:00Z",
    )
    .unwrap();
    goal.transition_to(GoalStatus::Running, "2026-01-01T00:00:00Z")
        .unwrap();
    store.create_goal(&goal).unwrap();

    let args = json!({"session_id": session.id});
    goal_api::goal_status(&args, &session, &store).unwrap();
    assert!(jobs().lock().unwrap().contains_key(&legacy_job_id));
    goal_api::goal_pause(&args, &session, &store).unwrap();
    assert!(jobs().lock().unwrap().contains_key(&legacy_job_id));
    goal_api::goal_resume(&args, &session, &store).unwrap();
    assert!(jobs().lock().unwrap().contains_key(&legacy_job_id));
    goal_api::goal_cancel(&args, &session, &store).unwrap();
    assert!(jobs().lock().unwrap().contains_key(&legacy_job_id));

    let stopped = stop_job(&json!({"job_id": legacy_job_id.to_string()}), &session)
        .await
        .unwrap();
    let stopped: Value = serde_json::from_str(text(&stopped)).unwrap();
    assert_eq!(stopped["status"], "stopped");

    let _ = tokio::fs::remove_dir_all(cwd).await;
    let _ = tokio::fs::remove_dir_all(state_root).await;
}
