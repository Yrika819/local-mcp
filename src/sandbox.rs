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
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::process_group::ProcessGroup;

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
    /// Host-owned evidence about whether the **requested command** started.
    ///
    /// This is deliberately not derived from `status`, `stdout` or `stderr`.
    /// When the sandbox is provided by a wrapper process, the wrapper's
    /// completion is not evidence that the requested command was ever exec'd,
    /// and the requested command controls its own output and exit value.
    pub command_start: CommandStart,
}

fn absolute(path: &Path) -> Result<AbsolutePathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    AbsolutePathBuf::from_absolute_path(path).map_err(|error| anyhow::anyhow!(error))
}

/// Host-owned evidence about whether the **requested command** started.
///
/// The sandbox helper is a separate process. Its exit status, stdout and stderr
/// describe the *helper*, never the requested command, and the requested command
/// fully controls its own output and exit value. No caller-visible text or code
/// can therefore establish [`CommandStart::Proven`]. Absence of host-owned proof
/// is always [`CommandStart::Unproven`], never an assumption that the command ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CommandStart {
    /// The host spawned the requested command itself and observed it start.
    Proven,
    /// The host proved the requested command was never started.
    Refuted,
    /// A sandbox wrapper ran to completion, but the wrapper provides the host no
    /// evidence about whether it exec'd the requested command. The requested
    /// command may or may not have started.
    Unproven,
}

impl CommandStart {
    /// Whether the requested command is host-proven to have started.
    ///
    /// Only [`CommandStart::Proven`] satisfies the lifecycle gate that guards
    /// executable host fallback.
    pub fn is_proven(self) -> bool {
        self == Self::Proven
    }
}

/// A sandbox setup refusal decided by trusted host code, before the sandbox
/// helper or the requested command could run.
///
/// Only Linux verifies its sandbox runtime in parent code before spawning
/// anything, so only Linux constructs a typed rejection. Every other platform
/// learns about a setup failure from the wrapper, which is unproven lifecycle
/// evidence rather than a host-owned refusal, so it produces no value here.
/// The type itself stays platform-independent because `fallback::classify`
/// consumes it as the authoritative setup authority on every target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "linux"),
    expect(
        dead_code,
        reason = "typed setup rejection is constructed by the parent-side runtime gate, which only Linux performs"
    )
)]
pub enum SetupRejection {
    /// A platform safety gate refused to establish the sandbox. Terminal: the
    /// request must not proceed, in or out of the sandbox.
    PlatformSafety,
    /// The sandbox environment could not be established. The requested command
    /// never started and must not be retried outside the sandbox.
    Environment,
}

#[derive(Debug)]
pub struct RunError {
    pub error: anyhow::Error,
    pub command_started: bool,
    pub command_finished: bool,
    /// Typed host-owned setup refusal, when the failure happened before the
    /// requested command could start. This is the authoritative signal the
    /// fallback classifier consumes; the message text is diagnostic only.
    pub setup_rejection: Option<SetupRejection>,
}

impl RunError {
    fn new(error: anyhow::Error, command_started: bool, command_finished: bool) -> Self {
        Self {
            error,
            command_started,
            command_finished,
            setup_rejection: None,
        }
    }

    /// A failure the host proved happened before the requested command started.
    fn not_started(error: anyhow::Error) -> Self {
        Self::new(error, false, false)
    }

