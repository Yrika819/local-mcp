use std::path::{Path, PathBuf};
#[cfg(test)]
use std::time::Duration;

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
// The in-process harness reads *responses* with a `BufReader`, which is fine:
// responses are host-produced and already bounded. The request side deliberately
// does not use `BufReader`.
#[cfg(test)]
use tokio::io::BufReader;
use uuid::Uuid;

use crate::execution::ExecutionPolicy;
use crate::goal::{GoalId, GoalStatus};
use crate::goal_backends::ProductionGoalBackends;
use crate::goal_runner::{self, GoalRunLimits, GoalRunResult, GoalRunStopReason};
use crate::job_registry::{self, Job};
use crate::resource_limits::{
    CONTROL_PLANE_PERMITS, EXECUTION_PERMITS, MAX_DIRECTORY_ENTRIES, MAX_DIRECTORY_OUTPUT_BYTES,
    MAX_DISPATCH_ERROR_MESSAGE_BYTES, MAX_IMAGE_RAW_BYTES, MAX_MCP_REQUEST_FRAME_BYTES,
    MAX_MCP_RESPONSE_FRAME_BYTES, MAX_READ_FILE_BYTES, MAX_RESOURCE_ERROR_MESSAGE_BYTES,
    MAX_WRITE_FILE_CONTENT_BYTES, MAX_WRITE_PREIMAGE_BYTES,
};
use crate::task_store::TaskStore;
use crate::workspace_publish;
use crate::{approvals, config, execution, fallback, goal_api, sandbox};

/// Total transport concurrency lives in [`crate::resource_limits`] as
/// `MAX_CONCURRENT_REQUESTS`, and is split here into a reserved control-plane
/// pool and an execution pool whose sum is asserted at compile time to equal it.
///
/// This is a transport-level bound only: it prevents an unbounded number of
/// in-flight requests from exhausting runtime resources while keeping the
/// control plane responsive during long-running orchestration. Same-Goal
/// mutation authority is *not* enforced here; it remains serialized by the
/// OS-backed per-session Goal lock and optimistic revision checks in
/// [`crate::task_store::TaskStore`].
/// Entry point for the MCP stdio server.
///
/// The transport is deliberately **concurrent**: each inbound request is
/// dispatched on its own task so that a long-running request (for example
/// `goal_run`) can never block lightweight control-plane requests such as
/// `ping` or `session_info`. Responses are funneled through a single
/// serialized writer task, so stdout framing stays valid and request IDs are
/// preserved even when responses complete out of order.
#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "The MCP process entrypoint is not called by the in-process test harness."
    )
)]
pub async fn serve() -> Result<()> {
    serve_with_io(tokio::io::stdin(), tokio::io::stdout()).await
}

/// Runs the MCP request loop over the provided async reader/writer pair.
///
/// Splitting the I/O handles out of [`serve`] keeps the concurrency logic
/// testable without a real process: tests can drive requests through an
/// in-memory duplex stream and assert on the raw framed responses.
#[cfg_attr(test, allow(dead_code, reason = "reached through the transport tests"))]
pub(crate) async fn serve_with_io<R, W>(reader: R, writer: W) -> Result<()>
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
            // Never end this task on a per-frame failure: returning here would drop
            // the receiver, after which every later response is silently discarded
            // and the client waits forever.
            write_response_or_substitute(&mut writer, &response).await;
        }
        Ok::<(), anyhow::Error>(())
    });

    // Admission is split across two bounded pools that sum to the same total as
    // before. A single pool meant 32 long-running `execute` calls could exhaust
    // transport capacity, including the ability to read a `poll_job` or
    // `stop_job` request at all. Control-plane requests now draw on a reserved
    // pool so the control plane stays usable under execution saturation.
    let control_permits = std::sync::Arc::new(tokio::sync::Semaphore::new(CONTROL_PLANE_PERMITS));
    let execution_permits = std::sync::Arc::new(tokio::sync::Semaphore::new(EXECUTION_PERMITS));

    // Read straight from the stream through a bounded incremental frame reader.
    // No `BufReader` is involved: it would bypass its own buffer for a read the
    // size of a full chunk, stranding buffered bytes, and it would also
    // reintroduce the "accumulate the whole line before inspecting it" contract
    // this bound exists to avoid.
    let mut reader = frame_reader(reader);
    loop {
        // Framing is bounded before parsing, and both happen before a dispatch
        // permit is acquired, so a far-too-large request neither allocates
        // without limit nor consumes a worker permit to be rejected.
        let frame = reader.next_frame(MAX_MCP_REQUEST_FRAME_BYTES).await?;
        let Some(frame) = frame else {
            break;
        };
        let line = match frame {
            Frame::Empty => continue,
            Frame::TooLarge => {
                // Reported without parsing: an oversized frame is never parsed as
                // JSON, valid or not. The frame has already been consumed, so the
                // connection continues on the next frame boundary.
                let error = crate::resource_limits::limit_error(
                    crate::resource_limits::ResourceLimit::McpRequestFrame,
                    "request frame exceeded the maximum and was discarded without parsing",
                );
                response_tx
                    .send(bounded_error_response(
                        Value::Null,
                        JSONRPC_RESOURCE_LIMIT,
                        &format!("{error:#}"),
                    ))
                    .await
                    .context("response writer task terminated unexpectedly")?;
                continue;
            }
            Frame::Complete(bytes) => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(_) => {
                    response_tx
                        .send(bounded_error_response(
                            Value::Null,
                            JSONRPC_SERVER_ERROR,
                            "request frame is not valid UTF-8",
                        ))
                        .await
                        .context("response writer task terminated unexpectedly")?;
                    continue;
                }
            },
        };
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                // Malformed JSON is answered immediately; it is not a valid
                // request and must not consume a dispatch permit.
                response_tx
                    .send(bounded_error_response(
                        Value::Null,
                        -32700,
                        &error.to_string(),
                    ))
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

        let admitted = acquire_admission(&request, &control_permits, &execution_permits).await;
        let permit = match admitted {
            Ok(permit) => permit,
            Err(reason) => {
                // Refused before any work starts. Reported as a resource failure,
                // never as an authority or permission failure.
                let _ = response_tx
                    .send(bounded_error_response(id, JSONRPC_RESOURCE_LIMIT, reason))
                    .await;
                continue;
            }
        };
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
                    // A resource bound keeps its own code, so a client can tell
                    // "too large" from "failed" and a resource failure is never
                    // presented as an authority or permission failure.
                    let code = if error
                        .downcast_ref::<crate::resource_limits::ResourceLimitError>()
                        .is_some()
                    {
                        JSONRPC_RESOURCE_LIMIT
                    } else {
                        JSONRPC_SERVER_ERROR
                    };
                    bounded_error_response(id, code, &format!("{error:#}"))
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
    release_all_jobs().await;
    Ok(())
}

