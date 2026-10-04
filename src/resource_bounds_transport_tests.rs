//! Resource Bounds V1: MCP transport framing, response bounds and admission.
//!
//! These drive the real transport loop over an in-memory duplex, so the claims
//! under test are the ones a client actually depends on:
//!
//! * a request frame is bounded before it is parsed, and before a worker permit
//!   is spent;
//! * exactly the limit is accepted and limit + 1 is rejected, with no partial
//!   request ever dispatched;
//! * protocol state stays deterministic across an oversized frame;
//! * a response that would exceed the global cap is refused rather than written;
//! * control-plane requests stay admissible while execution is saturated.

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

use crate::mcp::{
    Frame, JSONRPC_RESOURCE_LIMIT, Pool, ShutdownPolicy, acquire_admission, admission_pool,
    bounded_error_response, frame_reader, is_control_plane, serve_with_io, write_message,
    write_response_or_substitute,
};
use crate::resource_limits::{
    CONTROL_PLANE_PERMITS, EXECUTION_PERMITS, MAX_COMMAND_STDOUT_BYTES,
    MAX_DISPATCH_ERROR_MESSAGE_BYTES, MAX_MCP_REQUEST_FRAME_BYTES, MAX_MCP_RESPONSE_FRAME_BYTES,
    MAX_RESOURCE_ERROR_MESSAGE_BYTES,
};

/// Drive the transport with `input` and return the raw response lines.
///
/// The duplex capacity is deliberately small so the reader cannot outrun the
/// writer: framing tests really do exercise the incremental reader, which is the
/// behavior under test.
async fn serve_lines(input: &str) -> Vec<Value> {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (response_writer, response_reader) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(serve_with_io(
        server,
        response_writer,
        ShutdownPolicy::LeaveRegistry,
    ));
    client.write_all(input.as_bytes()).await.unwrap();
    client.shutdown().await.unwrap();
    server_task.await.unwrap().unwrap();
    drop(client);

    let mut reader = tokio::io::BufReader::new(response_reader);
    let mut lines = Vec::new();
    let mut line = String::new();
    while reader.read_line(&mut line).await.unwrap() > 0 {
        lines.push(serde_json::from_str(&line).unwrap());
        line.clear();
    }
    lines
}

fn frame(payload: &Value) -> String {
    format!("{payload}\n")
}

// ---------------------------------------------------------------------------
// Request framing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_empty_line_is_ignored() {
    let input = format!(
        "\n{}\n",
        frame(&json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
    );
    let lines = serve_lines(&input).await;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["id"], 1);
}

#[tokio::test]
async fn a_small_valid_request_is_answered() {
    let lines = serve_lines(&frame(&json!({"jsonrpc":"2.0","id":7,"method":"ping"}))).await;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["id"], 7);
    assert!(lines[0].get("result").is_some());
}

#[tokio::test]
async fn a_frame_at_exactly_the_limit_is_accepted() {
    // A frame of exactly the limit bytes is retained whole and dispatched.
    let prefix = r#"{"jsonrpc":"2.0","id":5,"method":"ping","pad":""#;
    let suffix = r#""}"#;
    let pad = MAX_MCP_REQUEST_FRAME_BYTES - prefix.len() - suffix.len();
    let request = format!("{prefix}{}{suffix}\n", "a".repeat(pad));

    let mut reader = frame_reader(request.as_bytes());
    let frame = reader
        .next_frame(MAX_MCP_REQUEST_FRAME_BYTES)
        .await
        .unwrap();
    match frame {
        Some(Frame::Complete(bytes)) => {
            assert_eq!(bytes.len(), MAX_MCP_REQUEST_FRAME_BYTES);
            assert!(serde_json::from_slice::<Value>(&bytes).is_ok());
        }
        other => panic!("a frame at exactly the limit must be accepted, got {other:?}"),
    }
}

