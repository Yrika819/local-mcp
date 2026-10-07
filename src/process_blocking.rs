//! Bounded, contained execution for the host-owned blocking Git seams.
//!
//! # Why these seams are separate
//!
//! `sandbox::run_unrestricted_clean_raw_with_limits` already runs trusted Git with
//! a `ProcessGroup` and a deadline, but it is asynchronous. Two Managed Worktrees
//! seams run Git from synchronous code that a runtime thread is blocked on:
//!
//! * `managed_worktree_observe::HostGit::run` — read-only observation, which fans
//!   out to roughly a dozen sequential Git children per repository observation,
//! * `managed_worktree_create::HostWorktreeCreator::create` — the **mutating**
//!   `git worktree add`.
//!
//! Both previously used a bare blocking `.output()`: no deadline, no containment,
//! and on Windows a raw `std::process::Command` that could not be cleaned up at
//! all. A hung Git therefore held a runtime thread forever, and the durable
//! managed creation sequence never reached reconciliation.
//!
//! # Why it is not just `ProcessGroup`
//!
//! The unreaped-leader ownership proof in `process_group` exists because a
//! process-group identifier is recycled once the leader is reaped. That proof
//! requires never reaping the leader while the identifier is still needed.
//!
//! Blocking `Command::output()` cannot satisfy that: it reaps the child before
//! returning, so by the time a timeout is noticed the identifier may already have
//! been recycled. Signalling a remembered identifier at that point would be
//! exactly the V2.1.1 defect this repository already repaired once.
//!
//! So this module never signals a remembered identifier after a reap:
//!
//! * The identifier is signalled **only** while the group leader is unreaped.
//!   While the leader is still running that is automatic; once it has exited, exit
//!   is observed with `waitid(WNOWAIT)`, which reports termination without
//!   consuming the status, so the leader stays unreaped and the kernel keeps the
//!   identifier reserved.
//! * The reap happens only after the group has been signalled, never before.
//!
//! That is the same rule the asynchronous path enforces, expressed for a
//! blocking caller, and it is why this seam can terminate a tree on ordinary
//! completion without reintroducing the recycled-identifier hazard.
//!
//! # Why the pipes are drained on their own threads
//!
//! `Command::output()` reads both pipes concurrently while waiting. A bounded
//! runner that polls for the deadline *without* draining would deadlock the child
//! the moment it wrote more than a pipe buffer — a `git status` on a large
//! checkout — and would then report a healthy command as timed out. The readers
//! below restore that concurrency, so a large but legitimate result completes
//! normally instead of being killed.

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(all(test, unix))]
#[derive(Debug, Default)]
pub(crate) struct ProcessTreeTestEvidence {
    pub(crate) leader_pid: Option<u32>,
    pub(crate) group_id: Option<i32>,
    pub(crate) ownership_observation: Option<String>,
    pub(crate) group_signal_result: Option<i32>,
    pub(crate) group_signal_errno: Option<i32>,
}

#[cfg(all(test, unix))]
std::thread_local! {
    static PROCESS_TREE_TEST_EVIDENCE: std::cell::RefCell<ProcessTreeTestEvidence> =
        std::cell::RefCell::new(ProcessTreeTestEvidence::default());
}

#[cfg(all(test, unix))]
pub(crate) fn take_process_tree_test_evidence() -> ProcessTreeTestEvidence {
    PROCESS_TREE_TEST_EVIDENCE.with(|evidence| std::mem::take(&mut *evidence.borrow_mut()))
}

#[cfg(all(test, unix))]
fn reset_process_tree_test_evidence() {
    PROCESS_TREE_TEST_EVIDENCE
        .with(|evidence| *evidence.borrow_mut() = ProcessTreeTestEvidence::default());
}

#[cfg(all(test, unix))]
fn record_process_tree_leader(pid: u32) {
    PROCESS_TREE_TEST_EVIDENCE.with(|evidence| {
        let mut evidence = evidence.borrow_mut();
        evidence.leader_pid = Some(pid);
        evidence.group_id = Some(pid as i32);
    });
}

#[cfg(all(test, unix))]
fn record_ownership_observation(observation: &io::Result<bool>) {
    let value = match observation {
        Ok(false) => "leader-running-and-unreaped".to_owned(),
        Ok(true) => "leader-exited-but-unreaped".to_owned(),
        Err(error) => format!("unproven: {error} (errno {:?})", error.raw_os_error()),
    };
    PROCESS_TREE_TEST_EVIDENCE.with(|evidence| {
        evidence.borrow_mut().ownership_observation = Some(value);
    });
}

