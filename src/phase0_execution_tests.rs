include!("mcp.rs");

#[cfg(unix)]
async fn phase0_execution_wait_for_pid(path: &Path) -> libc::pid_t {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(path) {
                break pid.trim().parse::<libc::pid_t>().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[cfg(unix)]
async fn phase0_execution_wait_for_process_exit(pid: libc::pid_t) -> bool {
    for _ in 0..50 {
        if unsafe { libc::kill(pid, 0) } == -1 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[cfg(unix)]
fn phase0_public_git(cwd: &Path, args: &[&str]) -> std::process::Output {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[cfg(unix)]
#[tokio::test]
async fn public_execute_and_start_command_reject_linked_worktree_filter_before_git() {
    let base = std::env::temp_dir().join(format!("local-mcp-public-worktree-filter-{}", Uuid::new_v4()));
    let repository = base.join("repository");
    let worktree = base.join("worktree");
    std::fs::create_dir_all(&repository).unwrap();
    phase0_public_git(&repository, &["init", "--quiet"]);
    phase0_public_git(&repository, &["config", "user.name", "Finding A"]);
    phase0_public_git(
        &repository,
        &["config", "user.email", "finding-a@example.invalid"],
    );
    std::fs::write(repository.join("tracked.txt"), "tracked").unwrap();
    phase0_public_git(&repository, &["add", "tracked.txt"]);
    phase0_public_git(&repository, &["commit", "--quiet", "-m", "init"]);
    phase0_public_git(
        &repository,
        &["config", "extensions.worktreeConfig", "true"],
    );
    let worktree_text = worktree.to_string_lossy().into_owned();
    phase0_public_git(
        &repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            &worktree_text,
            "HEAD",
        ],
    );
    let marker = worktree.join("filter.marker");
    let filter = format!("touch '{}'", marker.display());
    phase0_public_git(
        &worktree,
        &["config", "--worktree", "filter.marker.clean", &filter],
    );
    std::fs::write(worktree.join(".gitattributes"), "*.txt filter=marker\n").unwrap();
    std::fs::write(worktree.join("safe.txt"), "safe").unwrap();
    let worktree_git_dir = phase0_public_git(
        &worktree,
        &["rev-parse", "--absolute-git-dir"],
    );
    let worktree_git_dir =
        std::fs::canonicalize(String::from_utf8_lossy(&worktree_git_dir.stdout).trim()).unwrap();
    let common_git_dir = std::fs::canonicalize(repository.join(".git")).unwrap();
    let session = config::Session {
        id: format!("public-worktree-filter-{}", Uuid::new_v4()),
        cwd: worktree.clone(),
        permitted_directories: vec![worktree.clone(), worktree_git_dir, common_git_dir],
    };
    let args = json!({
        "command": ["git", "add", "--", "safe.txt"],
        "operation": {
            "type": "git_stage_paths",
            "paths": ["safe.txt"]
        }
    });
    let before = phase0_public_git(
        &worktree,
        &["diff", "--cached", "--name-only"],
    )
    .stdout;
    let execute_error = execute(&args, &session).await.unwrap_err();
    assert!(execute_error.to_string().contains("filter-driver"));
    let start_error = start_command(&args, &session).await.unwrap_err();
    assert!(start_error.to_string().contains("filter-driver"));
    let after = phase0_public_git(
        &worktree,
        &["diff", "--cached", "--name-only"],
    )
    .stdout;
    assert_eq!(before, after);
    assert!(!marker.exists());
    let _ = std::fs::remove_dir_all(base);
}

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

#[cfg(not(windows))]
fn phase0_exec_text(value: &Value) -> &str {
    value["content"][0]["text"].as_str().unwrap()
}

#[cfg(not(windows))]
#[tokio::test]
async fn execute_uses_current_sandboxed_result_contract() {
    let (session, cwd) = phase0_exec_session();
    let result = execute(&json!({"command": ["/usr/bin/printf", "phase0"]}), &session)
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

#[cfg(unix)]
#[tokio::test]
async fn stop_job_cancellation_kills_sandboxed_process_group() {
    let (session, cwd) = phase0_exec_session();
    let pid_path = cwd.join("descendant.pid");
    let script = cwd.join("cancel.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n/bin/sleep 5 &\nprintf '%s' \"$!\" > '{}'\nexec /bin/sleep 5\n",
            pid_path.display()
        ),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&script).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&script, permissions).unwrap();
    let started = start_command(
        &json!({"command": [script.to_string_lossy().into_owned()]}),
        &session,
    )
    .await
    .unwrap();
    let started: Value = serde_json::from_str(phase0_exec_text(&started)).unwrap();
    assert_eq!(started["status"], "running");
    let id = started["job_id"].as_str().unwrap();
    let pid = phase0_execution_wait_for_pid(&pid_path).await;
    let stopped = stop_job(&json!({"job_id": id}), &session).await.unwrap();
    let stopped: Value = serde_json::from_str(phase0_exec_text(&stopped)).unwrap();
    assert_eq!(stopped["status"], "stopped");
    let process_stopped = phase0_execution_wait_for_process_exit(pid).await;
    if !process_stopped {
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(process_stopped);
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn without_sandbox_requires_live_approval_authority_before_execution() {
    let (session, cwd) = phase0_exec_session();
    let sentinel = cwd.join("approval-must-deny");
    let error = without_sandbox(&json!({"command": ["/usr/bin/touch", sentinel]}), &session)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("is not running"));
    assert!(!cwd.join("approval-must-deny").exists());
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
            .contains("not available through the public MCP surface")
    );
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn public_fallback_cannot_use_caller_supplied_authority_facts() {
    let (session, cwd) = phase0_exec_session();
    let sentinel = cwd.join("caller-authority-must-not-run");
    let error = codex_fallback(
        &json!({
            "task": "phase0 caller authority",
            "blocker": "synthetic sandbox permission",
            "mode": "EXECUTE_AUTHORIZED_OPERATION",
            "failure_class": "SANDBOX_PERMISSION",
            "original_host_reached": true,
            "original_command_started": true,
            "original_command_finished": true,
            "command": ["/usr/bin/touch", sentinel],
            "operation": {
                "type": "read_only_command",
                "authorized": true,
                "argv": ["/usr/bin/touch", "caller-authority-must-not-run"],
                "side_effect_state": "CONFIRMED_NOT_PERFORMED"
            },
            "fallback_depth": 0,
            "remote_side_effect": "none"
        }),
        &session,
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("not available through the public MCP surface")
    );
    assert!(!sentinel.exists());
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn safety_refusal_is_terminal_when_caller_claims_host_was_not_reached() {
    let (session, cwd) = phase0_exec_session();
    let error = codex_fallback(
        &json!({
            "task": "phase0 hidden lifecycle safety",
            "blocker": "refused by safety policy",
            "mode": "DIAGNOSE_ONLY",
            "original_host_reached": false
        }),
        &session,
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("terminal platform/safety classification")
    );
    assert!(!error.to_string().contains("is not running"));
    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[tokio::test]
async fn safety_signal_blocks_fallback_before_approval() {
    let (session, cwd) = phase0_exec_session();
    let error = codex_fallback(
        &json!({
            "task": "phase0 safety terminal",
            "blocker": "refused by safety policy",
            "mode": "DIAGNOSE_ONLY"
        }),
        &session,
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("terminal platform/safety classification")
    );
    let _ = tokio::fs::remove_dir_all(cwd).await;
}