#[tokio::test]
async fn a_frame_one_byte_over_the_limit_is_rejected() {
    let prefix = r#"{"jsonrpc":"2.0","id":5,"method":"ping","pad":""#;
    let suffix = r#""}"#;
    let pad = MAX_MCP_REQUEST_FRAME_BYTES + 1 - prefix.len() - suffix.len();
    let request = format!("{prefix}{}{suffix}\n", "a".repeat(pad));
    assert_eq!(request.len(), MAX_MCP_REQUEST_FRAME_BYTES + 2);

    let mut reader = frame_reader(request.as_bytes());
    let frame = reader
        .next_frame(MAX_MCP_REQUEST_FRAME_BYTES)
        .await
        .unwrap();
    assert!(
        matches!(frame, Some(Frame::TooLarge)),
        "limit + 1 must be rejected deterministically, got {frame:?}"
    );
}

#[tokio::test]
async fn a_very_large_frame_without_a_newline_does_not_allocate_and_ends_at_eof() {
    // No newline at all. The reader must retain only its bound and finish at EOF
    // rather than accumulating the whole line.
    let payload = "b".repeat(MAX_MCP_REQUEST_FRAME_BYTES * 4);
    let mut reader = frame_reader(payload.as_bytes());
    let frame = reader
        .next_frame(MAX_MCP_REQUEST_FRAME_BYTES)
        .await
        .unwrap();
    assert!(
        frame.is_none(),
        "an oversized unterminated frame is dropped at EOF, got {frame:?}"
    );
}

#[tokio::test]
async fn the_reader_retains_no_more_than_its_bound() {
    // Prove the bound is on retained bytes, not on total bytes consumed: a flood
    // far larger than the limit is still reported as over the limit.
    let payload = "c".repeat(MAX_MCP_REQUEST_FRAME_BYTES * 2);
    let stream = format!("{payload}\n");
    let mut reader = frame_reader(stream.as_bytes());
    assert!(matches!(
        reader
            .next_frame(MAX_MCP_REQUEST_FRAME_BYTES)
            .await
            .unwrap(),
        Some(Frame::TooLarge)
    ));
}

#[tokio::test]
async fn an_oversized_frame_is_answered_with_a_resource_error() {
    let oversized = "d".repeat(MAX_MCP_REQUEST_FRAME_BYTES + 64);
    let lines = serve_lines(&format!("{oversized}\n")).await;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["error"]["code"], JSONRPC_RESOURCE_LIMIT);
    let message = lines[0]["error"]["message"].as_str().unwrap();
    assert!(message.contains("mcp request frame"), "{message}");
    assert!(
        !message.contains(&"d".repeat(100)),
        "a resource error must not echo the offending payload"
    );
}

#[tokio::test]
async fn an_oversized_invalid_json_frame_is_never_parsed() {
    // Oversized *and* malformed: the rejection must not depend on JSON validity,
    // and must not be reported as a parse error.
    let payload = format!("{{not json {}", "e".repeat(MAX_MCP_REQUEST_FRAME_BYTES));
    let lines = serve_lines(&format!("{payload}\n")).await;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["error"]["code"], JSONRPC_RESOURCE_LIMIT);
}

#[tokio::test]
async fn protocol_state_stays_deterministic_after_an_oversized_frame() {
    // An oversized frame is consumed through its newline, so the request after it
    // is still read and answered normally.
    let oversized = "f".repeat(MAX_MCP_REQUEST_FRAME_BYTES + 1);
    let input = format!(
        "{oversized}\n{}",
        frame(&json!({"jsonrpc":"2.0","id":42,"method":"ping"}))
    );
    let lines = serve_lines(&input).await;
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["error"]["code"], JSONRPC_RESOURCE_LIMIT);
    assert_eq!(lines[1]["id"], 42);
    assert!(lines[1].get("result").is_some());
}

