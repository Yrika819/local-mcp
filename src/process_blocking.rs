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
//! * The identifier is signalled **only** while [`std::process::Child::try_wait`]
//!   still reports the child as running. That is exactly the window in which the
//!   leader is unreaped and the identifier provably still belongs to this tree.
//! * Once the child has been observed to have exited, the result is returned
//!   without any signal at all.
//!
//! That is the same rule the asynchronous path enforces, expressed for a
//! blocking caller.
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
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// First poll interval while waiting for a bounded blocking child.
///
/// Short, because most host-owned Git children finish in milliseconds and the
/// caller is blocked on this thread.
const POLL_START: Duration = Duration::from_micros(200);

/// Upper bound on the blocking poll interval.
const POLL_MAX: Duration = Duration::from_millis(5);

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
}

/// Run a host-owned command to completion under a deadline, containing its tree.
///
/// The command is spawned into its own process group on Unix so a descendant
/// cannot survive it, and the whole group is terminated if the deadline expires.
/// Both pipes are drained concurrently so a result larger than a pipe buffer
/// completes instead of deadlocking.
pub(crate) fn run_bounded_blocking(
    command: &mut Command,
    timeout: Duration,
) -> io::Result<BlockingOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A fresh group whose identifier is the child's own process identifier, so
        // no process outside this tree can ever be a member.
        command.process_group(0);
    }

    let mut child = spawn_ready(command)?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = spawn_reader(stdout);
    let stderr_reader = spawn_reader(stderr);

    let (status, timed_out) = wait_bounded(&mut child, timeout);
    // The readers own the pipe ends, so they only end at EOF. After a terminated
    // or exited child that is immediate; joining cannot hang indefinitely.
    let stdout = join_reader(stdout_reader);
    let stderr = join_reader(stderr_reader);

    Ok(BlockingOutput {
        status,
        stdout,
        stderr,
        timed_out,
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
fn spawn_reader<R>(reader: Option<R>) -> JoinHandle<Vec<u8>>
where
    R: Read + Send + 'static,
{
    let handle = std::thread::spawn(move || {
        let Some(mut reader) = reader else {
            return Vec::new();
        };
        let mut buffer = Vec::new();
        // A read failure ends this stream only. The exit status and the other
        // stream still decide the outcome, so it is not an execution failure.
        let _ = reader.read_to_end(&mut buffer);
        buffer
    });
    handle
}

/// Collect a reader's bytes. A panicking reader yields an empty stream rather
/// than taking down the caller, matching the tolerance above.
fn join_reader(handle: JoinHandle<Vec<u8>>) -> Vec<u8> {
    handle.join().unwrap_or_default()
}

/// Wait for `child`, terminating its tree if `timeout` expires.
///
/// Returns the exit status and whether the deadline expired. The group signal is
/// issued only while `try_wait` still reports the child as running, which is
/// precisely the window in which the identifier still belongs to this tree.
#[cfg(unix)]
fn wait_bounded(child: &mut Child, timeout: Duration) -> (ExitStatus, bool) {
    use std::os::unix::process::ExitStatusExt;

    let deadline = Instant::now() + timeout;
    let mut interval = POLL_START;
    let status = loop {
        // Safety: `try_wait` only observes. A `Some` result means the child has
        // exited and been reaped, and the loop returns there without ever
        // signalling the identifier afterwards.
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            // The child can no longer be observed. Treat the deadline as expired
            // and terminate the tree rather than leaving it unowned.
            Err(_) => {
                terminate_tree(child);
                return (ExitStatus::from_raw(0), true);
            }
        }
        if Instant::now() >= deadline {
            terminate_tree(child);
            return match child.wait() {
                Ok(status) => (status, true),
                Err(_) => (ExitStatus::from_raw(0), true),
            };
        }
        std::thread::sleep(interval);
        interval = std::cmp::min(interval * 2, POLL_MAX);
    };
    (status, false)
}

/// Wait for `child`, terminating it if `timeout` expires.
///
/// Windows has no process group. Containment for the asynchronous paths comes
/// from the Job Object in [`crate::process_job`]; these blocking seams are
/// host-owned, argv-allowlisted Git with no descendant-bearing argv, so a bounded
/// direct termination is the whole guarantee here.
#[cfg(windows)]
fn wait_bounded(child: &mut Child, timeout: Duration) -> (ExitStatus, bool) {
    use std::os::windows::process::ExitStatusExt;

    let deadline = Instant::now() + timeout;
    let mut interval = POLL_START;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (status, false),
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                return (ExitStatus::from_raw(0), true);
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return match child.wait() {
                Ok(status) => (status, true),
                Err(_) => (ExitStatus::from_raw(0), true),
            };
        }
        std::thread::sleep(interval);
        interval = std::cmp::min(interval * 2, POLL_MAX);
    }
}

/// Terminate the child's whole process group.
///
/// Called only while the child is known to still be running, so the group
/// identifier is still reserved by an unreaped leader and cannot have been
/// recycled to another process group.
#[cfg(unix)]
fn terminate_tree(child: &mut Child) {
    let pid = child.id();
    if pid == 0 {
        return;
    }
    // Safety: the leader is unreaped and still running, so its process identifier
    // — which is also the group identifier — has not been returned to the kernel,
    // and the group provably contains only this tree.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}