    /// A host-owned refusal to establish the sandbox at all.
    ///
    /// Only platforms that gate their sandbox runtime up front produce this; on
    /// other platforms the refusal is made by the wrapper and is unproven rather
    /// than typed here.
    #[cfg(target_os = "linux")]
    fn setup_rejected(error: anyhow::Error, rejection: SetupRejection) -> Self {
        Self {
            error,
            command_started: false,
            command_finished: false,
            setup_rejection: Some(rejection),
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

/// Host-owned evidence about the requested command after a sandboxed process
/// exits.
///
/// Where the platform sandbox is a separate wrapper process, the wrapper can
/// fail at any point before exec'ing the requested command: an absent or
/// version-vulnerable runtime, a namespace or mount setup failure, a seccomp
/// setup failure, or an exec failure. The wrapper reports all of these the same
/// way, through its own exit status and output, and the requested command
/// controls its own output and exit value. There is therefore no host-owned
/// evidence that the requested command started, and the lifecycle must stay
/// unproven rather than being inferred from the wrapper having run.
///
/// Where no wrapper is involved the host spawns the requested command itself,
/// so its start is host-proven.
const fn completed_command_start() -> CommandStart {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        CommandStart::Unproven
    } else {
        CommandStart::Proven
    }
}

fn sandbox_process(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    stdin_present: bool,
    #[allow(unused_variables, reason = "Only Linux setup preflight reads a PATH.")] path: Option<
        &std::ffi::OsStr,
    >,
) -> Result<(PathBuf, Command)> {
    // Production is restricted on every platform. The only build that can ever
    // answer otherwise is the test build, and only after the host itself has
    // been shown to be unable to build a restricted sandbox, so that the answer
    // comes from the machine rather than from anything a caller can set.
    #[cfg(all(test, target_os = "linux"))]
    let network_policy = test_linux_network_policy();
    #[cfg(not(all(test, target_os = "linux")))]
    let network_policy = NetworkSandboxPolicy::Restricted;
    build_sandbox_process(
        command,
        cwd,
        writable_roots,
        stdin_present,
        path,
        network_policy,
    )
}

/// Network policy used by the test build on a Linux host.
///
/// Delegated to a host-owned capability probe so a caller cannot relax it, and
/// so the suite is honest about what the machine it is running on can prove.
#[cfg(all(test, target_os = "linux"))]
fn test_linux_network_policy() -> NetworkSandboxPolicy {
    if restricted_network_sandbox_is_supported() {
        NetworkSandboxPolicy::Restricted
    } else {
        NetworkSandboxPolicy::Enabled
    }
}

/// Host-owned answer to: can this host build the production restricted-network
/// sandbox at all?
///
/// Restricted networking needs the wrapper to own an isolated network namespace
/// whose loopback device it can bring up, which in turn means privileges inside
/// the user namespace the wrapper just created. A host that refuses an
/// unprivileged process's write to `/proc/<pid>/uid_map` hands the wrapper the
/// namespaces but never the privileges, so the wrapper dies during setup and
/// every sandboxed request on that host fails before the requested command
/// runs. The GitHub-hosted Ubuntu images are such a host: a plain
/// `unshare -Ur -- true` with no wrapper involved fails the same way.
///
/// The question is answered by running the real wrapper with the real
/// restricted profile and reading nothing but the wrapper's own exit status. It
/// is host-owned in the sense that matters: the command is fixed here, and
/// nothing about it comes from a caller, an environment variable, or a model.
/// The answer is memoized because a host's ability to build a namespace does
/// not change between tests, and because an unmemoized probe would add a spawn
/// to every sandboxed request.
///
/// The polarity is fail-closed. Only an observed successful restricted run
/// answers "supported". A missing or vulnerable runtime, a wrapper that cannot
/// be constructed, a workspace that cannot be named, and a wrapper that fails
/// for any reason all answer "not observed", which leaves the production policy
/// in place so the tests that genuinely need a sandbox fail loudly on the real
/// cause instead of being quietly relaxed to accommodate it.
#[cfg(all(test, target_os = "linux"))]
fn restricted_network_sandbox_is_supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        use crate::bubblewrap_support::{self, BubblewrapSupport};
        let path = std::env::var_os("PATH").unwrap_or_default();
        if bubblewrap_support::check(&path) != BubblewrapSupport::Supported {
            // A missing or version-vulnerable runtime is a different fault with
            // its own typed setup rejection. Do not re-describe it as a missing
            // network capability.
            return true;
        }
        let Ok(root) = std::env::temp_dir()
            .join(format!(
                "local-mcp-network-capability-{}",
                uuid::Uuid::new_v4()
            ))
            .canonicalize()
        else {
            return true;
        };
        let _ = std::fs::create_dir_all(&root);
        let probe = ["/bin/true".to_owned()];
        let supported = build_sandbox_process(
            &probe,
            &root,
            &[],
            false,
            None,
            NetworkSandboxPolicy::Restricted,
        )
        .is_ok_and(|(_, process)| {
            // `std::process::Command` is not `Clone`, so rebuild an equivalent
            // child from the helper invocation the real run would have used.
            // The probe blocks rather than awaits so it stays usable from the
            // synchronous construction path, and it is memoized so it runs once.
            let helper = process.as_std();
            let mut child = std::process::Command::new(helper.get_program());
            child.args(helper.get_args());
            if let Some(directory) = helper.get_current_dir() {
                child.current_dir(directory);
            }
            for (key, value) in helper.get_envs() {
                match value {
                    Some(value) => child.env(key, value),
                    None => child.env_remove(key),
                };
            }
            child.stdin(Stdio::null());
            child.output().is_ok_and(|output| output.status.success())
        });
        let _ = std::fs::remove_dir_all(&root);
        supported
    })
}