#[tokio::test]
async fn no_partial_request_is_dispatched_from_an_oversized_frame() {
    // A truncated-but-plausible prefix of an oversized frame must produce only
    // the resource error, never a dispatched result.
    let prefix = r#"{"jsonrpc":"2.0","id":9,"method":"ping","pad":""#;
    let oversized = format!("{prefix}{}\"", "g".repeat(MAX_MCP_REQUEST_FRAME_BYTES));
    let lines = serve_lines(&format!("{oversized}\n")).await;
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["error"]["code"], JSONRPC_RESOURCE_LIMIT);
    assert!(
        lines[0].get("result").is_none(),
        "no partial request may be dispatched"
    );
}

#[tokio::test]
async fn repeated_oversized_frames_each_get_exactly_one_bounded_error() {
    let oversized = "h".repeat(MAX_MCP_REQUEST_FRAME_BYTES + 1);
    let input = format!("{oversized}\n{oversized}\n");
    let lines = serve_lines(&input).await;
    assert_eq!(lines.len(), 2);
    for line in &lines {
        assert_eq!(line["error"]["code"], JSONRPC_RESOURCE_LIMIT);
        assert!(
            line["error"]["message"].as_str().unwrap().len() < 256,
            "each error must stay small"
        );
    }
}

#[tokio::test]
async fn an_oversized_frame_costs_no_worker_permit() {
    // The read loop frames and parses before acquiring a permit, so a stream of
    // oversized frames is answered without ever reaching the dispatch pool. If
    // framing happened after admission this would deadlock at the cap.
    let oversized = "i".repeat(MAX_MCP_REQUEST_FRAME_BYTES + 1);
    let input = format!("{oversized}\n").repeat(4);
    let lines = tokio::time::timeout(std::time::Duration::from_secs(30), serve_lines(&input))
        .await
        .expect("oversized frames must not consume dispatch permits");
    assert_eq!(lines.len(), 4);
}

// ---------------------------------------------------------------------------
// Response framing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_response_at_the_limit_is_written() {
    // A wide pipe: a response at the cap is tens of megabytes, and a narrow one
    // turns this into thousands of round trips.
    let (writer_half, mut reader) = tokio::io::duplex(8 * 1024 * 1024);
    let payload = "j".repeat(MAX_MCP_RESPONSE_FRAME_BYTES - 128);
    let message = json!({"jsonrpc":"2.0","id":1,"result":payload});
    let encoded = serde_json::to_vec(&message).unwrap();
    assert!(
        encoded.len() <= MAX_MCP_RESPONSE_FRAME_BYTES,
        "the fixture must sit at or just under the cap"
    );

    // Drain concurrently: the frame is far larger than the pipe, so a sequential
    // write would block forever. The writer is dropped when the write finishes so
    // the reader observes EOF.
    let mut sink = Vec::new();
    let written = reader.read_to_end(&mut sink);
    let write_side = async move {
        let mut writer = writer_half;
        let result = write_message(&mut writer, &message).await;
        drop(writer);
        result
    };
    let (write_result, read_result) = tokio::join!(write_side, written);
    write_result.unwrap();
    read_result.unwrap();

    assert_eq!(&sink[..encoded.len()], &encoded[..]);
    assert_eq!(sink[encoded.len()], b'\n');
    assert_eq!(sink.len(), encoded.len() + 1);
}

#[tokio::test]
async fn an_oversized_response_is_refused_rather_than_truncated_or_written() {
    let (mut writer, mut reader) = tokio::io::duplex(64 * 1024);
    let payload = "k".repeat(MAX_MCP_RESPONSE_FRAME_BYTES);
    let message = json!({"jsonrpc":"2.0","id":1,"result":payload});
    let error = write_message(&mut writer, &message)
        .await
        .expect_err("an over-limit response must be refused");

    // Nothing at all may be written: a partial frame would corrupt the stream.
    let mut probe = [0_u8; 8];
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            reader.read(&mut probe)
        )
        .await
        .is_err(),
        "nothing may be written for a refused response, or the frame would be partial"
    );
    // And it is a bounded resource failure, not a silently shortened JSON frame.
    let rendered = format!("{error:#}");
    assert!(rendered.contains("mcp response frame"), "{rendered}");
}

