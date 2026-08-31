use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{approvals, config, fallback, sandbox};

const FOREGROUND_TIMEOUT: Duration = Duration::from_secs(30);
const HOST_FOREGROUND_TIMEOUT: Duration = Duration::from_secs(20);

struct Job {
    session_id: String,
    command: String,
    handle: JoinHandle<Result<String>>,
}

fn jobs() -> &'static Mutex<HashMap<Uuid, Job>> {
    static JOBS: OnceLock<Mutex<HashMap<Uuid, Job>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub async fn serve() -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                write_message(&mut stdout, &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error.to_string()}})).await?;
                continue;
            }
        };
        if request.get("id").is_none() {
            continue;
        }
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let response = match dispatch(&request).await {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(error) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":format!("{error:#}")}})
            }
        };
        write_message(&mut stdout, &response).await?;
    }
    Ok(())
}

async fn write_message(stdout: &mut tokio::io::Stdout, message: &Value) -> Result<()> {
    stdout
        .write_all(serde_json::to_string(message)?.as_bytes())
        .await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

async fn dispatch(request: &Value) -> Result<Value> {
    match request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "local-mcp", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Every tool call requires the local-mcp session_id supplied by the user. Call session_info with that ID to inspect its working directory and sandbox roots."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => call_tool(request.get("params").unwrap_or(&Value::Null)).await,
        method => anyhow::bail!("method not found: {method}"),
    }
}

fn tools() -> Value {
    #[cfg(not(windows))]
    let write_file_description = "Write a UTF-8 file in the Codex sandbox. Relative paths use the session working directory.";
    #[cfg(windows)]
    let write_file_description = "Write a UTF-8 file directly on the Windows host without a Codex sandbox. Relative paths use the session working directory.";
    #[cfg(not(windows))]
    let execute_description = "Execute argv without a shell in the Local MCP sandbox. Returns the normal result when it finishes within 30 seconds; otherwise returns a job_id for use with poll_job or stop_job. Network is disabled and approval is not required. Policy V2 accepted_exit_codes and structured operation metadata are additive.";
    #[cfg(windows)]
    let execute_description = "Execute argv without a shell directly on the Windows host. Returns the normal result when it finishes within 30 seconds; otherwise returns a job_id for use with poll_job or stop_job. This has the user's filesystem and network access and requires approval unless the session is in yolo mode. Policy V2 metadata is accepted, but executable SANDBOX_PERMISSION fallback requires a sandboxed primary execution and does not apply to this host-native path.";
    #[cfg(not(windows))]
    let start_command_description = "Start argv immediately as a background job in the Local MCP sandbox and return a job_id without waiting for completion. Network is disabled and approval is not required. Policy V2 accepted_exit_codes and structured operation metadata are additive.";
    #[cfg(windows)]
    let start_command_description = "Start argv immediately as a background job directly on the Windows host and return a job_id without waiting for completion. This has the user's filesystem and network access and requires approval unless the session is in yolo mode. Policy V2 metadata is accepted, but executable SANDBOX_PERMISSION fallback requires a sandboxed primary execution and does not apply to this host-native path.";

    let mut tools = json!([
        {"name":"session_info","description":"Show a local-mcp session's ID, working directory, and allowed sandbox roots.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"}},"required":["session_id"],"additionalProperties":false}},
        {"name":"read_file","description":"Read a UTF-8 file from the local machine. Relative paths use the session working directory.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"path":{"type":"string"}},"required":["session_id","path"]}},
        {"name":"get_image","description":"Read a local image and return it as MCP image content. Relative paths use the session working directory.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"path":{"type":"string","description":"Path to a PNG, JPEG, GIF, WebP, BMP, TIFF, or AVIF image."}},"required":["session_id","path"],"additionalProperties":false}},
        {"name":"list_directory","description":"List entries in a local directory. Relative paths use the session working directory.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"path":{"type":"string"}},"required":["session_id","path"]}},
        {"name":"write_file","description":write_file_description,"inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"path":{"type":"string"},"content":{"type":"string"}},"required":["session_id","path","content"]}},
        {"name":"execute","description":execute_description,"inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"},"accepted_exit_codes":{"type":"array","items":{"type":"integer"},"maxItems":32},"operation":{"type":"object"},"fallback_depth":{"type":"integer","minimum":0,"maximum":1}},"required":["session_id","command"]}},
        {"name":"start_command","description":start_command_description,"inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"},"accepted_exit_codes":{"type":"array","items":{"type":"integer"},"maxItems":32},"operation":{"type":"object"},"fallback_depth":{"type":"integer","minimum":0,"maximum":1}},"required":["session_id","command"]}},
        {"name":"poll_job","description":"Poll a background command returned by execute or start_command. Returns running while active, or the command result once completed.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"job_id":{"type":"string","format":"uuid"}},"required":["session_id","job_id"],"additionalProperties":false}},
        {"name":"stop_job","description":"Stop a background command returned by execute or start_command.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"job_id":{"type":"string","format":"uuid"}},"required":["session_id","job_id"],"additionalProperties":false}},
        {"name":"codex_fallback","description":"Policy V2 explicit Codex fallback. Defaults to DIAGNOSE_ONLY and always runs Codex read-only. EXECUTE_AUTHORIZED_OPERATION additionally requires an explicit structured allowlisted operation and exact command; the host, not Codex, executes the generated exact operation and independently verifies it. Platform/safety blocks are terminal.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"task":{"type":"string","minLength":1},"blocker":{"type":"string","minLength":1},"phase":{"type":"string"},"mode":{"type":"string","enum":["DIAGNOSE_ONLY","EXECUTE_AUTHORIZED_OPERATION"],"default":"DIAGNOSE_ONLY"},"failure_class":{"type":"string","enum":["SANDBOX_PERMISSION","HOST_ENVIRONMENT","TOOL_MISSING","NETWORK_REMOTE","SEMANTIC_FAILURE","PLATFORM_SAFETY","TRANSPORT_FAILURE","UNKNOWN"]},"original_host_reached":{"type":"boolean"},"original_command_started":{"type":"boolean"},"original_command_finished":{"type":"boolean"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"operation":{"type":"object"},"fallback_depth":{"type":"integer","minimum":0,"maximum":1},"cwd":{"type":"string"},"requires_code_change":{"type":"boolean","default":false},"remote_side_effect":{"type":"string","enum":["none","not_started","unknown"],"default":"none"}},"required":["session_id","task","blocker"],"additionalProperties":false}},
        {"name":"without_sandbox","description":"Execute argv directly on the host with full user permissions and network access. Every call requires approval unless the session is in yolo mode. Returns normally when it finishes within 20 seconds; longer commands continue as a background job and return a job_id for poll_job/stop_job.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"}},"required":["session_id","command"]}}
    ]);
    let operation_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "type": {
                "type": "string",
                "enum": [
                    "read_only_command",
                    "git_stage_paths",
                    "git_create_ref",
                    "git_commit",
                    "git_push_ref",
                    "github_workflow_dispatch",
                    "other_remote_mutation",
                    "unstructured"
                ]
            },
            "authorized": {"type": "boolean", "default": false},
            "operation_id": {"type": "string", "maxLength": 128},
            "paths": {"type": "array", "items": {"type": "string"}},
            "argv": {"type": "array", "items": {"type": "string"}},
            "source": {"type": "string"},
            "destination": {"type": "string"},
            "target": {"type": "string"},
            "create_only": {"type": "boolean", "default": false},
            "force": {"type": "boolean", "default": false},
            "attempt_budget_remaining": {"type": "integer", "minimum": 0, "default": 1},
            "side_effect_budget_remaining": {"type": "integer", "minimum": 0, "default": 1},
            "side_effect_state": {
                "type": "string",
                "enum": ["CONFIRMED_NOT_PERFORMED", "CONFIRMED_PERFORMED", "UNKNOWN"]
            }
        },
        "required": ["type"]
    });
    for tool in tools.as_array_mut().unwrap() {
        if matches!(
            tool.get("name").and_then(Value::as_str),
            Some("execute" | "start_command" | "codex_fallback")
        ) && let Some(operation) = tool.pointer_mut("/inputSchema/properties/operation")
        {
            *operation = operation_schema.clone();
        }
        if let Some(session_id) = tool
            .pointer_mut("/inputSchema/properties/session_id")
            .and_then(Value::as_object_mut)
        {
            session_id.remove("format");
        }
    }
    tools
}

