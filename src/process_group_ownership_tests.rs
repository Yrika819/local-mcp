//! Deterministic regressions for the Unix process-group ownership defect.
//!
//! V2.1.1 stored a raw process-group identifier and issued
//! `kill(-process_group_id, SIGKILL)` unconditionally from the guard's `Drop`.
//! Once the group leader had been reaped the kernel is free to hand that
//! identifier to an unrelated process, and the guard had no durable proof that
//! the number still belonged to the tree Local MCP launched.
//!
//! The regressions below build that hazard deterministically instead of waiting
//! for natural PID reuse. Every process they use is created and owned by the
//! test, and whether a group was signalled is observed through a pipe whose
//! write end is held only by that group, so no assertion depends on PID
//! recycling, on zombie reaping, or on a timing window.
//!
//! The set covers the whole contract:
//!
//! * an identifier with no ownership proof is never signalled (the defect),
//! * a group signal requires the proof, not merely a still-live lease,
//! * a group with a held proof is still terminated, including after its leader
//!   has exited (cleanup is not traded away for PID-reuse safety),
//! * the unreaped owner is never reaped while the lease needs its identifier,
//! * the unreaped owner is always collected, so no zombie is left behind.

#![cfg(all(test, unix))]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::process_group::{ProcessGroup, ProcessGroupOwnership};

/// How long a group is given to show that it is still populated.
///
/// Group kills are issued synchronously, so this only has to outlast scheduling
/// of the observer; it is not a race window.
const NEGATIVE_SETTLE: Duration = Duration::from_millis(500);

/// How long a terminated group is given to release the witness pipe.
const TEARDOWN_SETTLE: Duration = Duration::from_secs(5);

/// Create a pipe whose two descriptors are owned by this process and are not
/// inherited across `exec`.
///
/// `pipe2(O_CLOEXEC)` would be the obvious spelling, but it is Linux-specific:
/// Darwin's libc does not provide it, so a V2.1.2 harness written against it
/// could not even be compiled for macOS. `pipe` plus an explicit
/// `F_SETFD`/`FD_CLOEXEC` on both ends is the portable Unix spelling and gives
/// the same guarantee: the descriptors stay out of every other process this test
/// binary spawns, and the child under test clears the flag again on the single
/// descriptor it is supposed to inherit.
///
/// The residual difference from `pipe2` is the two-syscall window in which the
/// descriptors are not yet close-on-exec. It is two adjacent `fcntl` calls wide
/// and is accepted deliberately: the alternative is an Apple-only `syscall`
/// shim, which would make the harness less portable than the production code it
/// is meant to test.
fn controlled_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // Safety: `pipe` only writes the two descriptors into the array it is given.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let [read_end, write_end] = fds;
    for descriptor in [read_end, write_end] {
        // Safety: both descriptors are open and owned by this function, so
        // `fcntl` may be applied to either of them.
        if unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            let error = io::Error::last_os_error();
            // Safety: this function owns both descriptors and has not yet
            // transferred either, so a partial failure closes each exactly once
            // and leaks neither.
            unsafe {
                libc::close(read_end);
                libc::close(write_end);
            }
            return Err(error);
        }
    }
    // Safety: ownership of both open descriptors transfers to the returned
    // `OwnedFd`s, which close them on drop.
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(read_end),
            OwnedFd::from_raw_fd(write_end),
        )
    })
}

/// Whether `pid` has terminated without being reaped yet.
///
/// This reports the leader's termination with `WNOWAIT`, so observing it here
/// does not consume the exit status. That distinction is the whole point: a
/// zombie still occupies a slot in its process group, so it is exactly the state
/// a "no group member remains" measurement has to be able to tolerate.
fn is_unreaped_zombie(pid: libc::pid_t) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // Safety: `waitid` only fills in the `siginfo_t` it is handed, and `P_PID`
    // restricts the query to one direct child of this process.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &raw mut info,
            libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
        )
    };
    if result == -1 {
        return false;
    }
    // `libc` exposes the same C union either as plain fields (Apple) or as
    // accessor methods (Linux and most other targets).
    #[cfg(target_os = "macos")]
    let child = info.si_pid;
    #[cfg(not(target_os = "macos"))]
    let child = unsafe { info.si_pid() };
    child == pid
}

/// Observes whether a process group is still alive.
///
/// The pipe write end is installed as the group leader's stdout, so it stays
/// open for exactly as long as some *live* member of the group exists: a process
/// that has exited has already had its descriptors closed by the kernel, even
/// while it remains an unreaped zombie. `signalled_within` therefore resolves to
/// `true` when no live member remains and `false` when the group was left
/// running. Both outcomes are decided by the kernel, not by a timer.
struct GroupWitness {
    read_end: tokio::fs::File,
}

impl GroupWitness {
    fn new() -> (GroupWitness, OwnedFd) {
        let (read_end, write_end) =
            controlled_pipe().expect("the controlled group pipe must be created");
        (
            GroupWitness {
                read_end: tokio::fs::File::from_std(read_end.into()),
            },
            write_end,
        )
    }

