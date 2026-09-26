//! Process-group ownership for every child process Local MCP launches.
//!
//! # Why a lease instead of a raw identifier
//!
//! Every Unix child Local MCP launches is placed in its own process group and is
//! terminated as a group, so that descendants cannot survive a timeout, a
//! cancellation or a failed run. Terminating a group means
//! `kill(-<pgid>, SIGKILL)`.
//!
//! A process-group identifier is a process identifier, and the kernel is free to
//! hand an identifier back out once the process that owned it has been reaped.
//! V2.1.1 stored the raw group identifier and issued the group signal
//! unconditionally from `Drop`, so once the group leader had been reaped the
//! guard could signal a group that no longer belonged to the tree Local MCP
//! launched.
//!
//! The invariant enforced here is:
//!
//! > A negative process-group signal may be issued **only** while Local MCP
//! > still holds a host-owned proof that the group identifier belongs to the
//! > process tree it launched.
//!
//! # The proof: an unreaped group owner
//!
//! The group leader is this process's own direct child, and a process
//! identifier is not recycled while the process has not been reaped. This module
//! therefore never reaps the leader: group termination is observed with
//! `waitid(WEXITED | WNOWAIT | WNOHANG)`, which reports a leader's exit without
//! consuming its status, and the leader stays unreaped for the whole lifetime of
//! the lease. While that unreaped leader exists the kernel cannot assign its
//! identifier — and therefore cannot assign the group identifier, which is the
//! same number — to any other process.
//!
//! This gives the proof an explicit and bounded lifetime: it begins when the
//! child is spawned and ends when the child handle is dropped, because dropping
//! the handle is what lets the leader be reaped. Every group signal in this
//! module is issued before that point and never after it.
//!
//! The semantics are identical on Linux and on macOS. POSIX requires an exited
//! child to stay in the terminated state until it is waited for, and both
//! kernels keep the process identifier reserved for exactly that interval, so an
//! unreaped leader is a portable ownership proof rather than a platform
//! accident.
//!
//! # Why this costs nothing
//!
//! Holding the leader unreaped costs one zombie per in-flight child, and no
//! more: the zombie never outlives the lease, because the child handle is dropped
//! at the end of every scope that holds it and the runtime reaps the leader
//! then. No zombie accumulates, no extra supervisor process is created, and
//! descendant cleanup is unchanged: the group is still signalled on timeout, on
//! failure and on cancellation, including after the leader itself has exited.

use std::io;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

/// First poll interval used when waiting for a group leader to terminate.
///
/// The leader is reaped by the runtime only after this lease is dropped, so
/// waiting for it is a poll rather than a signal-driven wait. The interval
/// starts small so a short-lived child is observed almost immediately, then
/// grows so a long-running child costs very few wakeups.
const TERMINATION_POLL_START: Duration = Duration::from_micros(200);

/// Upper bound on the termination poll interval.
const TERMINATION_POLL_MAX: Duration = Duration::from_millis(4);

/// How long a start is retried while the requested executable is momentarily
/// still open for writing somewhere else.
#[cfg(unix)]
const EXEC_BUSY_RETRY_BUDGET: Duration = Duration::from_millis(100);

/// Gap between retries of a momentarily busy executable.
#[cfg(unix)]
const EXEC_BUSY_RETRY_INTERVAL: Duration = Duration::from_millis(1);

/// A launched process group whose identifier Local MCP owns for the lifetime of
/// this value.
///
/// This value *is* the ownership proof. While it exists, the group leader is an
/// unreaped child of this process, so the group identifier cannot have been
/// recycled and the group may be signalled. When it is dropped the leader
/// becomes reapable and the identifier is released to the kernel, so no signal
/// is ever sent for the group afterwards.
pub(crate) struct ProcessGroup {
    child: Child,
    #[cfg(unix)]
    ownership: ProcessGroupOwnership,
}

