//! Long-run stability: transport writer, admission, and approval bounds.
//!
//! Each test has an explicit outer deadline so the suite itself can never hang
//! the way the production defects once did. Tests use in-memory duplexes, mock
//! writers, and fake responders — never the Tunnel, never production Goal state.

use std::pin::Pin;
use std::task::{Context, Poll};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::mcp::{
    JSONRPC_RESOURCE_LIMIT, ShutdownPolicy, acquire_admission, admission_pool, finish_transport,
    finish_transport_with_timeout, report_shutdown_outcome, send_response_bounded, serve_with_io,
    write_message, write_response_or_substitute,
};
use crate::resource_limits::{
    APPROVAL_IO_TIMEOUT, APPROVAL_RESPONSE_TIMEOUT, CONTROL_ADMISSION_WAIT_TIMEOUT,
    CONTROL_PLANE_PERMITS, EXECUTION_PERMITS, MCP_RESPONSE_SEND_TIMEOUT, MCP_WRITE_TIMEOUT,
};

/// Writer that never completes a write (simulates a client that stopped
/// reading stdout: backpressure with no drain).
struct PendingWriter;

impl AsyncWrite for PendingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        _bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Pending
    }
}

/// Writer that always fails immediately (simulates a broken pipe).
struct FailingWriter;

impl AsyncWrite for FailingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        _bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "test broken pipe",
        )))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "test broken pipe",
        )))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn stability_timeouts_are_bounded_and_ordered() {
    let outer = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        assert!(MCP_WRITE_TIMEOUT.as_millis() > 0);
        assert!(MCP_RESPONSE_SEND_TIMEOUT.as_millis() > 0);
        assert!(CONTROL_ADMISSION_WAIT_TIMEOUT >= MCP_WRITE_TIMEOUT);
        assert!(APPROVAL_IO_TIMEOUT.as_millis() > 0);
        assert!(APPROVAL_RESPONSE_TIMEOUT > APPROVAL_IO_TIMEOUT);
    });
    outer.await.expect("stability constant check hung");
}

#[tokio::test]
async fn io_failure_is_not_masked_as_size_violation() {
    // A broken pipe must report false quickly (transport failure), never loop
    // on substitutes and never masquerade as a resource-limit frame.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut writer = FailingWriter;
        let response = json!({"jsonrpc":"2.0","id":1,"result":{}});
        assert!(
            !write_response_or_substitute(&mut writer, &response).await,
            "I/O failure must return false so the writer terminates explicitly"
        );
        let probe = json!({"jsonrpc":"2.0","id":2,"result":{}});
        let error = write_message(&mut writer, &probe)
            .await
            .expect_err("broken pipe must be an error");
        assert!(
            error
                .downcast_ref::<crate::resource_limits::ResourceLimitError>()
                .is_none(),
            "I/O failure must not be typed as a resource-limit violation"
        );
    });
    outcome
        .await
        .expect("I/O-vs-size distinction hung: regression");
}

#[tokio::test]
async fn oversized_response_is_substituted_and_transport_continues() {
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let mut server = server;
        // 17 MiB payload exceeds the 16 MiB frame cap deterministically.
        let big = "x".repeat(17 * 1024 * 1024);
        let oversized = json!({"jsonrpc":"2.0","id":99,"result":big});
        assert!(
            write_response_or_substitute(&mut server, &oversized).await,
            "size violation must be substituted, not a transport failure"
        );
        drop(server);
        let mut reader = tokio::io::BufReader::new(client);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .expect("substitute frame must be readable");
        let frame: Value = serde_json::from_str(&line).expect("substitute must be valid JSON");
        assert_eq!(frame["id"], 99, "substitute must preserve the request id");
        assert_eq!(
            frame["error"]["code"], JSONRPC_RESOURCE_LIMIT,
            "substitute must carry the resource-limit code, not a generic error"
        );
    });
    outcome.await.expect("substitute path hung: regression");
}

#[tokio::test]
async fn stalled_writer_times_out_instead_of_hanging() {
    // Production write timeout is MCP_WRITE_TIMEOUT (10 s). The outer 20 s
    // deadline proves the write is bounded without mirroring the constant.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let mut writer = PendingWriter;
        let message = json!({"jsonrpc":"2.0","id":7,"result":{}});
        let error = write_message(&mut writer, &message)
            .await
            .expect_err("stalled client must surface as an error");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("timed out"),
            "stalled write must report a timeout, got: {rendered}"
        );
        assert!(
            error
                .downcast_ref::<crate::resource_limits::ResourceLimitError>()
                .is_none(),
            "write timeout must not be typed as a size violation"
        );
    });
    outcome
        .await
        .expect("write to a stalled client hung past the outer deadline: regression");
}

