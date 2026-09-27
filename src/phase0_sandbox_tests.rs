#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::sandbox;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
fn executable_script(root: &Path, name: &str, body: &str) -> PathBuf {
    let path = root.join(name);
    std::fs::write(&path, body).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

#[cfg(unix)]
fn script_command(path: &Path) -> Vec<String> {
    vec![path.to_string_lossy().into_owned()]
}

#[cfg(unix)]
async fn wait_for_pid(path: &Path) -> libc::pid_t {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            // The fixture writes its pid with a non-atomic redirect, so the file
            // can exist while still empty. Only a parseable pid is readiness.
            if let Ok(pid) = std::fs::read_to_string(path)
                && let Ok(pid) = pid.trim().parse::<libc::pid_t>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[cfg(unix)]
async fn wait_for_process_exit(pid: libc::pid_t) -> bool {
    for _ in 0..50 {
        if unsafe { libc::kill(pid, 0) } == -1 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

#[tokio::test]
async fn sandbox_prestart_and_missing_executable_lifecycle_are_frozen() {
    let cwd = std::env::temp_dir().join(format!("local-mcp-phase0-sandbox-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let missing = cwd.join("definitely-not-present");
    let command = vec![missing.to_string_lossy().into_owned()];

    // Unix launches a sandbox wrapper first, so a missing target completes
    // as a nonzero sandbox attempt. Windows is host-native and therefore
    // reports the missing executable as a genuine pre-start failure.
    #[cfg(not(windows))]
    {
        let output = sandbox::run_tracked(&command, &cwd, &[], None)
            .await
            .expect("sandbox wrapper should complete the missing-target attempt");
        assert_ne!(output.status, 0);
    }
    #[cfg(windows)]
    {
        let error = sandbox::run_tracked(&command, &cwd, &[], None)
            .await
            .expect_err("host-native missing target must fail before process start");
        assert!(!error.command_started);
        assert!(!error.command_finished);
    }

    // A failure while constructing the sandbox process remains genuinely
    // pre-start and must retain false/false lifecycle evidence.
    let missing_cwd = cwd.join("missing-cwd");
    #[cfg(unix)]
    let no_op = "/bin/true";
    #[cfg(windows)]
    let no_op = "cmd.exe";
    let error = sandbox::run_tracked(&[no_op.into()], &missing_cwd, &[], None)
        .await
        .expect_err("unresolvable cwd must fail before the command starts");
    assert!(!error.command_started);
    assert!(!error.command_finished);

    let _ = tokio::fs::remove_dir_all(cwd).await;
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_clean_runner_kills_hanging_child_promptly() {
    let root = std::env::temp_dir().join(format!("local-mcp-bounded-timeout-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let pid_path = root.join("child.pid");
    let script = executable_script(
        &root,
        "hang.sh",
        &format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nexec /bin/sleep 30\n",
            pid_path.display()
        ),
    );
    let started = Instant::now();
    let result = sandbox::run_unrestricted_clean_with_limits(
        &script_command(&script),
        &root,
        None,
        Duration::from_secs(2),
        1024,
        1024,
    )
    .await;
    let error = result.expect_err("hanging child must time out");
    assert!(error.command_started);
    assert!(started.elapsed() < Duration::from_secs(4));
    let pid = wait_for_pid(&pid_path).await;
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_clean_runner_kills_descendant_retaining_pipes() {
    let root =
        std::env::temp_dir().join(format!("local-mcp-bounded-descendant-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let pid_path = root.join("descendant.pid");
    let script = executable_script(
        &root,
        "descendant.sh",
        &format!(
            "#!/bin/sh\n/bin/sleep 4 &\nprintf '%s' \"$!\" > '{}'\nexit 0\n",
            pid_path.display()
        ),
    );
    let timeout = Duration::from_secs(1);
    let started = Instant::now();
    let result = sandbox::run_unrestricted_clean_with_limits(
        &script_command(&script),
        &root,
        None,
        timeout,
        1024,
        1024,
    )
    .await;
    let error = result.expect_err("retained descendant pipes must time out");
    assert!(error.command_started);
    assert!(error.command_finished);
    assert!(
        started.elapsed()
            < timeout + sandbox::TRUSTED_GIT_CLEANUP_GRACE + Duration::from_millis(250)
    );
    let pid = wait_for_pid(&pid_path).await;
    let mut alive = true;
    for _ in 0..50 {
        if unsafe { libc::kill(pid, 0) } == -1 {
            alive = false;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!alive);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_clean_runner_cancellation_kills_group_and_joins_parent() {
    let root = std::env::temp_dir().join(format!("local-mcp-bounded-cancel-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let pid_path = root.join("descendant.pid");
    let script = executable_script(
        &root,
        "cancel.sh",
        &format!(
            "#!/bin/sh\n/bin/sleep 5 &\nprintf '%s' \"$!\" > '{}'\nexec /bin/sleep 5\n",
            pid_path.display()
        ),
    );
    let command = script_command(&script);
    let cwd = root.clone();
    let handle = tokio::spawn(async move {
        sandbox::run_unrestricted_clean_with_limits(
            &command,
            &cwd,
            None,
            Duration::from_secs(30),
            1024,
            1024,
        )
        .await
    });
    let pid = wait_for_pid(&pid_path).await;
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
    let stopped = wait_for_process_exit(pid).await;
    if !stopped {
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(stopped);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn unrestricted_cancellation_kills_group_and_joins_parent() {
    let root =
        std::env::temp_dir().join(format!("local-mcp-unrestricted-cancel-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let pid_path = root.join("descendant.pid");
    let script = executable_script(
        &root,
        "cancel.sh",
        &format!(
            "#!/bin/sh\n/bin/sleep 5 &\nprintf '%s' \"$!\" > '{}'\nexec /bin/sleep 5\n",
            pid_path.display()
        ),
    );
    let command = script_command(&script);
    let cwd = root.clone();
    let handle = tokio::spawn(async move { sandbox::run_unrestricted(&command, &cwd, None).await });
    let pid = wait_for_pid(&pid_path).await;
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
    let stopped = wait_for_process_exit(pid).await;
    if !stopped {
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(stopped);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_clean_runner_rejects_stdout_overflow_without_surviving_child() {
    let root = std::env::temp_dir().join(format!("local-mcp-bounded-stdout-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let script = executable_script(&root, "stdout.sh", "#!/bin/sh\nexec /usr/bin/yes\n");
    let started = Instant::now();
    let result = sandbox::run_unrestricted_clean_with_limits(
        &script_command(&script),
        &root,
        None,
        Duration::from_secs(2),
        32,
        32,
    )
    .await;
    let error = result.expect_err("oversized stdout must fail");
    assert!(error.command_started);
    assert!(started.elapsed() < Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_clean_runner_rejects_stderr_overflow() {
    let root = std::env::temp_dir().join(format!("local-mcp-bounded-stderr-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let script = executable_script(
        &root,
        "stderr.sh",
        "#!/bin/sh\nwhile :; do printf 'overflow\\n' >&2; done\n",
    );
    let result = sandbox::run_unrestricted_clean_with_limits(
        &script_command(&script),
        &root,
        None,
        Duration::from_secs(2),
        32,
        32,
    )
    .await;
    let error = result.expect_err("oversized stderr must fail");
    assert!(error.command_started);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_clean_runner_preserves_normal_output() {
    let root = std::env::temp_dir().join(format!("local-mcp-bounded-clean-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let script = executable_script(
        &root,
        "normal.sh",
        "#!/bin/sh\nprintf stdout\nprintf stderr >&2\n",
    );
    let output = sandbox::run_unrestricted_clean_with_limits(
        &script_command(&script),
        &root,
        None,
        Duration::from_secs(2),
        1024,
        1024,
    )
    .await
    .expect("normal child should complete");
    assert_eq!(output.status, 0);
    assert_eq!(output.stdout, "stdout");
    assert_eq!(output.stderr, "stderr");
    let _ = std::fs::remove_dir_all(root);
}