async fn call_tool(params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .context("missing tool name")?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let session_id = required_session_id(&args)?;
    let session = config::load_session(&session_id).await?;
    match name {
        "session_info" => {
            approvals::activity(&session.id, "Read session info", None).await;
            text_result(serde_json::to_string_pretty(&session)?)
        }
        "get_image" => {
            let path = resolve_path(&session.cwd, required_path(&args, "path")?);
            let result = get_image(&path).await;
            report_result(
                &session.id,
                format!("Read image {}", display_path(&path, &session.cwd)),
                &result,
            )
            .await;
            result
        }
        "read_file" => {
            let path = resolve_path(&session.cwd, required_path(&args, "path")?);
            let result = tokio::fs::read_to_string(&path)
                .await
                .context("failed to read file");
            report_result(
                &session.id,
                format!("Read {}", display_path(&path, &session.cwd)),
                &result,
            )
            .await;
            text_result(result?)
        }
        "list_directory" => {
            let path = resolve_path(&session.cwd, required_path(&args, "path")?);
            let result = list_directory(&path).await;
            report_result(
                &session.id,
                format!("Listed {}", display_path(&path, &session.cwd)),
                &result,
            )
            .await;
            text_result(result?)
        }
        "write_file" => write_file(&args, &session).await,
        "execute" => execute(&args, &session).await,
        "start_command" => start_command(&args, &session).await,
        "poll_job" => poll_job(&args, &session).await,
        "stop_job" => stop_job(&args, &session).await,
        "codex_fallback" => codex_fallback(&args, &session).await,
        "without_sandbox" => without_sandbox(&args, &session).await,
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}

async fn report_result<T>(session_id: &str, title: String, result: &Result<T>) {
    let detail = result
        .as_ref()
        .err()
        .map(|error| format!("└ Error: {error:#}"));
    approvals::activity(session_id, title, detail).await;
}

fn display_path<'a>(path: &'a Path, session_cwd: &Path) -> std::borrow::Cow<'a, str> {
    path.strip_prefix(session_cwd)
        .unwrap_or(path)
        .to_string_lossy()
}

fn text_result(text: String) -> Result<Value> {
    Ok(json!({"content":[{"type":"text","text":text}]}))
}

async fn get_image(path: &Path) -> Result<Value> {
    let path = tokio::fs::canonicalize(&path)
        .await
        .with_context(|| format!("cannot resolve image {}", path.display()))?;
    let metadata = tokio::fs::metadata(&path).await?;
    anyhow::ensure!(
        metadata.is_file(),
        "image path is not a file: {}",
        path.display()
    );
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("cannot read image {}", path.display()))?;
    let mime_type = image_mime_type(&bytes)
        .with_context(|| format!("unsupported image format: {}", path.display()))?;
    Ok(json!({
        "content": [{
            "type": "image",
            "data": STANDARD.encode(bytes),
            "mimeType": mime_type
        }]
    }))
}

fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        Some("image/tiff")
    } else if bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && matches!(&bytes[8..12], b"avif" | b"avis")
    {
        Some("image/avif")
    } else {
        None
    }
}

fn required_path(args: &Value, name: &str) -> Result<PathBuf> {
    args.get(name)
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .context(format!("missing {name}"))
}

fn required_session_id(args: &Value) -> Result<String> {
    let value = args
        .get("session_id")
        .and_then(Value::as_str)
        .context("missing session_id; ask the user to run `local-mcp start` and provide its ID")?;
    config::validate_session_id(value)?;
    Ok(value.to_owned())
}

fn resolve_path(session_cwd: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        session_cwd.join(path)
    }
}

fn cwd(args: &Value, session_cwd: &Path) -> Result<PathBuf> {
    let path = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .map(|path| resolve_path(session_cwd, path))
        .unwrap_or_else(|| session_cwd.to_owned());
    std::fs::canonicalize(&path).with_context(|| format!("cannot resolve cwd {}", path.display()))
}

async fn list_directory(path: &Path) -> Result<String> {
    let mut entries = tokio::fs::read_dir(path).await?;
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let suffix = if entry.file_type().await?.is_dir() {
            "/"
        } else {
            ""
        };
        names.push(format!("{}{}", entry.file_name().to_string_lossy(), suffix));
    }
    names.sort();
    Ok(names.join("\n"))
}