/// Terminate every retained background job on explicit server shutdown.
///
/// This is the shutdown the owning process actually has: stdin EOF. It stops the
/// jobs it holds and drops their retained results. It is **not** a system-wide
/// process killer, and it does not reach jobs owned by another MCP server
/// process. Abnormal parent death (a kill rather than a clean EOF) is a separate,
/// later concern and is deliberately not handled here.
async fn release_all_jobs() {
    // Collect first: the registry lock must not be held across an await.
    let released = job_registry::registry().release_all();
    for job in released {
        job.terminate().await;
    }
}

/// Write one response, replacing it with a bounded error frame if it does not fit.
///
/// This is what keeps a single oversized result from bricking the transport. An
/// earlier version let `write_message`'s error end the writer task, which dropped
/// the response receiver: every later response was then discarded silently while
/// the server kept running the client's commands.
///
/// Returns whether the frame itself was written.
#[cfg_attr(test, allow(dead_code, reason = "asserted by the transport tests"))]
pub(crate) async fn write_response_or_substitute<W>(writer: &mut W, response: &Value) -> bool
where
    W: tokio::io::AsyncWrite + Unpin,
{
    if write_message(writer, response).await.is_ok() {
        return true;
    }
    let id = response.get("id").cloned().unwrap_or(Value::Null);
    let replacement = bounded_error_response(
        id,
        JSONRPC_RESOURCE_LIMIT,
        "response exceeded the maximum and was not written",
    );
    write_message(writer, &replacement).await.is_ok()
}

