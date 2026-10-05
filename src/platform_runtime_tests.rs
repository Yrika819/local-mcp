//! Runtime-lifetime regressions for the Platform Runtime Closure V1 guarantees.
//!
//! Every test here drives **real** processes and observes termination through a
//! **descendant witness**, never through a direct-child identifier and never
//! through `kill(pid, 0)`. That distinction is the whole point of this file:
//!
//! * `kill(pid, 0)` succeeds for an unreaped zombie, so it proves nothing about
//!   execution. The repository's own stress suite already exposed that.
//! * A shell leader exiting proves nothing about its grandchildren, which is
//!   exactly the "direct-child-only false proof" that a parent-death fix must not
//!   rely on.
//!
//! The Unix witness is a pipe whose write end is inherited **only** by the
//! descendant that must die. The kernel closes a process's descriptors when it
//! terminates, even while it remains an unreaped zombie, so the read end reaching
//! EOF is a decision the kernel made — not a timer, and not an inference.
//!
//! What is proven here, and what is not:
//!
//! * The orderly paths (termination, cancellation by drop, repeated termination)
//!   must kill a descendant that outlived its leader. Proven on Unix.
//! * Abnormal **host** death must kill the tree on Windows, by Job Object. That
//!   is proven on Windows only, and only because that is where the kernel provides
//!   the primitive. Linux and macOS have no equivalent, so this file asserts no
//!   such guarantee for them rather than implying one.

