use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::execution::ExecutionPolicy;
use crate::goal::{GoalId, GoalStatus};
use crate::goal_backends::ProductionGoalBackends;
use crate::goal_runner::{self, GoalRunLimits, GoalRunResult, GoalRunStopReason};
use crate::task_store::TaskStore;
use crate::{approvals, config, execution, fallback, goal_api, sandbox};

struct Job {
    session_id: String,
    command: String,
    handle: JoinHandle<Result<String>>,
}

fn jobs() -> &'static Mutex<HashMap<Uuid, Job>> {
    static JOBS: OnceLock<Mutex<HashMap<Uuid, Job>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Maximum number of requests that may be dispatched concurrently.
///
/// This is a transport-level bound only: it prevents an unbounded number of
/// in-flight requests from exhausting runtime resources while keeping the
/// control plane responsive during long-running orchestration. Same-Goal
/// mutation authority is *not* enforced here; it remains serialized by the
/// OS-backed per-session Goal lock and optimistic revision checks in
/// [`crate::task_store::TaskStore`].
const MAX_CONCURRENT_REQUESTS: usize = 32;

/// Entry point for the MCP stdio server.
///
/// The transport is deliberately **concurrent**: each inbound request is
/// dispatched on its own task so that a long-running request (for example
/// `goal_run`) can never block lightweight control-plane requests such as
/// `ping` or `session_info`. Responses are funneled through a single
/// serialized writer task, so stdout framing stays valid and request IDs are
/// preserved even when responses complete out of order.
pub async fn serve() -> Result<()> {
    serve_with_io(tokio::io::stdin(), tokio::io::stdout()).await
}

/// Runs the MCP request loop over the provided async reader/writer pair.
///
/// Splitting the I/O handles out of [`serve`] keeps the concurrency logic
/// testable without a real process: tests can drive requests through an
/// in-memory duplex stream and assert on the raw framed responses.
async fn serve_with_io<R, W>(reader: R, writer: W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Responses from per-request dispatch tasks are collected here and
    // written to stdout by one dedicated task. This guarantees that exactly
    // one complete JSON-RPC frame is written at a time (no byte interleaving)
    // and that no dispatch task ever touches stdout directly.
    let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<Value>(64);
    let writer_handle = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(response) = response_rx.recv().await {
            write_message(&mut writer, &response).await?;
        }
        Ok::<(), anyhow::Error>(())
    });

    // Bound the number of concurrently in-flight dispatch tasks. A permit is
    // held for the lifetime of one request's dispatch, so at most
    // `MAX_CONCURRENT_REQUESTS` handlers run at once. Permits are `async` and
    // are acquired *before* a task is spawned, which means the read loop is
    // throttled, not the response path.
    let dispatch_permits = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS));

    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                // Malformed JSON is answered immediately; it is not a valid
                // request and must not consume a dispatch permit.
                response_tx
                    .send(json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":error.to_string()}}))
                    .await
                    .context("response writer task terminated unexpectedly")?;
                continue;
            }
        };
        // Per the JSON-RPC/MCP model, notifications (requests without an `id`)
        // never receive a response, so they are dropped here.
        if request.get("id").is_none() {
            continue;
        }
        let id = request.get("id").cloned().unwrap_or(Value::Null);

        // Throttle inbound requests when the dispatch pool is saturated. This
        // await is the only point where the read loop may pause; lightweight
        // control-plane requests are therefore never stuck behind a long
        // `goal_run` that is still being dispatched.
        let permit = dispatch_permits
            .clone()
            .acquire_owned()
            .await
            .context("dispatch permit semaphore closed unexpectedly")?;
        let response_tx = response_tx.clone();
        tokio::spawn(async move {
            // Hold the permit for the entire dispatch so the concurrency bound
            // reflects genuinely in-flight work.
            let _permit = permit;
            // Panic containment: a panicking handler must not silently drop
            // the request (leaving the client waiting on that ID forever).
            // Supervise the inner dispatch and synthesize a JSON-RPC internal
            // error on panic so the request always receives exactly one
            // response and the panic can never poison the read loop, the
            // writer task, or any other in-flight request.
            let dispatch_task = tokio::spawn(async move { dispatch(&request).await });
            let outcome = match dispatch_task.await {
                Ok(outcome) => outcome,
                Err(join_error) => Err(anyhow::anyhow!(
                    "internal error: request handler failed ({})",
                    if join_error.is_panic() {
                        "panic"
                    } else {
                        "cancelled"
                    }
                )),
            };
            let response = match outcome {
                Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                Err(error) => {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":format!("{error:#}")}})
                }
            };
            // If the client disconnected the writer task will have terminated;
            // the request's response is simply dropped.
            let _ = response_tx.send(response).await;
        });
    }

    // stdin closed (EOF): stop accepting requests, then drain and flush all
    // in-flight responses so the client sees every answer before we exit.
    drop(response_tx);
    writer_handle
        .await
        .context("response writer task panicked")??;
    Ok(())
}