async fn write_file(args: &Value, session: &config::Session) -> Result<Value> {
    let absolute = resolve_path(&session.cwd, required_path(args, "path")?);
    let parent = absolute.parent().context("file has no parent directory")?;
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("parent does not exist: {}", parent.display()))?;
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .context("missing content")?;
    let previous = tokio::fs::read_to_string(&absolute)
        .await
        .unwrap_or_default();
    #[cfg(unix)]
    let output = {
        let command = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "cat > \"$1\"".to_owned(),
            "local-mcp-write".to_owned(),
            absolute.display().to_string(),
        ];
        sandbox::run(
            &command,
            &parent,
            std::slice::from_ref(&parent),
            Some(content.as_bytes()),
        )
        .await?
    };
    #[cfg(windows)]
    let output = {
        // Windows has no application sandbox here, so avoid depending on a
        // shell utility for the file-edit operation.
        tokio::fs::write(&absolute, content).await?;
        sandbox::Output {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    };
    let result = render_output(output);
    let (added, removed, diff) = render_diff(&previous, content);
    let title = format!(
        "Edited {} (+{added} -{removed})",
        display_path(&absolute, &session.cwd)
    );
    let detail = match &result {
        Ok(_) => (!diff.is_empty()).then_some(diff),
        Err(error) => Some(format!("└ Error: {error:#}")),
    };
    approvals::activity(&session.id, title, detail).await;
    text_result(result?)
}

async fn execute(args: &Value, session: &config::Session) -> Result<Value> {
    let (rendered_command, mut handle) = spawn_sandboxed_command("execute", args, session).await?;

    match tokio::time::timeout(FOREGROUND_TIMEOUT, &mut handle).await {
        Ok(joined) => text_result(joined.context("command task failed")??),
        Err(_) => store_job(session, rendered_command, handle, "Backgrounded").await,
    }
}

async fn start_command(args: &Value, session: &config::Session) -> Result<Value> {
    let (rendered_command, handle) =
        spawn_sandboxed_command("start_command", args, session).await?;
    store_job(session, rendered_command, handle, "Started").await
}

#[derive(Clone)]
struct ExecutionPolicy {
    request_id: String,
    accepted_exit_codes: Vec<i32>,
    operation: Option<fallback::OperationContract>,
    fallback_depth: u8,
    primary_execution_mode: fallback::PrimaryExecutionMode,
}

fn primary_execution_mode() -> fallback::PrimaryExecutionMode {
    #[cfg(windows)]
    {
        fallback::PrimaryExecutionMode::HostNative
    }
    #[cfg(not(windows))]
    {
        fallback::PrimaryExecutionMode::Sandboxed
    }
}

fn primary_execution_requires_approval(mode: fallback::PrimaryExecutionMode) -> bool {
    mode == fallback::PrimaryExecutionMode::HostNative
}

fn ensure_primary_execution_authorized(
    mode: fallback::PrimaryExecutionMode,
    approved: bool,
    operation: &str,
) -> Result<()> {
    if primary_execution_requires_approval(mode) && !approved {
        anyhow::bail!("user denied {operation}");
    }
    Ok(())
}

fn execution_policy(args: &Value) -> Result<ExecutionPolicy> {
    let fallback_depth = args
        .get("fallback_depth")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    anyhow::ensure!(
        fallback_depth <= fallback::max_depth() as u64,
        "fallback_depth exceeds configured maximum"
    );
    Ok(ExecutionPolicy {
        request_id: Uuid::new_v4().to_string(),
        accepted_exit_codes: fallback::accepted_exit_codes(args.get("accepted_exit_codes"))?,
        operation: fallback::operation_from_value(args.get("operation"))?,
        fallback_depth: fallback_depth as u8,
        primary_execution_mode: primary_execution_mode(),
    })
}

async fn spawn_sandboxed_command(
    operation: &str,
    args: &Value,
    session: &config::Session,
) -> Result<(String, JoinHandle<Result<String>>)> {
    let command = required_command(args)?;
    let cwd = cwd(args, &session.cwd)?;
    let policy = execution_policy(args)?;
    if primary_execution_requires_approval(policy.primary_execution_mode) {
        let approved = approvals::request(
            &session.id,
            operation,
            format!("argv: {command:?}"),
            cwd.clone(),
        )
        .await?;
        ensure_primary_execution_authorized(policy.primary_execution_mode, approved, operation)?;
    }
    let mut roots = session.permitted_directories.clone();
    if !roots.iter().any(|root| cwd.starts_with(root)) {
        roots.push(cwd.clone());
    }

    let pre_index_snapshot =
        capture_index_snapshot_if_needed(&command, &cwd, policy.operation.as_ref()).await;

    let rendered_command = render_command(&command);
    approvals::activity(
        &session.id,
        format!("Running {rendered_command}"),
        Some(format!("└ request_id={}", policy.request_id)),
    )
    .await;
    let session_id = session.id.clone();
    let task_command = rendered_command.clone();
    let handle = tokio::spawn(async move {
        let attempt = sandbox::run_tracked(&command, &cwd, &roots, None).await;
        let result = process_sandboxed_attempt(
            &session_id,
            &command,
            &cwd,
            &policy,
            pre_index_snapshot,
            attempt,
        )
        .await;
        report_command_finished(session_id, &task_command, &result).await;
        result
    });
    Ok((rendered_command, handle))
}

