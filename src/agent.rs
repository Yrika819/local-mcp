use std::env::VarError;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::task::JoinHandle;

use crate::process_group::ProcessGroup;
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
    stderr_bytes_retained: usize,
    stderr_total_bytes: usize,
    stderr_truncated: bool,
}

#[allow(
    dead_code,
    reason = "Frozen model-output accessors are retained for the staged Goal Orchestrator backend interface."
)]
impl ModelInvocationOutput {
    /// Builds an output from an injected diagnostic string, as a test transport
    /// supplies.
    ///
    /// An injected transport never crossed a capture, so the host holds exactly
    /// the diagnostic it was handed and nothing was dropped: retained and total
    /// are that string's length and it is not truncated. Only
    /// [`Self::from_bounded_run`] has a raw byte count to report.
    pub(crate) fn new(stdout: Vec<u8>, safe_stderr: String, exit_status: i32) -> Self {
        Self {
            stdout,
            stderr_bytes_retained: safe_stderr.len(),
            stderr_total_bytes: safe_stderr.len(),
            stderr_truncated: false,
            safe_stderr,
            exit_status,
        }
    }

    /// Builds an output from a real bounded run, recording what the diagnostic
    /// stream's bounding did.
    ///
    /// An injected transport supplies a diagnostic string directly and never
    /// crossed a capture, so [`Self::new`] reports it as wholly retained. A real
    /// run reports the raw retained byte count, the full observed byte count, and
    /// whether the retention budget was passed, so the host can report the
    /// bounding without ever exposing the discarded bytes.
    fn from_bounded_run(
        stdout: Vec<u8>,
        stderr: BoundedDiagnosticCapture,
        exit_status: i32,
    ) -> Self {
        Self {
            stdout,
            safe_stderr: safe_diagnostic(&stderr.retained),
            exit_status,
            stderr_bytes_retained: stderr.retained.len(),
            stderr_total_bytes: stderr.total_bytes,
            stderr_truncated: stderr.truncated,
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

    pub(crate) fn stderr_bytes_retained(&self) -> usize {
        self.stderr_bytes_retained
    }

    pub(crate) fn stderr_total_bytes(&self) -> usize {
        self.stderr_total_bytes
    }

    pub(crate) fn stderr_truncated(&self) -> bool {
        self.stderr_truncated
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
            "goal_model_invocation role={} outcome=SUCCESS duration_ms={} stdout_bytes_retained={} stderr_bytes_retained={} stderr_total_bytes={} stderr_truncated={} exit_status={}",
            request.role().as_str(),
            started.elapsed().as_millis(),
            output.stdout.len(),
            output.stderr_bytes_retained(),
            output.stderr_total_bytes(),
            output.stderr_truncated(),
            output.exit_status
        ),
        // Naming the stream keeps the overflow diagnosable without changing the
        // display text that legacy durable host-limit reports are matched
        // against. Only semantic stdout can reach this verdict.
        Err(AgentError::ResponseTooLarge) => eprintln!(
            "goal_model_invocation role={} outcome=ResponseTooLarge overflow_stream=STDOUT stdout_limit_bytes={} duration_ms={}",
            request.role().as_str(),
            MODEL_STDOUT_LIMIT,
            started.elapsed().as_millis()
        ),
        Err(error) => eprintln!(
            "goal_model_invocation role={} outcome={error:?} overflow_stream=NONE duration_ms={}",
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

/// A failure of a STRICT capture.
///
/// This is reachable only from the strict semantic capture below, so `TooLarge`
/// means a stream whose bytes the host actually validates overflowed its bound.
/// The diagnostic capture cannot produce this type at all.
#[derive(Debug)]
enum CaptureError {
    TooLarge,
    Io,
}

/// A failure of a bounded DIAGNOSTIC capture.
///
/// Overflow is deliberately not representable here. A diagnostic stream that
/// passes its retention budget is truncated and the run continues, so the only
/// way this capture can fail is a real IO failure. Keeping overflow out of the
/// type is what makes `ResponseTooLarge` mean semantic stdout overflow and
/// nothing else: a noisy stderr has no path to that verdict.
#[derive(Debug)]
enum DiagnosticCaptureError {
    Io,
}

/// A bounded diagnostic stream, together with what its bounding did.
///
/// `retained` never exceeds the configured retention limit. `total_bytes`
/// counts everything the child actually produced, including the bytes that were
/// read off the pipe and discarded, and `truncated` records that the two differ.
/// The discarded bytes themselves are not retained and are not reported.
#[derive(Debug)]
struct BoundedDiagnosticCapture {
    retained: Vec<u8>,
    total_bytes: usize,
    truncated: bool,
}

/// STRICT capture: the semantic stdout stream.
///
/// stdout carries the model's response and the payload the host validates, so
/// passing `limit` is a real transport failure. The caller stops the run,
/// cleans the process up per the bounded lifecycle, and reports
/// `ResponseTooLarge`. Nothing is ever silently truncated here, because a
/// truncated proposal is not a proposal the host can validate.
///
/// This is deliberately a different function from the diagnostic capture below,
/// with a different error type, rather than one parameterized capture with a
/// policy flag. The security property is that a reader cannot tell this stream is
/// safe to truncate by looking at the code: there is no spelling of "truncate
/// stdout" to reach for, and the only capture that can report an overflow is
/// this one.
async fn capture_strict_semantic_stdout<R>(
    mut reader: R,
    limit: usize,
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
                return Err(CaptureError::TooLarge);
            }
            Ok(read) => output.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Err(CaptureError::Io),
        }
    }
}

/// DIAGNOSTIC capture: the non-authoritative stderr stream.
///
/// stderr is progress and diagnostic transport output that the host only
/// sanitizes for reporting, so `limit` is a retention budget rather than a
/// correctness bound. Bytes past the budget are counted and discarded, and the
/// read loop runs on to end of file regardless of how far over the budget it
/// is: stopping at the cap would leave a verbose child blocked on a full pipe
/// and turn its own progress output into a false timeout. Memory stays bounded
/// regardless of how much the child writes, because `retained` stops growing at
/// the cap and the read buffer is a fixed-size array.
///
/// The trade this makes is deliberate and worth stating: a child that emits
/// unbounded diagnostics no longer fails fast on a limit, it runs until the
/// process ends or the run's own deadline ends it. That is the correct verdict,
/// because such a child genuinely did not finish, and the alternative — a cap on
/// how long the host is willing to keep draining — is the same blocked-pipe
/// deadlock this function exists to prevent.
async fn capture_diagnostic_stderr_truncate_and_drain<R>(
    mut reader: R,
    limit: usize,
) -> Result<BoundedDiagnosticCapture, DiagnosticCaptureError>
where
    R: AsyncRead + Unpin,
{
    let mut retained = Vec::with_capacity(limit.min(8192));
    let mut total_bytes = 0_usize;
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                return Ok(BoundedDiagnosticCapture {
                    retained,
                    total_bytes,
                    truncated,
                });
            }
            Ok(read) => {
                // Saturating, so a child that could out-count a `usize` across a
                // long drain still reports a bounded, non-wrapping total.
                total_bytes = total_bytes.saturating_add(read);
                let room = limit.saturating_sub(retained.len());
                let keep = read.min(room);
                retained.extend_from_slice(&buffer[..keep]);
                if keep < read {
                    truncated = true;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return Err(DiagnosticCaptureError::Io),
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

/// The diagnostic capture has no overflow failure to map, so a diagnostic stream
/// can only ever contribute a genuine transport failure.
fn diagnostic_capture_error_to_agent(error: DiagnosticCaptureError) -> AgentError {
    match error {
        DiagnosticCaptureError::Io => AgentError::TransportFailure,
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

async fn abort_and_join<T>(task: &mut Option<AbortOnDrop<T>>) {
    if let Some(task) = task.as_mut() {
        task.abort_and_join().await;
    }
    task.take();
}

async fn cleanup_async_process(
    group: &mut ProcessGroup,
    stdin_task: &mut Option<AbortOnDrop<io::Result<()>>>,
    stdout_task: &mut Option<AbortOnDrop<Result<Vec<u8>, CaptureError>>>,
    stderr_task: &mut Option<AbortOnDrop<Result<BoundedDiagnosticCapture, DiagnosticCaptureError>>>,
    cleanup_deadline: Instant,
) {
    group.terminate();
    let _ = tokio::time::timeout_at(
        tokio::time::Instant::from_std(cleanup_deadline),
        group.wait_termination(),
    )
    .await;
    group.terminate();
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
    stderr: BoundedDiagnosticCapture,
}

fn raw_output_to_sandbox(output: RawProcessOutput) -> sandbox::Output {
    sandbox::Output {
        status: output.exit_status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        // Only the retained portion is exposed. The discarded remainder is never
        // reconstructed, so a verbose diagnostic stream cannot inflate a report.
        // This path does not surface the truncation flag: it feeds tool
        // execution, whose stderr is diagnostic text only, and `sandbox::Output`
        // is a serialized contract that must not change for a bounding change.
        stderr: safe_diagnostic(&output.stderr.retained),
        // The host spawned this process directly, so its start is host-proven.
        command_start: sandbox::CommandStart::Proven,
    }
}

/// Runs `command` under the bounded-process lifecycle, with one bound per stream
/// and one policy per stream.
///
/// `stdout_limit` is a strict semantic bound: passing it fails the run with
/// `ResponseTooLarge`, because the bytes on that stream are the response the
/// host validates. `stderr_limit` is a diagnostic retention budget: passing it
/// truncates what is retained and the run continues, because the bytes on that
/// stream are non-authoritative output the host only sanitizes for reporting.
/// The two are read concurrently and independently, so one stream's policy can
/// never decide the other's verdict.
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
    let mut group = ProcessGroup::spawn(&mut process).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            AgentError::ExecutableUnavailable
        } else {
            AgentError::SpawnFailed
        }
    })?;
    let Some(stdin) = group.take_stdin() else {
        group.terminate();
        let _ = tokio::time::timeout_at(
            tokio::time::Instant::from_std(cleanup_deadline),
            group.wait_termination(),
        )
        .await;
        return Err(AgentError::SpawnFailed);
    };
    let Some(stdout) = group.take_stdout() else {
        group.terminate();
        let _ = tokio::time::timeout_at(
            tokio::time::Instant::from_std(cleanup_deadline),
            group.wait_termination(),
        )
        .await;
        return Err(AgentError::SpawnFailed);
    };
    let Some(stderr) = group.take_stderr() else {
        group.terminate();
        let _ = tokio::time::timeout_at(
            tokio::time::Instant::from_std(cleanup_deadline),
            group.wait_termination(),
        )
        .await;
        return Err(AgentError::SpawnFailed);
    };
    let mut stdin_task = Some(AbortOnDrop::new(tokio::spawn(write_stdin(
        stdin,
        prompt.to_vec(),
    ))));
    // STRICT: the semantic response. Overflow is fatal and is never truncated.
    let mut stdout_task = Some(AbortOnDrop::new(tokio::spawn(
        capture_strict_semantic_stdout(stdout, stdout_limit),
    )));
    // DIAGNOSTIC: non-authoritative output. Overflow truncates and drains to end
    // of file, so it is bounded in memory but never fatal on its own.
    let mut stderr_task = Some(AbortOnDrop::new(tokio::spawn(
        capture_diagnostic_stderr_truncate_and_drain(stderr, stderr_limit),
    )));
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
            result = group.wait_termination(), if status.is_none() => {
                match result {
                    Ok(exit_status) => {
                        // A child that has exited is not a timeout, and the
                        // writer's completion is not what decides that. The
                        // prompt is still outstanding here, and the launcher's
                        // contract is that an outstanding write resolves as
                        // `Ok`, as a broken pipe, or as a real IO failure,
                        // whichever happens first. Converting the observation
                        // itself into a timeout made the verdict a function of
                        // the order in which the scheduler happened to report a
                        // child exit and a task completion, so a child that had
                        // already exited successfully was reported as a timeout
                        // purely because its exit was seen first.
                        //
                        // The run's own deadline is what still ends a run whose
                        // IO never resolves, and that is the only condition that
                        // is a timeout: a prompt still blocked because another
                        // live process holds the read end, or output a
                        // surviving descendant keeps open.
                        status = Some(exit_status);
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
                    Ok(Ok(capture)) => stderr_result = Some(Ok(capture)),
                    Ok(Err(error)) => {
                        failure = Some(diagnostic_capture_error_to_agent(error));
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
            &mut group,
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
        .map_err(diagnostic_capture_error_to_agent)?;
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
    Ok(ModelInvocationOutput::from_bounded_run(
        output.stdout,
        output.stderr,
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

#[cfg(all(test, unix))]
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