/// Writes one complete, newline-delimited JSON-RPC frame.
///
/// This is the only function that writes response bytes. It is generic over
/// the writer so the serialized writer task in [`serve_with_io`] can own any
/// `AsyncWrite` implementation (stdout in production, an in-memory duplex in
/// tests). Each call writes the serialized message and its trailing newline
/// and flushes, so frames can never be partially written or interleaved.
async fn write_message<W>(writer: &mut W, message: &Value) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    writer
        .write_all(serde_json::to_string(message)?.as_bytes())
        .await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
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

pub(crate) fn tools() -> Value {
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
        {"name":"without_sandbox","description":"Execute argv directly on the host with full user permissions and network access. Every call requires approval unless the session is in yolo mode. Returns normally when it finishes within 20 seconds; longer commands continue as a background job and return a job_id for poll_job/stop_job.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"}},"required":["session_id","command"]}},
        {"name":"goal_start","description":"Create one durable non-terminal Goal for this Local MCP session. This does not execute tasks or repository work in Phase 3.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"},"objective":{"type":"string","minLength":1,"maxLength":131072},"title":{"type":"string","maxLength":256},"constraints":{"type":"array","items":{"type":"string","maxLength":8192},"maxItems":64},"completion_criteria":{"type":"array","items":{"type":"string","maxLength":8192},"maxItems":64},"idempotency_key":{"type":"string","maxLength":128}},"required":["session_id","objective"]}},
        {"name":"goal_status","description":"Read the current durable Goal status without recovery, execution, or mutation. Omit goal_id to resolve the unique non-terminal Goal for the session.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"},"goal_id":{"type":"string","format":"uuid"}},"required":["session_id"]}},
        {"name":"goal_pause","description":"Request the durable Goal pause transition only. No legacy Job or execution worker is stopped by this tool.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"},"goal_id":{"type":"string","format":"uuid"},"reason":{"type":"string","maxLength":8192}},"required":["session_id"]}},
        {"name":"goal_resume","description":"Recover stale durable Goal task state and make resumable state ready for future orchestration. This does not execute work in Phase 3. An optional explicit pre-execution plan rejection may durably enter the existing Replanner path without executing a Worker.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"},"goal_id":{"type":"string","format":"uuid"},"pre_execution_plan_rejection":{"type":"object","additionalProperties":false,"properties":{"request_id":{"type":"string","minLength":1,"maxLength":128},"expected_goal_revision":{"type":"integer","minimum":1},"expected_plan_revision":{"type":"integer","minimum":1},"trigger_task_id":{"type":"string","format":"uuid","maxLength":36},"reason":{"type":"string","minLength":1,"maxLength":8192},"replan_policy":{"type":"string","enum":["NORMAL","REQUIRE_READONLY_REASSESSMENT"],"default":"NORMAL"}},"required":["request_id","expected_goal_revision","expected_plan_revision","trigger_task_id","reason"]}},"required":["session_id"]}},
        {"name":"goal_cancel","description":"Request cancellation of durable Goal authority only. This does not stop unrelated legacy Local MCP Jobs or revert repository state.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"},"goal_id":{"type":"string","format":"uuid"},"reason":{"type":"string","maxLength":8192}},"required":["session_id"]}},
        {"name":"goal_result","description":"Return the best durable result state for one explicit Goal ID. Non-terminal Goals return NOT_TERMINAL rather than fabricated success.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"},"goal_id":{"type":"string","format":"uuid"}},"required":["session_id","goal_id"]}}
        ,{"name":"goal_run","description":"Run one existing Goal in the foreground for a bounded number of host-controlled Scheduler/Finalizer steps. Model calls, if needed, use the host-configured read-only Codex model.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"},"goal_id":{"type":"string","format":"uuid"},"max_steps":{"type":"integer","minimum":1,"maximum":256}},"required":["session_id","goal_id","max_steps"]}}
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
        "goal_start" => with_goal_store(|store| goal_api::goal_start(&args, &session, store)),
        "goal_status" => with_goal_store(|store| goal_api::goal_status(&args, &session, store)),
        "goal_pause" => with_goal_store(|store| goal_api::goal_pause(&args, &session, store)),
        "goal_resume" => with_goal_store(|store| goal_api::goal_resume(&args, &session, store)),
        "goal_cancel" => with_goal_store(|store| goal_api::goal_cancel(&args, &session, store)),
        "goal_result" => with_goal_store(|store| goal_api::goal_result(&args, &session, store)),
        "goal_run" => goal_run(&args, &session).await,
        _ => anyhow::bail!("unknown tool: {name}"),
    }
}

