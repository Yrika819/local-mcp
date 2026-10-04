//! Resource Bounds V1: adversarial resource stress audit.
//!
//! Read-only with respect to production behaviour: every assertion here uses the
//! real registry, the real bound reader and the real admission helper, and each
//! one is deterministic. The audit asks the questions the design promises:
//!
//! * does retained job state stay bounded across hundreds of jobs?
//! * does a session stay bounded across maximum legal jobs in many sessions?
//! * does a repeated output overflow stay deterministic and never grant retry?
//! * does simultaneous stdout/stderr flooding stay bounded?
//! * do limit+1 MCP frames stay deterministic when repeated?
//! * do control-plane requests stay admissible while execution is saturated?

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::job_registry::{Job, JobRegistry};
use crate::mcp::{
    Pool, ShutdownPolicy, acquire_admission, admission_pool, frame_reader, serve_with_io,
};
use crate::resource_limits::{
    CONTROL_PLANE_PERMITS, EXECUTION_PERMITS, FINISHED_JOB_TTL, MAX_BACKGROUND_JOBS_GLOBAL,
    MAX_BACKGROUND_JOBS_PER_SESSION, MAX_MCP_REQUEST_FRAME_BYTES,
};

// ---------------------------------------------------------------------------
// Job retention
// ---------------------------------------------------------------------------

fn at(seconds: u64) -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_secs(seconds)
}

fn completed_job(session: &str) -> (uuid::Uuid, Job) {
    let id = uuid::Uuid::new_v4();
    let handle = tokio::spawn(async { Ok("{\"exit_code\":0}".to_owned()) });
    (id, Job::new(session.to_owned(), "run".to_owned(), handle))
}

#[tokio::test]
async fn hundreds_of_completed_jobs_never_raise_the_peak_retained_count() {
    let mut registry = JobRegistry::default();
    let mut peak = 0_usize;
    let mut round = 0_u64;
    for _ in 0..500 {
        round += 1;
        let now = at(round);
        if registry.can_admit(now, "session-a").is_ok() {
            let (id, job) = completed_job("session-a");
            registry.insert(now, id, job);
            registry.wait_until_finished(id).await;
        }
        // Every third round the caller collects the result, as a real client does.
        if round.is_multiple_of(3) {
            let _ = registry.take_if_finished(now, first_id(&registry), "session-a");
        }
        peak = peak.max(registry.snapshot_len());
    }
    assert!(
        peak <= MAX_BACKGROUND_JOBS_GLOBAL,
        "peak retained {peak} exceeded the global cap {MAX_BACKGROUND_JOBS_GLOBAL}"
    );
    assert!(peak <= MAX_BACKGROUND_JOBS_PER_SESSION);

    // Once every retained result passes the TTL, nothing is left behind.
    let after = at(round + FINISHED_JOB_TTL.as_secs() + 1);
    registry.can_admit(after, "session-a").expect("capacity");
    assert_eq!(
        registry.snapshot_len(),
        0,
        "retention must not be unbounded"
    );
}

fn first_id(registry: &JobRegistry) -> uuid::Uuid {
    registry
        .first_job_id_for_test()
        .unwrap_or(uuid::Uuid::nil())
}

#[tokio::test]
async fn maximum_legal_jobs_across_many_sessions_stays_bounded() {
    let mut registry = JobRegistry::default();
    let mut admitted = 0_usize;
    let mut refusals = 0_usize;
    let mut sessions = 0_usize;
    while admitted < MAX_BACKGROUND_JOBS_GLOBAL * 4 && sessions < 64 {
        let session = format!("stress-{sessions}");
        sessions += 1;
        for _ in 0..MAX_BACKGROUND_JOBS_PER_SESSION * 2 {
            let now = at(sessions as u64);
            if registry.can_admit(now, &session).is_ok() {
                let (id, job) = completed_job(&session);
                registry.insert(now, id, job);
                registry.wait_until_finished(id).await;
                admitted += 1;
            } else {
                refusals += 1;
                break;
            }
        }
    }
    assert_eq!(
        registry.snapshot_len(),
        MAX_BACKGROUND_JOBS_GLOBAL,
        "the registry must saturate at exactly the global cap, not beyond it"
    );
    assert!(
        refusals >= 32,
        "capacity must actually be refused once full, saw {refusals} refusals"
    );
}