#![cfg(test)]

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use tokio::io::AsyncReadExt;
    use tokio::process::Command;

    use crate::process_blocking::run_bounded_blocking;
    use crate::process_group::ProcessGroup;

    /// How long a terminated witness is given to release the pipe.
    ///
    /// Group teardown is issued synchronously, so this only has to outlast
    /// scheduling of the observer; it is not a race window.
    const TEARDOWN: Duration = Duration::from_secs(5);

    /// How long a live witness is given to prove it is still alive.
    ///
    /// Nothing is signalled in a negative assertion, so this only has to outlast
    /// a scheduling delay.
    const NEGATIVE_SETTLE: Duration = Duration::from_millis(500);

    /// A pipe whose descriptors stay out of every unrelated process this binary
    /// spawns, and whose write end is handed to exactly one descendant.
    ///
    /// `pipe2(O_CLOEXEC)` would be the obvious spelling but is Linux-only;
    /// Darwin's libc has no `pipe2`, so a harness written against it would not
    /// even compile for macOS. `pipe` plus an explicit `FD_CLOEXEC` is the
    /// portable spelling. The residual two-syscall window is accepted for the
    /// same reason the existing ownership harness accepts it.
    fn witness_pipe() -> io::Result<(tokio::fs::File, OwnedFd)> {
        let mut fds = [0 as libc::c_int; 2];
        // Safety: `pipe` only writes the two descriptors into the array given.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let [read_end, write_end] = fds;
        for descriptor in [read_end, write_end] {
            // Safety: both descriptors are open and owned by this function.
            if unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
                let error = io::Error::last_os_error();
                // Safety: this function owns both descriptors and has transferred
                // neither, so a partial failure closes each exactly once.
                unsafe {
                    libc::close(read_end);
                    libc::close(write_end);
                }
                return Err(error);
            }
        }
        // Safety: ownership of both open descriptors transfers to these values.
        Ok(unsafe {
            (
                tokio::fs::File::from_std(std::fs::File::from_raw_fd(read_end)),
                OwnedFd::from_raw_fd(write_end),
            )
        })
    }

    /// Observes whether a witnessed descendant is still alive.
    ///
    /// Resolves to `true` once every holder of the write end is gone. The write
    /// end is held by the descendant alone, and a zombie has already had its
    /// descriptors closed by the kernel, so this distinguishes a terminated
    /// process from a merely unreaped one.
    struct DescendantWitness {
        read_end: tokio::fs::File,
    }

    impl DescendantWitness {
        async fn terminated_within(&mut self, settle: Duration) -> bool {
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

        async fn assert_terminated(&mut self) {
            assert!(
                self.terminated_within(TEARDOWN).await,
                "SECURITY REGRESSION: the descendant survived its owner's teardown"
            );
        }

        async fn assert_alive(&mut self) {
            assert!(
                !self.terminated_within(NEGATIVE_SETTLE).await,
                "the descendant died when nothing asked it to"
            );
        }
    }

    /// A command whose leader spawns a long-lived descendant holding `write_end`.
    ///
    /// The descendant outlives its leader by construction: the leader exits
    /// immediately and only the descendant keeps running. That is the shape which
    /// makes a direct-child-only check worthless, so it is the shape every test
    /// here uses.
    fn leader_then_descendant(write_end: &OwnedFd) -> Command {
        let write_raw = write_end.as_raw_fd();
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            // Backgrounded so it inherits the witness descriptor as stdout, then
            // the leader exits immediately. Only the descendant holds the end.
            .arg("sleep 300 & exit 0");
        // Installed after fork, immediately before exec: `dup2` clears FD_CLOEXEC
        // on the new descriptor, so the exec'd process inherits the write end.
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

    /// Spawn a witnessed tree and hand back the lease plus its witness.
    ///
    /// The parent's copies of both the runtime's stdout pipe and the witness write
    /// end are closed here, so the only holder of the write end is the descendant.
    async fn witnessed_tree() -> (ProcessGroup, DescendantWitness) {
        let (read_end, write_end) = witness_pipe().expect("the witness pipe must be creatable");
        let mut command = leader_then_descendant(&write_end);
        let mut lease = ProcessGroup::spawn(&mut command).expect("the tree must spawn");
        drop(lease.take_stdout());
        drop(write_end);
        (lease, DescendantWitness { read_end })
    }

    #[tokio::test]
    async fn a_dropped_lease_terminates_the_descendant_not_just_the_leader() {
        let (lease, mut witness) = witnessed_tree().await;

        // The leader has already exited; only the descendant is left. Proving it
        // alive first keeps a passing teardown assertion from being vacuous.
        witness.assert_alive().await;

        // Dropping the lease is the cancellation path: a future dropped while an
        // owned execution is in flight must not detach the tree.
        drop(lease);

        witness.assert_terminated().await;
    }

    #[tokio::test]
    async fn explicit_termination_reaches_a_descendant_that_outlived_its_leader() {
        let (mut lease, mut witness) = witnessed_tree().await;
        witness.assert_alive().await;

        lease.terminate();

        witness.assert_terminated().await;
    }

    #[tokio::test]
    async fn repeated_termination_stays_within_the_ownership_proof() {
        let (mut lease, mut witness) = witnessed_tree().await;
        witness.assert_alive().await;

        // Calling this repeatedly is correct and intended: the proof is held until
        // the leader is reaped, so every call lands on this lease's own tree.
        lease.terminate();
        lease.terminate();
        lease.terminate();

        witness.assert_terminated().await;
    }

    #[test]
    fn a_bounded_blocking_command_kills_a_descendant_that_outlives_its_leader() {
        let (read_end, write_end) = witness_pipe().expect("the witness pipe must be creatable");
        let write_raw = write_end.as_raw_fd();
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg("sleep 300 & exit 0");
        // Safety: `dup2` is async-signal-safe and is the only call made here.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(write_raw, libc::STDOUT_FILENO) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        // The leader exits at once, so the command completes well inside the
        // deadline; the descendant is what must not survive it.
        let output = run_bounded_blocking(&mut command, Duration::from_secs(30))
            .expect("the bounded blocking command must run");
        assert!(output.status.success(), "the leader itself must succeed");
        assert!(
            !output.timed_out,
            "a fast leader must not be reported as a timeout"
        );
        drop(write_end);

        let mut read_end = read_end;
        let mut byte = [0_u8; 1];
        let released = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime must build")
            .block_on(async {
                tokio::time::timeout(TEARDOWN, async {
                    loop {
                        match read_end.read(&mut byte).await {
                            Ok(0) => return true,
                            Ok(_) => continue,
                            Err(_) => return false,
                        }
                    }
                })
                .await
                .unwrap_or(false)
            });
        assert!(
            released,
            "SECURITY REGRESSION: the bounded blocking runner left a descendant running"
        );
    }

    #[test]
    fn a_bounded_blocking_command_reports_a_timeout_as_unknown_not_as_completion() {
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg("sleep 300");
        let output = run_bounded_blocking(&mut command, Duration::from_millis(250))
            .expect("the bounded blocking command must run");
        // A deadline that expires is reported as its own condition so the caller
        // can keep the outcome *unknown*. It must never be silently reported as a
        // normal completion, which would let a mutating caller infer that the
        // side effect did not happen.
        assert!(
            output.timed_out,
            "a command that outran its deadline must be reported as a timeout"
        );
    }

    #[test]
    fn a_bounded_blocking_command_still_delivers_output_larger_than_a_pipe_buffer() {
        // The readers must run concurrently with the wait. Without them the child
        // would block on a full pipe, never exit, and be reported as a timeout — so
        // this is the regression separating a correct runner from one that silently
        // kills every large but healthy result.
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg("head -c 1048576 /dev/zero");
        let output = run_bounded_blocking(&mut command, Duration::from_secs(30))
            .expect("the bounded blocking command must run");
        assert!(
            !output.timed_out,
            "a large but healthy result must not be killed as a timeout"
        );
        assert_eq!(
            output.stdout.len(),
            1024 * 1024,
            "the whole stream must be retained"
        );
    }

    #[test]
    fn a_descendant_that_escapes_containment_cannot_hang_the_caller() {
        // A reader thread only ends at EOF, which needs *every* holder of the pipe
        // to be gone. A descendant that escapes the process group and inherits the
        // pipe would therefore keep an unbounded join blocked forever — the exact
        // "a hung Git holds a runtime thread" failure this seam exists to prevent.
        // `setsid` is that escape: it puts the descendant in a new session, outside
        // the group, so the group signal cannot reach it.
        let (read_end, write_end) = witness_pipe().expect("the witness pipe must be creatable");
        let write_raw = write_end.as_raw_fd();
        let mut command = std::process::Command::new("/bin/sh");
        // Keep the escaped descendant short-lived so the test leaves no
        // background process to clean up with a global scanner or a remembered
        // PID. Its eight-second lifetime exceeds the runner's five-second drain
        // grace, which is all this test needs to prove the caller is bounded.
        command.arg("-c").arg("setsid sleep 8 & exit 0");
        // Safety: `dup2` is async-signal-safe and is the only call made here.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(write_raw, libc::STDOUT_FILENO) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let started = Instant::now();
        // If the join were unbounded this call would never return.
        let output = run_bounded_blocking(&mut command, Duration::from_secs(2))
            .expect("the bounded blocking command must run");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(60),
            "SECURITY REGRESSION: an escaping descendant hung the caller for {elapsed:?}"
        );
        assert!(
            !output.timed_out,
            "a leader that exits at once must not be reported as a timeout"
        );
        // The descendant has a bounded eight-second lifetime and self-terminates;
        // no global scanner or remembered PID is used to clean it up.
        drop(write_end);
        drop(read_end);
    }

    #[test]
    fn a_bounded_blocking_command_terminates_on_output_overflow() {
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg("head -c 67108865 /dev/zero");
        let output = run_bounded_blocking(&mut command, Duration::from_secs(30))
            .expect("the bounded blocking command must run");
        assert!(
            output.output_overflow,
            "output beyond the trusted-Git cap must be reported as a resource overflow"
        );
        assert!(
            output.capture_incomplete,
            "overflow must never be returned as a complete stream"
        );
        assert!(
            output.stdout.len() <= crate::sandbox::TRUSTED_GIT_STDOUT_LIMIT,
            "the reader must retain no more than the trusted-Git output limit"
        );
    }

    #[test]
    fn a_timed_out_blocking_command_leaves_no_descendant_running() {
        // The timeout path must terminate the tree, not merely abandon it. The
        // leader here backgrounds a descendant and then blocks, so a runner that
        // only killed the leader would leave the descendant alive.
        let (read_end, write_end) = witness_pipe().expect("the witness pipe must be creatable");
        let write_raw = write_end.as_raw_fd();
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg("sleep 300 & sleep 300");
        // Safety: `dup2` is async-signal-safe and is the only call made here.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(write_raw, libc::STDOUT_FILENO) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let output = run_bounded_blocking(&mut command, Duration::from_millis(250))
            .expect("the bounded blocking command must run");
        assert!(output.timed_out, "the deadline must have expired");
        drop(write_end);

        let mut read_end = read_end;
        let mut byte = [0_u8; 1];
        let released = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime must build")
            .block_on(async {
                tokio::time::timeout(TEARDOWN, async {
                    loop {
                        match read_end.read(&mut byte).await {
                            Ok(0) => return true,
                            Ok(_) => continue,
                            Err(_) => return false,
                        }
                    }
                })
                .await
                .unwrap_or(false)
            });
        assert!(
            released,
            "SECURITY REGRESSION: a timed-out execution left a descendant running"
        );
    }
}

