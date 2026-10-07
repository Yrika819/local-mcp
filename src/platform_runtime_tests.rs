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
//! The Unix witness is a FIFO whose writer is opened by the intended child in its
//! `pre_exec` hook, not held open in the parent and inherited by concurrent forks.
//! The kernel closes a process's descriptors when it terminates, even while it
//! remains an unreaped zombie, so the read end reaching EOF is a decision the
//! kernel made — not a timer, and not an inference.
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
    use std::ffi::CString;
    use std::io;
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use tokio::io::AsyncReadExt;
    use tokio::process::Command;

    use crate::execution::AbortOnDropJoinHandle;
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

    /// A FIFO witness whose writer is opened by the intended child in `pre_exec`.
    /// No writer descriptor exists in the parent, so unrelated fork/exec activity
    /// cannot temporarily inherit a false witness holder during the spawn race.
    struct WitnessPipe {
        read_end: tokio::fs::File,
        path: std::path::PathBuf,
    }

    impl Drop for WitnessPipe {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn witness_pipe() -> io::Result<WitnessPipe> {
        let path = std::env::temp_dir().join(format!(
            "local-mcp-runtime-witness-{}.fifo",
            uuid::Uuid::new_v4()
        ));
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        // Safety: `c_path` is NUL-terminated and the mode is a valid permission mask.
        if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let read_fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if read_fd == -1 {
            let error = io::Error::last_os_error();
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        let flags = unsafe { libc::fcntl(read_fd, libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(read_fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } == -1
        {
            let error = io::Error::last_os_error();
            unsafe { libc::close(read_fd) };
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        // Safety: ownership of the fresh descriptor transfers to the returned file.
        let read_end = unsafe { tokio::fs::File::from_std(std::fs::File::from_raw_fd(read_fd)) };
        Ok(WitnessPipe { read_end, path })
    }

    fn redirect_stdout_to_witness(
        command: &mut std::process::Command,
        witness: &WitnessPipe,
    ) -> io::Result<()> {
        let path = CString::new(witness.path.as_os_str().as_bytes())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        // SAFETY: the closure uses only async-signal-safe open/dup2/close calls
        // and captures an owned, NUL-terminated path.
        unsafe {
            command.pre_exec(move || {
                let writer = libc::open(
                    path.as_ptr(),
                    libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
                );
                if writer == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::dup2(writer, libc::STDOUT_FILENO) == -1 {
                    let error = io::Error::last_os_error();
                    libc::close(writer);
                    return Err(error);
                }
                if libc::close(writer) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }

    /// Observes whether a witnessed descendant is still alive.
    ///
    /// Resolves to `true` once every holder of the write end is gone. The write
    /// end is held by the descendant alone, and a zombie has already had its
    /// descriptors closed by the kernel, so this distinguishes a terminated
    /// process from a merely unreaped one.
    struct DescendantWitness {
        pipe: WitnessPipe,
    }

    impl DescendantWitness {
        async fn terminated_within(&mut self, settle: Duration) -> bool {
            let mut byte = [0_u8; 1];
            tokio::time::timeout(settle, async {
                loop {
                    match self.pipe.read_end.read(&mut byte).await {
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

    /// A command whose leader spawns a long-lived descendant holding the FIFO writer.
    ///
    /// The descendant outlives its leader by construction: the leader exits
    /// immediately and only the descendant keeps running. That is the shape which
    /// makes a direct-child-only check worthless, so it is the shape every test
    /// here uses.
    fn leader_then_descendant(witness: &WitnessPipe) -> Command {
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg("sleep 300 & exit 0");
        redirect_stdout_to_witness(&mut command, witness)
            .expect("the descendant witness path is valid");
        let mut command = Command::from(command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    /// Spawn a witnessed tree and hand back the lease plus its witness.
    ///
    /// The witness writer is opened only by the intended child after fork, so no
    /// other concurrently spawned test process can hold the FIFO open.
    async fn witnessed_tree() -> (ProcessGroup, DescendantWitness) {
        let pipe = witness_pipe().expect("the witness pipe must be creatable");
        let mut command = leader_then_descendant(&pipe);
        let mut lease = ProcessGroup::spawn(&mut command).expect("the tree must spawn");
        drop(lease.take_stdout());
        (lease, DescendantWitness { pipe })
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
    async fn dropping_a_foreground_task_owner_terminates_its_descendant() {
        let (lease, mut witness) = witnessed_tree().await;
        witness.assert_alive().await;

        let task = tokio::spawn(async move {
            let _lease = lease;
            std::future::pending::<()>().await;
        });
        drop(AbortOnDropJoinHandle::new(task));

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
        let mut witness = witness_pipe().expect("the witness pipe must be creatable");
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg("sleep 300 & exit 0");
        redirect_stdout_to_witness(&mut command, &witness).expect("witness path is valid");

        // The leader exits at once, so the command completes well inside the
        // deadline; the descendant is what must not survive it.
        let output = run_bounded_blocking(&mut command, Duration::from_secs(30))
            .expect("the bounded blocking command must run");
        assert!(output.status.success(), "the leader itself must succeed");
        assert!(
            !output.timed_out,
            "a fast leader must not be reported as a timeout"
        );

        let read_end = &mut witness.read_end;
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
        // The helper below starts its own process group rather than relying on the
        // non-portable `setsid` command (which is absent on macOS).
        let mut witness = witness_pipe().expect("the witness pipe must be creatable");
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("test executable path"));
        command
            .args([
                "--exact",
                "platform_runtime_tests::unix::escaped_descendant_helper",
                "--nocapture",
            ])
            .env("GOALLATCH_ESCAPED_DESCENDANT_HELPER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        redirect_stdout_to_witness(&mut command, &witness).expect("witness path is valid");
        let started = Instant::now();
        // If the join were unbounded this call would never return.
        let _output = run_bounded_blocking(&mut command, Duration::from_secs(2))
            .expect("the bounded blocking command must run");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(60),
            "SECURITY REGRESSION: an escaping descendant hung the caller for {elapsed:?}"
        );
        // The command's reader pipe is not the independent descendant witness:
        // pre_exec deliberately redirects stdout to the witness pipe. Verify that
        // the escaped process still holds that pipe after the bounded runner returns.
        let read_end = &mut witness.read_end;
        let witness_still_open = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime must build")
            .block_on(async {
                tokio::time::timeout(Duration::from_millis(200), async {
                    let mut byte = [0_u8; 1];
                    loop {
                        match read_end.read(&mut byte).await {
                            Ok(0) | Err(_) => return false,
                            Ok(_) => continue,
                        }
                    }
                })
                .await
                .is_err()
            });
        assert!(
            witness_still_open,
            "the escaped descendant must retain its independent witness pipe"
        );
        // The descendant has a bounded eight-second lifetime and self-terminates;
        // no global scanner or remembered PID is used to clean it up.
    }

    #[test]
    fn escaped_descendant_helper() {
        if std::env::var_os("GOALLATCH_ESCAPED_DESCENDANT_HELPER").is_none() {
            return;
        }
        let mut command = std::process::Command::new("/bin/sleep");
        command
            .arg("8")
            .stdout(Stdio::inherit())
            .stderr(Stdio::null());
        // Give the witness process a different group so the runner cannot signal it.
        command.process_group(0);
        let child = command.spawn().expect("escaped witness must spawn");
        drop(child);
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

    fn install_descendant_readiness_handshake(command: &mut std::process::Command) {
        let shell = CString::new("/bin/sh").unwrap();
        let option = CString::new("-c").unwrap();
        let script = CString::new("printf R; printf S >&9; exec sleep 30").unwrap();
        unsafe {
            command.pre_exec(move || {
                let mut descriptors = [0; 2];
                if libc::pipe(descriptors.as_mut_ptr()) == -1 {
                    return Err(io::Error::last_os_error());
                }
                let descendant = libc::fork();
                if descendant == -1 {
                    let error = io::Error::last_os_error();
                    libc::close(descriptors[0]);
                    libc::close(descriptors[1]);
                    return Err(error);
                }
                if descendant == 0 {
                    libc::close(descriptors[0]);
                    const READY_FD: libc::c_int = 9;
                    if libc::dup2(descriptors[1], READY_FD) == -1 {
                        libc::_exit(126);
                    }
                    if descriptors[1] != READY_FD && libc::close(descriptors[1]) == -1 {
                        libc::_exit(126);
                    }
                    let arguments = [
                        shell.as_ptr(),
                        option.as_ptr(),
                        script.as_ptr(),
                        std::ptr::null(),
                    ];
                    libc::execv(shell.as_ptr(), arguments.as_ptr());
                    libc::_exit(127);
                }

                libc::close(descriptors[1]);
                let mut poll_descriptor = libc::pollfd {
                    fd: descriptors[0],
                    events: libc::POLLIN,
                    revents: 0,
                };
                let poll_result = libc::poll(&mut poll_descriptor, 1, 5_000);
                let mut ready = 0_u8;
                let mut read_result = -1;
                if poll_result > 0 {
                    read_result = libc::read(
                        descriptors[0],
                        &mut ready as *mut u8 as *mut libc::c_void,
                        1,
                    );
                }
                libc::close(descriptors[0]);
                if poll_result <= 0 || read_result != 1 || ready != b'S' {
                    libc::kill(descendant, libc::SIGKILL);
                    let mut status = 0;
                    libc::waitpid(descendant, &mut status, 0);
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "descendant readiness handshake did not arrive before command exec",
                    ));
                }
                Ok(())
            });
        }
    }

    fn failure_process_evidence(group_id: Option<i32>, witness_path: &std::path::Path) -> String {
        let Some(group_id) = group_id else {
            return "process inspection unavailable: process-group id was not recorded".to_owned();
        };
        let processes = std::process::Command::new("ps")
            .args(["-axo", "pid,ppid,pgid,stat,command"])
            .output()
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter(|line| {
                        line.split_whitespace()
                            .nth(2)
                            .and_then(|value| value.parse::<i32>().ok())
                            == Some(group_id)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|error| format!("ps unavailable: {error}"));
        let holders = std::process::Command::new("lsof")
            .args(["-n", &witness_path.to_string_lossy()])
            .output()
            .map(|output| {
                format!(
                    "status={} stdout={} stderr={}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                )
            })
            .unwrap_or_else(|error| format!("lsof unavailable: {error}"));
        format!("process_group={group_id} members={processes:?}; fifo_holders={holders}")
    }

    #[test]
    fn a_timed_out_blocking_command_leaves_no_descendant_running() {
        // `spawn_ready` returns only after exec succeeds, and `wait_bounded` starts
        // its deadline afterwards. The child-side pre_exec has therefore opened
        // the witness before this 250 ms run deadline begins. The descendant emits
        // its own marker before exec, proving cleanup is not judged vacuously.
        let mut witness = witness_pipe().expect("the witness pipe must be creatable");
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg("sleep 30");
        redirect_stdout_to_witness(&mut command, &witness).expect("witness path is valid");
        install_descendant_readiness_handshake(&mut command);
        let output = run_bounded_blocking(&mut command, Duration::from_millis(250))
            .expect("the bounded blocking command must run");
        let process_evidence = crate::process_blocking::take_process_tree_test_evidence();

        let read_end = &mut witness.read_end;
        let mut byte = [0_u8; 64];
        let (descendant_ready, released) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime must build")
            .block_on(async {
                tokio::time::timeout(TEARDOWN, async {
                    let mut descendant_ready = false;
                    loop {
                        match read_end.read(&mut byte).await {
                            Ok(0) => return (descendant_ready, true),
                            Ok(count) => {
                                descendant_ready |= byte[..count].contains(&b'R');
                            }
                            Err(_) => return (descendant_ready, false),
                        }
                    }
                })
                .await
                .unwrap_or((false, false))
            });
        let failure_evidence = if descendant_ready && released && output.timed_out {
            String::new()
        } else {
            failure_process_evidence(process_evidence.group_id, &witness.path)
        };
        assert!(
            output.timed_out,
            "deadline did not expire; leader_pid={:?} pgid={:?} ownership={:?} \
             group_signal={:?}/errno={:?} descendant_ready={descendant_ready} eof={released} \
             process_evidence={failure_evidence}",
            process_evidence.leader_pid,
            process_evidence.group_id,
            process_evidence.ownership_observation,
            process_evidence.group_signal_result,
            process_evidence.group_signal_errno,
        );
        assert!(
            descendant_ready,
            "fixture did not prove a descendant held the writer before timeout cleanup; \
             leader_pid={:?} pgid={:?} timeout={} ownership={:?} group_signal={:?}/errno={:?} \
             fifo={} eof={released} process_evidence={failure_evidence}",
            process_evidence.leader_pid,
            process_evidence.group_id,
            output.timed_out,
            process_evidence.ownership_observation,
            process_evidence.group_signal_result,
            process_evidence.group_signal_errno,
            witness.path.display(),
        );
        assert!(
            released,
            "SECURITY REGRESSION: timed-out tree retained the FIFO writer; \
             leader_pid={:?} pgid={:?} timeout={} ownership={:?} group_signal={:?}/errno={:?} \
             fifo={} process_evidence={failure_evidence}",
            process_evidence.leader_pid,
            process_evidence.group_id,
            output.timed_out,
            process_evidence.ownership_observation,
            process_evidence.group_signal_result,
            process_evidence.group_signal_errno,
            witness.path.display(),
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
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use tokio::{io::AsyncReadExt, process::Command};

    use crate::process_blocking::run_bounded_blocking;
    use crate::process_group::ProcessGroup;

    /// How long a terminated Job is given to drain its accounting.
    const TEARDOWN: Duration = Duration::from_secs(10);

    const TREE_HELPER_ENV: &str = "GOALLATCH_WINDOWS_TREE_HELPER";
    const TREE_LEAF_ENV: &str = "GOALLATCH_WINDOWS_TREE_LEAF";
    const TREE_MODE_ENV: &str = "GOALLATCH_WINDOWS_TREE_MODE";
    const TREE_READY_ENV: &str = "GOALLATCH_WINDOWS_TREE_READY";

    fn new_tree_ready_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "local-mcp-windows-tree-ready-{}.marker",
            uuid::Uuid::new_v4()
        ))
    }

    fn windows_tree_leader(mode: &str, ready: &std::path::Path) -> std::process::Command {
        let mut command =
            std::process::Command::new(std::env::current_exe().expect("test executable path"));
        command
            .args([
                "--exact",
                "platform_runtime_tests::windows::windows_tree_leader_helper",
                "--nocapture",
            ])
            .env(TREE_HELPER_ENV, "1")
            .env(TREE_MODE_ENV, mode)
            .env(TREE_READY_ENV, ready)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    fn leader_then_descendant(mode: &str, ready: &std::path::Path) -> Command {
        Command::from(windows_tree_leader(mode, ready))
    }

    fn wait_for_tree_ready(path: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "descendant helper never became ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn windows_tree_leader_helper() {
        if std::env::var_os(TREE_HELPER_ENV).is_none() {
            return;
        }
        let ready = PathBuf::from(
            std::env::var_os(TREE_READY_ENV)
                .expect("the ready-marker path is passed by the parent"),
        );
        let mut leaf =
            std::process::Command::new(std::env::current_exe().expect("test executable path"));
        leaf.args([
            "--exact",
            "platform_runtime_tests::windows::windows_tree_leaf_helper",
            "--nocapture",
        ])
        .env(TREE_LEAF_ENV, "1")
        .env(TREE_READY_ENV, &ready)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::null());
        let child = leaf.spawn().expect("the descendant helper must spawn");
        wait_for_tree_ready(&ready);
        drop(child);
        if std::env::var(TREE_MODE_ENV).is_ok_and(|mode| mode == "hold") {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    fn windows_tree_leaf_helper() {
        if std::env::var_os(TREE_LEAF_ENV).is_none() {
            return;
        }
        let ready = PathBuf::from(
            std::env::var_os(TREE_READY_ENV)
                .expect("the ready-marker path is passed by the parent"),
        );
        std::fs::write(ready, b"ready").expect("the leaf signals its start");
        std::thread::sleep(Duration::from_secs(60));
    }

    #[tokio::test]
    async fn abnormal_owner_death_closes_the_job_and_terminates_its_descendant() {
        // Run a fresh copy of this test executable as the Local MCP owner. It
        // creates a real ProcessGroup, proves a descendant entered its Job, and
        // then aborts itself so no Rust destructor can close the Job. Windows must
        // close that handle as part of process termination, which must kill the
        // descendant. The descendant inherits stdout; pipe EOF is the witness.
        let executable = std::env::current_exe().expect("test executable path");
        let ready = new_tree_ready_path();
        let mut owner = tokio::process::Command::new(executable);
        owner
            .args([
                "--exact",
                "platform_runtime_tests::windows::abnormal_owner_child",
                "--nocapture",
            ])
            .env("GOALLATCH_ABNORMAL_OWNER_HELPER", "1")
            .env(TREE_READY_ENV, &ready)
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
        assert!(
            ready.exists(),
            "the descendant must signal readiness before owner death"
        );
        let mut bytes = Vec::new();
        tokio::time::timeout(TEARDOWN, witness.read_to_end(&mut bytes))
            .await
            .expect("SECURITY REGRESSION: descendant held the owner pipe after abnormal death")
            .expect("stdout witness must read");
        let _ = std::fs::remove_file(ready);
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
        let ready = PathBuf::from(
            std::env::var_os(TREE_READY_ENV).expect("the parent provides the witness marker"),
        );
        let mut command = leader_then_descendant("hold", &ready);
        command.as_std_mut().stdout(Stdio::inherit());
        let lease = ProcessGroup::spawn(&mut command).expect("the owned tree must spawn");
        wait_for_tree_ready(&ready);
        assert!(
            lease.active_processes_for_test().unwrap_or(0) >= 2,
            "the Job must include both leader and descendant before owner death"
        );
        // Deliberately bypass all Rust destructors. If kill-on-close is missing,
        // the descendant keeps the inherited owner pipe open.
        std::process::abort();
    }

    #[test]
    fn blocking_git_runner_terminates_descendants_on_ordinary_completion() {
        let ready = new_tree_ready_path();
        let mut command = windows_tree_leader("exit", &ready);
        let output = run_bounded_blocking(&mut command, Duration::from_secs(30))
            .expect("blocking Git command must run");
        assert!(
            !output.timed_out,
            "the leader should complete before deadline"
        );
        assert!(
            !output.capture_incomplete,
            "the Job must terminate a descendant that inherited the output pipe"
        );
        assert!(
            ready.exists(),
            "the descendant must have started before completion"
        );
        let _ = std::fs::remove_file(ready);
    }

    #[test]
    fn blocking_git_runner_timeout_terminates_the_descendant_tree() {
        let ready = new_tree_ready_path();
        let mut command = windows_tree_leader("hold", &ready);
        let output = run_bounded_blocking(&mut command, Duration::from_secs(2))
            .expect("blocking Git command must run");
        assert!(
            output.timed_out,
            "the foreground leader must exceed the deadline"
        );
        assert!(
            !output.capture_incomplete,
            "the Job must close descendant-held output after timeout"
        );
        assert!(
            ready.exists(),
            "the descendant must have started before timeout"
        );
        let _ = std::fs::remove_file(ready);
    }

    #[tokio::test]
    async fn a_windows_lease_reports_real_tree_containment() {
        let ready = new_tree_ready_path();
        let mut command = leader_then_descendant("hold", &ready);
        let lease = ProcessGroup::spawn(&mut command).expect("the tree must spawn");
        wait_for_tree_ready(&ready);
        // A real assertion rather than a compile-time one: a host that refuses job
        // assignment must fail here instead of the suite claiming a guarantee the
        // platform did not provide.
        assert!(
            lease.owns_process_tree() && lease.active_processes_for_test().unwrap_or(0) >= 2,
            "SECURITY REGRESSION: the descendant did not enter the assigned Job"
        );
        drop(lease);
        let _ = std::fs::remove_file(ready);
    }

    #[tokio::test]
    async fn termination_empties_the_whole_tree_not_just_the_leader() {
        let ready = new_tree_ready_path();
        let mut command = leader_then_descendant("exit", &ready);
        let mut lease = ProcessGroup::spawn(&mut command).expect("the tree must spawn");
        wait_for_tree_ready(&ready);
        let status = lease
            .wait_termination()
            .await
            .expect("the leader exits normally");
        assert!(status.success());
        assert!(
            lease.active_processes_for_test().unwrap_or(0) >= 1,
            "the descendant must outlive the leader before tree termination"
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
        let _ = std::fs::remove_file(ready);
    }

    #[tokio::test]
    async fn dropping_the_lease_empties_the_whole_tree() {
        let ready = new_tree_ready_path();
        let mut command = leader_then_descendant("exit", &ready);
        let mut lease = ProcessGroup::spawn(&mut command).expect("the tree must spawn");
        wait_for_tree_ready(&ready);
        let status = lease
            .wait_termination()
            .await
            .expect("the leader exits normally");
        assert!(status.success());
        assert!(
            lease.active_processes_for_test().unwrap_or(0) >= 1,
            "the descendant must outlive the leader before lease drop"
        );

        // The descendant inherits stdout, so EOF proves it is gone after Job close.
        let mut witness = lease.take_stdout().expect("stdout witness pipe");
        drop(lease);
        let mut output = Vec::new();
        tokio::time::timeout(TEARDOWN, witness.read_to_end(&mut output))
            .await
            .expect("SECURITY REGRESSION: descendant survived lease drop")
            .expect("stdout witness must read");
        let _ = std::fs::remove_file(ready);
    }

    #[tokio::test]
    async fn a_leader_that_exits_normally_still_empties_the_tree() {
        let mut command = Command::new("cmd.exe");
        // No descendant: the Job must drain from a single-process tree too, which
        // is the ordinary completion path.
        command.arg("/C").arg("exit /b 0");
        let lease = ProcessGroup::spawn(&mut command).expect("the command must spawn");

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
        let mut lease = ProcessGroup::spawn(&mut command).expect("the command must spawn");
        let result = tokio::time::timeout(Duration::from_secs(20), lease.wait_termination()).await;
        assert!(
            result.is_ok(),
            "a suspended child was never resumed, so every execution would hang"
        );
    }
}