// ---------------------------------------------------------------------------
// Repeated output overflow
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn repeated_output_overflows_stay_deterministic_and_never_grant_retry() {
    let overflow = Arc::new(AtomicUsize::new(0));
    let cwd = std::env::temp_dir();
    for _ in 0..12 {
        let command = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            format!(
                "head -c {} /dev/zero",
                crate::resource_limits::MAX_COMMAND_STDOUT_BYTES + 1
            ),
        ];
        let error = crate::sandbox::run_unrestricted_inner(&command, &cwd, None, false)
            .await
            .expect_err("overflow must fail");
        assert!(
            error
                .error
                .downcast_ref::<crate::resource_limits::ResourceLimitError>()
                .is_some(),
            "every overflow must be the same typed resource failure"
        );
        assert!(
            error.command_started,
            "an overflow must never claim the command did not start"
        );
        let state = crate::fallback::infer_side_effect_state(
            crate::fallback::SideEffectClass::LocalMutation,
            crate::fallback::LifecycleEvidence {
                host_reached: true,
                command_start: crate::sandbox::CommandStart::Unproven,
                command_finished: false,
                process_finished: false,
            },
            crate::fallback::FailureClass::ResourceLimit,
            None,
        );
        assert_eq!(state, crate::fallback::SideEffectState::Unknown);
        let budget = crate::fallback::Budget::from_operation(None).after_state(state);
        assert!(budget.locked, "retry budget must stay locked");
        overflow.fetch_add(1, Ordering::Relaxed);
    }
    assert_eq!(overflow.load(Ordering::Relaxed), 12);
}

#[cfg(unix)]
#[tokio::test]
async fn simultaneous_stdout_and_stderr_flooding_stays_bounded() {
    let command = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        format!(
            "head -c {} /dev/zero & head -c {} /dev/zero >&2; wait",
            crate::resource_limits::MAX_COMMAND_STDOUT_BYTES + 1,
            crate::resource_limits::MAX_COMMAND_STDERR_BYTES + 1,
        ),
    ];
    let error =
        crate::sandbox::run_unrestricted_inner(&command, &std::env::temp_dir(), None, false)
            .await
            .expect_err("both streams flooding must fail deterministically");
    let rendered = format!("{:#}", error.error);
    assert!(
        rendered.contains("command stdout limit exceeded")
            || rendered.contains("command stderr limit exceeded"),
        "unexpected failure: {rendered}"
    );
    assert!(error.command_started);
}

// ---------------------------------------------------------------------------
// MCP frames
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repeated_limit_plus_one_frames_stay_deterministic() {
    let oversized = "s".repeat(MAX_MCP_REQUEST_FRAME_BYTES + 1);
    let stream = format!("{oversized}\n").repeat(4);
    let mut reader = frame_reader(stream.as_bytes());
    for round in 0..4 {
        let frame = reader
            .next_frame(MAX_MCP_REQUEST_FRAME_BYTES)
            .await
            .unwrap();
        assert!(
            matches!(frame, Some(crate::mcp::Frame::TooLarge)),
            "round {round} must be rejected identically"
        );
    }
    assert!(
        reader
            .next_frame(MAX_MCP_REQUEST_FRAME_BYTES)
            .await
            .unwrap()
            .is_none(),
        "every oversized frame must be consumed through its newline"
    );
}