#[cfg(all(test, unix))]
fn record_group_signal(result: i32, errno: Option<i32>) {
    PROCESS_TREE_TEST_EVIDENCE.with(|evidence| {
        let mut evidence = evidence.borrow_mut();
        evidence.group_signal_result = Some(result);
        evidence.group_signal_errno = errno;
    });
}

/// First poll interval while waiting for a bounded blocking child.
///
/// Short, because most host-owned Git children finish in milliseconds and the
/// caller is blocked on this thread.
const POLL_START: Duration = Duration::from_micros(200);

/// Upper bound on the blocking poll interval.
const POLL_MAX: Duration = Duration::from_millis(5);

/// How long a reader thread is given to reach EOF after the tree is terminated.
///
/// Bounds the one part of this function that could otherwise wait on a process
/// outside this process's control: a descendant that inherited a pipe and did not
/// die. When the grace expires the bytes read so far are still returned, and the
/// leak is the thread's, not a hang of the caller.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Maximum time spent observing reaping after SIGKILL/Job termination.
///
/// Kernel teardown is normally immediate, but a host must never block forever
/// waiting for an uninterruptible process or a broken OS wait implementation.
const REAP_GRACE: Duration = Duration::from_secs(5);

/// Blocking trusted-Git output limits. Match the stricter trusted-Git capture
/// contract rather than borrowing the larger generic execute budgets.
const STDOUT_LIMIT: usize = crate::resource_limits::TRUSTED_GIT_STDOUT_LIMIT;
const STDERR_LIMIT: usize = crate::resource_limits::TRUSTED_GIT_STDERR_LIMIT;

/// Exit status used when the child's own status could not be observed.
///
/// Deliberately **not** zero. A status of zero reads as `success()`, which would
/// let a mutating caller pair "timed out" with "succeeded" and infer that the side
/// effect did not happen. A non-zero status makes that combination impossible to
/// construct by accident, which is the whole point of keeping the outcome unknown.
#[cfg(unix)]
const UNOBSERVED_RAW: i32 = 1 << 8;
#[cfg(windows)]
const UNOBSERVED_RAW: u32 = u32::MAX;

/// Bounded output of a host-owned blocking command.
pub(crate) struct BlockingOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    /// Whether the deadline expired and the process tree was terminated.
    ///
    /// This is unknown lifecycle evidence, never a "did not run" verdict: the
    /// command was started and may have performed side effects before the
    /// deadline. Callers keep their existing fail-closed classification.
    pub(crate) timed_out: bool,
    /// A stream exceeded its trusted-Git cap or did not reach EOF before the
    /// bounded drain grace. Callers must reject the result rather than parse a
    /// truncated prefix as a complete observation.
    pub(crate) capture_incomplete: bool,
    /// Output exceeded the trusted-Git bound and the process tree was terminated.
    pub(crate) output_overflow: bool,
}

/// Run a host-owned command to completion under a deadline, containing its tree.
///
/// The command is spawned into its own process group on Unix, or a Job Object on
/// Windows, so descendants cannot survive ordinary completion or the deadline.
/// Both pipes are drained concurrently so a result larger than a pipe buffer
/// completes instead of deadlocking.
pub(crate) fn run_bounded_blocking(
    command: &mut Command,
    timeout: Duration,
) -> io::Result<BlockingOutput> {
    run_bounded_blocking_with_limits(command, timeout, STDOUT_LIMIT, STDERR_LIMIT)
}

