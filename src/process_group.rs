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
#[cfg(unix)]
use std::time::Duration;

use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::exec_ready::BusyProgramRetry;
#[cfg(windows)]
use crate::process_job::Job;
#[cfg(windows)]
use windows_sys::Win32::Foundation::HANDLE;

/// First poll interval used when waiting for a group leader to terminate.
///
/// The leader is reaped by the runtime only after this lease is dropped, so
/// waiting for it is a poll rather than a signal-driven wait. The interval
/// starts small so a short-lived child is observed almost immediately, then
/// grows so a long-running child costs very few wakeups.
#[cfg(unix)]
const TERMINATION_POLL_START: Duration = Duration::from_micros(200);

/// Upper bound on the termination poll interval.
#[cfg(unix)]
const TERMINATION_POLL_MAX: Duration = Duration::from_millis(4);

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
    /// Windows tree containment. A ProcessGroup is constructed only if the Job
    /// assignment succeeded, so this is always `Some` on a live Windows lease.
    #[cfg(windows)]
    job: Option<Job>,
}

#[cfg(unix)]
pub(crate) fn normalize_child_signal_policy() -> io::Result<()> {
    // A remembered PGID is safe to signal only while its direct-child leader is
    // still an unreaped child. An inherited SIGCHLD=SIG_IGN disposition (or
    // SA_NOCLDWAIT) silently auto-reaps leaders and invalidates that proof. This
    // standalone server owns its process signal policy, so restore the POSIX
    // default before the async runtime or any child process is started.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = libc::SIG_DFL;
    action.sa_flags = 0;
    // SAFETY: `action` is initialized as a default disposition with an empty
    // signal mask before it is installed for SIGCHLD.
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `action` is valid for the duration of this call and no old action
    // needs to be retained by the standalone executable.
    if unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

impl ProcessGroup {
    /// Spawn `command` as the leader of a new process group owned by this lease.
    ///
    /// On Unix the child is placed in a fresh group whose identifier is the
    /// leader's own process identifier, so no process outside this tree can be a
    /// member. On Windows the child is created suspended, put in a Job Object, and
    /// only then resumed; see [`crate::process_job`] for why.
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            command.process_group(0);
            let child = spawn_ready_to_exec(command)?;
            let ownership = ProcessGroupOwnership::held(child.id().unwrap_or(0) as libc::pid_t);
            Ok(Self { child, ownership })
        }
        #[cfg(windows)]
        {
            spawn_contained(command)
        }
    }

    /// Whether this lease's whole execution tree is contained on this platform.
    ///
    /// Unix always answers `true`: the group is owned outright. Windows answers
    /// `true` only when the host accepted the Job assignment, so a test can prove
    /// containment rather than infer it.
    #[cfg(windows)]
    pub(crate) fn owns_process_tree(&self) -> bool {
        self.job.as_ref().is_some_and(Job::is_assigned)
    }

    /// Kernel-reported count of processes still running in this lease's tree.
    ///
    /// Test-only witness. It observes the whole Job rather than a direct child,
    /// and the kernel is the one deciding the count, so it cannot be satisfied by
    /// an idle, suspended, or merely unreaped process.
    #[cfg(all(test, windows))]
    pub(crate) fn active_processes_for_test(&self) -> io::Result<u32> {
        self.job
            .as_ref()
            .ok_or_else(|| io::Error::other("this lease has no Job containment"))?
            .active_processes()
    }

    /// The group leader's process identifier, which is also the group identifier.
    #[cfg(all(test, unix))]
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
        // Job termination reaches every descendant, not just the leader, so the
        // direct-child kill below is belt and braces rather than the mechanism.
        #[cfg(windows)]
        if let Some(job) = self.job.as_ref() {
            job.terminate();
        }
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

    /// Whether a negative process-group signal is currently permitted.
    #[cfg(all(test, unix))]
    pub(crate) fn owns_process_group(&self) -> bool {
        self.ownership.is_proven()
    }

    /// Relinquish the process-group ownership proof without ending the lease.
    ///
    /// Models the moment the group identifier stops being this lease's, and is
    /// used by the ownership regression to show that a group signal is gated on
    /// the proof rather than on the lease still being alive.
    #[cfg(all(test, unix))]
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
        // Terminating the Job here rather than relying only on the handle close
        // makes teardown synchronous, so a caller that drops the lease has
        // requested tree teardown before this frame returns. The handle close that
        // follows is still the backstop that covers abnormal owner death.
        #[cfg(windows)]
        if let Some(job) = self.job.as_ref() {
            job.terminate();
        }
        let _ = self.child.start_kill();
    }
}

