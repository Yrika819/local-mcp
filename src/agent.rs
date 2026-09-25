use std::env::VarError;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::task::JoinHandle;

use crate::{approvals, fallback, sandbox};

pub(crate) const MODEL_TIMEOUT: Duration = Duration::from_secs(120);
pub(crate) const PROCESS_CLEANUP_GRACE: Duration = Duration::from_millis(500);
pub(crate) const MODEL_STDOUT_LIMIT: usize = 2 * 1024 * 1024;
pub(crate) const MODEL_STDERR_LIMIT: usize = 64 * 1024;
pub(crate) const MODEL_PROMPT_LIMIT: usize = 256 * 1024;
pub(crate) const GOAL_MODEL_ENV: &str = "LOCAL_MCP_GOAL_MODEL";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelRole {
    Planner,
    Readonly,
    Writer,
    Reviewer,
    Replanner,
}

impl ModelRole {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Planner => "PLANNER",
            Self::Readonly => "READONLY",
            Self::Writer => "WRITER",
            Self::Reviewer => "REVIEWER",
            Self::Replanner => "REPLANNER",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelInvocation {
    session_id: String,
    cwd: PathBuf,
    role: ModelRole,
    prompt: String,
}

impl ModelInvocation {
    pub(crate) fn new(
        session_id: impl Into<String>,
        cwd: PathBuf,
        role: ModelRole,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            cwd,
            role,
            prompt: prompt.into(),
        }
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub(crate) fn role(&self) -> ModelRole {
        self.role
    }

    pub(crate) fn prompt(&self) -> &str {
        &self.prompt
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelInvocationOutput {
    stdout: Vec<u8>,
    safe_stderr: String,
    exit_status: i32,
}

#[allow(
    dead_code,
    reason = "Frozen model-output accessors are retained for the staged Goal Orchestrator backend interface."
)]
impl ModelInvocationOutput {
    pub(crate) fn new(stdout: Vec<u8>, safe_stderr: String, exit_status: i32) -> Self {
        Self {
            stdout,
            safe_stderr,
            exit_status,
        }
    }

    pub(crate) fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    pub(crate) fn safe_stderr(&self) -> &str {
        &self.safe_stderr
    }

    pub(crate) fn exit_status(&self) -> i32 {
        self.exit_status
    }

    pub(crate) fn into_stdout(self) -> Vec<u8> {
        self.stdout
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentError {
    ApprovalDenied,
    ExecutableUnavailable,
    SpawnFailed,
    Timeout,
    NonZeroExit,
    EmptyResponse,
    ResponseTooLarge,
    TransportFailure,
    Cancelled,
    InvalidConfiguration,
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ApprovalDenied => "model invocation approval was denied",
            Self::ExecutableUnavailable => "Codex executable is unavailable",
            Self::SpawnFailed => "Codex process failed to spawn",
            Self::Timeout => "model invocation timed out",
            Self::NonZeroExit => "Codex exited unsuccessfully",
            Self::EmptyResponse => "model response was empty",
            Self::ResponseTooLarge => "model response exceeded the host limit",
            Self::TransportFailure => "model transport failed",
            Self::Cancelled => "model invocation was cancelled",
            Self::InvalidConfiguration => "invalid Goal model configuration",
        })
    }
}

impl std::error::Error for AgentError {}

pub(crate) trait ModelTransport: Send + Sync {
    fn invoke(&self, request: &ModelInvocation) -> Result<ModelInvocationOutput, AgentError>;
}

#[derive(Clone)]
pub(crate) struct GoalModelAgent {
    session_id: String,
    session_cwd: PathBuf,
    transport: Arc<dyn ModelTransport>,
}

impl GoalModelAgent {
    pub(crate) fn production(session_id: impl Into<String>, session_cwd: PathBuf) -> Self {
        Self::with_transport(session_id, session_cwd, Arc::new(ProductionModelTransport))
    }

    pub(crate) fn with_transport(
        session_id: impl Into<String>,
        session_cwd: PathBuf,
        transport: Arc<dyn ModelTransport>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            session_cwd,
            transport,
        }
    }

    pub(crate) fn invoke(
        &self,
        role: ModelRole,
        cwd: PathBuf,
        prompt: String,
    ) -> Result<ModelInvocationOutput, AgentError> {
        if prompt.len() > MODEL_PROMPT_LIMIT {
            return Err(AgentError::InvalidConfiguration);
        }
        self.transport.invoke(&ModelInvocation::new(
            self.session_id.clone(),
            cwd,
            role,
            prompt,
        ))
    }

    pub(crate) fn invoke_at_session_cwd(
        &self,
        role: ModelRole,
        prompt: String,
    ) -> Result<ModelInvocationOutput, AgentError> {
        self.invoke(role, self.session_cwd.clone(), prompt)
    }
}

struct ProductionModelTransport;