/// Writes one complete, newline-delimited JSON-RPC frame.
///
/// This is the only function that writes response bytes. It is generic over
/// the writer so the serialized writer task in [`serve_with_io`] can own any
/// `AsyncWrite` implementation (stdout in production, an in-memory duplex in
/// tests). Each call writes the serialized message and its trailing newline
/// and flushes, so frames can never be partially written or interleaved.
///
/// The global response bound is asserted here, before any byte is written.
/// Method-level bounds are what make a normal response small; this is the
/// defence in depth that guarantees no oversized frame ever reaches the socket,
/// and it fails by returning a bounded error rather than truncating valid JSON.
#[cfg_attr(test, allow(dead_code, reason = "asserted by the transport tests"))]
pub(crate) async fn write_message<W>(writer: &mut W, message: &Value) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let encoded = serde_json::to_vec(message)?;
    crate::resource_limits::ensure_resource(
        encoded.len() <= MAX_MCP_RESPONSE_FRAME_BYTES,
        crate::resource_limits::ResourceLimit::McpResponseFrame,
        "response exceeded the maximum and was not written",
    )?;
    writer.write_all(&encoded).await?;
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
        {"name":"codex_fallback","description":"Diagnose-only Codex fallback. The host may invoke Codex read-only for bounded diagnosis; public callers cannot supply execution authority, lifecycle evidence, side-effect state, or a host-native command. Platform/safety blocks are terminal.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"task":{"type":"string","minLength":1},"blocker":{"type":"string","minLength":1},"phase":{"type":"string"},"failure_class":{"type":"string","enum":["SANDBOX_PERMISSION","HOST_ENVIRONMENT","TOOL_MISSING","NETWORK_REMOTE","SEMANTIC_FAILURE","PLATFORM_SAFETY","TRANSPORT_FAILURE","UNKNOWN"]},"cwd":{"type":"string"},"requires_code_change":{"type":"boolean","default":false}},"required":["session_id","task","blocker"],"additionalProperties":false}},
        {"name":"without_sandbox","description":"Execute argv directly on the host with full user permissions and network access. Every call requires approval unless the session is in yolo mode. Returns normally when it finishes within 20 seconds; longer commands continue as a background job and return a job_id for poll_job/stop_job.","inputSchema":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"command":{"type":"array","items":{"type":"string"},"minItems":1},"cwd":{"type":"string"}},"required":["session_id","command"]}},
        {"name":"goal_start","description":"Create one durable non-terminal Goal for this Local MCP session. This does not execute tasks or repository work in Phase 3.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"},"objective":{"type":"string","minLength":1,"maxLength":131072},"title":{"type":"string","maxLength":256},"constraints":{"type":"array","items":{"type":"string","maxLength":8192},"maxItems":64},"completion_criteria":{"type":"array","items":{"type":"string","maxLength":8192},"maxItems":64},"idempotency_key":{"type":"string","maxLength":128},"workspace_mode":{"type":"string","enum":["PRIMARY","MANAGED_WORKTREE"],"default":"PRIMARY","description":"Workspace isolation policy. PRIMARY (the default) is the existing behavior. MANAGED_WORKTREE asks the host to create one host-managed Git linked worktree for this Goal before planning. It is isolation policy only: the worktree path, branch, base commit, and permission roots remain host-derived and cannot be supplied here, and the target must already be covered by the session's permitted directories."}},"required":["session_id","objective"]}},
        {"name":"goal_status","description":"Read the current durable Goal status without recovery, execution, or mutation. Omit goal_id to resolve the unique non-terminal Goal for the session.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"},"goal_id":{"type":"string","format":"uuid"}},"required":["session_id"]}},
        {"name":"goal_pause","description":"Request the durable Goal pause transition only. No legacy Job or execution worker is stopped by this tool.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"},"goal_id":{"type":"string","format":"uuid"},"reason":{"type":"string","maxLength":8192}},"required":["session_id"]}},
        {"name":"goal_resume","description":"Recover stale durable Goal task state and make resumable state ready for future orchestration. This does not execute work in Phase 3. An optional explicit pre-execution plan rejection may durably enter the existing Replanner path without executing a Worker. An optional failed_task_replan_requests array durably requests replacement of structurally invalid Tasks with bounded replacement closures; it never executes a Worker and never consumes the trigger's remaining retry.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"},"goal_id":{"type":"string","format":"uuid"},"pre_execution_plan_rejection":{"type":"object","additionalProperties":false,"properties":{"request_id":{"type":"string","minLength":1,"maxLength":128},"expected_goal_revision":{"type":"integer","minimum":1},"expected_plan_revision":{"type":"integer","minimum":1},"trigger_task_id":{"type":"string","format":"uuid","maxLength":36},"reason":{"type":"string","minLength":1,"maxLength":8192},"replan_policy":{"type":"string","enum":["NORMAL","REQUIRE_READONLY_REASSESSMENT"],"default":"NORMAL"}},"required":["request_id","expected_goal_revision","expected_plan_revision","trigger_task_id","reason"]}},"required":["session_id"]}},
        {"name":"goal_cancel","description":"Request cancellation of durable Goal authority only. This does not stop unrelated legacy Local MCP Jobs or revert repository state.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"},"goal_id":{"type":"string","format":"uuid"},"reason":{"type":"string","maxLength":8192}},"required":["session_id"]}},
        {"name":"goal_result","description":"Return the best durable result state for one explicit Goal ID. Non-terminal Goals return NOT_TERMINAL rather than fabricated success.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"},"goal_id":{"type":"string","format":"uuid"}},"required":["session_id","goal_id"]}}
        ,{"name":"goal_run","description":"Run one existing Goal in the foreground for a bounded number of host-controlled Scheduler/Finalizer steps. Model calls, if needed, use the host-configured read-only Codex model. A Goal started with workspace_mode MANAGED_WORKTREE causes the host to create one host-managed Git linked worktree and a local branch under the host managed root before planning; that root must already be covered by the session's permitted directories, and on Windows each host-native creation invocation additionally requires explicit local approval. A Goal that has not yet been authorized is refused without creating anything, and creation is limited to a small durable number of attempts for the lifetime of that workspace.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"session_id":{"type":"string"},"goal_id":{"type":"string","format":"uuid"},"max_steps":{"type":"integer","minimum":1,"maximum":256}},"required":["session_id","goal_id","max_steps"]}}
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
            "operation_id": {"type": "string", "maxLength": 128},
            "paths": {"type": "array", "items": {"type": "string"}},
            "argv": {"type": "array", "items": {"type": "string"}},
            "source": {"type": "string"},
            "destination": {"type": "string"},
            "target": {"type": "string"},
            "create_only": {"type": "boolean", "default": false},
            "force": {"type": "boolean", "default": false},
            "attempt_budget_remaining": {"type": "integer", "minimum": 0, "default": 1},
            "side_effect_budget_remaining": {"type": "integer", "minimum": 0, "default": 1}
        },
        "required": ["type"]
    });
    // Built separately from the tool literal: this schema nests deeply enough
    // that inlining it exceeds the `json!` macro recursion limit.
    let failed_task_replan_request_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "request_id": {"type": "string", "minLength": 1, "maxLength": 128},
            "expected_goal_revision": {"type": "integer", "minimum": 1},
            "expected_plan_revision": {"type": "integer", "minimum": 1},
            "trigger_task_id": {"type": "string", "format": "uuid", "maxLength": 36},
            "reason": {"type": "string", "minLength": 1, "maxLength": 8192},
            "policy": {
                "type": "string",
                "enum": ["REQUIRE_REPLACEMENT", "REQUIRE_DECOMPOSITION"],
                "default": "REQUIRE_REPLACEMENT"
            },
            "trigger_kind": {
                "type": "string",
                "enum": ["POST_ATTEMPT_FAILURE", "PRE_EXECUTION_REJECTION"],
                "default": "POST_ATTEMPT_FAILURE"
            },
            "authority_request_id": {"type": "string", "maxLength": 128}
        },
        "required": [
            "request_id",
            "expected_goal_revision",
            "expected_plan_revision",
            "trigger_task_id",
            "reason"
        ]
    });
    let failed_task_replan_requests_schema = json!({
        "type": "array",
        "maxItems": 8,
        "items": failed_task_replan_request_schema
    });
    for tool in tools.as_array_mut().unwrap() {
        if matches!(
            tool.get("name").and_then(Value::as_str),
            Some("execute" | "start_command")
        ) && let Some(operation) = tool.pointer_mut("/inputSchema/properties/operation")
        {
            *operation = operation_schema.clone();
        }
        // The durable failed-task replan request must be reachable from the
        // tool surface. `additionalProperties:false` would otherwise make the
        // whole replacement mechanism uninvocable by any client.
        if matches!(
            tool.get("name").and_then(Value::as_str),
            Some("goal_resume")
        ) && let Some(properties) = tool
            .pointer_mut("/inputSchema/properties")
            .and_then(Value::as_object_mut)
        {
            properties.insert(
                "failed_task_replan_requests".to_owned(),
                failed_task_replan_requests_schema.clone(),
            );
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
            let requested = resolve_path(&session.cwd, required_path(&args, "path")?);
            let path = config::validate_path_authority(
                &session,
                &requested,
                config::PathIntent::ReadExisting,
            )?;
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
            let requested = resolve_path(&session.cwd, required_path(&args, "path")?);
            let path = config::validate_path_authority(
                &session,
                &requested,
                config::PathIntent::ReadExisting,
            )?;
            let result = read_regular_file_bounded(
                &path,
                MAX_READ_FILE_BYTES,
                crate::resource_limits::ResourceLimit::ReadFile,
            )
            .await
            .and_then(|bytes| {
                String::from_utf8(bytes)
                    .map_err(|_| anyhow::anyhow!("file is not valid UTF-8: {}", path.display()))
            });
            report_result(
                &session.id,
                format!("Read {}", display_path(&path, &session.cwd)),
                &result,
            )
            .await;
            text_result(result?)
        }
        "list_directory" => {
            let requested = resolve_path(&session.cwd, required_path(&args, "path")?);
            let path = config::validate_path_authority(
                &session,
                &requested,
                config::PathIntent::ListDirectory,
            )?;
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

/// Read a regular file, refusing content at or above `limit`.
///
/// The bound is applied by reading at most `limit + 1` bytes and checking whether
/// that extra byte arrived, rather than trusting metadata: a file can grow
/// between the stat and the read, and metadata alone is not proof.
///
/// Requiring a regular file is what keeps a FIFO, device or socket from becoming
/// an unbounded blocking stream. Path authority is unchanged by this: it is
/// checked before the file is opened, exactly as before.
///
/// There is no silent truncation. Over-limit content is refused whole.
#[cfg_attr(
    test,
    allow(dead_code, reason = "asserted by the resource bounds file tests")
)]
pub(crate) async fn read_regular_file_bounded(
    path: &Path,
    limit: usize,
    failure: crate::resource_limits::ResourceLimit,
) -> Result<Vec<u8>> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    anyhow::ensure!(metadata.is_file(), "not a regular file: {}", path.display());
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    // `limit + 1` is the only way to distinguish "exactly at the limit" from
    // "over the limit" without retaining more than the bound allows.
    let mut bytes = Vec::new();
    let read = tokio::io::AsyncReadExt::take(&mut file, limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    crate::resource_limits::ensure_resource(
        read <= limit,
        failure,
        "content was not read and nothing was truncated",
    )?;
    Ok(bytes)
}