/// Start the command, tolerating a momentarily busy executable.
///
/// See [`crate::exec_ready`] for why that refusal is retried, and why no other
/// spawn error is.
fn spawn_ready_to_exec(command: &mut Command) -> io::Result<Child> {
    let retry = BusyProgramRetry::new();
    loop {
        match command.spawn() {
            Err(error) if retry.retry(&error) => {}
            result => return result,
        }
    }
}

/// Spawn a child into a Job Object without ever letting it run uncontained.
///
/// The ordering is the whole point, so it is spelled out rather than composed:
///
/// 1. Create the Job, so containment exists before any process does.
/// 2. Spawn with `CREATE_SUSPENDED`, so the child executes no user code at all.
/// 3. Assign the child to the Job, while it is provably still inert.
/// 4. Resume it, which is the first instant it may do anything.
///
/// If creating the Job, assigning the child, or resuming it fails, spawn fails
/// closed. Assignment failure is especially safe to handle: the child has not run
/// a single instruction, so the known suspended child is killed without ever
/// resuming an uncontained process.
#[cfg(windows)]
fn spawn_contained(command: &mut Command) -> io::Result<ProcessGroup> {
    spawn_contained_with(command, assign_and_resume)
}

/// Test seam for deterministically exercising setup failures with a real
/// suspended child and the production cleanup path.
#[cfg(windows)]
fn spawn_contained_with(
    command: &mut Command,
    setup: impl FnOnce(&mut Job, &Child) -> io::Result<()>,
) -> io::Result<ProcessGroup> {
    use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

    let mut job = Job::create()?;
    command.creation_flags(CREATE_SUSPENDED);
    let mut child = spawn_ready_to_exec(command)?;
    match setup(&mut job, &child) {
        Ok(()) => Ok(ProcessGroup {
            child,
            job: Some(job),
        }),
        Err(error) => {
            // A failed assignment or resume is terminal. The child has not run
            // user code, so killing its direct process handle is sufficient and
            // safe; never resume an uncontained child as a fallback.
            job.terminate();
            let _ = child.start_kill();
            Err(error)
        }
    }
}

/// Assign and resume `child`, failing closed if either step fails.
#[cfg(windows)]
fn assign_and_resume(job: &mut Job, child: &tokio::process::Child) -> io::Result<()> {
    assign_and_resume_with(job, child, Job::assign, Job::resume)
}

/// Shared ordering/error-propagation path used by production and fault-injection
/// tests. The injected operations model a kernel-call result; child ownership and
/// the outer fail-closed cleanup branch remain production code.
#[cfg(windows)]
fn assign_and_resume_with(
    job: &mut Job,
    child: &tokio::process::Child,
    assign: impl FnOnce(&mut Job, HANDLE, u32) -> io::Result<()>,
    resume: impl FnOnce(u32) -> io::Result<()>,
) -> io::Result<()> {
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("spawned child has no process identifier"))?;
    // Safety: `raw_handle` is the process handle `spawn` returned and remains valid
    // while `child` is borrowed.
    let handle = child
        .raw_handle()
        .ok_or_else(|| io::Error::other("spawned child has no process handle"))?;
    assign(job, handle as HANDLE, pid)?;
    resume(pid)
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

#[cfg(all(unix, not(target_os = "macos")))]
fn reported_child(info: &libc::siginfo_t) -> libc::pid_t {
    // Safety: reading a field of a `siginfo_t` that `waitid` has just filled in.
    unsafe { info.si_pid() }
}

/// The exit status or terminating signal carried by a termination report.
#[cfg(target_os = "macos")]
fn reported_status(info: &libc::siginfo_t) -> libc::c_int {
    info.si_status
}

#[cfg(all(unix, not(target_os = "macos")))]
fn reported_status(info: &libc::siginfo_t) -> libc::c_int {
    // Safety: reading a field of a `siginfo_t` that `waitid` has just filled in.
    unsafe { info.si_status() }
}

#[cfg(all(test, windows))]
mod windows_setup_failure_tests {

    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use tokio::process::Command;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    use super::{assign_and_resume_with, spawn_contained_with};

    const WORKLOAD_ENV: &str = "GOALLATCH_WINDOWS_SETUP_FAILURE_WORKLOAD";