async fn process_sandboxed_attempt(
    session_id: &str,
    command: &[String],
    cwd: &Path,
    policy: &ExecutionPolicy,
    pre_index_snapshot: Option<String>,
    attempt: std::result::Result<sandbox::Output, sandbox::RunError>,
) -> Result<String> {
    let (lifecycle, exit_code, stdout, stderr, execution_error) = match attempt {
        Ok(output) => (
            fallback::LifecycleEvidence::completed(),
            Some(output.status),
            output.stdout,
            output.stderr,
            None,
        ),
        Err(error) => (
            fallback::LifecycleEvidence {
                host_reached: true,
                command_started: error.command_started,
                command_finished: error.command_finished,
            },
            None,
            String::new(),
            String::new(),
            Some(format!("{:#}", error.error)),
        ),
    };

    let side_effect_class = fallback::infer_side_effect_class(command, policy.operation.as_ref());
    let classification = fallback::classify(fallback::ClassificationInput {
        command,
        accepted_exit_codes: &policy.accepted_exit_codes,
        primary_execution_mode: policy.primary_execution_mode,
        lifecycle,
        exit_code,
        stdout: &stdout,
        stderr: &stderr,
        execution_error: execution_error.as_deref(),
        side_effect_class,
        authoritative_platform_safety: false,
    });

    let local_not_performed_proof = if policy
        .operation
        .as_ref()
        .is_some_and(|operation| operation.kind == fallback::OperationType::GitStagePaths)
        && classification.failure_class != fallback::FailureClass::Success
    {
        match (
            pre_index_snapshot.as_ref(),
            capture_git_index_snapshot(command, cwd).await.as_ref(),
        ) {
            (Some(before), Some(after)) => Some(before == after),
            _ => None,
        }
    } else {
        None
    };

    let side_effect_state = fallback::infer_side_effect_state(
        side_effect_class,
        lifecycle,
        classification.failure_class,
        local_not_performed_proof,
    );
    let budget_before = fallback::Budget::from_operation(policy.operation.as_ref());
    let budget_after_primary = budget_before.after_state(side_effect_state);
    let scope_valid = policy
        .operation
        .as_ref()
        .is_some_and(|operation| operation.scope_matches(command));
    let decision = fallback::decide(fallback::DecisionInput {
        failure_class: classification.failure_class,
        safety_signal: classification.safety_signal,
        primary_execution_mode: policy.primary_execution_mode,
        lifecycle,
        operation: policy.operation.as_ref(),
        side_effect_class,
        side_effect_state,
        fallback_depth: policy.fallback_depth,
        max_depth: fallback::max_depth(),
        budget: budget_after_primary,
        scope_valid,
        automatic_enabled: fallback::automatic_enabled(),
        auto_execute_enabled: fallback::auto_execute_enabled(),
    });

    emit_fallback_trace(
        session_id,
        policy,
        lifecycle,
        exit_code,
        classification.failure_class,
        side_effect_class,
        side_effect_state,
        &decision,
        budget_before,
        budget_after_primary,
        None,
    )
    .await;

    if decision.action == fallback::FallbackAction::Execute {
        let operation = policy
            .operation
            .as_ref()
            .context("execute fallback missing operation contract")?;
        match execute_authorized_operation_fallback(
            session_id,
            command,
            cwd,
            policy,
            operation,
            pre_index_snapshot.as_deref(),
            budget_after_primary,
        )
        .await
        {
            Ok((fallback_output, budget_after_fallback, verification_passed)) => {
                emit_fallback_trace(
                    session_id,
                    policy,
                    lifecycle,
                    Some(fallback_output.status),
                    classification.failure_class,
                    side_effect_class,
                    fallback::SideEffectState::ConfirmedPerformed,
                    &decision,
                    budget_before,
                    budget_after_fallback,
                    Some(verification_passed),
                )
                .await;
                return Ok(execution_payload(
                    policy,
                    lifecycle,
                    Some(fallback_output.status),
                    &fallback_output.stdout,
                    &fallback_output.stderr,
                    None,
                    classification.failure_class,
                    side_effect_class,
                    fallback::SideEffectState::ConfirmedPerformed,
                    &decision,
                    budget_after_fallback,
                    Some(verification_passed),
                    None,
                ));
            }
            Err(fallback_error) => {
                emit_fallback_trace(
                    session_id,
                    policy,
                    lifecycle,
                    exit_code,
                    classification.failure_class,
                    side_effect_class,
                    side_effect_state,
                    &decision,
                    budget_before,
                    budget_after_primary,
                    Some(false),
                )
                .await;
                let payload = execution_payload(
                    policy,
                    lifecycle,
                    exit_code,
                    &stdout,
                    &stderr,
                    execution_error.as_deref(),
                    classification.failure_class,
                    side_effect_class,
                    side_effect_state,
                    &decision,
                    budget_after_primary,
                    Some(false),
                    Some(&format!("{fallback_error:#}")),
                );
                anyhow::bail!(payload);
            }
        }
    }

    let payload = execution_payload(
        policy,
        lifecycle,
        exit_code,
        &stdout,
        &stderr,
        execution_error.as_deref(),
        classification.failure_class,
        side_effect_class,
        side_effect_state,
        &decision,
        budget_after_primary,
        None,
        None,
    );
    if matches!(
        classification.failure_class,
        fallback::FailureClass::Success | fallback::FailureClass::ExpectedState
    ) {
        Ok(payload)
    } else {
        anyhow::bail!(payload)
    }
}

async fn capture_index_snapshot_if_needed(
    command: &[String],
    cwd: &Path,
    operation: Option<&fallback::OperationContract>,
) -> Option<String> {
    if operation.is_some_and(|operation| {
        operation.kind == fallback::OperationType::GitStagePaths && operation.scope_matches(command)
    }) {
        capture_git_index_snapshot(command, cwd).await
    } else {
        None
    }
}

async fn capture_git_index_snapshot(command: &[String], cwd: &Path) -> Option<String> {
    let git = command.first()?.clone();
    let output = sandbox::run_unrestricted(
        &[git, "ls-files".into(), "--stage".into(), "-z".into()],
        cwd,
        None,
    )
    .await
    .ok()?;
    (output.status == 0).then_some(output.stdout)
}