    /// Resolve to `true` once every member of the group has gone.
    ///
    /// A `false` result means "the write end was never closed within
    /// `settle`", so `settle` must comfortably exceed the time a group kill
    /// needs to reach every member. Group kills are issued synchronously by the
    /// code under test, so the observed teardown is immediate.
    async fn signalled_within(&mut self, settle: Duration) -> bool {
        let mut byte = [0_u8; 1];
        tokio::time::timeout(settle, async {
            loop {
                match self.read_end.read(&mut byte).await {
                    Ok(0) => return true,
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
        })
        .await
        .unwrap_or(false)
    }

    /// Assert the group is still fully populated.
    async fn assert_intact(&mut self, group_id: u32) {
        assert!(
            !self.signalled_within(NEGATIVE_SETTLE).await,
            "SECURITY REGRESSION: process group {group_id} was signalled"
        );
    }

    /// Assert the group has been terminated.
    async fn assert_terminated(&mut self) {
        assert!(
            self.signalled_within(TEARDOWN_SETTLE).await,
            "the process group was never terminated"
        );
    }
}

/// Build a command whose group leader holds the witness pipe as its stdout.
///
/// The caller keeps ownership of `write_end` and must keep it open until the
/// command has been spawned, otherwise the descriptor would be closed before the
/// fork and the leader would never hold the write end.
fn witnessed_command(script: &str, write_end: &OwnedFd) -> Command {
    let write_raw = write_end.as_raw_fd();
    let mut command = std::process::Command::new("/bin/sh");
    command.arg("-c").arg(script);
    // Installed after fork, immediately before exec: `dup2` clears FD_CLOEXEC on
    // the new descriptor, so the exec'd process inherits the write end.
    //
    // Safety: `dup2` is async-signal-safe and is the only call made here.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(write_raw, libc::STDOUT_FILENO) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut command = Command::from(command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

/// A process group plus a lease that provably owns it.
async fn owned_group(script: &str) -> (ProcessGroup, GroupWitness) {
    let (witness, write_end) = GroupWitness::new();
    let mut command = witnessed_command(script, &write_end);
    let mut lease = ProcessGroup::spawn(&mut command).expect("owned group must spawn");
    // The witness pipe replaces the leader's stdout; the runtime's own stdout
    // pipe is unused here and is closed in the parent. The parent's copy of the
    // witness write end is closed here too, so only the group can hold it.
    drop(lease.take_stdout());
    drop(write_end);
    (lease, witness)
}

/// A process group that no lease owns, spawned directly.
async fn unowned_group(script: &str) -> (u32, GroupWitness, tokio::process::Child) {
    let (witness, write_end) = GroupWitness::new();
    let mut command = witnessed_command(script, &write_end);
    let mut child = command.spawn().expect("unowned group must spawn");
    let group_id = child.id().expect("unowned group pid");
    drop(child.stdout.take());
    drop(write_end);
    (group_id, witness, child)
}

async fn shutdown_unowned(group_id: u32, mut child: tokio::process::Child) {
    // The signal is unconditional, and that is the safe direction: this scope
    // still holds the group leader unreaped, so the kernel cannot have handed
    // the identifier to anything else. The V2.1.2 spelling consulted
    // `kill(-pgid, 0)` first, which is a group-occupancy probe rather than a
    // liveness measurement, and would have silently skipped the cleanup on any
    // host that answers it with an error other than success.
    let _ = unsafe { libc::kill(-(group_id as libc::pid_t), libc::SIGKILL) };
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Wait until `pid` is no longer running, then confirm nothing is left to collect.
async fn assert_collected(pid: libc::pid_t) {
    let mut status = 0;
    for _ in 0..200 {
        let reaped = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
        if reaped != 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // A second probe must find nothing left to collect. If the first probe
    // consumed the leader it reports ECHILD; if the runtime already collected
    // it, the probe also reports ECHILD. Either way no zombie survives.
    let second = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
    assert_eq!(
        second, -1,
        "the group leader must not be left as an unreaped zombie"
    );
}

#[tokio::test]
async fn stale_process_group_identity_is_never_signalled_without_ownership_proof() {
    let (group_id, mut witness, child) = unowned_group("exec /bin/sleep 300").await;

    // The post-reap hazard in isolation: an identifier with no proof that it is
    // still ours, exactly as a recycled process-group identifier would look.
    {
        let _stale = ProcessGroupOwnership::unproven_for_test(group_id);
    }
    // The ownership value is gone. Nothing else may signal the group from here.
    assert!(
        !witness.signalled_within(NEGATIVE_SETTLE).await,
        "SECURITY REGRESSION: a process group without a host-owned ownership proof \
         was signalled by a released process-group guard (group {group_id})"
    );
    shutdown_unowned(group_id, child).await;
}

#[tokio::test]
async fn a_group_signal_requires_the_ownership_proof_not_merely_a_live_lease() {
    // The leader exits immediately while a descendant keeps the inherited stdout
    // open, so the group is provably still populated after the leader is gone.
    let (mut lease, mut witness) = owned_group("/bin/sleep 300 & exec /bin/sleep 0").await;
    let leader = lease.leader_id() as libc::pid_t;
    let status = tokio::time::timeout(Duration::from_secs(5), lease.wait_termination())
        .await
        .expect("the group leader must terminate")
        .expect("the termination report must succeed");
    assert!(status.success(), "the controlled leader exits successfully");
    witness.assert_intact(leader as u32).await;

    // Drop the ownership proof while the descendant is still running and the
    // leader is still unreaped. A live lease and a live descendant must not be
    // enough to authorise a group signal; only the proof is.
    lease.release_process_group();
    assert!(!lease.owns_process_group());
    drop(lease);

    witness.assert_intact(leader as u32).await;
    let _ = unsafe { libc::kill(-(leader as libc::pid_t), libc::SIGKILL) };
    assert_collected(leader).await;
}

#[tokio::test]
async fn an_owned_process_group_is_terminated_when_the_lease_is_released() {
    let (lease, mut witness) = owned_group("exec /bin/sleep 300").await;
    let leader = lease.leader_id() as libc::pid_t;
    assert!(lease.owns_process_group());

    drop(lease);

    witness.assert_terminated().await;
    assert_collected(leader).await;
}

#[tokio::test]
async fn descendants_surviving_the_leader_are_still_terminated_safely() {
    // This is the case V2.1.1 handled by signalling a process group whose leader
    // it had already reaped. The leader is observed as terminated but is
    // deliberately not reaped, so the group identifier is still provably this
    // lease's when the surviving descendant is terminated.
    let (mut lease, mut witness) = owned_group("/bin/sleep 300 & exec /bin/sleep 0").await;
    let leader = lease.leader_id() as libc::pid_t;
    let status = tokio::time::timeout(Duration::from_secs(5), lease.wait_termination())
        .await
        .expect("the group leader must terminate")
        .expect("the termination report must succeed");
    assert!(status.success(), "the controlled leader exits successfully");

    drop(lease);

    witness.assert_terminated().await;
    assert_collected(leader).await;
}

#[tokio::test]
async fn a_terminated_leader_is_never_reaped_while_the_lease_holds_its_group() {
    let (mut lease, _witness) = owned_group("exec /bin/sleep 0").await;
    let leader = lease.leader_id() as libc::pid_t;
    lease.wait_termination().await.expect("leader terminates");

    // `waitpid(WNOHANG)` reports the leader while it is an unreaped zombie,
    // which is exactly the state the ownership proof depends on. `ECHILD` would
    // mean somebody reaped the leader and released the group identifier, so the
    // lease must never observe it.
    let mut status = 0;
    let mut observed_unreaped = false;
    for _ in 0..100 {
        let reaped = unsafe { libc::waitpid(leader, &raw mut status, libc::WNOHANG) };
        assert_ne!(
            reaped, -1,
            "SECURITY REGRESSION: the group leader was reaped while the lease still \
             needs its process-group identifier"
        );
        if reaped == leader {
            // Leave the leader unreaped for the rest of the test.
            observed_unreaped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        observed_unreaped,
        "a terminated group leader must be observed as an unreaped zombie"
    );
    drop(lease);
    assert_collected(leader).await;
}

#[tokio::test]
async fn the_witness_reports_cleanup_while_only_an_unreaped_zombie_remains() {
    // The leader exits at once and is deliberately left unreaped, so an
    // unreaped zombie occupies the group for the whole of this test. That is the
    // state a negative-group existence probe cannot express: the group still
    // exists, but nothing in it is running.
    let (mut lease, mut witness) = owned_group("/bin/sleep 300 & exec /bin/sleep 0").await;
    let leader = lease.leader_id() as libc::pid_t;
    let status = tokio::time::timeout(Duration::from_secs(5), lease.wait_termination())
        .await
        .expect("the group leader must terminate")
        .expect("the termination report must succeed");
    assert!(status.success(), "the controlled leader exits successfully");

    // While a live descendant holds the witness, cleanup is not complete even
    // though the leader is already a zombie.
    assert!(
        is_unreaped_zombie(leader),
        "the group leader must be a zombie this test has not reaped"
    );
    witness.assert_intact(leader as u32).await;

    // Terminating the group kills the descendant. The witness then reports
    // cleanup complete even though the leader is still an unreaped zombie, which
    // is the distinction this harness exists to make.
    lease.terminate();
    witness.assert_terminated().await;
    assert!(
        is_unreaped_zombie(leader),
        "the witness reported cleanup while the leader is still an unreaped zombie"
    );

    drop(lease);
    assert_collected(leader).await;
}

#[tokio::test]
async fn many_short_lived_groups_leave_no_owner_or_zombie_behind() {
    let mut leaders = Vec::new();
    for _ in 0..64 {
        let (mut lease, _witness) = owned_group("exec /bin/sleep 0").await;
        let leader = lease.leader_id() as libc::pid_t;
        leaders.push(leader);
        lease
            .wait_termination()
            .await
            .expect("the group leader must terminate");
        drop(lease);
    }
    for leader in leaders {
        assert_collected(leader).await;
    }
}