pub(crate) fn run_bounded_blocking_with_limits(
    command: &mut Command,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> io::Result<BlockingOutput> {
    #[cfg(all(test, unix))]
    reset_process_tree_test_evidence();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    #[cfg(windows)]
    use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
    #[cfg(windows)]
    let mut job = crate::process_job::Job::create()?;
    #[cfg(windows)]
    command.creation_flags(CREATE_SUSPENDED);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A fresh group whose identifier is the child's own process identifier, so
        // no process outside this tree can ever be a member.
        command.process_group(0);
    }

    let mut child = spawn_ready(command)?;
    #[cfg(all(test, unix))]
    record_process_tree_leader(child.id());
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let pid = child.id();
        if let Err(error) = job.assign(
            child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            pid,
        ) {
            // The child has not run because it is still suspended. Assignment
            // failure is terminal; kill the one known child and never resume it
            // uncontained.
            let _ = child.kill();
            let _ = wait_after_termination(&mut child);
            return Err(error);
        }
        if let Err(error) = crate::process_job::Job::resume(pid) {
            job.terminate();
            let _ = child.kill();
            let _ = wait_after_termination(&mut child);
            return Err(error);
        }
    }
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let output_overflow = Arc::new(AtomicBool::new(false));
    let stdout_reader = match spawn_reader(stdout, stdout_limit, Arc::clone(&output_overflow)) {
        Ok(reader) => reader,
        Err(error) => {
            #[cfg(unix)]
            {
                terminate_tree(&mut child);
                let _ = wait_after_termination(&mut child);
            }
            #[cfg(windows)]
            {
                job.terminate();
                let _ = child.kill();
                let _ = wait_after_termination(&mut child);
            }
            return Err(error);
        }
    };
    let stderr_reader = match spawn_reader(stderr, stderr_limit, Arc::clone(&output_overflow)) {
        Ok(reader) => reader,
        Err(error) => {
            #[cfg(unix)]
            {
                terminate_tree(&mut child);
                let _ = wait_after_termination(&mut child);
            }
            #[cfg(windows)]
            {
                job.terminate();
                let _ = child.kill();
                let _ = wait_after_termination(&mut child);
            }
            let _ = join_reader_bounded(stdout_reader, DRAIN_GRACE);
            return Err(error);
        }
    };

    #[cfg(windows)]
    let (status, timed_out, _wait_reported_overflow) =
        wait_bounded(&mut child, timeout, &job, &output_overflow);
    #[cfg(unix)]
    let (status, timed_out, _wait_reported_overflow) =
        wait_bounded(&mut child, timeout, &output_overflow);

    // A reader only ends at EOF, which requires **every** holder of the write end
    // to be gone — not merely the leader. A descendant that inherited the pipe and
    // outlived the leader would therefore keep the join blocked forever, which is
    // the very "a hung Git holds a runtime thread indefinitely" failure this seam
    // exists to prevent. `wait_bounded` has already terminated the tree, so the
    // only remaining possibility is a descendant that escaped containment; the
    // join is given a bounded grace and then abandoned rather than waited on.
    let (stdout, stdout_incomplete) = join_reader_bounded(stdout_reader, DRAIN_GRACE);
    let (stderr, stderr_incomplete) = join_reader_bounded(stderr_reader, DRAIN_GRACE);
    // A fast leader may exit before the reader thread consumes the final byte
    // beyond the cap. The wait loop's snapshot can therefore be stale even though
    // the bounded readers have now completed. Read the flag only after joining
    // them so output overflow cannot be accepted as a complete observation.
    let output_overflowed = output_overflow.load(Ordering::Acquire);

    Ok(BlockingOutput {
        status,
        stdout,
        stderr,
        timed_out,
        capture_incomplete: stdout_incomplete || stderr_incomplete,
        output_overflow: output_overflowed,
    })
}

/// Spawn, tolerating only a momentarily busy executable.
///
/// Matches the asynchronous path: a busy binary is retried, and nothing else is.
fn spawn_ready(command: &mut Command) -> io::Result<Child> {
    let retry = crate::exec_ready::BusyProgramRetry::new();
    loop {
        match command.spawn() {
            Err(error) if retry.retry(&error) => {}
            result => return result,
        }
    }
}

/// Drain one pipe on its own thread so the child never blocks on a full pipe.
fn spawn_reader<R>(
    reader: Option<R>,
    limit: usize,
    output_overflow: Arc<AtomicBool>,
) -> io::Result<JoinHandle<(Vec<u8>, bool)>>
where
    R: Read + Send + 'static,
{
    std::thread::Builder::new().spawn(move || {
        let Some(mut reader) = reader else {
            return (Vec::new(), true);
        };
        let mut buffer = Vec::with_capacity(limit.min(8192));
        // Read at most limit + 1 so over-limit output is detected without ever
        // retaining more than one byte beyond the configured cap.
        let read_result = reader
            .by_ref()
            .take(u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX))
            .read_to_end(&mut buffer);
        let over_limit = buffer.len() > limit;
        if over_limit {
            output_overflow.store(true, Ordering::Release);
        }
        let incomplete = read_result.is_err() || over_limit;
        buffer.truncate(limit);
        (buffer, incomplete)
    })
}