impl ModelTransport for ProductionModelTransport {
    fn invoke(&self, request: &ModelInvocation) -> Result<ModelInvocationOutput, AgentError> {
        let request = request.clone();
        std::thread::Builder::new()
            .name("local-mcp-goal-model".to_owned())
            .spawn(move || joined_model_invocation(&request))
            .map_err(|_| AgentError::SpawnFailed)?
            .join()
            .map_err(|_| AgentError::Cancelled)?
    }
}

fn joined_model_invocation(request: &ModelInvocation) -> Result<ModelInvocationOutput, AgentError> {
    let started = Instant::now();
    let result = (|| {
        let model = configured_model()?;
        let command = fallback::codex_read_only_command_with_model(
            request.cwd(),
            fallback::Effort::Medium,
            &model,
        )
        .map_err(|_| AgentError::ExecutableUnavailable)?;
        let deadline = Instant::now() + MODEL_TIMEOUT;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| AgentError::TransportFailure)?;
        let approved = runtime.block_on(async {
            let remaining = deadline.saturating_duration_since(Instant::now());
            tokio::time::timeout(
                remaining,
                approvals::request(
                    request.session_id(),
                    "goal_model",
                    format!(
                        "Run the approved read-only Goal model as {}",
                        request.role().as_str()
                    ),
                    request.cwd().to_owned(),
                ),
            )
            .await
        });
        match approved {
            Ok(Ok(approved)) => require_approval(approved)?,
            Ok(Err(_)) => return Err(AgentError::TransportFailure),
            Err(_) => return Err(AgentError::Timeout),
        }

        run_bounded_process(&command, request.prompt().as_bytes(), None, deadline)
    })();

    match &result {
        Ok(output) => eprintln!(
            "goal_model_invocation role={} outcome=SUCCESS duration_ms={} stdout_bytes={} stderr_bytes={} exit_status={}",
            request.role().as_str(),
            started.elapsed().as_millis(),
            output.stdout.len(),
            output.safe_stderr.len(),
            output.exit_status
        ),
        Err(error) => eprintln!(
            "goal_model_invocation role={} outcome={error:?} duration_ms={}",
            request.role().as_str(),
            started.elapsed().as_millis()
        ),
    }
    result
}

fn configured_model() -> Result<String, AgentError> {
    let fallback_model = fallback::model();
    match std::env::var(GOAL_MODEL_ENV) {
        Ok(model) => configured_model_from(Some(&model), &fallback_model),
        Err(VarError::NotPresent) => configured_model_from(None, &fallback_model),
        Err(VarError::NotUnicode(_)) => Err(AgentError::InvalidConfiguration),
    }
}

fn configured_model_from(
    goal_model: Option<&str>,
    fallback_model: &str,
) -> Result<String, AgentError> {
    let model = goal_model.unwrap_or(fallback_model);
    if model.trim().is_empty() || model.len() > 256 || model.chars().any(char::is_control) {
        return Err(AgentError::InvalidConfiguration);
    }
    Ok(model.to_owned())
}

fn require_approval(approved: bool) -> Result<(), AgentError> {
    if approved {
        Ok(())
    } else {
        Err(AgentError::ApprovalDenied)
    }
}

#[derive(Debug)]
enum CaptureError {
    TooLarge,
    Io,
}

async fn capture_bounded<R>(mut reader: R, limit: usize) -> Result<Vec<u8>, CaptureError>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return Ok(output),
            Ok(read) if output.len().saturating_add(read) > limit => {
                return Err(CaptureError::TooLarge);
            }
            Ok(read) => output.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Err(CaptureError::Io),
        }
    }
}

async fn write_stdin(mut stdin: ChildStdin, prompt: Vec<u8>) -> io::Result<()> {
    stdin.write_all(&prompt).await
}

fn capture_error_to_agent(error: CaptureError) -> AgentError {
    match error {
        CaptureError::TooLarge => AgentError::ResponseTooLarge,
        CaptureError::Io => AgentError::TransportFailure,
    }
}

struct AbortOnDrop<T> {
    handle: Option<JoinHandle<T>>,
}