fn build_sandbox_process(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    stdin_present: bool,
    #[allow(
        unused_variables,
        reason = "The PATH is consumed by the separate parent-side setup preflight, never here"
    )]
    path: Option<&std::ffi::OsStr>,
    network_policy: NetworkSandboxPolicy,
) -> Result<(PathBuf, Command)> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    let roots = writable_roots
        .iter()
        .map(|path| absolute(path))
        .collect::<Result<Vec<_>>>()?;
    #[cfg(not(windows))]
    let permissions = PermissionProfile::workspace_write_with(&roots, network_policy, true, true)
        .materialize_project_roots_with_workspace_roots(&[absolute(&cwd)?]);
    #[cfg(windows)]
    let _ = (roots, network_policy);

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
        .envs(safe_environment_with_path(path))
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
    run_tracked_with_path(command, cwd, writable_roots, stdin, None).await
}

/// Same as [`run_tracked`] but with the `PATH` the sandbox helper will search
/// for its platform sandbox runtime supplied explicitly.
///
/// Production always passes `None` and uses the ambient `PATH`. Tests pass a
/// fixture directory so an unsupported Bubblewrap version can be emulated
/// without depending on the version installed on the host.
pub(crate) async fn run_tracked_with_path(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    stdin: Option<&[u8]>,
    path: Option<&std::ffi::OsStr>,
) -> std::result::Result<Output, RunError> {
    // Trusted parent-side setup gate. The Linux sandbox runtime is validated
    // here, before the helper is spawned, so an unusable runtime produces a
    // typed host-owned setup refusal while the requested command provably has
    // not started. The helper keeps its own identical gate as defence in depth.
    verify_platform_sandbox_support(path)?;
    let (_, mut process) = sandbox_process(command, cwd, writable_roots, stdin.is_some(), path)
        .map_err(RunError::not_started)?;
    let mut group = ProcessGroup::spawn(&mut process)
        .context("failed to start primary command")
        .map_err(RunError::not_started)?;

    if let Some(bytes) = stdin
        && let Some(mut child_stdin) = group.take_stdin()
    {
        child_stdin
            .write_all(bytes)
            .await
            .map_err(|error| RunError::new(error.into(), true, false))?;
    }

    let output = group
        .terminate_and_capture()
        .await
        .map_err(|error| RunError::new(error.into(), true, false))?;
    Ok(Output {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        command_start: completed_command_start(),
    })
}