impl ProcessGroup {
    /// Spawn `command` as the leader of a new process group owned by this lease.
    ///
    /// The child is placed in a fresh group whose identifier is the leader's own
    /// process identifier, so no process outside this tree can be a member.
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        command.process_group(0);
        let child = spawn_ready_to_exec(command)?;
        #[cfg(unix)]
        let ownership = ProcessGroupOwnership::held(child.id().unwrap_or(0) as libc::pid_t);
        Ok(Self {
            child,
            #[cfg(unix)]
            ownership,
        })
    }

    /// The group leader's process identifier, which is also the group identifier.
    #[cfg(test)]
    pub(crate) fn leader_id(&self) -> u32 {
        self.child.id().unwrap_or(0)
    }

    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub(crate) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// Terminate the whole process group, then the group leader itself.
    ///
    /// The group signal is only issued while this lease still holds its
    /// ownership proof. The leader is then signalled by its own process
    /// identifier, which needs no proof because the leader is this process's
    /// direct child.
    ///
    /// Calling this repeatedly is correct and intended: the proof is held until
    /// the leader is reaped, so every call lands on the group this lease
    /// launched.
    pub(crate) fn terminate(&mut self) {
        #[cfg(unix)]
        self.ownership.terminate_group();
        let _ = self.child.start_kill();
    }

    /// Wait for the group leader to terminate, without reaping it.
    ///
    /// Returns the leader's exit status, reconstructed from the termination
    /// report so the status is never consumed. The leader therefore stays
    /// unreaped, this lease keeps its ownership proof, and the leader is
    /// reaped when the child handle is dropped.
    pub(crate) async fn wait_termination(&mut self) -> io::Result<ExitStatus> {
        #[cfg(unix)]
        {
            wait_for_termination_unreaped(self.ownership.group_id).await
        }
        #[cfg(not(unix))]
        {
            self.child.wait().await
        }
    }

    /// Capture the complete output of the group and terminate it.
    ///
    /// Equivalent to waiting for a child with its output, except that
    /// termination is observed without reaping, so the group can still be
    /// signalled afterwards.
    pub(crate) async fn terminate_and_capture(&mut self) -> io::Result<CapturedOutput> {
        drop(self.child.stdin.take());
        let mut stdout = self.child.stdout.take();
        let mut stderr = self.child.stderr.take();
        let (stdout, stderr) = tokio::join!(drain(stdout.as_mut()), drain(stderr.as_mut()));
        let status = self.wait_termination().await?;
        self.terminate();
        Ok(CapturedOutput {
            status,
            stdout: stdout.unwrap_or_default(),
            stderr: stderr.unwrap_or_default(),
        })
    }

    /// Whether a negative process-group signal is currently permitted.
    #[cfg(test)]
    pub(crate) fn owns_process_group(&self) -> bool {
        self.ownership.is_proven()
    }

    /// Relinquish the process-group ownership proof without ending the lease.
    ///
    /// Models the moment the group identifier stops being this lease's, and is
    /// used by the ownership regression to show that a group signal is gated on
    /// the proof rather than on the lease still being alive.
    #[cfg(test)]
    pub(crate) fn release_process_group(&mut self) {
        self.ownership.release();
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // This runs before the child handle is dropped, so the leader is still
        // unreaped and the ownership proof is still valid here. It is the last
        // moment at which a group signal may be issued; once this returns, the
        // leader is reapable and the identifier belongs to the kernel again.
        #[cfg(unix)]
        self.ownership.terminate_group();
        let _ = self.child.start_kill();
    }
}

async fn drain<R>(reader: Option<&mut R>) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = Vec::new();
    match reader {
        Some(reader) => reader.read_to_end(&mut buffer).await.map(|_| buffer),
        None => Ok(buffer),
    }
}

/// Start the command, tolerating a momentarily busy executable.
///
/// The kernel refuses to `execve` a file that some process still has open for
/// writing, and reports it as `ETXTBSY`. That refusal is transient and
/// side-effect free: no process is started and nothing about the requested
/// command is decided, so retrying cannot double-start anything and cannot turn
/// a refusal into a weaker one. It is worth tolerating because writing an
/// executable and then running it is an ordinary thing to do — a caller may
/// legitimately hand Local MCP a script that was just written — and because a
/// busy executable must not be reported as a missing or unusable one.
///
/// Only `ETXTBSY` is retried. Every other error, including a missing or
/// non-executable program, is returned immediately and unchanged, so a genuinely
/// unusable command still fails closed and is still classified as pre-start.
///
/// The retry is bounded by [`EXEC_BUSY_RETRY_BUDGET`], so a permanently busy
/// executable fails as before, just after that budget instead of instantly.
fn spawn_ready_to_exec(command: &mut Command) -> io::Result<Child> {
    #[cfg(not(unix))]
    {
        command.spawn()
    }
    #[cfg(unix)]
    {
        let deadline = std::time::Instant::now() + EXEC_BUSY_RETRY_BUDGET;
        loop {
            match command.spawn() {
                Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(error);
                    }
                    std::thread::sleep(EXEC_BUSY_RETRY_INTERVAL);
                }
                result => return result,
            }
        }
    }
}

