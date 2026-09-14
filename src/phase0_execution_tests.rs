include!("mcp.rs");

fn phase0_exec_session() -> (config::Session, PathBuf) {
    let cwd = std::env::temp_dir().join(format!("local-mcp-phase0-exec-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    (
        config::Session {
            id: format!("phase0-exec-{}", Uuid::new_v4()),
            cwd: cwd.clone(),
            permitted_directories: vec![cwd.clone()],
        },
        cwd,
    )
}

fn phase0_exec_text(value: &Value) -> &str {
    value["content"][0]["text"].as_str().unwrap()
}

#[cfg(not(windows))]
#[tokio::test]
async fn execute_uses_current_sandboxed_result_contract() {
    let (session, cwd) = phase0_exec_session();
    let result = execute(
        &json!({"command": ["/usr/bin/printf", "phase0"]}),
        &session,
    )
    .await
    .unwrap();
    let payload: Value = serde_json::from_str(phase0_exec_text(&result)).unwrap();
    assert_eq!(payload["exit_code"], 0);
    assert_eq!(payload["stdout"], "phase0");
    assert_eq!(payload["failure_class"], "SUCCESS");
    assert_eq!(payload["primary_execution_mode"], "SANDBOXED");
    assert_eq!(payload["fallback_decision"]["action"], "NONE");
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[cfg(not(windows))]
#[tokio::test]
async fn start_command_returns_job_and_poll_reaches_terminal_result() {
    let (session, cwd) = phase0_exec_session();
    let started = start_command(
        &json!({"command": ["/usr/bin/printf", "async-phase0"]}),
        &session,
    )
    .await
    .unwrap();
    let started: Value = serde_json::from_str(phase0_exec_text(&started)).unwrap();
    assert_eq!(started["status"], "running");
    let job_id = started["job_id"].as_str().unwrap().to_owned();
    let args = json!({"job_id": job_id});

    let terminal = loop {
        match poll_job(&args, &session).await {
            Ok(value) => {
                let payload: Value = serde_json::from_str(phase0_exec_text(&value)).unwrap();
                if payload["status"] == "running" {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
                break payload;
            }
            Err(error) => panic!("unexpected job failure: {error:#}"),
        }
    };
    assert_eq!(terminal["exit_code"], 0);
    assert_eq!(terminal["stdout"], "async-phase0");
    assert_eq!(terminal["failure_class"], "SUCCESS");
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn without_sandbox_requires_live_approval_authority_before_execution() {
    let (session, cwd) = phase0_exec_session();
    let error = without_sandbox(
        &json!({"command": ["/usr/bin/true"]}),
        &session,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("is not running"));
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn codex_fallback_requires_live_approval_without_live_inference() {
    let (session, cwd) = phase0_exec_session();
    let error = codex_fallback(
        &json!({
            "task": "phase0 deterministic plumbing",
            "blocker": "synthetic local blocker",
            "mode": "DIAGNOSE_ONLY",
            "failure_class": "HOST_ENVIRONMENT"
        }),
        &session,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("is not running"));
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn executable_codex_fallback_rejects_non_sandbox_failure_before_inference() {
    let (session, cwd) = phase0_exec_session();
    let error = codex_fallback(
        &json!({
            "task": "phase0 executable policy",
            "blocker": "synthetic semantic failure",
            "mode": "EXECUTE_AUTHORIZED_OPERATION",
            "failure_class": "SEMANTIC_FAILURE",
            "original_host_reached": true,
            "original_command_started": true,
            "command": ["/usr/bin/true"],
            "operation": {
                "type": "read_only_command",
                "authorized": true,
                "argv": ["/usr/bin/true"],
                "side_effect_state": "CONFIRMED_NOT_PERFORMED"
            }
        }),
        &session,
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("executable fallback requires SANDBOX_PERMISSION")
    );
    let _ = tokio::fs::remove_dir_all(cwd).await;
}