/// Validate that the platform sandbox runtime can be established, before any
/// sandbox helper or requested command process is started.
///
/// On Linux the sandbox is implemented by bubblewrap, so an absent, unusable or
/// version-vulnerable runtime is refused here. Refusing a vulnerable runtime is
/// a platform safety decision: it is terminal, and the requested command must
/// not be retried outside the sandbox.
fn verify_platform_sandbox_support(
    path: Option<&std::ffi::OsStr>,
) -> std::result::Result<(), RunError> {
    #[cfg(target_os = "linux")]
    {
        use crate::bubblewrap_support::{self, BubblewrapSupport};
        let path = path
            .map(std::borrow::ToOwned::to_owned)
            .or_else(|| std::env::var_os("PATH"));
        let Some(path) = path else {
            return Err(RunError::setup_rejected(
                anyhow::anyhow!(
                    "{}",
                    bubblewrap_support::rejection_message(BubblewrapSupport::Unavailable)
                ),
                SetupRejection::Environment,
            ));
        };
        match bubblewrap_support::check(&path) {
            BubblewrapSupport::Supported => Ok(()),
            BubblewrapSupport::UnsupportedVersion => Err(RunError::setup_rejected(
                anyhow::anyhow!(
                    "{}",
                    bubblewrap_support::rejection_message(BubblewrapSupport::UnsupportedVersion)
                ),
                SetupRejection::PlatformSafety,
            )),
            BubblewrapSupport::Unavailable => Err(RunError::setup_rejected(
                anyhow::anyhow!(
                    "{}",
                    bubblewrap_support::rejection_message(BubblewrapSupport::Unavailable)
                ),
                SetupRejection::Environment,
            )),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Ok(())
    }
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

/// Windows has no application sandbox in this program: `build_sandbox_process` execs
/// the requested command directly and documents that this is argv execution plus a
/// restricted environment, not a filesystem or network boundary.
///
/// This entry point exists so a caller can request a sandboxed run on either platform
/// without a `cfg` split of its own. On Windows it applies the same argv execution and
/// restricted environment that every other Windows command already gets, and grants no
/// containment it does not already have.
#[cfg(not(unix))]
pub async fn run(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    let _ = writable_roots;
    run_tracked(command, cwd, &[], stdin)
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
    let output = run_unrestricted_clean_raw_with_limits(
        command,
        cwd,
        stdin,
        timeout,
        stdout_limit,
        stderr_limit,
    )
    .await?;
    Ok(Output {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        command_start: output.command_start,
    })
}

/// The bounded trusted-process result before any text decoding.
#[derive(Debug)]
pub(crate) struct RawOutput {
    pub(crate) status: i32,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) command_start: CommandStart,
}

/// Run a bounded trusted command while preserving stdout bytes for path authority.
pub(crate) async fn run_unrestricted_clean_raw_with_limits(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> std::result::Result<RawOutput, RunError> {
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
    let mut group = ProcessGroup::spawn(&mut process)
        .context("failed to start bounded Git command")
        .map_err(|error| RunError::new(error, false, false))?;
    let mut stdout_task: Option<CaptureTask> = None;
    let mut stderr_task: Option<CaptureTask> = None;
    let Some(stdout) = group.take_stdout() else {
        let finished = cleanup_bounded_child(
            &mut group,
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
    let Some(stderr) = group.take_stderr() else {
        let finished = cleanup_bounded_child(
            &mut group,
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
        let Some(mut child_stdin) = group.take_stdin() else {
            let finished = cleanup_bounded_child(
                &mut group,
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
                &mut group,
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
    drop(group.take_stdin());
    let wait_result = loop {
        if state.too_large.load(Ordering::Acquire) {
            break Err(anyhow::anyhow!("bounded Git command output limit exceeded"));
        }
        if state.failed.load(Ordering::Acquire) {
            break Err(anyhow::anyhow!("bounded Git command output capture failed"));
        }
        tokio::select! {
            result = group.wait_termination() => break result.map_err(anyhow::Error::from),
            _ = tokio::time::sleep_until(deadline) => {
                break Err(anyhow::anyhow!("bounded Git command timed out"));
            }
            _ = state.notify.notified() => {}
        }
    };
    let status = match wait_result {
        Ok(_status) if state.too_large.load(Ordering::Acquire) => {
            let finished = cleanup_bounded_child(
                &mut group,
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
                &mut group,
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
                &mut group,
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
                &mut group,
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
                &mut group,
                &mut stdout_task,
                &mut stderr_task,
                cleanup_deadline,
            )
            .await;
            return Err(RunError::new(anyhow::anyhow!("{error}"), true, finished));
        }
    };
    Ok(RawOutput {
        status: status.code().unwrap_or(-1),
        stdout,
        stderr,
        command_start: CommandStart::Proven,
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

async fn cleanup_bounded_child(
    group: &mut ProcessGroup,
    stdout_task: &mut Option<CaptureTask>,
    stderr_task: &mut Option<CaptureTask>,
    cleanup_deadline: tokio::time::Instant,
) -> bool {
    group.terminate();
    let finished = matches!(
        tokio::time::timeout_at(cleanup_deadline, group.wait_termination()).await,
        Ok(Ok(_))
    );
    group.terminate();
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
    let mut group = ProcessGroup::spawn(&mut process)
        .context("failed to start unsandboxed command")
        .map_err(|error| RunError::new(error, false, false))?;
    if let Some(bytes) = stdin
        && let Some(mut child_stdin) = group.take_stdin()
    {
        child_stdin
            .write_all(bytes)
            .await
            .map_err(|error| RunError::new(error.into(), true, false))?;
    }
    let output = group
        .terminate_and_capture()
        .await
        .map_err(|error| RunError::new(error.into(), true, false))?;
    Ok(Output {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        command_start: CommandStart::Proven,
    })
}

fn safe_environment() -> HashMap<String, String> {
    safe_environment_with_path(None)
}

fn safe_environment_with_path(path: Option<&std::ffi::OsStr>) -> HashMap<String, String> {
    let mut environment: HashMap<String, String> = [
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
    .collect();
    if let Some(path) = path {
        environment.insert("PATH".to_owned(), path.to_string_lossy().into_owned());
    }
    environment
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

    /// Write a fake `bwrap` onto a private `PATH` that reports `version`.
    fn fixture_bwrap(root: &Path, version: &str) -> PathBuf {
        let bin = root.join("fixture-bin");
        std::fs::create_dir_all(&bin).expect("create fixture bin");
        let bwrap = bin.join("bwrap");
        std::fs::write(
            &bwrap,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf 'bubblewrap {version}\\n'; exit 0; fi\nexit 97\n"
            ),
        )
        .expect("write fixture bwrap");
        let mut permissions = std::fs::metadata(&bwrap)
            .expect("stat fixture bwrap")
            .permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&bwrap, permissions).expect("chmod fixture bwrap");
        bin
    }

    /// A completed sandbox attempt proves the executed process ran, never that
    /// the requested command was exec'd inside the wrapper.
    ///
    /// This is the boundary that the executable fallback gate depends on: on a
    /// wrapper platform the requested command's start stays unproven even when
    /// the command provably ran and produced its own output.
    #[tokio::test]
    async fn completed_sandbox_run_leaves_requested_command_start_unproven() -> Result<()> {
        let root = test_directory();
        std::fs::create_dir_all(&root)?;
        let marker = root.join("requested-command-ran");
        let output = run(
            &[
                "/bin/sh".into(),
                "-c".into(),
                format!("printf started > '{}'", marker.display()),
            ],
            &root,
            &[],
            None,
        )
        .await?;
        assert_eq!(output.status, 0, "{}", output.stderr);
        assert!(marker.is_file(), "the requested command really did run");
        assert_eq!(output.command_start, CommandStart::Unproven);
        assert!(!output.command_start.is_proven());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    /// The trusted parent refuses an unsupported sandbox runtime before the
    /// sandbox helper is spawned, with a typed setup refusal and no proof that
    /// the requested command started.
    #[tokio::test]
    async fn unsupported_runtime_is_refused_before_the_helper_is_spawned() -> Result<()> {
        let root = test_directory();
        std::fs::create_dir_all(&root)?;
        let marker = root.join("requested-command-ran");
        let path = fixture_bwrap(&root, "0.11.1");
        let error = run_tracked_with_path(
            &[
                "/bin/sh".into(),
                "-c".into(),
                format!("printf started > '{}'", marker.display()),
            ],
            &root,
            &[],
            None,
            Some(path.as_os_str()),
        )
        .await
        .expect_err("an unsupported runtime must not produce a completed run");
        assert_eq!(error.setup_rejection, Some(SetupRejection::PlatformSafety));
        assert!(!error.command_started);
        assert!(!error.command_finished);
        assert!(
            !marker.exists(),
            "the requested command must not start under an unsupported runtime"
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    /// An absent sandbox runtime is an environment fault, not a platform safety
    /// refusal, and is still terminal: the requested command never starts.
    #[tokio::test]
    async fn missing_runtime_is_an_environment_setup_rejection() -> Result<()> {
        let root = test_directory();
        std::fs::create_dir_all(&root)?;
        let empty = root.join("empty-path");
        std::fs::create_dir_all(&empty)?;
        let error = run_tracked_with_path(
            &["/bin/true".into()],
            &root,
            &[],
            None,
            Some(empty.as_os_str()),
        )
        .await
        .expect_err("an absent runtime must not produce a completed run");
        assert_eq!(error.setup_rejection, Some(SetupRejection::Environment));
        assert!(!error.command_started);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    /// Failing to build the sandbox process is a pre-start failure, so the
    /// requested command provably never started.
    #[tokio::test]
    async fn unconstructible_sandbox_refutes_requested_command_start() -> Result<()> {
        let root = test_directory();
        std::fs::create_dir_all(&root)?;
        let error = run_tracked(
            &["/bin/true".into()],
            &root.join("does-not-exist"),
            &[],
            None,
        )
        .await
        .expect_err("an unresolvable cwd must fail before the command starts");
        assert!(!error.command_started);
        assert!(!error.command_finished);
        assert_eq!(error.setup_rejection, None);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    /// Host-native execution spawns the requested command directly, so its
    /// start is host-proven.
    #[tokio::test]
    async fn host_native_execution_proves_requested_command_start() -> Result<()> {
        let root = test_directory();
        std::fs::create_dir_all(&root)?;
        let output = run_unrestricted(&["/bin/true".into()], &root, None).await?;
        assert_eq!(output.command_start, CommandStart::Proven);
        assert!(output.command_start.is_proven());
        std::fs::remove_dir_all(root)?;
        Ok(())
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
    async fn bubblewrap_restricted_network_blocks_ip_sockets_and_exposes_only_loopback()
    -> Result<()> {
        // This is the only test that observes the network half of the sandbox,
        // and it cannot observe it on a host that cannot build a restricted
        // sandbox at all. There the wrapper's own setup failure would satisfy
        // the first assertions below while the requested command never ran, so
        // the test would report isolation it never saw. Decide from the host's
        // own answer, before touching the sandbox, so a host that can prove
        // isolation still proves it and a host that cannot says so.
        if !restricted_network_sandbox_is_supported() {
            eprintln!(
                "skipping: this host cannot build the restricted-network sandbox, \
                 so there is no network isolation to observe"
            );
            return Ok(());
        }

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