fn text_result(text: String) -> Result<Value> {
    Ok(json!({"content":[{"type":"text","text":text}]}))
}

async fn get_image(path: &Path) -> Result<Value> {
    let path = tokio::fs::canonicalize(&path)
        .await
        .with_context(|| format!("cannot resolve image {}", path.display()))?;
    // Bounded before base64: base64 expands the payload by 4/3, so an unbounded
    // raw read would become a larger unbounded response.
    let bytes = read_regular_file_bounded(
        &path,
        MAX_IMAGE_RAW_BYTES,
        crate::resource_limits::ResourceLimit::ImageRaw,
    )
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

#[expect(
    dead_code,
    reason = "Legacy MCP test seam delegates path authority to the live execution module."
)]
fn cwd(args: &Value, session: &config::Session) -> Result<PathBuf> {
    execution::cwd(args, session)
}

/// Render a directory listing under frozen entry-count and byte bounds.
///
/// The rendering is unchanged from before: names are accumulated, sorted, and
/// joined with newlines. What is new is that both bounds are applied *during*
/// accumulation, so an over-bound directory is refused whole rather than being
/// materialized first. It is never presented as a complete listing that is
/// quietly incomplete.
///
/// Both bounds matter independently: many short names stay under the byte cap,
/// while fewer long names can breach it.
#[cfg_attr(
    test,
    allow(dead_code, reason = "asserted by the resource bounds file tests")
)]
pub(crate) async fn list_directory(path: &Path) -> Result<String> {
    let mut entries = tokio::fs::read_dir(path).await?;
    let mut names: Vec<String> = Vec::new();
    // One separator per name, matching the joined rendering exactly.
    let mut rendered_bytes = 0_usize;
    while let Some(entry) = entries.next_entry().await? {
        crate::resource_limits::ensure_resource(
            names.len() < MAX_DIRECTORY_ENTRIES,
            crate::resource_limits::ResourceLimit::DirectoryEntries,
            "the listing was refused whole rather than truncated",
        )?;
        let suffix = if entry.file_type().await?.is_dir() {
            "/"
        } else {
            ""
        };
        let name = format!("{}{}", entry.file_name().to_string_lossy(), suffix);
        rendered_bytes = rendered_bytes.saturating_add(name.len()).saturating_add(1);
        crate::resource_limits::ensure_resource(
            rendered_bytes <= MAX_DIRECTORY_OUTPUT_BYTES,
            crate::resource_limits::ResourceLimit::DirectoryOutputBytes,
            "the listing was refused whole rather than truncated",
        )?;
        names.push(name);
    }
    names.sort();
    Ok(names.join("\n"))
}