impl<T> AbortOnDrop<T> {
    fn new(handle: JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    fn is_finished(&self) -> bool {
        self.handle.as_ref().is_some_and(JoinHandle::is_finished)
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

#[cfg(unix)]
fn kill_child(child: &mut Child, process_group_id: u32) {
    if process_group_id != 0 {
        let _ = unsafe { libc::kill(-(process_group_id as libc::pid_t), libc::SIGKILL) };
    }
    let _ = child.start_kill();
}

#[cfg(not(unix))]
fn kill_child(child: &mut Child, _process_group_id: u32) {
    let _ = child.start_kill();
}

async fn abort_and_join<T>(task: &mut Option<AbortOnDrop<T>>) {
    if let Some(task) = task.as_mut() {
        task.abort_and_join().await;
    }
    task.take();
}

async fn cleanup_async_process(
    child: &mut Child,
    process_group_id: u32,
    stdin_task: &mut Option<AbortOnDrop<io::Result<()>>>,
    stdout_task: &mut Option<AbortOnDrop<Result<Vec<u8>, CaptureError>>>,
    stderr_task: &mut Option<AbortOnDrop<Result<Vec<u8>, CaptureError>>>,
    cleanup_deadline: Instant,
) {
    kill_child(child, process_group_id);
    let _ = tokio::time::timeout_at(
        tokio::time::Instant::from_std(cleanup_deadline),
        child.wait(),
    )
    .await;
    kill_child(child, process_group_id);
    abort_and_join(stdin_task).await;
    abort_and_join(stdout_task).await;
    abort_and_join(stderr_task).await;
}

fn safe_diagnostic(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                '?'
            } else {
                character
            }
        })
        .collect()
}

struct RawProcessOutput {
    success: bool,
    exit_status: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn raw_output_to_sandbox(output: RawProcessOutput) -> sandbox::Output {
    sandbox::Output {
        status: output.exit_status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: safe_diagnostic(&output.stderr),
        // The host spawned this process directly, so its start is host-proven.
        command_start: sandbox::CommandStart::Proven,
    }
}

async fn run_bounded_process_async(
    command: &[String],
    prompt: &[u8],
    cwd: Option<&Path>,
    deadline: Instant,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<RawProcessOutput, AgentError> {
    let cleanup_deadline = deadline + PROCESS_CLEANUP_GRACE;
    let (program, arguments) = command
        .split_first()
        .ok_or(AgentError::InvalidConfiguration)?;
    let cwd = cwd
        .map(|path| std::fs::canonicalize(path).map_err(|_| AgentError::SpawnFailed))
        .transpose()?;
    let mut process = Command::new(program);
    process
        .kill_on_drop(true)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd.as_deref() {
        process.current_dir(cwd);
    }
    #[cfg(unix)]
    process.process_group(0);
    let mut child = process.spawn().map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            AgentError::ExecutableUnavailable
        } else {
            AgentError::SpawnFailed
        }
    })?;
    let process_group_id = child.id().unwrap_or(0);
    #[cfg(unix)]
    let _process_group_guard = UnixProcessGroupGuard::new(process_group_id);
    let Some(stdin) = child.stdin.take() else {
        kill_child(&mut child, process_group_id);
        let _ = tokio::time::timeout_at(
            tokio::time::Instant::from_std(cleanup_deadline),
            child.wait(),
        )
        .await;
        return Err(AgentError::SpawnFailed);
    };
    let Some(stdout) = child.stdout.take() else {
        kill_child(&mut child, process_group_id);
        let _ = tokio::time::timeout_at(
            tokio::time::Instant::from_std(cleanup_deadline),
            child.wait(),
        )
        .await;
        return Err(AgentError::SpawnFailed);
    };
    let Some(stderr) = child.stderr.take() else {
        kill_child(&mut child, process_group_id);
        let _ = tokio::time::timeout_at(
            tokio::time::Instant::from_std(cleanup_deadline),
            child.wait(),
        )
        .await;
        return Err(AgentError::SpawnFailed);
    };
    let mut stdin_task = Some(AbortOnDrop::new(tokio::spawn(write_stdin(
        stdin,
        prompt.to_vec(),
    ))));
    let mut stdout_task = Some(AbortOnDrop::new(tokio::spawn(capture_bounded(
        stdout,
        stdout_limit,
    ))));
    let mut stderr_task = Some(AbortOnDrop::new(tokio::spawn(capture_bounded(
        stderr,
        stderr_limit,
    ))));
    let mut status = None;
    let mut stdin_result = None;
    let mut stdout_result = None;
    let mut stderr_result = None;
    let mut failure = None;
    let timer = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
    tokio::pin!(timer);
    loop {
        if status.is_some()
            && stdin_task.is_none()
            && stdout_task.is_none()
            && stderr_task.is_none()
        {
            break;
        }
        tokio::select! {
            result = child.wait(), if status.is_none() => {
                match result {
                    Ok(exit_status) => {
                        status = Some(exit_status);
                        if stdin_task
                            .as_ref()
                            .is_some_and(|task| !task.is_finished())
                        {
                            failure = Some(AgentError::Timeout);
                            break;
                        }
                    }
                    Err(_) => {
                        failure = Some(AgentError::TransportFailure);
                        break;
                    }
                }
            }
            result = async {
                stdin_task
                    .as_mut()
                    .expect("stdin task missing")
                    .await
            }, if stdin_task.is_some() => {
                stdin_task = None;
                match result {
                    Ok(Ok(())) => stdin_result = Some(()),
                    Ok(Err(error)) if error.kind() == ErrorKind::BrokenPipe => {
                        stdin_result = Some(());
                    }
                    Ok(Err(_)) | Err(_) => {
                        failure = Some(AgentError::TransportFailure);
                        break;
                    }
                }
            }
            result = async {
                stdout_task
                    .as_mut()
                    .expect("stdout task missing")
                    .await
            }, if stdout_task.is_some() => {
                stdout_task = None;
                match result {
                    Ok(Ok(output)) => stdout_result = Some(Ok(output)),
                    Ok(Err(error)) => {
                        failure = Some(capture_error_to_agent(error));
                        break;
                    }
                    Err(_) => {
                        failure = Some(AgentError::TransportFailure);
                        break;
                    }
                }
            }
            result = async {
                stderr_task
                    .as_mut()
                    .expect("stderr task missing")
                    .await
            }, if stderr_task.is_some() => {
                stderr_task = None;
                match result {
                    Ok(Ok(output)) => stderr_result = Some(Ok(output)),
                    Ok(Err(error)) => {
                        failure = Some(capture_error_to_agent(error));
                        break;
                    }
                    Err(_) => {
                        failure = Some(AgentError::TransportFailure);
                        break;
                    }
                }
            }
            _ = &mut timer => {
                failure = Some(AgentError::Timeout);
                break;
            }
        }
    }
    if let Some(error) = failure {
        cleanup_async_process(
            &mut child,
            process_group_id,
            &mut stdin_task,
            &mut stdout_task,
            &mut stderr_task,
            cleanup_deadline,
        )
        .await;
        return Err(error);
    }
    let status = status.ok_or(AgentError::TransportFailure)?;
    if stdin_result.is_none() {
        return Err(AgentError::TransportFailure);
    }
    let stdout = stdout_result
        .ok_or(AgentError::TransportFailure)?
        .map_err(capture_error_to_agent)?;
    let stderr = stderr_result
        .ok_or(AgentError::TransportFailure)?
        .map_err(capture_error_to_agent)?;
    Ok(RawProcessOutput {
        success: status.success(),
        exit_status: status.code().unwrap_or(-1),
        stdout,
        stderr,
    })
}