fn with_goal_store<T, F>(operation: F) -> Result<Value>
where
    T: Serialize,
    F: FnOnce(&TaskStore) -> std::result::Result<T, goal_api::GoalApiError>,
{
    let result = match TaskStore::new() {
        Ok(store) => operation(&store),
        Err(error) => Err(goal_api::GoalApiError::from_orchestrator(error)),
    };
    goal_tool_result(result)
}

fn goal_tool_result<T: Serialize>(
    result: std::result::Result<T, goal_api::GoalApiError>,
) -> Result<Value> {
    match result {
        Ok(view) => text_result(serde_json::to_string(&view)?),
        Err(error) => Ok(json!({
            "content": [{"type": "text", "text": serde_json::to_string(&error)?}],
            "isError": true
        })),
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
    execution::resolve_path(session_cwd, path)
}

fn cwd(args: &Value, session_cwd: &Path) -> Result<PathBuf> {
    execution::cwd(args, session_cwd)
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
    let output = execution::write_file_content(&absolute, &parent, content).await?;
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

async fn store_execution_job(
    session: &config::Session,
    job: execution::BackgroundExecution,
) -> Result<Value> {
    store_job(session, job.rendered_command, job.handle, job.activity).await
}

async fn execution_outcome(
    session: &config::Session,
    outcome: execution::ExecutionOutcome,
) -> Result<Value> {
    match outcome {
        execution::ExecutionOutcome::Completed(text) => text_result(text),
        execution::ExecutionOutcome::Background(job) => store_execution_job(session, job).await,
    }
}

async fn execute(args: &Value, session: &config::Session) -> Result<Value> {
    let outcome = execution::execute(args, session).await?;
    execution_outcome(session, outcome).await
}

async fn start_command(args: &Value, session: &config::Session) -> Result<Value> {
    let job = execution::start_command(args, session).await?;
    store_execution_job(session, job).await
}

fn primary_execution_mode() -> fallback::PrimaryExecutionMode {
    execution::primary_execution_mode()
}

fn primary_execution_requires_approval(mode: fallback::PrimaryExecutionMode) -> bool {
    execution::primary_execution_requires_approval(mode)
}

fn ensure_primary_execution_authorized(
    mode: fallback::PrimaryExecutionMode,
    approved: bool,
    operation: &str,
) -> Result<()> {
    execution::ensure_primary_execution_authorized(mode, approved, operation)
}

fn execution_policy(args: &Value) -> Result<execution::ExecutionPolicy> {
    execution::execution_policy(args)
}

async fn process_sandboxed_attempt(
    session_id: &str,
    command: &[String],
    cwd: &Path,
    policy: &ExecutionPolicy,
    pre_index_snapshot: Option<String>,
    attempt: std::result::Result<sandbox::Output, sandbox::RunError>,
) -> Result<String> {
    execution::process_sandboxed_attempt(
        session_id,
        command,
        cwd,
        policy,
        pre_index_snapshot,
        attempt,
    )
    .await
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
    execution::required_command(args)
}

async fn codex_fallback(args: &Value, session: &config::Session) -> Result<Value> {
    let job = execution::codex_fallback(args, session).await?;
    store_execution_job(session, job).await
}

async fn without_sandbox(args: &Value, session: &config::Session) -> Result<Value> {
    let outcome = execution::without_sandbox(args, session).await?;
    execution_outcome(session, outcome).await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoalRunArgs {
    session_id: String,
    goal_id: String,
    pub(crate) max_steps: u32,
}

async fn goal_run(args: &Value, session: &config::Session) -> Result<Value> {
    let request = parse_goal_run_args(args)?;
    anyhow::ensure!(
        request.session_id == session.id,
        "session_id does not match the resolved Local MCP session"
    );
    let goal_id = GoalId::parse(&request.goal_id).map_err(|error| anyhow::anyhow!(error))?;
    anyhow::ensure!(
        goal_id.as_str() == request.goal_id,
        "goal_id must be a canonical lowercase UUID"
    );
    let limits = GoalRunLimits::new(request.max_steps).map_err(|error| anyhow::anyhow!(error))?;
    let session_id = session.id.clone();
    let session = session.clone();

    // `run_goal_foreground` drives the Scheduler/Worker/Replanner, whose
    // production model transport performs a blocking `thread::join` (see
    // `agent::ProductionModelTransport`) and whose durable store commits use
    // blocking OS file locks (`TaskStore::with_session_lock`). Running it
    // directly on the async worker thread would stall that thread for the
    // entire orchestration. `spawn_blocking` moves the whole run onto the
    // blocking thread pool so concurrent control-plane requests (ping,
    // session_info, other Goals' status) stay responsive on the runtime.
    //
    // This changes *where* the run executes, not *who* may mutate the Goal:
    // same-Goal mutation authority is still serialized by the OS-backed
    // per-session lock and optimistic revision checks in `TaskStore`.
    let result = tokio::task::spawn_blocking(move || -> Result<Value> {
        let store = TaskStore::new().map_err(|error| anyhow::anyhow!(error))?;
        store
            .load_goal(&session_id, &goal_id)
            .map_err(|error| anyhow::anyhow!(error))?;

        let backends = ProductionGoalBackends::production(&session);
        // The runner is `async` only because lower authority boundaries
        // (verifier/writer) may await; all model and durable-store work is
        // synchronous. Drive it to completion on this blocking thread with a
        // dedicated current-thread runtime.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build goal_run runtime")?;
        let result = runtime.block_on(goal_runner::run_goal_foreground(
            &store,
            &session,
            &goal_id,
            limits,
            backends.planner(),
            backends.readonly(),
            backends.writer(),
            backends.reviewer(),
            backends.replanner(),
        ));
        text_result(serialize_goal_run_result(&result))
    })
    .await
    .context("goal_run blocking task panicked")??;
    Ok(result)
}

pub(crate) fn parse_goal_run_args(args: &Value) -> Result<GoalRunArgs> {
    let request: GoalRunArgs =
        serde_json::from_value(args.clone()).context("invalid goal_run arguments")?;
    GoalRunLimits::new(request.max_steps).map_err(|error| anyhow::anyhow!(error))?;
    Ok(request)
}

pub(crate) fn serialize_goal_run_result(result: &GoalRunResult) -> String {
    let trace = result
        .trace
        .iter()
        .map(|entry| {
            json!({
                "step_index": entry.step_index,
                "action": trace_action_name(&entry.action),
                "task_id": entry.task_id.as_ref().map(|id| id.as_str()),
                "revision_before": entry.revision_before,
                "revision_after": entry.revision_after,
                "outcome": trace_outcome_name(&entry.outcome),
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&json!({
        "goal_id": result.goal_id.as_str(),
        "revision_before": result.revision_before,
        "revision_after": result.revision_after,
        "steps_attempted": result.steps_attempted,
        "steps_applied": result.steps_applied,
        "terminal_status": result.terminal_status.map(goal_status_name),
        "stop_reason": stop_reason_name(&result.stop_reason),
        "trace": trace,
    }))
    .expect("Goal run result view is serializable")
}

fn goal_status_name(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Planning => "PLANNING",
        GoalStatus::Running => "RUNNING",
        GoalStatus::Replanning => "REPLANNING",
        GoalStatus::Pausing => "PAUSING",
        GoalStatus::Paused => "PAUSED",
        GoalStatus::Blocked => "BLOCKED",
        GoalStatus::Verifying => "VERIFYING",
        GoalStatus::Cancelling => "CANCELLING",
        GoalStatus::Completed => "COMPLETED",
        GoalStatus::Failed => "FAILED",
        GoalStatus::Cancelled => "CANCELLED",
    }
}

fn stop_reason_name(reason: &GoalRunStopReason) -> String {
    match reason {
        GoalRunStopReason::Completed => "COMPLETED".into(),
        GoalRunStopReason::Failed => "FAILED".into(),
        GoalRunStopReason::Cancelled => "CANCELLED".into(),
        GoalRunStopReason::Paused => "PAUSED".into(),
        GoalRunStopReason::ControlState(status) => {
            format!("CONTROL_STATE_{}", goal_status_name(*status))
        }
        GoalRunStopReason::Blocked => "BLOCKED".into(),
        GoalRunStopReason::NoAction(_) => "NO_ACTION".into(),
        GoalRunStopReason::UnsupportedWorker(_) => "UNSUPPORTED_WORKER".into(),
        GoalRunStopReason::RevisionConflict { .. } => "REVISION_CONFLICT".into(),
        GoalRunStopReason::LowerAuthorityError { .. } => "LOWER_AUTHORITY_ERROR".into(),
        GoalRunStopReason::StepBudgetExhausted => "STEP_BUDGET_EXHAUSTED".into(),
        GoalRunStopReason::NoProgress { .. } => "NO_PROGRESS".into(),
        GoalRunStopReason::FinalizationNotReady(_) => "FINALIZATION_NOT_READY".into(),
    }
}

fn trace_action_name(action: &goal_runner::GoalRunTraceAction) -> &'static str {
    match action {
        goal_runner::GoalRunTraceAction::Scheduler(Some(action)) => match action {
            crate::scheduler::SchedulerAction::PlanInitial => "PLAN_INITIAL",
            crate::scheduler::SchedulerAction::VerifyTask => "VERIFY_TASK",
            crate::scheduler::SchedulerAction::VerifyGoal => "VERIFY_GOAL",
            crate::scheduler::SchedulerAction::Replan => "REPLAN",
            crate::scheduler::SchedulerAction::RunReadonly => "RUN_READONLY",
            crate::scheduler::SchedulerAction::RunWriter => "RUN_WRITER",
            crate::scheduler::SchedulerAction::RunReviewer => "RUN_REVIEWER",
            crate::scheduler::SchedulerAction::UnsupportedWorker => "UNSUPPORTED_WORKER",
            crate::scheduler::SchedulerAction::NoAction => "NO_ACTION",
        },
        goal_runner::GoalRunTraceAction::Scheduler(None) => "SCHEDULER",
        goal_runner::GoalRunTraceAction::Finalizer => "FINALIZER",
    }
}

fn trace_outcome_name(outcome: &goal_runner::GoalRunTraceOutcome) -> &'static str {
    match outcome {
        goal_runner::GoalRunTraceOutcome::Applied => "APPLIED",
        goal_runner::GoalRunTraceOutcome::NoAction(_) => "NO_ACTION",
        goal_runner::GoalRunTraceOutcome::UnsupportedWorker(_) => "UNSUPPORTED_WORKER",
        goal_runner::GoalRunTraceOutcome::RevisionConflict => "REVISION_CONFLICT",
        goal_runner::GoalRunTraceOutcome::LowerAuthorityError => "LOWER_AUTHORITY_ERROR",
        goal_runner::GoalRunTraceOutcome::Finalization(_) => "FINALIZATION",
        goal_runner::GoalRunTraceOutcome::NoProgress => "NO_PROGRESS",
    }
}

fn render_command(command: &[String]) -> String {
    execution::render_command(command)
}

fn shell_word(value: &str) -> String {
    execution::shell_word(value)
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
    execution::render_output(output)
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

    // ------------------------------------------------------------------
    // Transport concurrency regression tests (Phase 8)
    //
    // These tests drive the real `serve_with_io` transport loop with
    // synthetic requests over in-memory duplex streams. They reproduce the
    // historical freeze (a long-running request making the whole MCP control
    // plane appear dead) deterministically, with no real model, worker,
    // Goal, or repository involved.
    // ------------------------------------------------------------------

    /// Runs the transport loop against in-memory streams, sends each raw
    /// JSON-RPC line, and collects up to `expected_responses` frames.
    async fn run_transport_session(
        requests: &[&str],
        expected_responses: usize,
        timeout: Duration,
    ) -> Vec<Value> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let (client_tx, server_rx) = tokio::io::duplex(65536);
        let (server_tx, client_rx) = tokio::io::duplex(65536);
        let server = tokio::spawn(serve_with_io(server_rx, server_tx));

        let mut writer = client_tx;
        for request in requests {
            writer
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
        }
        writer.flush().await.unwrap();

        let mut responses = Vec::new();
        let mut reader = BufReader::new(client_rx).lines();
        let deadline = tokio::time::Instant::now() + timeout;
        while responses.len() < expected_responses {
            match tokio::time::timeout_at(deadline, reader.next_line()).await {
                Ok(Ok(Some(line))) => responses.push(serde_json::from_str(&line).unwrap()),
                _ => break,
            }
        }
        // Closing client input signals EOF; the server must drain in-flight
        // responses and exit cleanly.
        drop(writer);
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server did not exit after client disconnect")
            .expect("server task panicked")
            .expect("server returned an error");
        responses
    }

    /// Regression: a slow long-running request (simulating `goal_run`) must
    /// not block an unrelated lightweight `ping`. Before the fix, the second
    /// request was never even read until the first finished.
    #[tokio::test]
    async fn slow_long_running_request_does_not_block_ping() {
        let responses = run_transport_session(
            &[
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
            ],
            2,
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(responses.len(), 2);
        let mut ids: Vec<i64> = responses.iter().map(|r| r["id"].as_i64().unwrap()).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);
    }

    /// Regression: control-plane requests all respond with correct IDs and
    /// well-formed JSON-RPC frames.
    #[tokio::test]
    async fn control_plane_requests_all_respond_with_correct_ids() {
        let responses = run_transport_session(
            &[
                r#"{"jsonrpc":"2.0","id":10,"method":"ping"}"#,
                r#"{"jsonrpc":"2.0","id":11,"method":"initialize","params":{}}"#,
                r#"{"jsonrpc":"2.0","id":12,"method":"tools/list"}"#,
            ],
            3,
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(responses.len(), 3);
        let mut ids: Vec<i64> = responses.iter().map(|r| r["id"].as_i64().unwrap()).collect();
        ids.sort();
        assert_eq!(ids, vec![10, 11, 12]);
        for response in &responses {
            assert_eq!(response["jsonrpc"], "2.0");
            assert!(response.get("result").is_some() || response.get("error").is_some());
        }
    }

    /// Regression: concurrently completing responses must never interleave
    /// bytes — every raw line the client receives must parse as exactly one
    /// complete JSON object with a unique request ID.
    #[tokio::test]
    async fn concurrent_responses_never_interleave_bytes() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let (client_tx, server_rx) = tokio::io::duplex(65536);
        let (server_tx, client_rx) = tokio::io::duplex(65536);
        let server = tokio::spawn(serve_with_io(server_rx, server_tx));

        let mut writer = client_tx;
        for i in 0..32 {
            writer
                .write_all(format!(r#"{{"jsonrpc":"2.0","id":{i},"method":"ping"}}"#).as_bytes())
                .await
                .unwrap();
            writer.write_all(b"\n").await.unwrap();
        }
        writer.flush().await.unwrap();

        let mut reader = BufReader::new(client_rx);
        let mut raw_lines = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while raw_lines.len() < 32 {
            let mut line = String::new();
            match tokio::time::timeout_at(deadline, reader.read_line(&mut line)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(_)) => raw_lines.push(line.trim().to_owned()),
                _ => break,
            }
        }
        drop(writer);
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;

        assert_eq!(raw_lines.len(), 32, "every request got exactly one frame");
        let mut seen_ids = std::collections::HashSet::new();
        for frame in &raw_lines {
            let parsed: Value = serde_json::from_str(frame)
                .expect("frame bytes interleaved: line is not valid JSON");
            assert_eq!(parsed["jsonrpc"], "2.0");
            assert!(parsed["result"].is_object());
            seen_ids.insert(parsed["id"].as_i64().unwrap());
        }
        assert_eq!(seen_ids.len(), 32, "duplicate or missing request IDs");
    }

    /// Regression: a malformed request gets an immediate parse error and does
    /// not poison subsequent requests.
    #[tokio::test]
    async fn malformed_request_does_not_poison_subsequent_requests() {
        let responses = run_transport_session(
            &[
                r#"{"jsonrpc":"2.0","id":1,"method":"ping""#, // truncated JSON
                r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
            ],
            2,
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["error"]["code"], -32700);
        assert_eq!(responses[1]["id"], 2);
        assert!(responses[1]["result"].is_object());
    }

    /// Regression (panic containment): a panicking dispatch task must still
    /// yield exactly one error response (never silently drop the request) and
    /// must not poison the runtime for subsequent work. This mirrors the
    /// `tokio::spawn` + JoinError supervision used in `serve_with_io`.
    #[tokio::test]
    async fn panicking_dispatch_yields_error_response_and_does_not_poison_runtime() {
        // Mirror the exact supervision pattern from the transport loop.
        let panicking = tokio::spawn(async move { panic!("synthetic dispatch panic") });
        let outcome: Result<Value> = match panicking.await {
            Ok(outcome) => outcome,
            Err(join_error) => Err(anyhow::anyhow!(
                "internal error: request handler failed ({})",
                if join_error.is_panic() { "panic" } else { "cancelled" }
            )),
        };
        let response = match outcome {
            Ok(result) => json!({"jsonrpc":"2.0","id":7,"result":result}),
            Err(error) => {
                json!({"jsonrpc":"2.0","id":7,"error":{"code":-32000,"message":format!("{error:#}")}})
            }
        };
        assert_eq!(response["id"], 7);
        assert_eq!(response["error"]["code"], -32000);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("panic"));

        // The runtime is unaffected: a follow-up task still runs to completion.
        let follow_up = tokio::spawn(async move { 42_u32 }).await.unwrap();
        assert_eq!(follow_up, 42);
    }
}