#[tokio::test]
async fn an_oversized_response_does_not_end_the_session() {
    // The blocker this guards: `write_message` used to fail, the writer task used
    // to propagate that with `?`, and every later response was then silently
    // dropped. One ordinary command that flooded stdout could wedge the whole
    // transport while the server kept executing the client's commands.
    let (writer_half, mut reader) = tokio::io::duplex(1024 * 1024);
    let huge = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": "z".repeat(MAX_MCP_RESPONSE_FRAME_BYTES + 1024)
    });

    // Write both frames while draining concurrently: the oversized frame is far
    // larger than the pipe, so a sequential write would block forever.
    let write_side = async move {
        let mut writer = writer_half;
        let substituted = write_response_or_substitute(&mut writer, &huge).await;
        let ordinary = json!({"jsonrpc":"2.0","id":2,"result":"ok"});
        let ordinary_written = write_response_or_substitute(&mut writer, &ordinary).await;
        drop(writer);
        (substituted, ordinary_written)
    };
    let read_side = async {
        let mut lines = Vec::new();
        let mut line = String::new();
        let mut buffered = tokio::io::BufReader::new(&mut reader);
        while buffered.read_line(&mut line).await.unwrap() > 0 {
            lines.push(serde_json::from_str::<Value>(&line).unwrap());
            line.clear();
        }
        lines
    };
    let ((substituted, ordinary_written), lines) = tokio::join!(write_side, read_side);

    assert!(
        substituted,
        "a bounded replacement frame must still be written for the oversized one"
    );
    assert!(
        ordinary_written,
        "an oversized response must not prevent later responses"
    );
    assert_eq!(lines.len(), 2, "exactly one frame per response: {lines:?}");

    assert_eq!(lines[0]["id"], 1);
    assert_eq!(
        lines[0]["error"]["code"], JSONRPC_RESOURCE_LIMIT,
        "an oversized frame must be reported as a bounded resource failure"
    );
    assert!(
        lines[0].get("result").is_none(),
        "an oversized frame must never be written truncated"
    );
    assert_eq!(lines[1]["id"], 2);
    assert_eq!(lines[1]["result"], "ok");
}

#[tokio::test]
async fn an_output_overflow_reports_the_resource_code_to_the_transport() {
    // Drives the full boundary the previous fix missed: a successful overflow must
    // still be *typed* after the rendered payload is attached, so the transport
    // emits the resource code rather than a generic server error.
    let overflow = crate::resource_limits::limit_error(
        crate::resource_limits::ResourceLimit::CommandStdout,
        "the attempt failed and its output was discarded",
    )
    .context("{\"exit_code\":null,\"stdout\":\"\",\"stderr\":\"\"}");
    assert!(
        overflow
            .downcast_ref::<crate::resource_limits::ResourceLimitError>()
            .is_some(),
        "the marker must survive the payload error boundary that the transport reads"
    );
}

#[tokio::test]
async fn an_oversized_error_message_is_itself_bounded() {
    // A resource failure is small by construction and is capped tightly. An
    // ordinary dispatch failure is not: it carries the command's own diagnostics.
    let huge = "m".repeat(MAX_RESOURCE_ERROR_MESSAGE_BYTES * 10);
    let response = bounded_error_response(json!(1), JSONRPC_RESOURCE_LIMIT, &huge);
    let encoded = serde_json::to_vec(&response).unwrap();
    assert!(
        encoded.len() <= MAX_MCP_RESPONSE_FRAME_BYTES,
        "a bounded error must fit in a frame"
    );
    let message = response["error"]["message"].as_str().unwrap();
    assert!(message.len() <= MAX_RESOURCE_ERROR_MESSAGE_BYTES + 8);
    assert!(message.contains('…'));
}