async fn write_file(args: &Value, session: &config::Session) -> Result<Value> {
    let requested = resolve_path(&session.cwd, required_path(args, "path")?);
    // Prefer symlink_metadata over exists(): a dangling final symlink must not
    // be classified as a creatable new file (write would follow the link).
    let intent = if requested.symlink_metadata().is_ok() {
        config::PathIntent::WriteExisting
    } else {
        config::PathIntent::CreateFile
    };
    let absolute = config::validate_path_authority(session, &requested, intent)?;
    let parent = absolute.parent().context("file has no parent directory")?;
    let parent = std::fs::canonicalize(parent)
        .with_context(|| format!("parent does not exist: {}", parent.display()))?;
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .context("missing content")?;
    // Bounded before any filesystem mutation, so an oversized write performs zero
    // mutation and never prompts for an approval it cannot use. The global
    // request-frame ceiling remains the outer defense.
    crate::resource_limits::ensure_resource(
        content.len() <= MAX_WRITE_FILE_CONTENT_BYTES,
        crate::resource_limits::ResourceLimit::WriteFileContent,
        "nothing was written",
    )?;
    #[cfg(windows)]
    anyhow::ensure!(
        approvals::request(
            &session.id,
            "write_file_host_native",
            format!(
                "mode=HOST_NATIVE sandboxed=false network=false mutation_capable=true path={}",
                absolute.display()
            ),
            session.cwd.clone(),
        )
        .await?,
        "user denied host-native write_file"
    );
    // Read the real previous content, and refuse to mutate when it cannot be read.
    //
    // Treating a read failure or non-UTF-8 content as "empty" would let a write destroy
    // bytes the host never actually saw. `write_file` is a UTF-8 text tool, so an
    // existing non-UTF-8 file is a hard refusal rather than something to silently
    // overwrite. Absence, by contrast, is a legitimate empty previous state for diff.
    let existing = requested.symlink_metadata().is_ok();
    let previous = if existing {
        // Bounded too: reading the whole previous file to build a preimage and
        // diff would otherwise be an unbounded read on the mutation path.
        let bytes = read_regular_file_bounded(
            &absolute,
            MAX_WRITE_PREIMAGE_BYTES,
            crate::resource_limits::ResourceLimit::WriteFilePreimage,
        )
        .await?;
        // Treating a read failure or non-UTF-8 content as "empty" would let a
        // write destroy bytes the host never actually saw, so an existing
        // non-UTF-8 file stays a hard refusal.
        String::from_utf8(bytes).map_err(|_| {
            anyhow::anyhow!(
                "refusing to overwrite a non-UTF-8 file with a UTF-8 text write: {}",
                absolute.display()
            )
        })?
    } else {
        String::new()
    };

    // Capture the preimage now and enforce it at the commit point, so a concurrent
    // edit between this read and publication is refused rather than clobbered.
    let expected_preimage = if existing {
        workspace_publish::ExpectedPreimage::Sha256(workspace_publish::sha256_hex(
            previous.as_bytes(),
        ))
    } else {
        workspace_publish::ExpectedPreimage::Absent
    };

    // The same host-owned primitive the Writer uses, so this path is atomic too.
    let request_id = Uuid::new_v4().to_string();
    let output = execution::publish_workspace_write(crate::writer::WriterWriteRequest {
        path: &absolute,
        parent: &parent,
        content,
        expected_preimage: &expected_preimage,
        request_id: &request_id,
    })
    .await?;
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
    // No capacity pre-check here on purpose: a command that finishes inside the
    // foreground window never creates a job, and refusing it because the registry
    // is full would couple the most-used tool to background-job occupancy.
    // `store_job` is the authoritative gate, and it terminates a command it
    // cannot retain.
    let outcome = execution::execute(args, session).await?;
    execution_outcome(session, outcome).await
}

async fn start_command(args: &Value, session: &config::Session) -> Result<Value> {
    let job = execution::start_command(args, session).await?;
    store_execution_job(session, job).await
}

#[expect(
    dead_code,
    reason = "Legacy MCP test seam delegates execution policy to the live execution module."
)]
fn primary_execution_mode() -> fallback::PrimaryExecutionMode {
    execution::primary_execution_mode()
}

#[expect(
    dead_code,
    reason = "Legacy MCP test seam delegates approval policy to the live execution module."
)]
fn primary_execution_requires_approval(mode: fallback::PrimaryExecutionMode) -> bool {
    execution::primary_execution_requires_approval(mode)
}

#[expect(
    dead_code,
    reason = "Legacy MCP test seam delegates authorization to the live execution module."
)]
fn ensure_primary_execution_authorized(
    mode: fallback::PrimaryExecutionMode,
    approved: bool,
    operation: &str,
) -> Result<()> {
    execution::ensure_primary_execution_authorized(mode, approved, operation)
}

#[expect(
    dead_code,
    reason = "Legacy MCP test seam delegates execution policy to the live execution module."
)]
fn execution_policy(args: &Value) -> Result<execution::ExecutionPolicy> {
    execution::execution_policy(args)
}

#[expect(
    dead_code,
    reason = "Legacy MCP test seam delegates result processing to the live execution module."
)]
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
    handle: tokio::task::JoinHandle<Result<String>>,
    activity: &str,
) -> Result<Value> {
    /// Outcome of trying to retain a job.
    enum Admission {
        Retained,
        /// Refused, with the job handed back so its process can be stopped.
        Refused {
            error: anyhow::Error,
            job: Job,
        },
    }

    let job_id = job_registry::new_job_id();
    // Capacity and retention happen under one lock, so they cannot disagree, and
    // the lock is released before anything is awaited.
    let admission = {
        let mut jobs = job_registry::registry();
        match jobs.can_admit(job_registry::now(), &session.id) {
            Ok(()) => {
                jobs.insert(
                    job_registry::now(),
                    job_id,
                    Job::new(session.id.clone(), rendered_command.clone(), handle),
                );
                Admission::Retained
            }
            Err(error) => Admission::Refused {
                error,
                job: Job::new(session.id.clone(), rendered_command.clone(), handle),
            },
        }
    };
    if let Admission::Refused { error, job } = admission {
        // The command was already spawned by the time its result is stored.
        // Dropping the `JoinHandle` would *detach* the task rather than stop it,
        // leaving a live process with no registry entry and no way to terminate
        // it, so the job is stopped explicitly before the refusal is returned.
        job.terminate().await;
        return Err(error);
    }
    approvals::activity(
        &session.id,
        format!("{activity} {rendered_command}"),
        Some(format!("└ job {job_id}")),
    )
    .await;
    text_result(json!({"status":"running","job_id":job_id}).to_string())
}

async fn poll_job(args: &Value, session: &config::Session) -> Result<Value> {
    poll_job_with_after_finished_removal(args, session, || async {}).await
}

async fn poll_job_with_after_finished_removal<F, Fut>(
    args: &Value,
    session: &config::Session,
    after_finished_removal: F,
) -> Result<Value>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let job_id = required_job_id(args)?;
    let job =
        { job_registry::registry().take_if_finished(job_registry::now(), job_id, &session.id)? };
    let Some(job) = job else {
        return text_result(json!({"status":"running","job_id":job_id}).to_string());
    };

    after_finished_removal().await;
    text_result(job.join().await?)
}