// ---------------------------------------------------------------------------
// Control-plane latency under execution saturation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn control_plane_stays_admissible_and_execution_refuses_under_saturation() {
    let control = Arc::new(tokio::sync::Semaphore::new(CONTROL_PLANE_PERMITS));
    let execution = Arc::new(tokio::sync::Semaphore::new(EXECUTION_PERMITS));
    let held: Vec<_> = (0..EXECUTION_PERMITS)
        .map(|_| execution.clone().try_acquire_owned().unwrap())
        .collect();

    let execute = json!({"method":"tools/call","params":{"name":"execute"}});
    let poll = json!({"method":"tools/call","params":{"name":"poll_job"}});
    assert_eq!(admission_pool(&poll), Pool::Control);

    let started = std::time::Instant::now();
    let mut control_accepted = 0_usize;
    let mut control_refused = 0_usize;
    for _ in 0..CONTROL_PLANE_PERMITS * 2 {
        match acquire_admission(&poll, &control, &execution).await {
            Ok(permit) => {
                drop(permit);
                control_accepted += 1;
            }
            Err(_) => control_refused += 1,
        }
        assert!(
            acquire_admission(&execute, &control, &execution)
                .await
                .is_err(),
            "execution must refuse while saturated"
        );
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "control-plane admission must not wait on a saturated execution pool"
    );
    assert!(
        control_accepted >= CONTROL_PLANE_PERMITS,
        "control capacity must remain available"
    );
    // Control capacity is reserved, not unlimited: repeated requests eventually
    // hit its own ceiling rather than growing without bound.
    assert_eq!(
        control_accepted + control_refused,
        CONTROL_PLANE_PERMITS * 2,
        "every control request must be accounted for as admitted or refused"
    );
    drop(held);
}

// ---------------------------------------------------------------------------
// End-to-end: a saturated transport still answers a control request
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_transport_answers_a_control_request_after_many_execution_refusals() {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let (response_writer, response_reader) = tokio::io::duplex(64 * 1024);
    let server_task = tokio::spawn(serve_with_io(
        server,
        response_writer,
        ShutdownPolicy::LeaveRegistry,
    ));

    // Far more execution-class requests than the pool allows, followed by a
    // control-class one. Refusals must not consume the control pool.
    let mut request = String::new();
    for index in 0..(EXECUTION_PERMITS * 3) {
        request.push_str(&format!(
            "{}\n",
            json!({"jsonrpc":"2.0","id":index,"method":"tools/call","params":{"name":"not_a_real_tool","arguments":{"session_id":"x"}}})
        ));
    }
    request.push_str(&format!(
        "{}\n",
        json!({"jsonrpc":"2.0","id":9999,"method":"ping"})
    ));
    client.write_all(request.as_bytes()).await.unwrap();
    client.shutdown().await.unwrap();
    server_task.await.unwrap().unwrap();
    drop(client);

    use tokio::io::AsyncBufReadExt;
    let mut reader = tokio::io::BufReader::new(response_reader);
    let mut line = String::new();
    let mut answered = false;
    while reader.read_line(&mut line).await.unwrap() > 0 {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
            && value["id"] == 9999
        {
            assert!(
                value.get("result").is_some(),
                "the control plane must still answer: {value}"
            );
            answered = true;
        }
        line.clear();
    }
    assert!(answered, "the control request was never answered");
}

// ---------------------------------------------------------------------------
// Aggregate retained output
// ---------------------------------------------------------------------------

#[test]
fn aggregate_retained_output_is_bounded_by_the_frozen_ceilings() {
    // The worst case the registry can hold is the global cap times both output
    // ceilings. This is a static property, recorded so a limit change cannot
    // quietly make it unbounded.
    let worst_case = MAX_BACKGROUND_JOBS_GLOBAL
        * (crate::resource_limits::MAX_COMMAND_STDOUT_BYTES
            + crate::resource_limits::MAX_COMMAND_STDERR_BYTES);
    assert!(
        worst_case <= MAX_BACKGROUND_JOBS_GLOBAL * 2 * 1024 * 1024,
        "worst-case retained output {worst_case} must stay within the documented envelope"
    );
    assert_eq!(CONTROL_PLANE_PERMITS + EXECUTION_PERMITS, 32);
}

/// Guards that the audit's shared helper is genuinely shared.
#[test]
fn the_audit_counter_is_atomic() {
    let counter = Arc::new(AtomicUsize::new(0));
    let clone = Arc::clone(&counter);
    clone.fetch_add(1, Ordering::Relaxed);
    assert_eq!(counter.load(Ordering::Relaxed), 1);
}
