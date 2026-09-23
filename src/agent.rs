use std::env::VarError;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{approvals, fallback};

pub(crate) const MODEL_TIMEOUT: Duration = Duration::from_secs(120);
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

        run_bounded_process(&command, request.prompt().as_bytes(), deadline)
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

struct Capture {
    receiver: mpsc::Receiver<Result<Vec<u8>, CaptureError>>,
    handle: JoinHandle<()>,
    too_large: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
}

#[derive(Debug)]
enum CaptureError {
    TooLarge,
    Io,
}

fn start_capture<R: Read + Send + 'static>(mut reader: R, limit: usize) -> Capture {
    let (sender, receiver) = mpsc::channel();
    let too_large = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(AtomicBool::new(false));
    let too_large_for_reader = Arc::clone(&too_large);
    let failed_for_reader = Arc::clone(&failed);
    let handle = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 8192];
        let result = loop {
            match reader.read(&mut buffer) {
                Ok(0) => break Ok(output),
                Ok(read) if output.len().saturating_add(read) > limit => {
                    too_large_for_reader.store(true, Ordering::Release);
                    break Err(CaptureError::TooLarge);
                }
                Ok(read) => output.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    failed_for_reader.store(true, Ordering::Release);
                    break Err(CaptureError::Io);
                }
            }
        };
        let _ = sender.send(result);
    });
    Capture {
        receiver,
        handle,
        too_large,
        failed,
    }
}

fn finish_capture(capture: Capture) -> Result<Vec<u8>, CaptureError> {
    let result = capture.receiver.recv().unwrap_or(Err(CaptureError::Io));
    let _ = capture.handle.join();
    result
}

fn kill_and_join(child: &mut Child, stdout: Capture, stderr: Capture) {
    let _ = child.kill();
    let _ = child.wait();
    let _ = finish_capture(stdout);
    let _ = finish_capture(stderr);
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

/// Runs only the host-built read-only Codex command after approval.
///
/// The existing unrestricted helper waits for process completion and returns
/// fully buffered output, so it cannot enforce the Goal seam's hard streaming
/// stdout/stderr caps and one absolute approval+process deadline. This narrow
/// launcher therefore owns only the already-approved Codex process lifetime;
/// callers cannot supply an arbitrary command through this seam.
fn run_bounded_process(
    command: &[String],
    prompt: &[u8],
    deadline: Instant,
) -> Result<ModelInvocationOutput, AgentError> {
    let (program, arguments) = command
        .split_first()
        .ok_or(AgentError::InvalidConfiguration)?;
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                AgentError::ExecutableUnavailable
            } else {
                AgentError::SpawnFailed
            }
        })?;

    let Some(stdout_reader) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(AgentError::SpawnFailed);
    };
    let Some(stderr_reader) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(AgentError::SpawnFailed);
    };
    let stdout = start_capture(stdout_reader, MODEL_STDOUT_LIMIT);
    let stderr = start_capture(stderr_reader, MODEL_STDERR_LIMIT);
    let Some(mut stdin) = child.stdin.take() else {
        kill_and_join(&mut child, stdout, stderr);
        return Err(AgentError::SpawnFailed);
    };
    if let Err(error) = stdin.write_all(prompt) {
        // A child may exit successfully without reading stdin (for example, a
        // model shim that intentionally returns no response). On Unix that can
        // race with this write and surface as BrokenPipe. Preserve the child
        // exit/output mapping in that case instead of misclassifying it as a
        // transport failure. Other stdin I/O failures remain transport errors.
        if error.kind() != ErrorKind::BrokenPipe {
            kill_and_join(&mut child, stdout, stderr);
            return Err(AgentError::TransportFailure);
        }
    }
    drop(stdin);

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => {
                kill_and_join(&mut child, stdout, stderr);
                return Err(AgentError::TransportFailure);
            }
        }
        if stdout.too_large.load(Ordering::Acquire) || stderr.too_large.load(Ordering::Acquire) {
            kill_and_join(&mut child, stdout, stderr);
            return Err(AgentError::ResponseTooLarge);
        }
        if stdout.failed.load(Ordering::Acquire) || stderr.failed.load(Ordering::Acquire) {
            kill_and_join(&mut child, stdout, stderr);
            return Err(AgentError::TransportFailure);
        }
        if Instant::now() >= deadline {
            kill_and_join(&mut child, stdout, stderr);
            return Err(AgentError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let output = finish_capture(stdout).map_err(|error| match error {
        CaptureError::TooLarge => AgentError::ResponseTooLarge,
        CaptureError::Io => AgentError::TransportFailure,
    })?;
    let stderr = finish_capture(stderr).map_err(|error| match error {
        CaptureError::TooLarge => AgentError::ResponseTooLarge,
        CaptureError::Io => AgentError::TransportFailure,
    })?;
    if !status.success() {
        return Err(AgentError::NonZeroExit);
    }
    if output.iter().all(u8::is_ascii_whitespace) {
        return Err(AgentError::EmptyResponse);
    }
    Ok(ModelInvocationOutput::new(
        output,
        safe_diagnostic(&stderr),
        status.code().unwrap_or_default(),
    ))
}

#[cfg(test)]
pub(crate) fn run_bounded_process_for_test(
    command: &[String],
    prompt: &[u8],
    timeout: Duration,
) -> Result<ModelInvocationOutput, AgentError> {
    run_bounded_process(command, prompt, Instant::now() + timeout)
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