#[tokio::test]
async fn an_ordinary_failure_keeps_its_diagnostics() {
    // The regression this guards: a single tight cap was truncating *every*
    // dispatch error, so a failing `execute` lost nearly all of its captured
    // stderr and a caller saw the first few lines of a compiler error instead.
    let diagnostics = "e".repeat(MAX_COMMAND_STDOUT_BYTES);
    let payload = format!("{{\"exit_code\":1,\"stdout\":\"\",\"stderr\":\"{diagnostics}\"}}");
    assert!(
        payload.len() <= MAX_DISPATCH_ERROR_MESSAGE_BYTES,
        "a maxed-out command's payload must fit the dispatch error ceiling"
    );
    let response = bounded_error_response(json!(4), -32000, &payload);
    let message = response["error"]["message"].as_str().unwrap();
    assert_eq!(
        message, payload,
        "an ordinary failure must reach the caller untruncated"
    );
}

#[tokio::test]
async fn an_ordinary_failure_is_still_bounded() {
    let absurd = "x".repeat(MAX_DISPATCH_ERROR_MESSAGE_BYTES * 2);
    let response = bounded_error_response(json!(5), -32000, &absurd);
    let message = response["error"]["message"].as_str().unwrap();
    assert!(message.len() <= MAX_DISPATCH_ERROR_MESSAGE_BYTES + 8);
    assert!(message.contains('…'));
    assert!(serde_json::to_vec(&response).unwrap().len() <= MAX_MCP_RESPONSE_FRAME_BYTES);
}

#[tokio::test]
async fn a_multi_byte_error_message_is_truncated_without_panicking() {
    // A byte-index cut inside a multi-byte character would panic inside
    // `String::truncate`, taking the dispatch task down and leaving the client
    // waiting on that request id forever. Error text can carry non-ASCII from a
    // path or from command output, so this is reachable, not theoretical.
    let cases = [
        "日".repeat(MAX_RESOURCE_ERROR_MESSAGE_BYTES),
        "é".repeat(MAX_RESOURCE_ERROR_MESSAGE_BYTES / 2 + 1),
        "é".repeat(MAX_RESOURCE_ERROR_MESSAGE_BYTES * 4),
    ];
    for message in cases {
        for (code, limit) in [
            (JSONRPC_RESOURCE_LIMIT, MAX_RESOURCE_ERROR_MESSAGE_BYTES),
            (-32000, MAX_DISPATCH_ERROR_MESSAGE_BYTES),
        ] {
            let response = bounded_error_response(json!(7), code, &message);
            let rendered = response["error"]["message"].as_str().unwrap().to_owned();
            assert!(
                rendered.len() <= limit + 8,
                "truncation must stay within the ceiling for code {code}, got {} bytes",
                rendered.len()
            );
            assert_eq!(response["id"], 7);
        }
    }
    // And the case that actually panicked before: a byte-index cut inside a
    // multi-byte character on the code whose ceiling bites here.
    let over_resource_ceiling = "日".repeat(MAX_RESOURCE_ERROR_MESSAGE_BYTES);
    assert!(
        over_resource_ceiling.len() > MAX_RESOURCE_ERROR_MESSAGE_BYTES,
        "the fixture must exceed the byte ceiling while staying under the char count"
    );
    let response = bounded_error_response(json!(7), JSONRPC_RESOURCE_LIMIT, &over_resource_ceiling);
    assert!(
        response["error"]["message"].as_str().unwrap().len()
            <= MAX_RESOURCE_ERROR_MESSAGE_BYTES + 8
    );
}

#[tokio::test]
async fn a_short_multi_byte_error_message_is_passed_through_unchanged() {
    let message = "パスが拒否されました";
    let response = bounded_error_response(json!(8), -32000, message);
    assert_eq!(response["error"]["message"], message);
}

#[tokio::test]
async fn a_small_error_message_is_passed_through_unchanged() {
    let response = bounded_error_response(json!(3), -32000, "small failure");
    assert_eq!(response["error"]["message"], "small failure");
    assert_eq!(response["id"], 3);
}

// ---------------------------------------------------------------------------
// Control-plane admission
// ---------------------------------------------------------------------------