/// Collect a reader's bytes, giving up after `grace`.
///
/// A panicking reader yields an incomplete empty stream rather than taking down
/// the caller or letting a missing capture be accepted as a complete observation.
fn join_reader_bounded(handle: JoinHandle<(Vec<u8>, bool)>, grace: Duration) -> (Vec<u8>, bool) {
    let deadline = Instant::now() + grace;
    let handle = Some(handle);
    while Instant::now() < deadline && !handle.as_ref().is_some_and(JoinHandle::is_finished) {
        std::thread::sleep(Duration::from_millis(1));
    }
    match handle {
        // The thread finished, so joining is immediate and cannot block.
        Some(handle) if handle.is_finished() => handle.join().unwrap_or((Vec::new(), true)),
        // Still holding the pipe: detach rather than block the caller forever.
        // `JoinHandle`'s `Drop` already detaches, and the OS reclaims the thread
        // and its buffers when the pipe finally closes.
        _ => (Vec::new(), true),
    }
}

/// Wait for `child`, terminating its tree when the leader exits or the deadline
/// expires.
///
/// Returns the exit status and whether the deadline expired.
///
/// # Why the group is signalled even on ordinary completion
///
/// A command may background a descendant and exit immediately. Leaving that
/// descendant running would make this seam weaker than the asynchronous path,
/// which terminates the group once the leader is gone. So the group is signalled
/// on **both** paths.
///
/// # Why that is still safe
///
/// The group identifier is only signalled while the leader is **unreaped**, which
/// is exactly the window in which the kernel cannot have recycled the identifier
/// to an unrelated process group. On the timeout path the child is still running,
/// so it is trivially unreaped. On the completion path the leader has exited but
/// has deliberately *not* been reaped: exit is observed with
/// `waitid(WNOWAIT)`, which reports a child's termination without consuming its
/// status. The identifier is therefore still reserved when the group is signalled,
/// and the reap happens only afterwards.
///
/// Reaping first and then signalling a remembered identifier is precisely the
/// V2.1.1 hazard, and it is why this module observes exit without consuming it.
#[cfg(unix)]
fn wait_bounded(
    child: &mut Child,
    timeout: Duration,
    output_overflow: &AtomicBool,
) -> (ExitStatus, bool, bool) {
    use std::os::unix::process::ExitStatusExt;

    let deadline = Instant::now() + timeout;
    let mut interval = POLL_START;
    loop {
        if output_overflow.load(Ordering::Acquire) {
            let observation = has_exited_unreaped(child.id());
            #[cfg(test)]
            record_ownership_observation(&observation);
            match observation {
                Ok(_) => {
                    terminate_tree(child);
                    return (wait_after_termination(child), false, true);
                }
                Err(_) => {
                    // The leader may already have been reaped by an external
                    // SIGCHLD handler. Its remembered PID is no longer a safe
                    // signal target, so fail closed without signalling it.
                    return (ExitStatus::from_raw(UNOBSERVED_RAW), true, true);
                }
            }
        }
        let observation = has_exited_unreaped(child.id());
        #[cfg(test)]
        record_ownership_observation(&observation);
        match observation {
            // The leader is gone but unreaped, so the group identifier is still
            // reserved and the group may be signalled.
            Ok(true) => {
                let overflowed = output_overflow.load(Ordering::Acquire);
                terminate_tree(child);
                return (wait_after_termination(child), false, overflowed);
            }
            Ok(false) if output_overflow.load(Ordering::Acquire) => {
                terminate_tree(child);
                return (wait_after_termination(child), false, true);
            }
            Ok(false) => {}
            // The child can no longer be observed, so there is **no** ownership
            // proof for either the group identifier or the remembered direct-child
            // PID: an auto-reaping host (SIGCHLD ignored, SA_NOCLDWAIT, or a
            // `waitpid(-1)` sweep elsewhere) may already have recycled them. Do
            // not signal either identifier; return an unknown outcome and preserve
            // fail-closed retry semantics.
            Err(_) => return (ExitStatus::from_raw(UNOBSERVED_RAW), true, false),
        }
        if Instant::now() >= deadline {
            // Still running, therefore still unreaped: the identifier is ours.
            terminate_tree(child);
            return (wait_after_termination(child), true, false);
        }
        std::thread::sleep(interval);
        interval = std::cmp::min(interval * 2, POLL_MAX);
    }
}

/// Wait boundedly for a child after its group has already been terminated.
///
/// At this point the group signal has already been sent while the ownership proof
/// held. Reaping is safe, but waiting is bounded so an uninterruptible kernel task
/// cannot hang the server forever.
#[cfg(unix)]
fn wait_after_termination(child: &mut Child) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;

    let deadline = Instant::now() + REAP_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_MAX),
            Ok(None) | Err(_) => return ExitStatus::from_raw(UNOBSERVED_RAW),
        }
    }
}