async fn execute_authorized_operation_fallback(
    session_id: &str,
    original_command: &[String],
    cwd: &Path,
    policy: &ExecutionPolicy,
    operation: &fallback::OperationContract,
    pre_index_snapshot: Option<&str>,
    budget_before_fallback: fallback::Budget,
) -> Result<(sandbox::Output, fallback::Budget, bool)> {
    anyhow::ensure!(
        policy.fallback_depth == 0,
        "fallback recursion is not allowed"
    );
    anyhow::ensure!(
        operation.authorized,
        "operation is not explicitly authorized"
    );
    anyhow::ensure!(
        operation.auto_execute_allowlisted(),
        "operation is not executable-fallback allowlisted"
    );
    anyhow::ensure!(
        operation.scope_matches(original_command),
        "operation scope drift detected before fallback"
    );
    anyhow::ensure!(
        !budget_before_fallback.locked
            && budget_before_fallback.attempt_remaining > 0
            && budget_before_fallback.side_effect_remaining > 0,
        "fallback budget is exhausted or locked"
    );

    if !approvals::request(
        session_id,
        "codex_fallback_v2_execute",
        format!(
            "request_id={} mode=EXECUTE_AUTHORIZED_OPERATION operation={} paths={} budget={}/{}",
            policy.request_id,
            operation.kind.as_str(),
            operation.paths.len(),
            budget_before_fallback.attempt_remaining,
            budget_before_fallback.side_effect_remaining
        ),
        cwd.to_owned(),
    )
    .await?
    {
        anyhow::bail!("user denied executable Codex fallback");
    }

    // Codex is intentionally read-only in Policy V2. It cannot broaden the
    // mutation. The host executes only the exact structured operation below.
    let codex_command = fallback::codex_read_only_command(cwd, fallback::Effort::Low)?;
    let preflight_prompt = fallback::execute_preflight_prompt(cwd, &policy.request_id, operation);
    let codex_output =
        sandbox::run_unrestricted(&codex_command, cwd, Some(preflight_prompt.as_bytes())).await?;
    anyhow::ensure!(
        codex_output.status == 0,
        "Codex read-only preflight failed: exit={} stderr={}",
        codex_output.status,
        codex_output.stderr
    );

    let exact_command = operation.build_exact_command(original_command)?;
    let output = sandbox::run_unrestricted(&exact_command, cwd, None).await?;

    let verification_passed = match operation.kind {
        fallback::OperationType::GitStagePaths => {
            if output.status != 0 {
                false
            } else {
                let before =
                    pre_index_snapshot.context("missing pre-fallback Git index snapshot")?;
                let after = capture_git_index_snapshot(original_command, cwd)
                    .await
                    .context("failed to capture post-fallback Git index snapshot")?;
                fallback::verify_index_change_scope(before, &after, &operation.paths)
            }
        }
        fallback::OperationType::ReadOnlyCommand => {
            policy.accepted_exit_codes.contains(&output.status)
        }
        _ => false,
    };

    anyhow::ensure!(
        verification_passed,
        "post-fallback verification did not confirm the exact authorized postcondition"
    );

    let budget_after = if operation.side_effect_class() == fallback::SideEffectClass::None {
        fallback::Budget {
            attempt_remaining: budget_before_fallback.attempt_remaining.saturating_sub(1),
            side_effect_remaining: budget_before_fallback.side_effect_remaining,
            locked: false,
        }
    } else {
        budget_before_fallback.consume_fallback()
    };

    Ok((output, budget_after, true))
}

#[allow(clippy::too_many_arguments)]
fn execution_payload(
    policy: &ExecutionPolicy,
    lifecycle: fallback::LifecycleEvidence,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    execution_error: Option<&str>,
    failure_class: fallback::FailureClass,
    side_effect_class: fallback::SideEffectClass,
    side_effect_state: fallback::SideEffectState,
    decision: &fallback::FallbackDecision,
    budget_after: fallback::Budget,
    verification_passed: Option<bool>,
    fallback_error: Option<&str>,
) -> String {
    json!({
        "request_id": policy.request_id,
        "operation_type": policy.operation.as_ref().map(|operation| operation.kind.as_str()).unwrap_or("unstructured"),
        "primary_execution_mode": policy.primary_execution_mode.as_str(),
        "host_reached": lifecycle.host_reached,
        "command_started": lifecycle.command_started,
        "command_finished": lifecycle.command_finished,
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "execution_error": execution_error,
        "failure_class": failure_class.as_str(),
        "side_effect_class": side_effect_class.as_str(),
        "side_effect_state": side_effect_state.as_str(),
        "fallback_decision": {
            "action": decision.action.as_str(),
            "reason_code": decision.reason_code.as_str(),
            "operation_authorized": decision.operation_authorized,
            "side_effect_state": decision.side_effect_state.as_str(),
            "attempt_budget_remaining": decision.attempt_budget_remaining,
            "side_effect_budget_remaining": decision.side_effect_budget_remaining,
            "budget_locked": decision.budget_locked,
            "verification_required": decision.verification_required,
        },
        "fallback_mode": decision.mode.map(|mode| mode.as_str()),
        "fallback_depth": policy.fallback_depth,
        "remaining_attempt_budget": budget_after.attempt_remaining,
        "remaining_side_effect_budget": budget_after.side_effect_remaining,
        "budget_locked": budget_after.locked,
        "verification": {
            "required": decision.verification_required,
            "passed": verification_passed,
        },
        "fallback_error": fallback_error,
    })
    .to_string()
}

#[allow(clippy::too_many_arguments)]
async fn emit_fallback_trace(
    session_id: &str,
    policy: &ExecutionPolicy,
    lifecycle: fallback::LifecycleEvidence,
    exit_code: Option<i32>,
    failure_class: fallback::FailureClass,
    side_effect_class: fallback::SideEffectClass,
    side_effect_state: fallback::SideEffectState,
    decision: &fallback::FallbackDecision,
    budget_before: fallback::Budget,
    budget_after: fallback::Budget,
    verification_passed: Option<bool>,
) {
    let trace = json!({
        "request_id": policy.request_id,
        "operation_type": policy.operation.as_ref().map(|operation| operation.kind.as_str()).unwrap_or("unstructured"),
        "primary_execution_mode": policy.primary_execution_mode.as_str(),
        "host_reached": lifecycle.host_reached,
        "command_started": lifecycle.command_started,
        "command_finished": lifecycle.command_finished,
        "exit_code": exit_code,
        "failure_class": failure_class.as_str(),
        "side_effect_class": side_effect_class.as_str(),
        "side_effect_state": side_effect_state.as_str(),
        "fallback_action": decision.action.as_str(),
        "reason_code": decision.reason_code.as_str(),
        "fallback_depth": policy.fallback_depth,
        "budget_before": {
            "attempt": budget_before.attempt_remaining,
            "side_effect": budget_before.side_effect_remaining,
            "locked": budget_before.locked,
        },
        "budget_after": {
            "attempt": budget_after.attempt_remaining,
            "side_effect": budget_after.side_effect_remaining,
            "locked": budget_after.locked,
        },
        "verification_passed": verification_passed,
    });
    approvals::activity(
        session_id,
        format!("Fallback V2 {}", policy.request_id),
        Some(format!("└ {}", trace)),
    )
    .await;
}

