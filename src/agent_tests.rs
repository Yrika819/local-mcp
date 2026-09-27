use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::time::Instant;
#[cfg(unix)]
use uuid::Uuid;

#[cfg(unix)]
use crate::agent::safe_diagnostic_for_test;
use crate::agent::{
    AgentError, MODEL_PROMPT_LIMIT, MODEL_STDERR_LIMIT, MODEL_STDOUT_LIMIT, ModelInvocation,
    ModelInvocationOutput, ModelRole, ModelTransport, configured_model_from_for_test,
    require_approval_for_test, run_bounded_process_for_test,
};
#[cfg(unix)]
use crate::agent::{
    PROCESS_CLEANUP_GRACE, run_bounded_host_process, run_bounded_process_async_for_test,
};

#[cfg(unix)]
/// How long a fixture descendant may take to publish its pid before the fixture
/// is treated as never having started.
const DESCENDANT_READINESS: Duration = Duration::from_secs(5);

#[cfg(unix)]
/// Seconds a fixture descendant stays alive. It outlives every lifecycle
/// deadline below, so "the run timed out while the descendant still held the
/// inherited pipes" is unambiguous rather than a race against a sleep.
const DESCENDANT_LIFETIME_SECS: u64 = 10;

#[cfg(unix)]
/// Deadline for the bounded-lifecycle fixtures. It only has to outlast fixture
/// scheduling under load; the retained-pipe and blocked-stdin conditions, not
/// the deadline length, are what these tests assert.
const LIFECYCLE_TEST_DEADLINE: Duration = Duration::from_secs(3);

#[cfg(unix)]
/// Deadline for the outcome-mapping fixtures. Every one of them completes on the
/// fixture's own behaviour, never on this deadline.
const SLOW: Duration = Duration::from_secs(10);

