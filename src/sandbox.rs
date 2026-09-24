use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result};
#[cfg(not(windows))]
use codex_protocol::models::PermissionProfile;
#[cfg(not(windows))]
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub(crate) const TRUSTED_GIT_QUERY_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const TRUSTED_GIT_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const TRUSTED_GIT_STAGE_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const TRUSTED_GIT_STDOUT_LIMIT: usize = 64 * 1024 * 1024;
pub(crate) const TRUSTED_GIT_STDERR_LIMIT: usize = 1024 * 1024;
pub(crate) const TRUSTED_GIT_CLEANUP_GRACE: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

fn absolute(path: &Path) -> Result<AbsolutePathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    AbsolutePathBuf::from_absolute_path(path).map_err(|error| anyhow::anyhow!(error))
}

#[derive(Debug)]
pub struct RunError {
    pub error: anyhow::Error,
    pub command_started: bool,
    pub command_finished: bool,
}

impl RunError {
    fn new(error: anyhow::Error, command_started: bool, command_finished: bool) -> Self {
        Self {
            error,
            command_started,
            command_finished,
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for RunError {}

struct AbortOnDrop<T> {
    handle: Option<JoinHandle<T>>,
}

impl<T> AbortOnDrop<T> {
    fn new(handle: JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    async fn abort_and_join(&mut self) {
        if let Some(handle) = self.handle.as_ref() {
            handle.abort();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

impl<T> std::future::Future for AbortOnDrop<T> {
    type Output = Result<T, tokio::task::JoinError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(
            self.get_mut()
                .handle
                .as_mut()
                .expect("aborted task handle is missing"),
        )
        .poll(context)
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.as_ref() {
            handle.abort();
        }
    }
}

#[cfg(unix)]
struct UnixProcessGroupGuard {
    process_group_id: libc::pid_t,
}

#[cfg(unix)]
impl UnixProcessGroupGuard {
    fn new(process_group_id: u32) -> Self {
        Self {
            process_group_id: process_group_id as libc::pid_t,
        }
    }
}

#[cfg(unix)]
impl Drop for UnixProcessGroupGuard {
    fn drop(&mut self) {
        if self.process_group_id > 0 {
            let _ = unsafe { libc::kill(-self.process_group_id, libc::SIGKILL) };
        }
    }
}

fn sandbox_process(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    stdin_present: bool,
) -> Result<(PathBuf, Command)> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    let roots = writable_roots
        .iter()
        .map(|path| absolute(path))
        .collect::<Result<Vec<_>>>()?;
    #[cfg(all(test, target_os = "linux"))]
    let network_policy =
        if std::env::var("LOCAL_MCP_TEST_ALLOW_LINUX_NETWORK").as_deref() == Ok("1") {
            // GitHub-hosted Linux runners allow the bubblewrap filesystem/user
            // namespaces used here but deny RTM_NEWADDR while bwrap initializes an
            // isolated loopback device. Keep production restricted; only CI tests
            // opt out of the network namespace so the filesystem sandbox contract
            // remains executable in that environment.
            NetworkSandboxPolicy::Enabled
        } else {
            NetworkSandboxPolicy::Restricted
        };
    #[cfg(not(any(windows, all(test, target_os = "linux"))))]
    let network_policy = NetworkSandboxPolicy::Restricted;
    #[cfg(not(windows))]
    let permissions = PermissionProfile::workspace_write_with(&roots, network_policy, true, true)
        .materialize_project_roots_with_workspace_roots(&[absolute(&cwd)?]);
    #[cfg(windows)]
    let _ = roots;

    #[cfg(target_os = "linux")]
    let mut process = {
        let args =
            codex_sandboxing::landlock::create_linux_sandbox_command_args_for_permission_profile(
                command.to_vec(),
                &cwd,
                &permissions,
                &cwd,
                false,
                false,
            );
        let executable_dir = std::env::current_exe()?
            .parent()
            .context("local-mcp executable has no parent directory")?
            .to_owned();
        let executable = executable_dir.join("codex-linux-sandbox");
        #[cfg(test)]
        let executable = if !executable.is_file()
            && executable_dir.file_name().and_then(|name| name.to_str()) == Some("deps")
        {
            executable_dir
                .parent()
                .map(|debug_dir| debug_dir.join("codex-linux-sandbox"))
                .filter(|test_helper| test_helper.is_file())
                .unwrap_or(executable)
        } else {
            executable
        };
        anyhow::ensure!(
            executable.is_file(),
            "sandbox helper is missing: {}",
            executable.display()
        );
        let mut process = Command::new(executable);
        process.args(args);
        process
    };

    #[cfg(target_os = "macos")]
    let mut process = {
        use codex_sandboxing::seatbelt::CreateSeatbeltCommandArgsParams;
        use codex_sandboxing::seatbelt::MACOS_PATH_TO_SEATBELT_EXECUTABLE;
        use codex_sandboxing::seatbelt::create_seatbelt_command_args;

        let (file_system_policy, network_policy) = permissions.to_runtime_permissions();
        let args = create_seatbelt_command_args(CreateSeatbeltCommandArgsParams {
            command: command.to_vec(),
            file_system_sandbox_policy: &file_system_policy,
            network_sandbox_policy: network_policy,
            sandbox_policy_cwd: &cwd,
            enforce_managed_network: false,
            network: None,
            extra_allow_unix_sockets: &[],
        });
        let mut process = Command::new(MACOS_PATH_TO_SEATBELT_EXECUTABLE);
        process.args(args);
        process
    };

    #[cfg(windows)]
    let mut process = {
        // Windows has no equivalent of Landlock/Seatbelt in this application.
        // Preserve argv execution and the restricted environment so the
        // feature remains usable, while documenting that this is not a
        // filesystem/network sandbox.
        let mut process = Command::new(&command[0]);
        process.args(&command[1..]);
        process
    };

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let mut process = { anyhow::bail!("sandboxed execution is unsupported on this platform") };

    process
        .kill_on_drop(true)
        .current_dir(&cwd)
        .env_clear()
        .envs(safe_environment())
        .stdin(if stdin_present {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    Ok((cwd, process))
}

pub async fn run_tracked(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> std::result::Result<Output, RunError> {
    let (_, mut process) = sandbox_process(command, cwd, writable_roots, stdin.is_some())
        .map_err(|error| RunError::new(error, false, false))?;
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process
        .spawn()
        .context("failed to start primary command")
        .map_err(|error| RunError::new(error, false, false))?;
    #[cfg(unix)]
    let _process_group_guard = UnixProcessGroupGuard::new(child.id().unwrap_or(0));

    if let Some(bytes) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        child_stdin
            .write_all(bytes)
            .await
            .map_err(|error| RunError::new(error.into(), true, false))?;
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|error| RunError::new(error.into(), true, false))?;
    Ok(Output {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(unix)]
pub async fn run(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    run_tracked(command, cwd, writable_roots, stdin)
        .await
        .map_err(|error| error.error)
}

pub async fn run_unrestricted(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
) -> Result<Output> {
    run_unrestricted_inner(command, cwd, stdin, false)
        .await
        .map_err(|error| error.error)
}

#[allow(
    dead_code,
    reason = "Compatibility seam for bounded trusted Git callers."
)]
pub(crate) async fn run_unrestricted_clean(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
) -> Result<Output> {
    run_unrestricted_clean_with_limits(
        command,
        cwd,
        stdin,
        TRUSTED_GIT_STAGE_TIMEOUT,
        TRUSTED_GIT_STDOUT_LIMIT,
        TRUSTED_GIT_STDERR_LIMIT,
    )
    .await
    .map_err(|error| error.error)
}

#[allow(
    dead_code,
    reason = "Compatibility seam for bounded tracked Git callers."
)]
pub(crate) async fn run_unrestricted_clean_tracked(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
) -> std::result::Result<Output, RunError> {
    run_unrestricted_clean_with_limits(
        command,
        cwd,
        stdin,
        TRUSTED_GIT_STAGE_TIMEOUT,
        TRUSTED_GIT_STDOUT_LIMIT,
        TRUSTED_GIT_STDERR_LIMIT,
    )
    .await
}

pub(crate) async fn run_unrestricted_clean_with_limits(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> std::result::Result<Output, RunError> {
    if command.is_empty() {
        return Err(RunError::new(
            anyhow::anyhow!("command must not be empty"),
            false,
            false,
        ));
    }
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))
        .map_err(|error| RunError::new(error, false, false))?;
    let deadline = tokio::time::Instant::now() + timeout;
    let cleanup_deadline = deadline + TRUSTED_GIT_CLEANUP_GRACE;
    let mut process = Command::new(&command[0]);
    process
        .kill_on_drop(true)
        .args(&command[1..])
        .current_dir(&cwd)
        .env_clear()
        .envs(clean_git_environment())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process
        .spawn()
        .context("failed to start bounded Git command")
        .map_err(|error| RunError::new(error, false, false))?;
    let process_group_id = child.id().unwrap_or(0);
    #[cfg(unix)]
    let _process_group_guard = UnixProcessGroupGuard::new(process_group_id);
    let mut stdout_task: Option<CaptureTask> = None;
    let mut stderr_task: Option<CaptureTask> = None;
    let Some(stdout) = child.stdout.take() else {
        let finished = cleanup_bounded_child(
            &mut child,
            process_group_id,
            &mut stdout_task,
            &mut stderr_task,
            cleanup_deadline,
        )
        .await;
        return Err(RunError::new(
            anyhow::anyhow!("bounded Git command has no stdout pipe"),
            true,
            finished,
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        let finished = cleanup_bounded_child(
            &mut child,
            process_group_id,
            &mut stdout_task,
            &mut stderr_task,
            cleanup_deadline,
        )
        .await;
        return Err(RunError::new(
            anyhow::anyhow!("bounded Git command has no stderr pipe"),
            true,
            finished,
        ));
    };
    let state = Arc::new(CaptureState::new());
    stdout_task = Some(AbortOnDrop::new(tokio::spawn(capture_bounded(
        stdout,
        stdout_limit,
        Arc::clone(&state),
    ))));
    stderr_task = Some(AbortOnDrop::new(tokio::spawn(capture_bounded(
        stderr,
        stderr_limit,
        Arc::clone(&state),
    ))));
    if let Some(bytes) = stdin {
        let Some(mut child_stdin) = child.stdin.take() else {
            let finished = cleanup_bounded_child(
                &mut child,
                process_group_id,
                &mut stdout_task,
                &mut stderr_task,
                cleanup_deadline,
            )
            .await;
            return Err(RunError::new(
                anyhow::anyhow!("bounded Git command has no stdin pipe"),
                true,
                finished,
            ));
        };
        if !matches!(
            tokio::time::timeout_at(deadline, child_stdin.write_all(bytes)).await,
            Ok(Ok(()))
        ) {
            let finished = cleanup_bounded_child(
                &mut child,
                process_group_id,
                &mut stdout_task,
                &mut stderr_task,
                cleanup_deadline,
            )
            .await;
            return Err(RunError::new(
                anyhow::anyhow!("bounded Git command stdin write failed or timed out"),
                true,
                finished,
            ));
        }
    }
    drop(child.stdin.take());
    let wait_result = loop {
        if state.too_large.load(Ordering::Acquire) {
            break Err(anyhow::anyhow!("bounded Git command output limit exceeded"));
        }
        if state.failed.load(Ordering::Acquire) {
            break Err(anyhow::anyhow!("bounded Git command output capture failed"));
        }
        tokio::select! {
            result = child.wait() => break result.map_err(anyhow::Error::from),
            _ = tokio::time::sleep_until(deadline) => {
                break Err(anyhow::anyhow!("bounded Git command timed out"));
            }
            _ = state.notify.notified() => {}
        }
    };
    let status = match wait_result {
        Ok(_status) if state.too_large.load(Ordering::Acquire) => {
            let finished = cleanup_bounded_child(
                &mut child,
                process_group_id,
                &mut stdout_task,
                &mut stderr_task,
                cleanup_deadline,
            )
            .await;
            return Err(RunError::new(
                anyhow::anyhow!("bounded Git command output limit exceeded"),
                true,
                finished,
            ));
        }
        Ok(_status) if state.failed.load(Ordering::Acquire) => {
            let finished = cleanup_bounded_child(
                &mut child,
                process_group_id,
                &mut stdout_task,
                &mut stderr_task,
                cleanup_deadline,
            )
            .await;
            return Err(RunError::new(
                anyhow::anyhow!("bounded Git command output capture failed"),
                true,
                finished,
            ));
        }
        Ok(status) => status,
        Err(error) => {
            let finished = cleanup_bounded_child(
                &mut child,
                process_group_id,
                &mut stdout_task,
                &mut stderr_task,
                cleanup_deadline,
            )
            .await;
            return Err(RunError::new(error, true, finished));
        }
    };
    let stdout = match finish_capture_task(&mut stdout_task, cleanup_deadline).await {
        Ok(value) => value,
        Err(error) => {
            let finished = cleanup_bounded_child(
                &mut child,
                process_group_id,
                &mut stdout_task,
                &mut stderr_task,
                cleanup_deadline,
            )
            .await;
            return Err(RunError::new(anyhow::anyhow!("{error}"), true, finished));
        }
    };
    let stderr = match finish_capture_task(&mut stderr_task, cleanup_deadline).await {
        Ok(value) => value,
        Err(error) => {
            let finished = cleanup_bounded_child(
                &mut child,
                process_group_id,
                &mut stdout_task,
                &mut stderr_task,
                cleanup_deadline,
            )
            .await;
            return Err(RunError::new(anyhow::anyhow!("{error}"), true, finished));
        }
    };
    Ok(Output {
        status: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

struct CaptureState {
    too_large: AtomicBool,
    failed: AtomicBool,
    notify: Notify,
}

impl CaptureState {
    fn new() -> Self {
        Self {
            too_large: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }
}

#[derive(Debug)]
enum CaptureError {
    TooLarge,
    Io,
    DidNotFinish,
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => formatter.write_str("bounded Git command output limit exceeded"),
            Self::Io => formatter.write_str("bounded Git command output capture failed"),
            Self::DidNotFinish => {
                formatter.write_str("bounded Git command output capture did not finish")
            }
        }
    }
}

async fn capture_bounded<R>(
    mut reader: R,
    limit: usize,
    state: Arc<CaptureState>,
) -> Result<Vec<u8>, CaptureError>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return Ok(output),
            Ok(read) if output.len().saturating_add(read) > limit => {
                state.too_large.store(true, Ordering::Release);
                state.notify.notify_one();
                return Err(CaptureError::TooLarge);
            }
            Ok(read) => output.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                state.failed.store(true, Ordering::Release);
                state.notify.notify_one();
                return Err(CaptureError::Io);
            }
        }
    }
}

type CaptureTask = AbortOnDrop<Result<Vec<u8>, CaptureError>>;

async fn finish_capture_task(
    task: &mut Option<CaptureTask>,
    deadline: tokio::time::Instant,
) -> Result<Vec<u8>, CaptureError> {
    let Some(handle) = task.as_mut() else {
        return Err(CaptureError::DidNotFinish);
    };
    match tokio::time::timeout_at(deadline, &mut *handle).await {
        Ok(Ok(Ok(output))) => {
            task.take();
            Ok(output)
        }
        Ok(Ok(Err(error))) => {
            task.take();
            Err(error)
        }
        Ok(Err(_)) => {
            task.take();
            Err(CaptureError::DidNotFinish)
        }
        Err(_) => Err(CaptureError::DidNotFinish),
    }
}

async fn abort_capture_task(task: &mut Option<CaptureTask>) {
    if let Some(task) = task.as_mut() {
        task.abort_and_join().await;
    }
    task.take();
}

#[cfg(unix)]
fn kill_bounded_child(child: &mut Child, process_group_id: u32) {
    if process_group_id != 0 {
        let _ = unsafe { libc::kill(-(process_group_id as libc::pid_t), libc::SIGKILL) };
    }
    let _ = child.start_kill();
}

#[cfg(not(unix))]
fn kill_bounded_child(child: &mut Child, _process_group_id: u32) {
    let _ = child.start_kill();
}

async fn cleanup_bounded_child(
    child: &mut Child,
    process_group_id: u32,
    stdout_task: &mut Option<CaptureTask>,
    stderr_task: &mut Option<CaptureTask>,
    cleanup_deadline: tokio::time::Instant,
) -> bool {
    kill_bounded_child(child, process_group_id);
    let finished = matches!(
        tokio::time::timeout_at(cleanup_deadline, child.wait()).await,
        Ok(Ok(_))
    );
    kill_bounded_child(child, process_group_id);
    abort_capture_task(stdout_task).await;
    abort_capture_task(stderr_task).await;
    finished
}

async fn run_unrestricted_inner(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
    clean_environment: bool,
) -> std::result::Result<Output, RunError> {
    if command.is_empty() {
        return Err(RunError::new(
            anyhow::anyhow!("command must not be empty"),
            false,
            false,
        ));
    }
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))
        .map_err(|error| RunError::new(error, false, false))?;
    let mut process = Command::new(&command[0]);
    process
        .kill_on_drop(true)
        .args(&command[1..])
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if clean_environment {
        process.env_clear().envs(clean_git_environment());
    }
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process
        .spawn()
        .context("failed to start unsandboxed command")
        .map_err(|error| RunError::new(error, false, false))?;
    #[cfg(unix)]
    let _process_group_guard = UnixProcessGroupGuard::new(child.id().unwrap_or(0));
    if let Some(bytes) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        child_stdin
            .write_all(bytes)
            .await
            .map_err(|error| RunError::new(error.into(), true, false))?;
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| RunError::new(error.into(), true, false))?;
    Ok(Output {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn safe_environment() -> HashMap<String, String> {
    [
        "PATH",
        "LANG",
        "LC_ALL",
        "TERM",
        "TMPDIR",
        "TEMP",
        "TMP",
        "SystemRoot",
    ]
    .into_iter()
    .filter_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| (name.to_owned(), value))
    })
    .collect()
}

fn clean_git_environment() -> HashMap<String, String> {
    let mut environment = safe_environment();
    environment.insert("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned());
    environment.insert("GIT_CONFIG_GLOBAL".to_owned(), null_device_path());
    environment.insert("GIT_ATTR_NOSYSTEM".to_owned(), "1".to_owned());
    environment
}

#[cfg(windows)]
pub(crate) fn null_device_path() -> String {
    "NUL".to_owned()
}

#[cfg(not(windows))]
pub(crate) fn null_device_path() -> String {
    "/dev/null".to_owned()
}

#[cfg(test)]
mod unrestricted_tests {
    use super::*;

    #[test]
    fn clean_git_environment_drops_ambient_git_overrides() {
        let environment = clean_git_environment();
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ] {
            assert!(!environment.contains_key(name), "{name}");
        }
        assert_eq!(
            environment.get("GIT_CONFIG_NOSYSTEM"),
            Some(&"1".to_owned())
        );
    }

    #[tokio::test]
    async fn host_native_execution_preserves_exit_and_output() -> Result<()> {
        let cwd = std::env::temp_dir();
        #[cfg(unix)]
        let command = vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf stdout; printf stderr >&2; exit 0".into(),
        ];
        #[cfg(windows)]
        let command = vec![
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "[Console]::Out.Write('stdout'); [Console]::Error.Write('stderr'); exit 0".into(),
        ];
        let output = run_unrestricted(&command, &cwd, None).await?;
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout, "stdout");
        assert_eq!(output.stderr, "stderr");
        Ok(())
    }
}

