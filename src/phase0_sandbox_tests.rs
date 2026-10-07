#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::sandbox;

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::fd::FromRawFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
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
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
struct TempRoot(PathBuf);

#[cfg(unix)]
impl TempRoot {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

#[cfg(unix)]
impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
const WITNESS_READY: Duration = Duration::from_secs(10);

#[cfg(unix)]
const WITNESS_TEARDOWN: Duration = Duration::from_secs(5);

#[cfg(unix)]
struct LifetimeWitness {
    path: PathBuf,
    ready_path: PathBuf,
    read_end: tokio::fs::File,
}

#[cfg(unix)]
impl LifetimeWitness {
    fn new(root: &Path) -> Self {
        let path = root.join("descendant.witness");
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let read_fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        assert!(read_fd >= 0, "witness FIFO read end must open");
        let file = unsafe { std::fs::File::from_raw_fd(read_fd) };
        Self {
            ready_path: root.join("descendant.ready"),
            path,
            read_end: tokio::fs::File::from_std(file),
        }
    }

    async fn wait_until_ready(&self) -> bool {
        loop {
            if tokio::fs::try_exists(&self.ready_path)
                .await
                .unwrap_or(false)
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn observe_until_eof(&mut self) -> bool {
        use tokio::io::AsyncReadExt;

        let mut buffer = [0_u8; 64];
        loop {
            match self.read_end.read(&mut buffer).await {
                Ok(0) => return true,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(_) => return false,
            }
        }
    }
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
#[test]
fn descendant_lifetime_helper() {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let Some(path) = std::env::var_os("LOCAL_MCP_DESCENDANT_WITNESS") else {
        return;
    };
    let ready_path = std::env::var_os("LOCAL_MCP_DESCENDANT_READY")
        .expect("the readiness marker path must be configured");
    let path = CString::new(path.as_os_str().as_bytes()).expect("witness path must be NUL-free");
    let writer = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if writer == -1 {
        return;
    }
    let writer = unsafe { std::fs::File::from_raw_fd(writer) };
    let writer_fd = writer.as_raw_fd();
    let mut child = Command::new("/bin/sleep");
    child.arg("30");
    unsafe {
        child.pre_exec(move || {
            const WITNESS_FD: libc::c_int = 9;
            if libc::dup2(writer_fd, WITNESS_FD) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(WITNESS_FD, libc::F_GETFD);
            if flags == -1
                || libc::fcntl(WITNESS_FD, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            if writer_fd != WITNESS_FD && libc::close(writer_fd) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let ready = b"R";
            if libc::write(WITNESS_FD, ready.as_ptr().cast(), ready.len()) != ready.len() as isize {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = child.spawn().expect("the witnessed descendant must spawn");
    std::fs::write(ready_path, b"ready").expect("the descendant readiness marker must persist");
    // The runner owns cleanup of this process group; dropping detaches the child
    // handle so the helper can exit while the witnessed descendant stays alive.
    drop(child);
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_clean_runner_kills_descendant_retaining_pipes() {
    let root = TempRoot::new("local-mcp-bounded-descendant");
    let mut witness = LifetimeWitness::new(&root.0);
    let executable = std::env::current_exe().expect("test executable path");
    let script = executable_script(
        &root.0,
        "descendant.sh",
        &format!(
            "#!/bin/sh\nLOCAL_MCP_DESCENDANT_WITNESS={} LOCAL_MCP_DESCENDANT_READY={} exec {} --exact phase0_tests::sandbox_contract::descendant_lifetime_helper --nocapture\n",
            shell_quote(&witness.path.display().to_string()),
            shell_quote(&witness.ready_path.display().to_string()),
            shell_quote(&executable.display().to_string())
        ),
    );
    // The nested test harness startup competed with the bounded runner's original
    // one-second deadline. Under parallel load the runner could finish before the
    // helper had a chance to fork its witnessed descendant. Give fixture startup a
    // separately observed readiness phase before judging the cleanup result.
    let timeout = Duration::from_secs(3);
    let witness_path = witness.path.clone();
    let started = Instant::now();
    let command = script_command(&script);
    let cwd = root.0.clone();
    let runner = tokio::spawn(async move {
        sandbox::run_unrestricted_clean_with_limits(&command, &cwd, None, timeout, 1024, 1024).await
    });
    let ready = tokio::time::timeout(WITNESS_READY, witness.wait_until_ready())
        .await
        .unwrap_or(false);
    let result = runner
        .await
        .expect("the bounded runner task must not panic");
    let run_finished_at = Instant::now();
    let eof = tokio::time::timeout(WITNESS_TEARDOWN, witness.observe_until_eof())
        .await
        .expect("the kernel lifetime witness must reach EOF after bounded cleanup");
    assert!(
        ready,
        "the writer handshake was not observed before cleanup; this is a fixture-startup \
         failure, not descendant-cleanup evidence (run finished in {:?}, witness {})",
        run_finished_at.duration_since(started),
        witness_path.display()
    );
    let error = result.expect_err("the ready descendant must retain stdout/stderr until deadline");
    assert!(error.command_started);
    assert!(error.command_finished);
    assert!(
        run_finished_at.duration_since(started)
            < timeout + sandbox::TRUSTED_GIT_CLEANUP_GRACE + Duration::from_millis(250)
    );
    assert!(eof, "the FIFO observer must report kernel EOF");
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
async fn bounded_clean_raw_runner_preserves_non_utf8_stdout_bytes() {
    let root = std::env::temp_dir().join(format!("local-mcp-bounded-raw-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let script = executable_script(&root, "raw.sh", "#!/bin/sh\nprintf '\\377'\n");
    let output = sandbox::run_unrestricted_clean_raw_with_limits(
        &script_command(&script),
        &root,
        None,
        Duration::from_secs(2),
        1024,
        1024,
    )
    .await
    .expect("bounded raw child should complete");
    assert_eq!(output.status, 0);
    assert_eq!(output.stdout, vec![0xff]);
    assert!(output.command_start.is_proven());
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