#[cfg(unix)]
fn temporary_script(root: &Path, name: &str, body: &str) -> PathBuf {
    let path = root.join(name);
    std::fs::write(&path, body).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

#[derive(Default)]
struct RecordingTransport {
    calls: Mutex<Vec<ModelInvocation>>,
}

impl ModelTransport for RecordingTransport {
    fn invoke(&self, request: &ModelInvocation) -> Result<ModelInvocationOutput, AgentError> {
        self.calls.lock().unwrap().push(request.clone());
        Ok(ModelInvocationOutput::new(
            b"{\"exact\":true}\n".to_vec(),
            String::new(),
            0,
        ))
    }
}

#[test]
fn injectable_transport_preserves_typed_output_and_role_data() {
    let transport = Arc::new(RecordingTransport::default());
    let invocation = ModelInvocation::new(
        "session-1",
        PathBuf::from("/repo"),
        ModelRole::Writer,
        "fixed writer prompt\nREQUEST_JSON\n{\"x\":1}",
    );

    let output = transport.invoke(&invocation).unwrap();
    assert_eq!(output.stdout(), b"{\"exact\":true}\n");
    assert_eq!(output.safe_stderr(), "");
    assert_eq!(output.exit_status(), 0);
    let call = &transport.calls.lock().unwrap()[0];
    assert_eq!(call.session_id(), "session-1");
    assert_eq!(call.cwd(), std::path::Path::new("/repo"));
    assert_eq!(call.role(), ModelRole::Writer);
    assert_eq!(
        call.prompt(),
        "fixed writer prompt\nREQUEST_JSON\n{\"x\":1}"
    );
}

#[test]
fn model_roles_are_fixed_and_provider_free() {
    assert_eq!(ModelRole::Planner.as_str(), "PLANNER");
    assert_eq!(ModelRole::Writer.as_str(), "WRITER");
    assert_eq!(ModelRole::Reviewer.as_str(), "REVIEWER");
    assert_eq!(ModelRole::Replanner.as_str(), "REPLANNER");
}

#[cfg(unix)]
fn command(program: &str, args: &[&str]) -> Vec<String> {
    std::iter::once(program.to_owned())
        .chain(args.iter().map(|arg| (*arg).to_owned()))
        .collect()
}

#[cfg(unix)]
async fn wait_for_pid(path: &Path) -> libc::pid_t {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(path)
                && let Ok(pid) = pid.trim().parse::<libc::pid_t>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[cfg(unix)]
async fn wait_for_process_exit(pid: libc::pid_t) -> bool {
    for _ in 0..50 {
        if unsafe { libc::kill(pid, 0) } == -1 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Bounded readiness protocol for a fixture descendant.
///
/// A bare `read_to_string` on a fixture pid file is unsound: the fixture writes
/// it with a non-atomic redirect, so the file can be observed before it holds
/// any content, and a fixture the scheduler has not run yet writes nothing at
/// all. This returns only once the file holds a parseable pid, which is the
/// fixture recording the pid of the process it actually forked, so a caller can
/// never credit cleanup with killing a descendant that was never observed.
#[cfg(unix)]
async fn wait_for_descendant_pid(path: &Path) -> libc::pid_t {
    tokio::time::timeout(DESCENDANT_READINESS, async {
        loop {
            if let Ok(contents) = std::fs::read_to_string(path)
                && let Ok(pid) = contents.trim().parse::<libc::pid_t>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture descendant never published a parseable pid before the deadline")
}

#[cfg(unix)]
/// A fixture that satisfies the launcher's process contract deterministically.
///
/// The launcher writes the prompt on stdin, so a fixture that consumes it to end
/// of file resolves the writer before the fixture can exit. That keeps the
/// outcome-mapping fixtures below about the mapping they are named for rather
/// than about prompt completion, and it is why a short-lived fixture is only
/// ordered against its writer deliberately in the completion cases.
fn draining_script(root: &Path, name: &str, body: &str) -> PathBuf {
    temporary_script(
        root,
        name,
        &format!("#!/bin/sh\n/bin/cat > /dev/null\n{body}\n"),
    )
}

#[cfg(unix)]
#[test]
fn bounded_process_maps_success_empty_exit_timeout_and_output_limits() {
    let root = std::env::temp_dir().join(format!("local-mcp-agent-limits-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    let success = draining_script(&root, "success.sh", "printf exact\nexit 0");
    let output =
        run_bounded_process_for_test(&command(&success.to_string_lossy(), &[]), b"prompt", SLOW)
            .unwrap();
    assert_eq!(output.stdout(), b"exact");
    assert_eq!(output.exit_status(), 0);
    assert_eq!(output.safe_stderr(), "");

    let safe = safe_diagnostic_for_test(b"\x1b[31mdiagnostic\x1b[0m\n");
    assert!(safe.contains("diagnostic"));
    assert!(!safe.contains('\u{1b}'));

    let nonzero = draining_script(&root, "nonzero.sh", "exit 3");
    assert_eq!(
        run_bounded_process_for_test(&command(&nonzero.to_string_lossy(), &[]), b"prompt", SLOW)
            .unwrap_err(),
        AgentError::NonZeroExit
    );

    // A process that consumes its prompt, produces no output and exits cleanly
    // is an empty response, not a timeout.
    let empty = draining_script(&root, "empty.sh", "exit 0");
    assert_eq!(
        run_bounded_process_for_test(&command(&empty.to_string_lossy(), &[]), b"prompt", SLOW)
            .unwrap_err(),
        AgentError::EmptyResponse
    );

    assert_eq!(
        run_bounded_process_for_test(
            &command("/bin/sleep", &["1"]),
            b"prompt",
            Duration::from_millis(20),
        )
        .unwrap_err(),
        AgentError::Timeout
    );

    let flood = draining_script(&root, "flood.sh", "exec /usr/bin/yes");
    assert_eq!(
        run_bounded_process_for_test(&command(&flood.to_string_lossy(), &[]), b"prompt", SLOW)
            .unwrap_err(),
        AgentError::ResponseTooLarge
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn a_momentarily_busy_executable_is_started_once_it_stops_being_written() {
    let root = std::env::temp_dir().join(format!("local-mcp-agent-busy-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let script = draining_script(&root, "busy.sh", "printf ready");
    // Hold the executable open for writing, exactly as another process that has
    // just created or is still updating a program leaves it. The kernel refuses
    // to exec a file with an outstanding writer, so a launcher that treated the
    // refusal as a missing or unusable command would fail a request that is
    // perfectly valid a moment later.
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&script)
        .expect("the fixture must stay open for writing");
    // Released from another thread, because the launcher is being driven
    // synchronously on this one.
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        drop(writer);
    });
    let output =
        run_bounded_process_for_test(&command(&script.to_string_lossy(), &[]), b"prompt", SLOW)
            .expect("a momentarily busy executable must still be started");
    release.join().expect("the writer must be released");
    assert_eq!(output.stdout(), b"ready");
    assert_eq!(output.exit_status(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
/// A fixture whose direct child exits at once and whose descendant holds the
/// prompt's read end for exactly `seconds`, and no longer.
///
/// The child never reads the prompt. Its descendant is the only remaining reader
/// and it is released on a schedule this test chooses, so the writer is
/// guaranteed to still be unfinished when the child is observed and to resolve
/// at a moment this test also chooses. That makes the completion ordering a
/// property of the fixture rather than of the scheduler.
///
/// The descendant's own stdout and stderr are redirected away from the
/// launcher's pipes, so it retains the read end and nothing else: the child's
/// output is complete as soon as the child is gone, and only the prompt is
/// outstanding.
fn released_read_end_script(root: &Path, name: &str, body: &str, seconds: &str) -> PathBuf {
    temporary_script(
        root,
        name,
        &format!("#!/bin/sh\nexec 3<&0\n/bin/sleep {seconds} <&3 > /dev/null 2>&1 &\n{body}\n"),
    )
}

#[cfg(unix)]
/// A prompt far larger than any pipe buffer.
///
/// A child that never reads the prompt would let the writer finish if the prompt
/// fitted in the buffer, and that is the one ordering these cases must not
/// depend on: the writer has to still be unfinished when the child exits.
fn oversized_prompt() -> Vec<u8> {
    vec![b'x'; 1024 * 1024]
}

#[cfg(unix)]
#[test]
fn a_child_that_exits_before_its_prompt_is_written_still_reports_its_own_result() {
    let root = std::env::temp_dir().join(format!("local-mcp-agent-exit-first-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    // The child exits at once, and the prompt is still unwritten at that moment.
    // The launcher records the exit and keeps driving the remaining IO, so the
    // writer resolves as a broken pipe once the descendant releases the read end
    // and the run reports the child's own result.
    //
    // A launcher that turns an unfinished writer into a timeout the moment it
    // observes the child cannot distinguish this run from one whose prompt is
    // still genuinely blocked, and it answers both with the same wrong verdict.
    let script = released_read_end_script(&root, "exit-first.sh", "printf exact\nexit 0", "0.3");
    let started = Instant::now();
    let output = run_bounded_process_for_test(
        &command(&script.to_string_lossy(), &[]),
        &oversized_prompt(),
        SLOW,
    )
    .expect("a child that exits before its prompt is written must not be a timeout");
    assert_eq!(output.stdout(), b"exact");
    assert_eq!(output.exit_status(), 0);
    assert_eq!(output.safe_stderr(), "");
    // The run ends when the read end is released, so it is bounded by the
    // fixture rather than by the deadline.
    assert!(started.elapsed() < Duration::from_secs(2));
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn a_child_that_exits_with_no_output_is_an_empty_response_and_not_a_timeout() {
    let root = std::env::temp_dir().join(format!("local-mcp-agent-exit-silent-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    // A successful child with nothing to say is an empty response. Exiting before
    // the prompt is written does not change that into a timeout.
    let script = released_read_end_script(&root, "exit-silent.sh", "exit 0", "0.3");
    assert_eq!(
        run_bounded_process_for_test(
            &command(&script.to_string_lossy(), &[]),
            &oversized_prompt(),
            SLOW
        )
        .unwrap_err(),
        AgentError::EmptyResponse
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn a_child_that_exits_unsuccessfully_before_its_prompt_is_written_is_a_nonzero_exit() {
    let root = std::env::temp_dir().join(format!("local-mcp-agent-exit-failed-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    // The child's own exit status decides the outcome, even when it is observed
    // before the prompt could be written.
    let script = released_read_end_script(&root, "exit-failed.sh", "printf exact\nexit 7", "0.3");
    assert_eq!(
        run_bounded_process_for_test(
            &command(&script.to_string_lossy(), &[]),
            &oversized_prompt(),
            SLOW
        )
        .unwrap_err(),
        AgentError::NonZeroExit
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn a_child_that_closes_its_own_prompt_reports_a_broken_pipe_as_a_completed_write() {
    let root =
        std::env::temp_dir().join(format!("local-mcp-agent-closed-prompt-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    // The child closes its own read end without consuming the prompt and then
    // exits, so the writer can only end as a broken pipe. A closed prompt is the
    // child's own decision rather than a transport failure, so the run reports
    // what the child produced.
    let script = temporary_script(
        &root,
        "closed-prompt.sh",
        "#!/bin/sh\nexec 0<&-\nprintf exact\n/bin/sleep 0.3\nexit 0\n",
    );
    let output = run_bounded_process_for_test(
        &command(&script.to_string_lossy(), &[]),
        &oversized_prompt(),
        SLOW,
    )
    .expect("a child that closes its own prompt is not a transport failure");
    assert_eq!(output.stdout(), b"exact");
    assert_eq!(output.exit_status(), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn a_child_that_drains_its_prompt_and_exits_reports_its_own_output() {
    let root = std::env::temp_dir().join(format!("local-mcp-agent-drains-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    // The ordinary ordering: the prompt is consumed to end of file, and the
    // child's own output and exit status are reported unchanged.
    let script = draining_script(&root, "drains.sh", "printf exact\nexit 0");
    let output =
        run_bounded_process_for_test(&command(&script.to_string_lossy(), &[]), b"prompt", SLOW)
            .expect("a child that drains its prompt must complete");
    assert_eq!(output.stdout(), b"exact");
    assert_eq!(output.exit_status(), 0);
    assert_eq!(output.safe_stderr(), "");
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn a_prompt_that_stays_blocked_after_the_child_exits_times_out_at_the_deadline() {
    let root = std::env::temp_dir().join(format!("local-mcp-agent-blocked-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let pid_path = root.join("descendant.pid");
    // The direct child exits at once without reading the prompt, and a
    // descendant holds the inherited read end for far longer than the run's
    // deadline. Nothing ever resolves the writer, so this is the condition the
    // deadline exists for: the run ends as a timeout, and it ends at the
    // deadline rather than at the child's exit, because until the deadline the
    // prompt is merely outstanding rather than resolved.
    let script = temporary_script(
        &root,
        "blocked-prompt.sh",
        &format!(
            "#!/bin/sh\nexec 3<&0\n/bin/sleep {} <&3 > /dev/null 2>&1 &\nprintf '%s' \"$!\" > '{}'\nexit 0\n",
            DESCENDANT_LIFETIME_SECS,
            pid_path.display()
        ),
    );
    let timeout = LIFECYCLE_TEST_DEADLINE;
    let started = Instant::now();
    let command = [script.to_string_lossy().into_owned()];
    let prompt = oversized_prompt();
    // The descendant is observed while the run is still in flight, so the cleanup
    // assertion below is always about a process that really existed.
    let (result, pid) = tokio::join!(
        run_bounded_process_async_for_test(&command, &prompt, &root, timeout),
        wait_for_descendant_pid(&pid_path),
    );
    assert_eq!(result.unwrap_err(), AgentError::Timeout);
    assert!(
        started.elapsed() >= timeout,
        "unresolved prompt IO is a timeout at the deadline, not at the child's exit"
    );
    assert!(started.elapsed() < timeout + PROCESS_CLEANUP_GRACE + Duration::from_secs(1));
    let mut alive = true;
    for _ in 0..50 {
        if unsafe { libc::kill(pid, 0) } == -1 {
            alive = false;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !alive,
        "the timeout must reap the descendant holding the prompt"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_host_launcher_kills_descendant_retaining_pipes() {
    let root = std::env::temp_dir().join(format!("local-mcp-agent-descendant-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let pid_path = root.join("descendant.pid");
    // The descendant outlives the run deadline and keeps the inherited stdout and
    // stderr open, so the run cannot finish before cleanup. That retained-pipe
    // condition, not the deadline length, is what this test asserts.
    let script = temporary_script(
        &root,
        "descendant.sh",
        &format!(
            "#!/bin/sh\n/bin/sleep {} &\nprintf '%s' \"$!\" > '{}'\nexit 0\n",
            DESCENDANT_LIFETIME_SECS,
            pid_path.display()
        ),
    );
    let started = Instant::now();
    let timeout = LIFECYCLE_TEST_DEADLINE;
    let command = [script.to_string_lossy().into_owned()];
    let (result, pid) = tokio::join!(
        run_bounded_host_process(&command, b"prompt", &root, timeout),
        wait_for_descendant_pid(&pid_path),
    );
    assert_eq!(result.unwrap_err(), AgentError::Timeout);
    // The retained pipes are bounded by the run deadline and then bounded again
    // by the cleanup grace, so the run cannot outlive its own budget.
    assert!(started.elapsed() < timeout + PROCESS_CLEANUP_GRACE + Duration::from_secs(1));
    let mut alive = true;
    for _ in 0..50 {
        if unsafe { libc::kill(pid, 0) } == -1 {
            alive = false;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!alive);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_host_launcher_cancellation_kills_group_and_joins_parent() {
    let root = std::env::temp_dir().join(format!("local-mcp-agent-cancel-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let pid_path = root.join("descendant.pid");
    let script = temporary_script(
        &root,
        "cancel.sh",
        &format!(
            "#!/bin/sh\n/bin/sleep 5 &\nprintf '%s' \"$!\" > '{}'\nexec /bin/sleep 5\n",
            pid_path.display()
        ),
    );
    let command = vec![script.to_string_lossy().into_owned()];
    let cwd = root.clone();
    let handle = tokio::spawn(async move {
        run_bounded_host_process(&command, b"prompt", &cwd, Duration::from_secs(30)).await
    });
    let pid = wait_for_pid(&pid_path).await;
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
    let stopped = wait_for_process_exit(pid).await;
    if !stopped {
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(stopped);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_host_launcher_deadline_bounds_blocked_stdin_write() {
    let root = std::env::temp_dir().join(format!("local-mcp-agent-stdin-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let script = temporary_script(&root, "ignore-stdin.sh", "#!/bin/sh\nexec /bin/sleep 1\n");
    let prompt = vec![b'x'; 1024 * 1024];
    let started = Instant::now();
    let result = run_bounded_host_process(
        &[script.to_string_lossy().into_owned()],
        &prompt,
        &root,
        Duration::from_millis(150),
    )
    .await;
    assert_eq!(result.unwrap_err(), AgentError::Timeout);
    assert!(started.elapsed() < Duration::from_secs(1));
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn async_launcher_cleans_retained_pipe_tasks_by_deadline_plus_grace() {
    let root =
        std::env::temp_dir().join(format!("local-mcp-agent-async-cleanup-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let pid_path = root.join("descendant.pid");
    let script = temporary_script(
        &root,
        "retained-pipes.sh",
        &format!(
            "#!/bin/sh\n/bin/sleep {} &\nprintf '%s' \"$!\" > '{}'\nexit 0\n",
            DESCENDANT_LIFETIME_SECS,
            pid_path.display()
        ),
    );
    let timeout = LIFECYCLE_TEST_DEADLINE;
    let started = Instant::now();
    // The descendant is observed while the run is still in flight, so the
    // cleanup assertions below are always about a process that really existed.
    let command = [script.to_string_lossy().into_owned()];
    let (result, pid) = tokio::join!(
        run_bounded_process_async_for_test(&command, b"prompt", &root, timeout),
        wait_for_descendant_pid(&pid_path),
    );
    assert_eq!(result.unwrap_err(), AgentError::Timeout);
    assert!(started.elapsed() < timeout + PROCESS_CLEANUP_GRACE + Duration::from_millis(500));
    let mut alive = true;
    for _ in 0..50 {
        if unsafe { libc::kill(pid, 0) } == -1 {
            alive = false;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!alive);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn child_exit_kills_group_before_blocked_stdin_cleanup() {
    let root = std::env::temp_dir().join(format!("local-mcp-agent-exit-writer-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let pid_path = root.join("descendant.pid");
    let marker = root.join("descendant.marker");
    let release_path = root.join("descendant.release");
    // The descendant holds the inherited stdin open, so the launcher's write
    // cannot finish on its own. It only reaches the marker once this test
    // releases it, so the marker assertion below measures whether the group kill
    // landed instead of racing a fixed fixture delay.
    let script = temporary_script(
        &root,
        "exit-with-descendant.sh",
        &format!(
            "#!/bin/sh\nexec 3<&0\n/bin/sh -c 'while [ ! -e \"$1\" ]; do sleep 0.05; done; touch \"$2\"; sleep {}' sh '{}' '{}' <&3 &\nprintf '%s' \"$!\" > '{}'\nexit 0\n",
            DESCENDANT_LIFETIME_SECS,
            release_path.display(),
            marker.display(),
            pid_path.display()
        ),
    );
    let prompt = vec![b'x'; 1024 * 1024];
    let timeout = LIFECYCLE_TEST_DEADLINE;
    let started = Instant::now();
    // The launcher drives this run until its own deadline expires, then
    // terminates the group, so cleanup lands immediately after the deadline
    // rather than at the fixture's exit. Observing the pid concurrently is what
    // makes the causal ordering explicit instead of assumed.
    let command = [script.to_string_lossy().into_owned()];
    let (result, pid) = tokio::join!(
        run_bounded_host_process(&command, &prompt, &root, timeout),
        wait_for_descendant_pid(&pid_path),
    );
    assert_eq!(result.unwrap_err(), AgentError::Timeout);
    assert!(started.elapsed() < timeout + PROCESS_CLEANUP_GRACE + Duration::from_secs(1));
    let mut alive = true;
    for _ in 0..50 {
        if unsafe { libc::kill(pid, 0) } == -1 {
            alive = false;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !alive,
        "the group kill must reap the stdin-holding descendant"
    );
    // Release the gate. A descendant that outlived cleanup would now run its
    // work and create the marker, so its continued absence is a liveness proof
    // rather than an assumption.
    std::fs::write(&release_path, "released").unwrap();
    for _ in 0..50 {
        if marker.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !marker.exists(),
        "a released descendant must not outlive cleanup"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn bounded_process_maps_executable_and_spawn_failures() {
    assert_eq!(
        run_bounded_process_for_test(
            &["/definitely/not/a/codex-executable".into()],
            b"prompt",
            Duration::from_secs(2),
        )
        .unwrap_err(),
        AgentError::ExecutableUnavailable
    );
    #[cfg(unix)]
    assert_eq!(
        run_bounded_process_for_test(
            &[std::env::current_dir().unwrap().display().to_string()],
            b"prompt",
            Duration::from_secs(2),
        )
        .unwrap_err(),
        AgentError::SpawnFailed
    );
}

#[test]
fn model_config_and_approval_are_host_owned_and_typed() {
    assert_eq!(
        configured_model_from_for_test(Some("goal-model"), "fallback-model").unwrap(),
        "goal-model"
    );
    assert_eq!(
        configured_model_from_for_test(None, "fallback-model").unwrap(),
        "fallback-model"
    );
    assert_eq!(
        configured_model_from_for_test(Some(""), "fallback-model").unwrap_err(),
        AgentError::InvalidConfiguration
    );
    assert_eq!(
        configured_model_from_for_test(Some("bad\nmodel"), "fallback-model").unwrap_err(),
        AgentError::InvalidConfiguration
    );
    assert_eq!(
        require_approval_for_test(false),
        Err(AgentError::ApprovalDenied)
    );
    assert_eq!(require_approval_for_test(true), Ok(()));
}

#[test]
fn goal_read_only_command_allows_non_git_workspace_without_weakening_sandbox() {
    let cwd = PathBuf::from("/authoritative/non-git-goal");
    let command = crate::fallback::codex_read_only_command_with_model(
        &cwd,
        crate::fallback::Effort::Medium,
        "host-model",
    )
    .unwrap();

    assert!(command.contains(&"--skip-git-repo-check".to_owned()));
    assert!(command.windows(2).any(|pair| pair == ["-s", "read-only"]));
    assert!(command.contains(&"--ephemeral".to_owned()));
    assert!(command.contains(&"--ignore-user-config".to_owned()));
    assert!(!command.contains(&"--dangerously-bypass-approvals-and-sandbox".to_owned()));
    assert_eq!(command.last().map(String::as_str), Some("-"));
}

#[test]
fn model_bounds_and_read_only_command_are_finite_and_fixed() {
    assert_eq!(MODEL_STDOUT_LIMIT, 2 * 1024 * 1024);
    assert_eq!(MODEL_STDERR_LIMIT, 64 * 1024);
    assert_eq!(MODEL_PROMPT_LIMIT, 256 * 1024);
    let cwd = PathBuf::from("/authoritative/goal");
    let command = crate::fallback::codex_read_only_command_with_model(
        &cwd,
        crate::fallback::Effort::Medium,
        "host-model",
    )
    .unwrap();
    for flag in [
        "exec",
        "--ephemeral",
        "--ignore-user-config",
        "--skip-git-repo-check",
        "-s",
        "read-only",
        "-C",
    ] {
        assert!(command.contains(&flag.to_owned()));
    }
    assert!(command.windows(2).any(|pair| pair == ["-m", "host-model"]));
    assert!(
        command
            .windows(2)
            .any(|pair| pair == ["-C", "/authoritative/goal"])
    );
    assert_eq!(command.last().map(String::as_str), Some("-"));
}