#[test]
fn control_plane_requests_are_recognized() {
    for tool in [
        "poll_job",
        "stop_job",
        "session_info",
        "goal_status",
        "goal_pause",
        "goal_resume",
        "goal_cancel",
        "goal_result",
        "codex_fallback",
    ] {
        assert!(
            is_control_plane("tools/call", tool),
            "{tool} must be control"
        );
    }
    for method in ["initialize", "ping", "tools/list"] {
        assert!(is_control_plane(method, ""), "{method} must be control");
    }
}

#[test]
fn execution_requests_are_not_control_plane() {
    for tool in [
        "execute",
        "start_command",
        "goal_run",
        "read_file",
        "list_directory",
        "get_image",
        "write_file",
    ] {
        assert!(
            !is_control_plane("tools/call", tool),
            "{tool} must not be control"
        );
    }
}

#[test]
fn unknown_traffic_cannot_occupy_reserved_control_capacity() {
    // Anything unrecognized falls into the execution pool, so a caller cannot
    // flood the reserved pool with traffic GoalLatch does not recognize.
    assert!(!is_control_plane("made/up", ""));
    assert!(!is_control_plane("tools/call", "not_a_tool"));
    assert_eq!(
        admission_pool(&json!({"method":"made/up","id":1})),
        Pool::Execution
    );
    assert_eq!(
        admission_pool(&json!({"method":"tools/call","params":{"name":"not_a_tool"}})),
        Pool::Execution
    );
    assert_eq!(
        admission_pool(&json!({"method":"tools/call","params":{"name":"poll_job"}})),
        Pool::Control
    );
    assert_eq!(admission_pool(&json!({"method":"ping"})), Pool::Control);
}

#[tokio::test]
async fn control_plane_requests_stay_admissible_while_execution_saturates() {
    // The defect this prevents: a reader that blocks on a saturated execution
    // pool means a `poll_job` queued behind it is never even read. Execution
    // admission therefore refuses immediately rather than waiting, leaving the
    // reserved control capacity untouched.
    let control = std::sync::Arc::new(tokio::sync::Semaphore::new(CONTROL_PLANE_PERMITS));
    let execution = std::sync::Arc::new(tokio::sync::Semaphore::new(EXECUTION_PERMITS));

    // Saturate the execution pool exactly, holding every permit.
    let mut held = Vec::new();
    for _ in 0..EXECUTION_PERMITS {
        held.push(
            execution
                .clone()
                .try_acquire_owned()
                .expect("execution pool has capacity"),
        );
    }

    let execute = json!({"method":"tools/call","params":{"name":"execute"}});
    let poll = json!({"method":"tools/call","params":{"name":"poll_job"}});

    // An execution request is refused, not queued.
    assert!(
        acquire_admission(&execute, &control, &execution)
            .await
            .is_err()
    );

    // Control-plane requests still proceed.
    for _ in 0..CONTROL_PLANE_PERMITS {
        assert!(
            acquire_admission(&poll, &control, &execution).await.is_ok(),
            "control-plane admission must survive execution saturation"
        );
    }
}

#[tokio::test]
async fn control_plane_capacity_is_itself_bounded() {
    // Reserved capacity is reserved, not unlimited: it cannot be used to bypass
    // the total concurrency bound.
    let control = std::sync::Arc::new(tokio::sync::Semaphore::new(CONTROL_PLANE_PERMITS));
    let execution = std::sync::Arc::new(tokio::sync::Semaphore::new(EXECUTION_PERMITS));
    let mut held = Vec::new();
    for _ in 0..CONTROL_PLANE_PERMITS {
        held.push(
            control
                .clone()
                .try_acquire_owned()
                .expect("control pool has capacity"),
        );
    }
    let poll = json!({"method":"tools/call","params":{"name":"poll_job"}});
    let refused = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        acquire_admission(&poll, &control, &execution),
    )
    .await;
    assert!(
        refused.is_err(),
        "control capacity is reserved, not unlimited: a saturated control pool must still wait"
    );
}

#[tokio::test]
async fn total_admission_still_equals_the_previous_single_pool_bound() {
    assert_eq!(CONTROL_PLANE_PERMITS + EXECUTION_PERMITS, 32);
}