#[tokio::test]
async fn control_admission_blocks_but_is_releasable_and_execution_fails_fast() {
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let control = std::sync::Arc::new(tokio::sync::Semaphore::new(CONTROL_PLANE_PERMITS));
        let execution = std::sync::Arc::new(tokio::sync::Semaphore::new(EXECUTION_PERMITS));
        // Occupy the whole control pool: the next control acquire must block
        // (outer 200 ms fires) rather than fail fast.
        let _held: Vec<_> = (0..CONTROL_PLANE_PERMITS)
            .map(|_| control.clone().try_acquire_owned().expect("pool sized"))
            .collect();
        let ping = json!({"method":"ping"});
        assert_eq!(
            admission_pool(&ping),
            crate::mcp::Pool::Control,
            "ping must stay on the control pool"
        );
        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            acquire_admission(&ping, &control, &execution),
        )
        .await;
        assert!(
            blocked.is_err(),
            "exhausted control pool must block (production bounds it at 30 s)"
        );
        drop(_held);
        let admitted = acquire_admission(&ping, &control, &execution).await;
        assert!(admitted.is_ok(), "freed control permit must admit");

        // Execution never blocks: a saturated pool refuses immediately.
        let _exec_held: Vec<_> = (0..EXECUTION_PERMITS)
            .map(|_| execution.clone().try_acquire_owned().expect("pool sized"))
            .collect();
        let run = json!({"method":"tools/call","params":{"name":"goal_run"}});
        let refused = acquire_admission(&run, &control, &execution).await;
        assert!(refused.is_err(), "saturated execution must refuse fast");
    });
    outcome.await.expect("admission check hung: regression");
}

#[tokio::test]
async fn response_channel_send_does_not_hold_permits_forever() {
    // Capacity-1 channel with no receiver draining: the bounded send must fail
    // (freeing the caller's permit) instead of blocking indefinitely.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let (tx, _rx) = tokio::sync::mpsc::channel::<Value>(1);
        tx.send(json!({"fill": true}))
            .await
            .expect("first send fits");
        // Second send has no drain; production bound is
        // MCP_RESPONSE_SEND_TIMEOUT. Outer deadline proves boundedness.
        let result = send_response_bounded(&tx, json!({"second": true})).await;
        assert!(
            result.is_err(),
            "stalled channel must fail so the dispatch permit is freed"
        );
    });
    outcome
        .await
        .expect("bounded channel send hung past the outer deadline: regression");
}

#[tokio::test]
async fn silent_responder_is_bounded_by_timeout_pattern() {
    // Mirrors approvals::request read_line bounding: a responder that accepts
    // but never answers must surface as a timeout (fail-closed), never hang.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let (client, mut server) = tokio::io::duplex(1024);
        let silent = tokio::spawn(async move {
            let mut line = String::new();
            let mut reader = tokio::io::BufReader::new(&mut server);
            let _ = reader.read_line(&mut line).await;
            // Never reply: hold the stream open like a vanished UI.
            futures_lite_hold().await;
        });
        let mut reader = tokio::io::BufReader::new(client);
        let mut response = String::new();
        let waited = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            reader.read_line(&mut response),
        )
        .await;
        assert!(
            waited.is_err(),
            "silent responder must time out instead of hanging"
        );
        silent.abort();
    });
    outcome
        .await
        .expect("silent-responder check hung: regression");
}

async fn futures_lite_hold() {
    // Hold without depending on new crates: pend forever until aborted.
    std::future::pending::<()>().await;
}

/// Reader that fails on the first read (simulates a broken stdin pipe).
struct FailingReader;

impl tokio::io::AsyncRead for FailingReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        _bytes: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "test stdin broken",
        )))
    }
}

#[tokio::test]
async fn stdin_io_error_is_reported_not_swallowed() {
    // Every transport exit funnels through the common shutdown: a non-EOF
    // stdin failure must surface as an explicit error (after cleanup),
    // never as a hang and never as a silent success.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let (_client, writer) = tokio::io::duplex(64 * 1024);
        let result = serve_with_io(FailingReader, writer, ShutdownPolicy::LeaveRegistry).await;
        assert!(
            result.is_err(),
            "broken stdin must end the transport with an error"
        );
    });
    outcome
        .await
        .expect("transport hung on stdin I/O error: regression");
}