    fn marker_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "local-mcp-windows-setup-failure-{}.marker",
            uuid::Uuid::new_v4()
        ))
    }

    fn helper_command(marker: &std::path::Path) -> std::process::Command {
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("test executable path"));
        command
            .args([
                "--exact",
                "process_group::windows_setup_failure_tests::fault_injection_workload_helper",
                "--nocapture",
            ])
            .env(WORKLOAD_ENV, marker)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command
    }

    struct KillOnDrop(Option<std::process::Child>);

    struct RemoveOnDrop(PathBuf);

    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    impl KillOnDrop {
        fn stop(&mut self) -> std::io::Result<()> {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                child.wait()?;
            }
            Ok(())
        }
    }

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.stop();
        }
    }

    fn assert_workload_helper_is_a_real_witness() {
        let marker = marker_path();
        let _marker_cleanup = RemoveOnDrop(marker.clone());
        let helper = helper_command(&marker)
            .spawn()
            .expect("workload helper starts");
        let mut helper = KillOnDrop(Some(helper));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !marker.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "workload helper did not produce its readiness marker"
            );
            assert!(
                helper
                    .0
                    .as_mut()
                    .expect("helper remains owned until stopped")
                    .try_wait()
                    .expect("helper status is observable")
                    .is_none(),
                "helper exited without running the workload"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        helper.stop().expect("positive-control helper is reaped");
        std::fs::remove_file(marker).expect("positive-control marker is cleaned up");
    }

    fn assert_fault_fails_closed(fail_during_resume: bool) {
        assert_workload_helper_is_a_real_witness();
        let marker = marker_path();
        let _marker_cleanup = RemoveOnDrop(marker.clone());
        let mut command = Command::from(helper_command(&marker));
        let observed_pid = Arc::new(AtomicU32::new(0));
        let observed_handle = Arc::new(Mutex::new(None::<OwnedHandle>));
        let pid_for_setup = Arc::clone(&observed_pid);
        let handle_for_setup = Arc::clone(&observed_handle);
        let resume_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resume_called_by_setup = Arc::clone(&resume_called);
        let result = spawn_contained_with(&mut command, move |job, child| {
            let pid = child
                .id()
                .ok_or_else(|| std::io::Error::other("spawned child has no PID"))?;
            pid_for_setup.store(pid, Ordering::SeqCst);
            let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
            if process.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let process = unsafe { OwnedHandle::from_raw_handle(process as RawHandle) };
            *handle_for_setup.lock().unwrap() = Some(process);
            if fail_during_resume {
                assign_and_resume_with(
                    job,
                    child,
                    |job, process, pid| job.assign(process, pid),
                    |_| {
                        resume_called_by_setup.store(true, Ordering::SeqCst);
                        Err(std::io::Error::other("injected ResumeThread failure"))
                    },
                )
            } else {
                assign_and_resume_with(
                    job,
                    child,
                    |_, _, _| {
                        Err(std::io::Error::other(
                            "injected AssignProcessToJobObject failure",
                        ))
                    },
                    |_| {
                        resume_called_by_setup.store(true, Ordering::SeqCst);
                        Ok(())
                    },
                )
            }
        });
        let error = result
            .err()
            .expect("injected setup failure must fail the spawn");
        assert!(error.to_string().contains("injected"));
        assert_eq!(
            resume_called.load(Ordering::SeqCst),
            fail_during_resume,
            "assignment failure must stop before resume; the resume failure must be invoked"
        );

        let pid = observed_pid.load(Ordering::SeqCst);
        assert_ne!(pid, 0, "the setup seam must have observed the child");
        // Keep a kernel process handle until termination is observed. This proves
        // cleanup completed, rather than relying on a delay or a PID liveness probe.
        let process = observed_handle
            .lock()
            .unwrap()
            .take()
            .expect("the process handle was retained at failure");
        let exited = unsafe { WaitForSingleObject(process.as_raw_handle() as _, 10_000) };
        drop(process);
        assert_eq!(
            exited, WAIT_OBJECT_0,
            "the suspended child was not cleaned up"
        );
        assert!(
            !marker.exists(),
            "the workload executed despite a failed containment setup"
        );
        let _ = std::fs::remove_file(marker);
    }

    #[test]
    fn assignment_failure_kills_the_suspended_child_without_running_workload() {
        assert_fault_fails_closed(false);
    }

    #[test]
    fn resume_failure_kills_the_assigned_child_without_running_workload() {
        assert_fault_fails_closed(true);
    }

    #[test]
    fn fault_injection_workload_helper() {
        let Some(marker) = std::env::var_os(WORKLOAD_ENV) else {
            return;
        };
        std::fs::write(marker, b"executed").expect("workload marker is writable");
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}