#[cfg(test)]
mod abort_on_drop_tests {
    use std::sync::Arc;

    use tokio::sync::oneshot;

    use super::AbortOnDrop;

    struct DropSignal(Option<oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[tokio::test]
    async fn drop_aborts_and_releases_task() {
        let state = Arc::new(());
        let weak = Arc::downgrade(&state);
        let (started_sender, started_receiver) = oneshot::channel();
        let (dropped_sender, dropped_receiver) = oneshot::channel();
        let handle = tokio::spawn(async move {
            let _state = state;
            let _signal = DropSignal(Some(dropped_sender));
            let _ = started_sender.send(());
            std::future::pending::<()>().await;
        });
        let task = AbortOnDrop::new(handle);
        started_receiver.await.unwrap();
        drop(task);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            dropped_receiver.await.unwrap();
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_directory() -> PathBuf {
        std::env::temp_dir().join(format!("local-mcp-sandbox-test-{}", Uuid::new_v4()))
    }

    #[tokio::test]
    async fn seatbelt_allows_workspace_writes_and_denies_other_writes() -> Result<()> {
        // Nix's macOS build sandbox does not allow a nested Seatbelt profile.
        if std::env::var_os("NIX_BUILD_TOP").is_some() {
            return Ok(());
        }
        let root = test_directory();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&outside)?;

        let allowed = run(
            &["/usr/bin/touch".into(), "allowed".into()],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_eq!(allowed.status, 0, "{}", allowed.stderr);
        assert!(workspace.join("allowed").is_file());

        let denied_path = outside.join("denied");
        let denied = run(
            &[
                "/usr/bin/touch".into(),
                denied_path.to_string_lossy().into_owned(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(denied.status, 0);
        assert!(!denied_path.exists());

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn seatbelt_denies_network_access() -> Result<()> {
        if std::env::var_os("NIX_BUILD_TOP").is_some() {
            return Ok(());
        }
        let workspace = test_directory();
        std::fs::create_dir_all(&workspace)?;
        let output = run(
            &[
                "/usr/bin/curl".into(),
                "--fail".into(),
                "--max-time".into(),
                "2".into(),
                "https://example.com".into(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(output.status, 0);

        std::fs::remove_dir_all(workspace)?;
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    use uuid::Uuid;

    fn test_directory() -> PathBuf {
        std::env::temp_dir().join(format!("local-mcp-linux-sandbox-{}", Uuid::new_v4()))
    }

    async fn run_shell(cwd: &Path, script: &str) -> Result<Output> {
        run(
            &["/bin/sh".into(), "-c".into(), script.into()],
            cwd,
            &[],
            None,
        )
        .await
    }

    #[tokio::test]
    async fn bubblewrap_confines_filesystem_writes_to_workspace() -> Result<()> {
        let root = test_directory();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&outside)?;
        let existing_outside = outside.join("sentinel.txt");
        std::fs::write(&existing_outside, "untouched")?;

        let allowed = run_shell(
            &workspace,
            "printf readable > existing.txt && mkdir nested && printf new > nested/new.txt",
        )
        .await?;
        assert_eq!(allowed.status, 0, "{}", allowed.stderr);
        assert_eq!(
            std::fs::read_to_string(workspace.join("existing.txt"))?,
            "readable"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("nested/new.txt"))?,
            "new"
        );

        let absolute_escape = run_shell(
            &workspace,
            &format!(
                "printf escaped > '{}'",
                outside.join("absolute.txt").display()
            ),
        )
        .await?;
        assert_ne!(absolute_escape.status, 0, "{}", absolute_escape.stderr);

        let traversal_escape =
            run_shell(&workspace, "printf escaped > ../outside/traversal.txt").await?;
        assert_ne!(traversal_escape.status, 0, "{}", traversal_escape.stderr);

        std::os::unix::fs::symlink(&outside, workspace.join("intermediate-link"))?;
        std::os::unix::fs::symlink(&existing_outside, workspace.join("final-link"))?;
        std::os::unix::fs::symlink(
            outside.join("dangling-target.txt"),
            workspace.join("dangling-link"),
        )?;
        for path in [
            "intermediate-link/intermediate.txt",
            "final-link",
            "dangling-link",
        ] {
            let escaped = run_shell(&workspace, &format!("printf escaped > '{path}'")).await?;
            assert_ne!(escaped.status, 0, "path {path}: {}", escaped.stderr);
        }

        assert_eq!(std::fs::read_to_string(existing_outside)?, "untouched");
        assert!(!outside.join("absolute.txt").exists());
        assert!(!outside.join("traversal.txt").exists());
        assert!(!outside.join("intermediate.txt").exists());
        assert!(!outside.join("dangling-target.txt").exists());

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "run on native Linux with LOCAL_MCP_TEST_ALLOW_LINUX_NETWORK unset"]
    async fn bubblewrap_restricted_network_blocks_ip_sockets_and_exposes_only_loopback()
    -> Result<()> {
        anyhow::ensure!(
            std::env::var("LOCAL_MCP_TEST_ALLOW_LINUX_NETWORK").as_deref() != Ok("1"),
            "network isolation test requires the production restricted-network policy"
        );

        let workspace = test_directory();
        std::fs::create_dir_all(&workspace)?;

        let ip_socket = run_shell(
            &workspace,
            "/usr/bin/python3 -c 'import socket; socket.socket(socket.AF_INET, socket.SOCK_STREAM)'",
        )
        .await?;
        assert_ne!(ip_socket.status, 0, "IPv4 socket creation was allowed");
        assert!(
            ip_socket.stderr.contains("Operation not permitted"),
            "expected seccomp EPERM for IP socket creation, got: {}",
            ip_socket.stderr
        );

        let socket_families = run_shell(
            &workspace,
            r#"/usr/bin/python3 -c 'import errno, socket
n=0
for family in (socket.AF_INET, socket.AF_INET6):
 for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
  try: socket.socket(family, kind)
  except OSError as error:
   assert error.errno == errno.EPERM, error
   n += 1
  else: raise SystemExit("IP socket creation unexpectedly succeeded")
assert n == 4
socket.socketpair()
'"#,
        )
        .await?;
        assert_eq!(socket_families.status, 0, "{}", socket_families.stderr);

        let proxy_environment = run_shell(
            &workspace,
            r#"/usr/bin/python3 -c 'import os
names={"http_proxy", "https_proxy", "all_proxy", "no_proxy"}
present=[name for name in os.environ if name.lower() in names]
assert not present, present
'"#,
        )
        .await?;
        assert_eq!(proxy_environment.status, 0, "{}", proxy_environment.stderr);

        let interfaces = run(
            &["/bin/cat".into(), "/proc/net/dev".into()],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_eq!(interfaces.status, 0, "{}", interfaces.stderr);
        let visible_interfaces: Vec<_> = interfaces
            .stdout
            .lines()
            .skip(2)
            .filter_map(|line| line.split_once(':').map(|(name, _)| name.trim()))
            .collect();
        assert_eq!(visible_interfaces, ["lo"]);

        let external_network = run(
            &[
                "/usr/bin/curl".into(),
                "--noproxy".into(),
                "*".into(),
                "--connect-timeout".into(),
                "2".into(),
                "--max-time".into(),
                "3".into(),
                "--silent".into(),
                "--show-error".into(),
                "https://1.1.1.1".into(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(external_network.status, 0, "external network was reachable");

        let dns = run(
            &[
                "/usr/bin/getent".into(),
                "hosts".into(),
                "example.com".into(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(dns.status, 0, "external DNS resolution succeeded");

        std::fs::remove_dir_all(workspace)?;
        Ok(())
    }
}