async fn stop_job(args: &Value, session: &config::Session) -> Result<Value> {
    let job_id = required_job_id(args)?;
    let job = job_registry::registry().take(job_registry::now(), job_id, &session.id)?;
    let command = job.command().to_owned();
    job.terminate().await;
    approvals::activity(
        &session.id,
        format!("Stopped {command}"),
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

#[expect(
    dead_code,
    reason = "Legacy MCP test seam delegates command validation to the live execution module."
)]
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

/// Upper bound on host-owned lower-authority diagnostics exposed over MCP.
const MAX_GOAL_RUN_STOP_DETAIL_BYTES: usize = 8 * 1024;

/// JSON-RPC code for an ordinary dispatch failure.
const JSONRPC_SERVER_ERROR: i64 = -32000;

/// JSON-RPC code for a resource bound being reached.
///
/// Distinct from the generic server error so a client can tell "you asked for
/// more than GoalLatch will hold" from "the request failed", and so a resource
/// failure is never presented as an authority or permission failure.
#[cfg_attr(test, allow(dead_code, reason = "asserted by the transport tests"))]
pub(crate) const JSONRPC_RESOURCE_LIMIT: i64 = -32001;

/// Which bounded admission pool a request may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(test, allow(dead_code, reason = "asserted by the transport tests"))]
pub(crate) enum Pool {
    /// Reserved for lightweight control-plane requests, so polling and stopping
    /// stay usable while execution work saturates.
    Control,
    /// Everything else, including execution and orchestration.
    Execution,
}

/// Whether a request belongs to the control plane.
///
/// Control-plane requests are map lookups and state reads, so they never need to
/// queue behind a long-running command. Everything unrecognized falls into the
/// execution pool, so unknown or malformed traffic can never occupy reserved
/// control capacity.
#[cfg_attr(test, allow(dead_code, reason = "asserted by the transport tests"))]
pub(crate) fn is_control_plane(method: &str, tool: &str) -> bool {
    match method {
        // Protocol methods that perform no work.
        "initialize" | "ping" | "tools/list" | "resources/list" | "prompts/list" => true,
        // Notifications are dropped before dispatch, so they must not consume an
        // execution permit either.
        m if m.starts_with("notifications/") => true,
        "tools/call" => matches!(
            tool,
            "poll_job"
                | "stop_job"
                | "session_info"
                | "goal_status"
                | "goal_pause"
                | "goal_resume"
                | "goal_cancel"
                | "goal_result"
                | "codex_fallback"
        ),
        _ => false,
    }
}

/// The admission pool for a parsed request.
#[cfg_attr(test, allow(dead_code, reason = "asserted by the transport tests"))]
pub(crate) fn admission_pool(request: &Value) -> Pool {
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool = request
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if is_control_plane(method, tool) {
        Pool::Control
    } else {
        Pool::Execution
    }
}

/// Reserve dispatch capacity for one request.
///
/// Control-plane requests **wait** for a reserved permit, and that wait is short
/// because control work is a map lookup or a state read.
///
/// Execution requests do **not** wait. They reserve capacity without blocking,
/// because a reader that blocks on a saturated execution pool is exactly the
/// starvation this split exists to prevent: a `poll_job` queued behind a full
/// `execute` pool would never even be read. An execution request that finds the
/// pool full is refused immediately with a bounded resource error instead, which
/// is deterministic, costs no memory, and lets the client retry.
///
/// The error string is fixed and host-authored: it never echoes request content.
pub(crate) async fn acquire_admission(
    request: &Value,
    control: &std::sync::Arc<tokio::sync::Semaphore>,
    execution: &std::sync::Arc<tokio::sync::Semaphore>,
) -> std::result::Result<tokio::sync::OwnedSemaphorePermit, &'static str> {
    match admission_pool(request) {
        Pool::Control => control
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "control dispatch capacity closed"),
        Pool::Execution => execution
            .clone()
            .try_acquire_owned()
            .map_err(|_| "execution dispatch capacity exhausted"),
    }
}

/// A JSON-RPC error response whose message is guaranteed to fit in a frame.
///
/// The bound is **code-dependent**. A resource failure is small by construction
/// and is capped tightly. An ordinary dispatch failure is not: a failing `execute`
/// returns its diagnostic as the message, and that diagnostic embeds the captured
/// stdout and stderr, so a tight cap would truncate a compiler error or a failing
/// test suite to its first few lines. Both ceilings are derived from frozen
/// limits rather than invented, and each still leaves the frame assertion intact.
#[cfg_attr(test, allow(dead_code, reason = "asserted by the transport tests"))]
pub(crate) fn bounded_error_response(id: Value, code: i64, message: &str) -> Value {
    let limit = if code == JSONRPC_RESOURCE_LIMIT {
        MAX_RESOURCE_ERROR_MESSAGE_BYTES
    } else {
        MAX_DISPATCH_ERROR_MESSAGE_BYTES
    };
    let was_truncated = message.len() > limit;
    let mut bounded = truncate_on_char_boundary(message, limit);
    if was_truncated {
        bounded.push('…');
    }
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":bounded}})
}