fn run_bounded_process(
    command: &[String],
    prompt: &[u8],
    cwd: Option<&Path>,
    deadline: Instant,
) -> Result<ModelInvocationOutput, AgentError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| AgentError::TransportFailure)?;
    let output = runtime.block_on(run_bounded_process_async(
        command,
        prompt,
        cwd,
        deadline,
        MODEL_STDOUT_LIMIT,
        MODEL_STDERR_LIMIT,
    ))?;
    if !output.success {
        return Err(AgentError::NonZeroExit);
    }
    if output.stdout.iter().all(u8::is_ascii_whitespace) {
        return Err(AgentError::EmptyResponse);
    }
    Ok(ModelInvocationOutput::new(
        output.stdout,
        safe_diagnostic(&output.stderr),
        output.exit_status,
    ))
}

pub(crate) async fn run_bounded_host_process(
    command: &[String],
    prompt: &[u8],
    cwd: &Path,
    timeout: Duration,
) -> Result<sandbox::Output, AgentError> {
    let output = run_bounded_process_async(
        command,
        prompt,
        Some(cwd),
        Instant::now() + timeout,
        MODEL_STDOUT_LIMIT,
        MODEL_STDERR_LIMIT,
    )
    .await?;
    Ok(raw_output_to_sandbox(output))
}

#[cfg(test)]
pub(crate) async fn run_bounded_process_async_for_test(
    command: &[String],
    prompt: &[u8],
    cwd: &Path,
    timeout: Duration,
) -> Result<sandbox::Output, AgentError> {
    let output = run_bounded_process_async(
        command,
        prompt,
        Some(cwd),
        Instant::now() + timeout,
        MODEL_STDOUT_LIMIT,
        MODEL_STDERR_LIMIT,
    )
    .await?;
    Ok(raw_output_to_sandbox(output))
}

#[cfg(test)]
pub(crate) fn run_bounded_process_for_test(
    command: &[String],
    prompt: &[u8],
    timeout: Duration,
) -> Result<ModelInvocationOutput, AgentError> {
    run_bounded_process(command, prompt, None, Instant::now() + timeout)
}

#[cfg(test)]
pub(crate) fn configured_model_from_for_test(
    goal_model: Option<&str>,
    fallback_model: &str,
) -> Result<String, AgentError> {
    configured_model_from(goal_model, fallback_model)
}

#[cfg(test)]
pub(crate) fn require_approval_for_test(approved: bool) -> Result<(), AgentError> {
    require_approval(approved)
}

#[cfg(all(test, unix))]
pub(crate) fn safe_diagnostic_for_test(bytes: &[u8]) -> String {
    safe_diagnostic(bytes)
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