async fn store_job(
    session: &config::Session,
    rendered_command: String,
    handle: JoinHandle<Result<String>>,
    activity: &str,
) -> Result<Value> {
    let job_id = Uuid::new_v4();
    jobs().lock().unwrap().insert(
        job_id,
        Job {
            session_id: session.id.clone(),
            command: rendered_command.clone(),
            handle,
        },
    );
    approvals::activity(
        &session.id,
        format!("{activity} {rendered_command}"),
        Some(format!("└ job {job_id}")),
    )
    .await;
    text_result(json!({"status":"running","job_id":job_id}).to_string())
}

async fn poll_job(args: &Value, session: &config::Session) -> Result<Value> {
    let job_id = required_job_id(args)?;
    let finished = {
        let jobs = jobs().lock().unwrap();
        let job = jobs.get(&job_id).context("unknown job_id")?;
        anyhow::ensure!(
            job.session_id == session.id,
            "job does not belong to this session"
        );
        job.handle.is_finished()
    };
    if !finished {
        return text_result(json!({"status":"running","job_id":job_id}).to_string());
    }

    let job = jobs().lock().unwrap().remove(&job_id).unwrap();
    let result = job.handle.await.context("background command task failed")?;
    text_result(result?)
}

async fn stop_job(args: &Value, session: &config::Session) -> Result<Value> {
    let job_id = required_job_id(args)?;
    let job = {
        let mut jobs = jobs().lock().unwrap();
        let job = jobs.get(&job_id).context("unknown job_id")?;
        anyhow::ensure!(
            job.session_id == session.id,
            "job does not belong to this session"
        );
        jobs.remove(&job_id).unwrap()
    };
    job.handle.abort();
    let _ = job.handle.await;
    approvals::activity(
        &session.id,
        format!("Stopped {}", job.command),
        Some(format!("└ job {job_id}")),
    )
    .await;
    text_result(json!({"status":"stopped","job_id":job_id}).to_string())
}

fn required_job_id(args: &Value) -> Result<Uuid> {
    let value = args
        .get("job_id")
        .and_then(Value::as_str)
        .context("missing job_id")?;
    Uuid::parse_str(value).context("invalid job_id")
}

fn required_command(args: &Value) -> Result<Vec<String>> {
    args.get("command")
        .and_then(Value::as_array)
        .context("missing command")?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .context("command entries must be strings")
        })
        .collect()
}