/// Report whether `pid` has terminated **without reaping it**.
///
/// `WNOWAIT` leaves the exit status uncollected, so the kernel keeps the process
/// identifier reserved and it cannot be handed to another process. That is the
/// ownership proof the group signal depends on.
///
/// `EINTR` is retried rather than reported: an interrupted query says nothing
/// about the child, and treating it as "unobservable" would stop a perfectly
/// healthy command and report it as a timeout.
#[cfg(unix)]
fn has_exited_unreaped(pid: u32) -> io::Result<bool> {
    use std::io::ErrorKind;

    if pid == 0 {
        return Ok(false);
    }
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // Safety: `waitid` only writes into the `siginfo_t` it is handed, and
        // `P_PID` restricts the query to a single direct child of this process.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        if result != -1 {
            // `libc` exposes the same union as plain fields on Apple and as
            // accessors elsewhere, so the report is read through a helper.
            return Ok(reported_child(&info) != 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() == ErrorKind::Interrupted {
            continue;
        }
        return Err(error);
    }
}

/// The child a termination report refers to, or `0` when nothing was reported.
#[cfg(target_os = "macos")]
fn reported_child(info: &libc::siginfo_t) -> libc::pid_t {
    info.si_pid
}

/// The child a termination report refers to, or `0` when nothing was reported.
#[cfg(all(unix, not(target_os = "macos")))]
fn reported_child(info: &libc::siginfo_t) -> libc::pid_t {
    // Safety: reading a field of a `siginfo_t` that `waitid` has just filled in.
    unsafe { info.si_pid() }
}

/// Wait for `child`, terminating it if `timeout` expires.
///
/// Windows has no process group. Containment comes from the Job Object assigned
/// before the suspended child is resumed. The Job is terminated on both normal
/// completion and timeout, so a Git descendant cannot survive either path.
#[cfg(windows)]
fn wait_bounded(
    child: &mut Child,
    timeout: Duration,
    job: &crate::process_job::Job,
    output_overflow: &AtomicBool,
) -> (ExitStatus, bool, bool) {
    let deadline = Instant::now() + timeout;
    let mut interval = POLL_START;
    loop {
        if output_overflow.load(Ordering::Acquire) {
            job.terminate();
            let _ = child.kill();
            return (wait_after_termination(child), false, true);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // A command leader can exit while a descendant still runs. Kill
                // the Job on normal completion too, before the parent begins
                // joining output readers.
                job.terminate();
                return (status, false, false);
            }
            Ok(None) => {}
            Err(_) => {
                job.terminate();
                let _ = child.kill();
                return (wait_after_termination(child), true, false);
            }
        }
        if Instant::now() >= deadline {
            job.terminate();
            let _ = child.kill();
            return (wait_after_termination(child), true, false);
        }
        std::thread::sleep(interval);
        interval = std::cmp::min(interval * 2, POLL_MAX);
    }
}

/// Wait boundedly for a Windows child after its Job has already been terminated.
#[cfg(windows)]
fn wait_after_termination(child: &mut Child) -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;

    let deadline = Instant::now() + REAP_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_MAX),
            Ok(None) | Err(_) => return ExitStatus::from_raw(UNOBSERVED_RAW),
        }
    }
}

/// Terminate the child's whole process group.
///
/// Called only while the group leader is unreaped — either still running, or
/// exited but observed with `WNOWAIT` — so the group identifier is still reserved
/// by that leader and cannot have been recycled to another process group.
#[cfg(unix)]
fn terminate_tree(child: &mut Child) {
    let pid = child.id();
    if pid == 0 {
        return;
    }
    // Safety: the leader is unreaped and still running, so its process identifier
    // — which is also the group identifier — has not been returned to the kernel,
    // and the group provably contains only this tree.
    let result = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    #[cfg(test)]
    record_group_signal(
        result,
        (result == -1).then(|| {
            io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or_default()
        }),
    );
    #[cfg(not(test))]
    let _ = result;
}

#[cfg(test)]
mod tests {
    use super::join_reader_bounded;

    #[test]
    fn panicking_reader_is_marked_incomplete() {
        let reader = std::thread::spawn(|| -> (Vec<u8>, bool) { panic!("reader failure") });
        assert_eq!(
            join_reader_bounded(reader, std::time::Duration::from_secs(1)),
            (Vec::new(), true)
        );
    }
}