/// Bounded output of a terminated process group.
pub(crate) struct CapturedOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// Host-owned proof that a process-group identifier still belongs to the process
/// tree this process launched.
///
/// The proof is deliberately explicit rather than implied by a stored number: a
/// group identifier is only signalled through this type, and the proof can only
/// be acquired by spawning the group and is released when the group leader is
/// reaped.
#[cfg(unix)]
pub(crate) struct ProcessGroupOwnership {
    group_id: libc::pid_t,
    /// Whether this process still holds an unreaped group leader, and therefore
    /// whether the kernel can be relied on to still associate `group_id` with
    /// this process's own tree.
    ///
    /// `true` from spawn until the child handle is dropped.
    proven: bool,
}

#[cfg(unix)]
impl ProcessGroupOwnership {
    fn held(group_id: libc::pid_t) -> Self {
        Self {
            group_id,
            proven: true,
        }
    }

    /// Whether a negative process-group signal is currently permitted.
    pub(crate) fn is_proven(&self) -> bool {
        self.proven && self.group_id > 0
    }

    /// Terminate the owned process group, if ownership is still proven.
    fn terminate_group(&mut self) {
        if !self.is_proven() {
            return;
        }
        // Safety: `kill` is always safe to call. The signal is only sent because
        // this lease still holds an unreaped child whose process identifier is
        // the group identifier, so the kernel cannot have reassigned that
        // identifier to an unrelated process group.
        let _ = unsafe { libc::kill(-self.group_id, libc::SIGKILL) };
    }

    /// Give up the ownership proof.
    ///
    /// After this the group identifier is treated as no longer owned, and no
    /// signal is ever issued for it again.
    #[cfg(test)]
    pub(crate) fn release(&mut self) {
        self.proven = false;
    }

    /// A bare group identifier with no ownership proof at all.
    ///
    /// This is the post-reap hazard in isolation: an identifier that this
    /// process may or may not still own. It exists so the ownership regression
    /// can present the guard with a real, controlled process group without
    /// having to win a PID-reuse race first.
    #[cfg(test)]
    pub(crate) fn unproven_for_test(group_id: u32) -> Self {
        Self {
            group_id: group_id as libc::pid_t,
            proven: false,
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupOwnership {
    fn drop(&mut self) {
        // Belt and braces: the proof cannot outlive the value that grants it.
        self.proven = false;
    }
}

/// Poll until the group leader has terminated, leaving it unreaped.
#[cfg(unix)]
async fn wait_for_termination_unreaped(group_id: libc::pid_t) -> io::Result<ExitStatus> {
    let mut interval = TERMINATION_POLL_START;
    loop {
        if let Some(status) = termination_status(group_id)? {
            return Ok(status);
        }
        tokio::time::sleep(interval).await;
        interval = std::cmp::min(interval * 2, TERMINATION_POLL_MAX);
    }
}

/// Report the leader's exit status if it has terminated, without reaping it.
#[cfg(unix)]
fn termination_status(group_id: libc::pid_t) -> io::Result<Option<ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;

    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // Safety: `waitid` only writes into the `siginfo_t` it is handed, and
    // `P_PID` restricts the query to a single direct child of this process.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            group_id as libc::id_t,
            &raw mut info,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    if reported_child(&info) == 0 {
        return Ok(None);
    }
    // `WEXITED` restricts the report to termination, so the leader's exit status
    // is still waiting to be collected and `WNOWAIT` has left it untouched. The
    // raw wait status is rebuilt from the termination report so callers observe
    // the same `ExitStatus` a reap would have produced.
    let raw = match info.si_code {
        libc::CLD_EXITED => reported_status(&info) << 8,
        libc::CLD_KILLED | libc::CLD_DUMPED => reported_status(&info),
        // Unreachable with `WEXITED`, which reports only exit, kill and dump.
        _ => reported_status(&info),
    };
    Ok(Some(ExitStatus::from_raw(raw)))
}

/// The child the termination report refers to, or `0` when nothing was reported.
///
/// `libc` exposes the same C union either as plain fields (Apple targets) or as
/// accessor methods (Linux and most other targets), so the report is read
/// through this helper rather than through the type directly.
#[cfg(target_os = "macos")]
fn reported_child(info: &libc::siginfo_t) -> libc::pid_t {
    info.si_pid
}

#[cfg(not(target_os = "macos"))]
fn reported_child(info: &libc::siginfo_t) -> libc::pid_t {
    // Safety: reading a field of a `siginfo_t` that `waitid` has just filled in.
    unsafe { info.si_pid() }
}

/// The exit status or terminating signal carried by a termination report.
#[cfg(target_os = "macos")]
fn reported_status(info: &libc::siginfo_t) -> libc::c_int {
    info.si_status
}

#[cfg(not(target_os = "macos"))]
fn reported_status(info: &libc::siginfo_t) -> libc::c_int {
    // Safety: reading a field of a `siginfo_t` that `waitid` has just filled in.
    unsafe { info.si_status() }
}