async fn codex_fallback(args: &Value, session: &config::Session) -> Result<Value> {
    let task = args
        .get("task")
        .and_then(Value::as_str)
        .context("missing task")?;
    anyhow::ensure!(task.len() <= 128 * 1024, "task is too large");
    let blocker = args
        .get("blocker")
        .and_then(Value::as_str)
        .context("missing blocker")?;
    anyhow::ensure!(blocker.len() <= 64 * 1024, "blocker is too large");
    let cwd = cwd(args, &session.cwd)?;
    let request_id = Uuid::new_v4().to_string();
    let mode = args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("DIAGNOSE_ONLY");

    let explicit_failure_class = args
        .get("failure_class")
        .and_then(Value::as_str)
        .map(parse_failure_class)
        .transpose()?;
    let original_host_reached = args
        .get("original_host_reached")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let safety_probe = fallback::classify(fallback::ClassificationInput {
        command: &[],
        accepted_exit_codes: &[0],
        primary_execution_mode: primary_execution_mode(),
        lifecycle: fallback::LifecycleEvidence {
            host_reached: original_host_reached,
            command_started: false,
            command_finished: false,
        },
        exit_code: None,
        stdout: "",
        stderr: "",
        execution_error: Some(blocker),
        side_effect_class: fallback::SideEffectClass::Unknown,
        authoritative_platform_safety: explicit_failure_class
            == Some(fallback::FailureClass::PlatformSafety),
    });

    if safety_probe.safety_signal
        || explicit_failure_class == Some(fallback::FailureClass::PlatformSafety)
    {
        anyhow::bail!("terminal platform/safety classification: Codex fallback is not permitted");
    }

    match mode {
        "DIAGNOSE_ONLY" => {
            let failure_class = explicit_failure_class.unwrap_or(safety_probe.failure_class);
            let requires_code_change = args
                .get("requires_code_change")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let effort = if requires_code_change {
                fallback::Effort::Medium
            } else {
                fallback::Effort::Low
            };
            if !approvals::request(
                &session.id,
                "codex_fallback_v2_diagnose",
                format!(
                    "request_id={} mode=DIAGNOSE_ONLY class={} model={} effort={}",
                    request_id,
                    failure_class.as_str(),
                    fallback::model(),
                    effort.as_str()
                ),
                cwd.clone(),
            )
            .await?
            {
                anyhow::bail!("user denied diagnostic Codex fallback");
            }

            let command = fallback::codex_read_only_command(&cwd, effort)?;
            let prompt = fallback::diagnose_prompt(&cwd, &request_id, failure_class, blocker);
            let session_id = session.id.clone();
            let label = format!(
                "Codex fallback diagnose {}/{}",
                fallback::model(),
                effort.as_str()
            );
            let task_label = label.clone();
            let handle = tokio::spawn(async move {
                let result = sandbox::run_unrestricted(&command, &cwd, Some(prompt.as_bytes()))
                    .await
                    .and_then(render_output);
                report_command_finished(session_id, &task_label, &result).await;
                result
            });
            store_job(session, label, handle, "Started").await
        }
        "EXECUTE_AUTHORIZED_OPERATION" => {
            let failure_class = explicit_failure_class
                .context("EXECUTE_AUTHORIZED_OPERATION requires failure_class")?;
            anyhow::ensure!(
                failure_class == fallback::FailureClass::SandboxPermission,
                "executable fallback requires SANDBOX_PERMISSION"
            );
            anyhow::ensure!(
                args.get("original_host_reached").and_then(Value::as_bool) == Some(true),
                "executable fallback requires authoritative original_host_reached=true"
            );
            anyhow::ensure!(
                args.get("original_command_started")
                    .and_then(Value::as_bool)
                    == Some(true),
                "executable fallback requires authoritative original_command_started=true"
            );

            let command = required_command(args)?;
            let operation = fallback::operation_from_value(args.get("operation"))?
                .context("EXECUTE_AUTHORIZED_OPERATION requires operation")?;
            anyhow::ensure!(
                operation.side_effect_state
                    == Some(fallback::SideEffectState::ConfirmedNotPerformed),
                "executable fallback requires CONFIRMED_NOT_PERFORMED side-effect state"
            );
            let policy = ExecutionPolicy {
                request_id,
                accepted_exit_codes: vec![0],
                operation: Some(operation.clone()),
                fallback_depth: args
                    .get("fallback_depth")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u8,
                primary_execution_mode: primary_execution_mode(),
            };
            anyhow::ensure!(
                policy.fallback_depth == 0,
                "fallback recursion is not allowed"
            );

            let lifecycle = fallback::LifecycleEvidence {
                host_reached: true,
                command_started: true,
                command_finished: args
                    .get("original_command_finished")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            let budget = fallback::Budget::from_operation(Some(&operation));
            let decision = fallback::decide(fallback::DecisionInput {
                failure_class,
                safety_signal: false,
                primary_execution_mode: policy.primary_execution_mode,
                lifecycle,
                operation: Some(&operation),
                side_effect_class: operation.side_effect_class(),
                side_effect_state: fallback::SideEffectState::ConfirmedNotPerformed,
                fallback_depth: 0,
                max_depth: fallback::max_depth(),
                budget,
                scope_valid: operation.scope_matches(&command),
                automatic_enabled: fallback::automatic_enabled(),
                auto_execute_enabled: fallback::auto_execute_enabled(),
            });
            anyhow::ensure!(
                decision.action == fallback::FallbackAction::Execute,
                "fallback policy blocked execution: {}",
                decision.reason_code.as_str()
            );

            let pre_index_snapshot =
                capture_index_snapshot_if_needed(&command, &cwd, Some(&operation)).await;
            let session_id = session.id.clone();
            let label = format!(
                "Codex fallback execute {}/{}",
                fallback::model(),
                operation.kind.as_str()
            );
            let task_label = label.clone();
            let handle = tokio::spawn(async move {
                let result = execute_authorized_operation_fallback(
                    &session_id,
                    &command,
                    &cwd,
                    &policy,
                    &operation,
                    pre_index_snapshot.as_deref(),
                    budget,
                )
                .await
                .and_then(|(output, _, verified)| {
                    anyhow::ensure!(verified, "post-fallback verification failed");
                    render_output(output)
                });
                report_command_finished(session_id, &task_label, &result).await;
                result
            });
            store_job(session, label, handle, "Started").await
        }
        other => anyhow::bail!("unsupported fallback mode: {other}"),
    }
}

fn parse_failure_class(value: &str) -> Result<fallback::FailureClass> {
    serde_json::from_value(Value::String(value.to_owned()))
        .with_context(|| format!("invalid failure_class: {value}"))
}

async fn without_sandbox(args: &Value, session: &config::Session) -> Result<Value> {
    let command = required_command(args)?;
    let cwd = cwd(args, &session.cwd)?;
    if !approvals::request(
        &session.id,
        "without_sandbox",
        format!("argv: {command:?}"),
        cwd.clone(),
    )
    .await?
    {
        anyhow::bail!("user denied without_sandbox")
    }

    let rendered_command = render_command(&command);
    approvals::activity(&session.id, format!("Running {rendered_command}"), None).await;

    let session_id = session.id.clone();
    let task_command = rendered_command.clone();
    let handle = tokio::spawn(async move {
        let result = sandbox::run_unrestricted(&command, &cwd, None)
            .await
            .and_then(render_output);
        report_command_finished(session_id, &task_command, &result).await;
        result
    });

    let mut handle = handle;
    match tokio::time::timeout(HOST_FOREGROUND_TIMEOUT, &mut handle).await {
        Ok(joined) => text_result(joined.context("command task failed")??),
        Err(_) => {
            store_job(
                session,
                rendered_command,
                handle,
                "Backgrounded host command",
            )
            .await
        }
    }
}

fn render_command(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| shell_word(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

async fn report_command_finished(session_id: String, command: &str, result: &Result<String>) {
    let detail = match result {
        Ok(text) => command_summary(text),
        Err(error) => Some(format!("└ Error: {error:#}")),
    };
    approvals::activity(&session_id, format!("Ran {command}"), detail).await;
}

fn shell_word(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:=+".contains(c))
    {
        value.to_owned()
    } else {
        format!("{:?}", value)
    }
}

fn command_summary(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    let stdout = value
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end();
    let stderr = value
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end();
    let output = if stdout.is_empty() { stderr } else { stdout };
    if output.is_empty() {
        None
    } else {
        Some(
            output
                .lines()
                .map(|line| format!("└ {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

fn render_diff(old: &str, new: &str) -> (usize, usize, String) {
    let diff = TextDiff::from_lines(old, new);
    let mut added = 0;
    let mut removed = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    let rendered = diff.unified_diff().context_radius(3).to_string();
    (added, removed, rendered.trim_end().to_owned())
}

fn render_output(output: sandbox::Output) -> Result<String> {
    let text = json!({"exit_code":output.status,"stdout":output.stdout,"stderr":output.stderr})
        .to_string();
    if output.status == 0 {
        Ok(text)
    } else {
        anyhow::bail!(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_image_types() {
        assert_eq!(image_mime_type(b"\x89PNG\r\n\x1a\n"), Some("image/png"));
        assert_eq!(image_mime_type(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(image_mime_type(b"GIF89a"), Some("image/gif"));
        assert_eq!(image_mime_type(b"RIFF\0\0\0\0WEBP"), Some("image/webp"));
        assert_eq!(image_mime_type(b"not an image"), None);
    }

    #[test]
    fn renders_edit_counts_and_unified_diff() {
        let (added, removed, diff) = render_diff("one\ntwo\n", "one\nchanged\nthree\n");

        assert_eq!((added, removed), (2, 1));
        assert!(diff.contains("-two"));
        assert!(diff.contains("+changed"));
        assert!(diff.contains("+three"));
    }

    #[test]
    fn quotes_command_arguments_for_activity_display() {
        assert_eq!(shell_word("README.md"), "README.md");
        assert_eq!(shell_word("hello world"), "\"hello world\"");
    }

    #[test]
    fn host_native_primary_execution_requires_approval() {
        assert!(primary_execution_requires_approval(
            fallback::PrimaryExecutionMode::HostNative
        ));
        assert!(!primary_execution_requires_approval(
            fallback::PrimaryExecutionMode::Sandboxed
        ));
    }

    #[test]
    fn approval_denial_stops_host_native_primary_execution() {
        let error = ensure_primary_execution_authorized(
            fallback::PrimaryExecutionMode::HostNative,
            false,
            "execute",
        )
        .unwrap_err();
        assert!(error.to_string().contains("user denied execute"));
        assert!(
            ensure_primary_execution_authorized(
                fallback::PrimaryExecutionMode::HostNative,
                true,
                "execute",
            )
            .is_ok()
        );
    }

    #[cfg(windows)]
    #[test]
    fn describes_windows_command_execution_as_approved_host_access() {
        let tools = tools();
        for name in ["execute", "start_command"] {
            let description = tools
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap()["description"]
                .as_str()
                .unwrap();
            assert!(description.contains("Windows host"));
            assert!(description.contains("requires approval"));
            assert!(description.contains("filesystem and network access"));
            assert!(description.contains("SANDBOX_PERMISSION"));
            let properties = &tools
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap()["inputSchema"]["properties"];
            assert!(properties.get("accepted_exit_codes").is_some());
            assert!(properties.get("operation").is_some());
            assert!(properties.get("fallback_depth").is_some());
        }

        let write_file_description = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "write_file")
            .unwrap()["description"]
            .as_str()
            .unwrap();
        assert!(write_file_description.contains("Windows host"));
        assert!(write_file_description.contains("without a Codex sandbox"));
    }

    #[tokio::test]
    async fn get_image_returns_mcp_image_content() {
        let path = std::env::temp_dir().join(format!("local-mcp-{}.png", uuid::Uuid::new_v4()));
        let bytes = b"\x89PNG\r\n\x1a\nexample";
        tokio::fs::write(&path, bytes).await.unwrap();

        let result = get_image(&path).await.unwrap();
        tokio::fs::remove_file(path).await.unwrap();

        assert_eq!(result["content"][0]["type"], "image");
        assert_eq!(result["content"][0]["mimeType"], "image/png");
        assert_eq!(result["content"][0]["data"], STANDARD.encode(bytes));
    }

    #[tokio::test]
    async fn initialize_dispatch_remains_compatible() {
        let result = dispatch(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        }))
        .await
        .unwrap();
        assert_eq!(result["serverInfo"]["name"], "local-mcp");
        assert_eq!(result["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn execute_schema_exposes_policy_v2_metadata_additively() {
        let tools = tools();
        let execute = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "execute")
            .unwrap();
        let properties = &execute["inputSchema"]["properties"];
        assert!(properties.get("command").is_some());
        assert!(properties.get("accepted_exit_codes").is_some());
        assert!(properties.get("operation").is_some());
        assert!(properties.get("fallback_depth").is_some());
    }

    #[tokio::test]
    async fn expected_nonzero_exit_returns_normal_structured_result() {
        let policy = ExecutionPolicy {
            request_id: "expected-state-test".into(),
            accepted_exit_codes: vec![0, 1],
            operation: None,
            fallback_depth: 0,
            primary_execution_mode: fallback::PrimaryExecutionMode::Sandboxed,
        };
        let command = vec![
            "git".into(),
            "show-ref".into(),
            "--verify".into(),
            "refs/heads/absent".into(),
        ];
        let result = process_sandboxed_attempt(
            "non-running-test-session",
            &command,
            &std::env::temp_dir(),
            &policy,
            None,
            Ok(sandbox::Output {
                status: 1,
                stdout: String::new(),
                stderr: String::new(),
            }),
        )
        .await
        .unwrap();
        let value: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(value["exit_code"], 1);
        assert_eq!(value["failure_class"], "EXPECTED_STATE");
        assert_eq!(value["fallback_decision"]["action"], "NONE");
    }

    #[tokio::test]
    async fn successful_execution_preserves_stdout_stderr_and_exit_code() {
        let policy = ExecutionPolicy {
            request_id: "success-test".into(),
            accepted_exit_codes: vec![0],
            operation: None,
            fallback_depth: 0,
            primary_execution_mode: fallback::PrimaryExecutionMode::Sandboxed,
        };
        let result = process_sandboxed_attempt(
            "non-running-test-session",
            &["/bin/echo".into(), "ok".into()],
            &std::env::temp_dir(),
            &policy,
            None,
            Ok(sandbox::Output {
                status: 0,
                stdout: "stdout".into(),
                stderr: "stderr".into(),
            }),
        )
        .await
        .unwrap();
        let value: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["stdout"], "stdout");
        assert_eq!(value["stderr"], "stderr");
        assert_eq!(value["failure_class"], "SUCCESS");
        assert_eq!(value["primary_execution_mode"], "SANDBOXED");
    }

    #[tokio::test]
    async fn semantic_nonzero_exit_is_returned_without_executable_fallback() {
        let policy = ExecutionPolicy {
            request_id: "semantic-test".into(),
            accepted_exit_codes: vec![0],
            operation: None,
            fallback_depth: 0,
            primary_execution_mode: fallback::PrimaryExecutionMode::Sandboxed,
        };
        let error = process_sandboxed_attempt(
            "non-running-test-session",
            &["dotnet".into(), "test".into()],
            &std::env::temp_dir(),
            &policy,
            None,
            Ok(sandbox::Output {
                status: 1,
                stdout: "Test Run Failed. Failed tests: 1".into(),
                stderr: String::new(),
            }),
        )
        .await
        .unwrap_err();
        let value: Value = serde_json::from_str(&error.to_string()).unwrap();
        assert_eq!(value["failure_class"], "SEMANTIC_FAILURE");
        assert_eq!(value["fallback_decision"]["action"], "NONE");
    }

    #[tokio::test]
    async fn get_image_resolves_relative_paths_from_session_cwd() {
        let directory = std::env::temp_dir().join(format!("local-mcp-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir(&directory).await.unwrap();
        let path = directory.join("image.gif");
        tokio::fs::write(&path, b"GIF89a").await.unwrap();

        let result = get_image(&resolve_path(&directory, PathBuf::from("image.gif")))
            .await
            .unwrap();
        tokio::fs::remove_dir_all(directory).await.unwrap();

        assert_eq!(result["content"][0]["mimeType"], "image/gif");
    }
}