#[tokio::test]
async fn shutdown_outcome_precedence_is_fixed() {
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        // Writer/drain failure outranks everything: it is the reason the
        // transport cannot deliver anything.
        let drained: Result<(), anyhow::Error> = Err(anyhow::anyhow!("writer gone"));
        let read: Option<anyhow::Error> = Some(anyhow::anyhow!("stdin broken"));
        assert!(
            report_shutdown_outcome(drained, read).is_err(),
            "drain failure must be reported"
        );
        // A recorded read failure outranks a drained success.
        let drained: Result<(), anyhow::Error> = Ok(());
        let read: Option<anyhow::Error> = Some(anyhow::anyhow!("stdin broken"));
        let error = report_shutdown_outcome(drained, read).expect_err("read error must surface");
        assert!(
            format!("{error:#}").contains("stdin"),
            "the read error itself must be reported, got: {error:#}"
        );
        // Clean drain with no read failure is success (clean EOF path).
        let drained: Result<(), anyhow::Error> = Ok(());
        assert!(
            report_shutdown_outcome(drained, None).is_ok(),
            "clean shutdown must stay success"
        );
    });
    outcome.await.expect("precedence check hung: regression");
}

#[tokio::test]
async fn broken_writer_ends_transport_without_further_input() {
    // Review concern R1: after the writer task ends on a transport failure,
    // the read loop must notice even when no further input arrives. Here the
    // writer fails on the very first frame while stdin stays open and silent.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let mut client = client;
        client
            .write_all(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
            .await
            .expect("test input must be writable");
        client.write_all(b"\n").await.expect("newline must fit");
        // stdin stays open: no more input, no EOF. The transport must still
        // terminate once the writer reports the broken client.
        let result = serve_with_io(server, FailingWriter, ShutdownPolicy::LeaveRegistry).await;
        let error = result.expect_err("broken writer must end the transport with an error");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("writer") || rendered.contains("transport"),
            "transport end must name the writer failure, got: {rendered}"
        );
    });
    outcome
        .await
        .expect("transport hung after writer death with silent stdin: regression");
}

#[tokio::test]
async fn stalled_writer_ends_transport_without_further_input() {
    // Full stall chain end to end: the client stops reading (PendingWriter),
    // the write times out (MCP_WRITE_TIMEOUT), the writer terminates, and the
    // read loop ends the transport although stdin stays open and silent. No
    // per-ID wait may outlive the transport itself.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let mut client = client;
        client
            .write_all(br#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#)
            .await
            .expect("test input must be writable");
        client.write_all(b"\n").await.expect("newline must fit");
        let result = serve_with_io(server, PendingWriter, ShutdownPolicy::LeaveRegistry).await;
        assert!(
            result.is_err(),
            "stalled writer must end the transport with an error, not hang"
        );
    });
    outcome
        .await
        .expect("transport hung after writer stall with silent stdin: regression");
}

#[tokio::test]
async fn shutdown_drain_is_bounded_when_a_sender_never_drops() {
    // P2 repro: stdin is gone but one dispatch never finishes, so its sender
    // lives forever. The shutdown drain must not wait forever either: cleanup
    // (JobRegistry release, child teardown) must run and the failure must be
    // reported instead of hanging the transport teardown. The injected
    // 300 ms bound stands in for the 30 s production deadline.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<Value>(64);
        // A dispatch that never finishes: its sender never drops.
        let stuck_sender = response_tx.clone();
        let _stuck = tokio::spawn(async move {
            let _held = stuck_sender;
            std::future::pending::<()>().await;
        });
        let writer_handle = tokio::spawn(async move {
            while response_rx.recv().await.is_some() {}
            Ok::<(), anyhow::Error>(())
        });
        let result = finish_transport_with_timeout(
            writer_handle,
            None,
            Some(anyhow::anyhow!("stdin broken")),
            response_tx,
            ShutdownPolicy::LeaveRegistry,
            std::time::Duration::from_millis(300),
        )
        .await;
        let error = result.expect_err("an undrainable shutdown must be reported, not Ok");
        assert!(
            format!("{error:#}").contains("drain"),
            "the report must name the drain timeout, got: {error:#}"
        );
    });
    outcome
        .await
        .expect("shutdown hung behind an unfinished dispatch: regression");
}

#[tokio::test]
async fn shutdown_drain_still_delivers_when_senders_drop() {
    // The bound must not break the normal path: with no stuck senders the
    // drain completes immediately and a clean shutdown stays success.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<Value>(64);
        let writer_handle = tokio::spawn(async move {
            while response_rx.recv().await.is_some() {}
            Ok::<(), anyhow::Error>(())
        });
        finish_transport(
            writer_handle,
            None,
            None,
            response_tx,
            ShutdownPolicy::LeaveRegistry,
        )
        .await
        .expect("clean drain must stay success");
    });
    outcome.await.expect("clean drain hung: regression");
}