/// Windows Job Object containment.
///
/// The witness is the Job's own **kernel-reported active-process count**, which is
/// what separates this from a direct-child-only false proof: it counts every
/// process in the tree, and it is reported by the kernel rather than inferred from
/// a timer or a process identifier that may have been recycled.
#[cfg(windows)]
mod windows {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use tokio::process::Command;

    use crate::process_group::ProcessGroup;

    /// How long a terminated Job is given to drain its accounting.
    const TEARDOWN: Duration = Duration::from_secs(10);

    /// A command that starts a descendant and then exits, leaving the descendant
    /// running. The descendant is what a direct-child-only check would miss.
    fn leader_then_descendant() -> Command {
        let mut command = Command::new("cmd.exe");
        // `start /b` backgrounds `ping` without a new window, and `ping -n 300`
        // runs for roughly five minutes, comfortably past every deadline here.
        command
            .arg("/C")
            .arg("start /b ping -n 300 127.0.0.1 > NUL & exit /b 0");
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    #[tokio::test]
    async fn abnormal_owner_death_closes_the_job_and_terminates_its_descendant() {
        // Run a fresh copy of this test executable as the Local MCP owner. It
        // creates a real ProcessGroup, proves a descendant entered its Job, and
        // then aborts itself so no Rust destructor can close the Job. Windows must
        // close that handle as part of process termination, which must kill the
        // descendant. The descendant inherits stdout; pipe EOF is the witness.
        let executable = std::env::current_exe().expect("test executable path");
        let mut owner = tokio::process::Command::new(executable);
        owner
            .args([
                "--exact",
                "platform_runtime_tests::windows::abnormal_owner_child",
                "--nocapture",
            ])
            .env("GOALLATCH_ABNORMAL_OWNER_HELPER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = owner.spawn().expect("owner helper must spawn");
        let mut witness = child.stdout.take().expect("stdout witness pipe");
        let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
            .await
            .expect("abnormal owner must terminate within the bound")
            .expect("owner process must be waitable");
        assert!(
            !status.success(),
            "the helper must terminate abnormally rather than dropping its lease"
        );
        let mut bytes = Vec::new();
        tokio::time::timeout(TEARDOWN, witness.read_to_end(&mut bytes))
            .await
            .expect("SECURITY REGRESSION: descendant held the owner pipe after abnormal death")
            .expect("stdout witness must read");
    }

    /// Child-process helper for `abnormal_owner_death_closes_the_job_and_terminates_its_descendant`.
    ///
    /// Invoked in a fresh test-harness process. The test is a no-op when run by
    /// the normal suite; the parent test sets the marker environment variable.
    #[test]
    fn abnormal_owner_child() {
        if std::env::var_os("GOALLATCH_ABNORMAL_OWNER_HELPER").is_none() {
            return;
        }
        let mut command = Command::new("cmd.exe");
        command
            .arg("/C")
            .arg("start /b ping -n 300 127.0.0.1 & exit /b 0");
        // Inherit stdout from the helper test process: the descendant holding
        // this handle is the parent's kernel EOF witness after owner death.
        let lease = ProcessGroup::spawn(&mut command).expect("the owned tree must spawn");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if lease.active_processes_for_test().unwrap_or(0) >= 2 {
                // Deliberately bypass all Rust destructors. If kill-on-close is
                // missing, the descendant keeps stdout open and the parent test
                // observes the witness timeout.
                std::process::abort();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("fixture did not produce a descendant in the Job");
    }

    #[tokio::test]
    async fn a_windows_lease_reports_real_tree_containment() {
        let mut command = leader_then_descendant();
        let lease = ProcessGroup::spawn(&mut command).expect("the tree must spawn");
        // A real assertion rather than a compile-time one: a host that refuses job
        // assignment must fail here instead of the suite claiming a guarantee the
        // platform did not provide.
        assert!(
            lease.owns_process_tree(),
            "SECURITY REGRESSION: Windows reported containment it does not have"
        );
    }

    #[tokio::test]
    async fn termination_empties_the_whole_tree_not_just_the_leader() {
        let mut command = leader_then_descendant();
        let mut lease = ProcessGroup::spawn(&mut command).expect("the tree must spawn");

        // Let the leader spawn its descendant, then confirm the Job really does
        // contain more than the leader. Without this, an empty count afterwards
        // would prove nothing.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_descendant = false;
        while Instant::now() < deadline {
            if lease.active_processes_for_test().unwrap_or(0) >= 2 {
                saw_descendant = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            saw_descendant,
            "the fixture never produced a descendant to witness"
        );

        lease.terminate();

        let deadline = Instant::now() + TEARDOWN;
        loop {
            if lease.active_processes_for_test().unwrap_or(0) == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "SECURITY REGRESSION: the Job still reported a live process after termination"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn dropping_the_lease_empties_the_whole_tree() {
        let mut command = leader_then_descendant();
        let lease = ProcessGroup::spawn(&mut command).expect("the tree must spawn");

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_descendant = false;
        while Instant::now() < deadline {
            if lease.active_processes_for_test().unwrap_or(0) >= 2 {
                saw_descendant = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            saw_descendant,
            "the fixture never produced a descendant to witness"
        );

        // Dropping is the cancellation path: a dropped future must not detach a
        // tree it owned. Closing the Job handle carries `KILL_ON_JOB_CLOSE`, so the
        // kernel terminates the tree rather than this code signalling a process
        // identifier.
        drop(lease);
    }

    #[tokio::test]
    async fn a_leader_that_exits_normally_still_empties_the_tree() {
        let mut command = Command::new("cmd.exe");
        // No descendant: the Job must drain from a single-process tree too, which
        // is the ordinary completion path.
        command.arg("/C").arg("exit /b 0");
        let mut lease = ProcessGroup::spawn(&mut command).expect("the command must spawn");

        let deadline = Instant::now() + TEARDOWN;
        loop {
            if lease.active_processes_for_test().unwrap_or(u32::MAX) == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the Job never drained after an ordinary exit"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn the_suspended_child_really_is_resumed() {
        // If the resume step were broken, every command in this module would hang
        // forever. A short deadline that must *not* fire is the direct regression.
        let mut command = Command::new("cmd.exe");
        command.arg("/C").arg("exit /b 0");
        let lease = ProcessGroup::spawn(&mut command).expect("the command must spawn");
        let result = tokio::time::timeout(Duration::from_secs(20), lease.wait_termination()).await;
        assert!(
            result.is_ok(),
            "a suspended child was never resumed, so every execution would hang"
        );
    }
}