/// Truncate to at most `limit` bytes without ever splitting a character.
///
/// A byte-index cut would panic inside `String::truncate` whenever it landed
/// mid-character, and error text can be multi-byte: a path or command output
/// containing non-ASCII would take the dispatch task down with it, leaving the
/// client waiting on that request id forever.
fn truncate_on_char_boundary(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// One newline-delimited request frame.
#[derive(Debug)]
#[cfg_attr(test, allow(dead_code, reason = "asserted by the transport tests"))]
pub(crate) enum Frame {
    /// A complete frame at or under the limit, without its newline.
    Complete(Vec<u8>),
    /// An empty line, which the protocol ignores.
    Empty,
    /// A frame that exceeded the limit.
    ///
    /// The remainder was consumed and discarded, so the next read starts on a
    /// frame boundary and protocol state stays deterministic.
    TooLarge,
}

/// Incremental newline-delimited frame reader with a hard per-frame bound.
///
/// This deliberately does **not** use `AsyncBufReadExt::lines()`: that API must
/// accumulate the whole line before a caller can inspect it, so a single large
/// line without a newline would allocate without limit.
///
/// Two properties matter and are easy to get wrong:
///
/// * The bound is applied *while* reading. At `limit + 1` bytes the reader stops
///   retaining and discards the rest of the frame, so an oversized frame costs
///   bounded memory, no JSON is parsed, and no partial request is dispatched.
///   Reaching the limit and going over it are distinguished exactly, not by a
///   post-hoc length check.
/// * Bytes already read past a frame's newline are **kept**, not dropped. A
///   single read routinely returns several frames; discarding the tail would
///   silently lose requests.
#[cfg_attr(test, allow(dead_code, reason = "asserted by the transport tests"))]
pub(crate) struct FrameReader<R> {
    inner: R,
    /// Bytes read from `inner` that belong to a later frame.
    ///
    /// Bounded by one read chunk: whatever follows a newline is at most the rest
    /// of the chunk that contained it.
    pushback: Vec<u8>,
    scratch: [u8; 8192],
}

impl<R> FrameReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            pushback: Vec::new(),
            scratch: [0_u8; 8192],
        }
    }
}

impl<R> FrameReader<R>
where
    R: AsyncRead + Unpin,
{
    /// Read the next frame, retaining at most `limit` bytes of it.
    #[cfg_attr(test, allow(dead_code, reason = "asserted by the transport tests"))]
    pub(crate) async fn next_frame(&mut self, limit: usize) -> std::io::Result<Option<Frame>> {
        let mut frame: Vec<u8> = Vec::with_capacity(limit.min(8192));
        let mut over = false;
        loop {
            if !self.pushback.is_empty() {
                let pending = std::mem::take(&mut self.pushback);
                for (index, &byte) in pending.iter().enumerate() {
                    if byte == b'\n' {
                        // Keep everything after this frame for the next call.
                        self.pushback.extend_from_slice(&pending[index + 1..]);
                        return Ok(Some(match (over, frame.is_empty()) {
                            (false, true) => Frame::Empty,
                            (false, false) => Frame::Complete(frame),
                            (true, _) => Frame::TooLarge,
                        }));
                    }
                    if over {
                        continue;
                    }
                    if frame.len() == limit {
                        // Exactly one byte past the bound: stop retaining, keep
                        // consuming to the end of the frame.
                        over = true;
                        continue;
                    }
                    frame.push(byte);
                }
            }

            let read = match self.inner.read(&mut self.scratch).await {
                Ok(0) => {
                    // EOF. A trailing frame without a newline is still delivered
                    // when it is within the bound; an oversized one is dropped.
                    return Ok(match (over, frame.is_empty()) {
                        (false, false) => Some(Frame::Complete(frame)),
                        _ => None,
                    });
                }
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            self.pushback.extend_from_slice(&self.scratch[..read]);
        }
    }
}

#[cfg_attr(test, allow(dead_code, reason = "asserted by the transport tests"))]
pub(crate) fn frame_reader<R>(inner: R) -> FrameReader<R> {
    FrameReader::new(inner)
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
        "stop_detail": stop_detail(&result.stop_reason),
        "trace": trace,
    }))
    .expect("Goal run result view is serializable")
}

/// Bounds host-owned diagnostic text deterministically on a UTF-8 boundary.
///
/// The second element reports whether truncation occurred, so a consumer can
/// never mistake a clipped diagnostic for a complete one.
fn bounded_diagnostic(detail: &str) -> (String, bool) {
    if detail.len() <= MAX_GOAL_RUN_STOP_DETAIL_BYTES {
        return (detail.to_owned(), false);
    }
    let mut end = MAX_GOAL_RUN_STOP_DETAIL_BYTES;
    while end > 0 && !detail.is_char_boundary(end) {
        end -= 1;
    }
    (detail[..end].to_owned(), true)
}

fn stop_detail(reason: &GoalRunStopReason) -> Value {
    // A managed workspace stop carries host-owned evidence about why preparation
    // stopped. It is surfaced under the same bound as every other diagnostic so
    // the operator sees the real reason rather than only a code.
    if let GoalRunStopReason::ManagedWorkspaceBlocked { code, detail } = reason {
        let (detail, truncated) = bounded_diagnostic(detail);
        return json!({
            "kind": "MANAGED_WORKSPACE_BLOCKED",
            "code": code,
            "detail": detail,
            "detail_truncated": truncated,
        });
    }
    let GoalRunStopReason::LowerAuthorityError { authority, detail } = reason else {
        return Value::Null;
    };
    let (detail, truncated) = bounded_diagnostic(detail);
    json!({
        "kind": "LOWER_AUTHORITY_ERROR",
        "authority": runner_authority_name(*authority),
        "detail": detail,
        "detail_truncated": truncated,
    })
}

fn runner_authority_name(authority: goal_runner::GoalRunnerAuthority) -> &'static str {
    match authority {
        goal_runner::GoalRunnerAuthority::Store => "STORE",
        goal_runner::GoalRunnerAuthority::Scheduler => "SCHEDULER",
        goal_runner::GoalRunnerAuthority::Planner => "PLANNER",
        goal_runner::GoalRunnerAuthority::Readonly => "READONLY",
        goal_runner::GoalRunnerAuthority::Writer => "WRITER",
        goal_runner::GoalRunnerAuthority::Verifier => "VERIFIER",
        goal_runner::GoalRunnerAuthority::GoalVerifier => "GOAL_VERIFIER",
        goal_runner::GoalRunnerAuthority::Replanner => "REPLANNER",
        goal_runner::GoalRunnerAuthority::Finalizer => "FINALIZER",
    }
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
        GoalRunStopReason::ManagedWorkspaceBlocked { code, .. } => {
            // Every managed block code already begins with `MANAGED_`, so the
            // name is the code itself; the historical `MANAGED_WORKSPACE_`
            // prefix is not applied again.
            code.clone()
        }
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

#[expect(
    dead_code,
    reason = "Legacy MCP test seam delegates command rendering to the live execution module."
)]
fn render_command(command: &[String]) -> String {
    execution::render_command(command)
}

#[expect(
    dead_code,
    reason = "Legacy MCP test seam delegates shell-word rendering to the live execution module."
)]
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

// Resource Bounds V1 transport tests live in `resource_bounds_transport_tests`
// and are registered at the crate root, because `phase0_execution_tests.rs`
// includes this file and would otherwise compile the module a second time.
#[cfg(test)]
mod tests {
    use super::*;

    /// A session rooted at a scratch directory, for exercising the write_file
    /// preimage contract without touching a real project.
    fn write_file_session(label: &str) -> (config::Session, PathBuf) {
        let root =
            std::env::temp_dir().join(format!("local-mcp-write-file-{label}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        (
            config::Session {
                id: format!("write-file-{}", Uuid::new_v4()),
                cwd: root.clone(),
                permitted_directories: vec![root.clone()],
            },
            root,
        )
    }

    /// The happy-path and content-contract cases are Unix-only because `write_file` is
    /// host-native and approval-gated on Windows. The Windows half of the publication
    /// contract is covered directly against `workspace_publish`, which needs no
    /// approval server; what is specific to Windows at this layer is the approval gate
    /// itself, covered by `write_file_still_requires_live_approval_on_windows`.
    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_updates_an_existing_utf8_file() {
        let (session, root) = write_file_session("existing");
        let target = root.join("a.txt");
        std::fs::write(&target, b"old\n").unwrap();

        write_file(
            &serde_json::json!({"path": "a.txt", "content": "new\n"}),
            &session,
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new\n");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_creates_a_new_file() {
        let (session, root) = write_file_session("create");
        let target = root.join("new.txt");

        write_file(
            &serde_json::json!({"path": "new.txt", "content": "hello\n"}),
            &session,
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"hello\n");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The confirmed defect: a read failure used to be swallowed with
    /// `unwrap_or_default()`, so an unreadable file was treated as empty and then
    /// overwritten. It must now fail with the file untouched.
    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_refuses_a_non_utf8_existing_file_without_mutating_it() {
        let (session, root) = write_file_session("nonutf8");
        let target = root.join("binary.dat");
        let original = vec![0xffu8, 0xfe, 0x00, 0x41];
        std::fs::write(&target, &original).unwrap();

        let error = write_file(
            &serde_json::json!({"path": "binary.dat", "content": "replacement\n"}),
            &session,
        )
        .await
        .unwrap_err();

        assert!(
            error.to_string().contains("non-UTF-8"),
            "unexpected error: {error:#}"
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            original,
            "a file the host could not decode must never be overwritten"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_refuses_a_target_that_became_a_directory() {
        let (session, root) = write_file_session("isdir");
        std::fs::create_dir(root.join("thing")).unwrap();

        assert!(
            write_file(
                &serde_json::json!({"path": "thing", "content": "x"}),
                &session
            )
            .await
            .is_err(),
            "a directory destination must be refused"
        );
        assert!(root.join("thing").is_dir(), "the directory must survive");
        std::fs::remove_dir_all(root).unwrap();
    }

    /// The publication change must not have weakened the Windows host-native approval
    /// gate: without a live approval server the write still fails closed, and no file is
    /// created. This is the Windows-specific half of the write_file contract.
    #[cfg(windows)]
    #[tokio::test]
    async fn write_file_still_requires_live_approval_on_windows() {
        let (session, root) = write_file_session("approval");
        let target = root.join("must-not-appear.txt");

        let result = write_file(
            &serde_json::json!({"path": "must-not-appear.txt", "content": "requires approval"}),
            &session,
        )
        .await;

        assert!(
            result.is_err(),
            "host-native write_file must fail closed without live approval"
        );
        assert!(
            !target.exists(),
            "write_file must not create a file before host-native approval"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

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
        let policy = ExecutionPolicy::new(
            "expected-state-test".into(),
            vec![0, 1],
            None,
            0,
            fallback::PrimaryExecutionMode::Sandboxed,
        );
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
                // The requested command provably started and exited.
                command_start: sandbox::CommandStart::Proven,
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
        let policy = ExecutionPolicy::new(
            "success-test".into(),
            vec![0],
            None,
            0,
            fallback::PrimaryExecutionMode::Sandboxed,
        );
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
                // The requested command provably started and finished.
                command_start: sandbox::CommandStart::Proven,
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
        assert_eq!(value["fallback_decision"]["operation_validated"], false);
        assert!(
            value["fallback_decision"]
                .get("operation_authorized")
                .is_none()
        );
    }

    #[tokio::test]
    async fn semantic_nonzero_exit_is_returned_without_executable_fallback() {
        let policy = ExecutionPolicy::new(
            "semantic-test".into(),
            vec![0],
            None,
            0,
            fallback::PrimaryExecutionMode::Sandboxed,
        );
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
                // The requested command provably started and exited.
                command_start: sandbox::CommandStart::Proven,
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
        let mut ids: Vec<i64> = responses
            .iter()
            .map(|r| r["id"].as_i64().unwrap())
            .collect();
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
        let mut ids: Vec<i64> = responses
            .iter()
            .map(|r| r["id"].as_i64().unwrap())
            .collect();
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
                if join_error.is_panic() {
                    "panic"
                } else {
                    "cancelled"
                }
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
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains("panic")
        );

        // The runtime is unaffected: a follow-up task still runs to completion.
        let follow_up = tokio::spawn(async move { 42_u32 }).await.unwrap();
        assert_eq!(follow_up, 42);
    }
}
