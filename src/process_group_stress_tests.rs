//! Stress for the Unix process-group ownership lease.
//!
//! Every case drives a real process group through the production launcher and
//! then asks one question of it: does any *live* process in that group still
//! exist once the launcher is done with it?
//!
//! # Measuring liveness without a process table
//!
//! V2.1.2 answered that question with `kill(-pgid, 0)`. That is a group
//! *occupancy* probe, not a liveness measurement, and it cannot separate a live
//! process from an unreaped zombie: a group whose only remaining member has
//! exited still occupies a slot in the kernel, and the answer the probe gives
//! for that state is defined by the kernel rather than by the test. On the
//! Darwin kernel this file was repaired for, `kill(-pgid, 0)` reports `EPERM`
//! for exactly that state -- a live error meaning "the group still exists" --
//! and the probe's `== 0` test reads it as "gone". Other kernels answer the
//! same state with success. Either way the answer is about the group, never
//! about whether a process in it is running.
//!
//! The measurement used here is instead a kernel-backed lifetime witness, the
//! same idea the ownership regressions use and for the same reason. A FIFO is
//! created in the fixture root, the test opens the read end before the run
//! starts, and the fixture opens the write end as descriptor 9. The group
//! leader, every descendant it forks and every `exec` they perform all keep that
//! descriptor open, so:
//!
//! * **no end-of-file** means a live process still holds it, and
//! * **end-of-file** means no live process in the group holds it, however many
//!   unreaped zombies the group still lists.
//!
//! The property that makes this work is that a process which has exited has
//! already had its descriptors closed by the kernel, whether or not its status
//! has been reaped. Nothing here needs `/proc`, `ps`, process names, sleeps or a
//! group identifier, so it behaves identically on Linux and on macOS.
//!
//! The guards clean up on ordinary return, panic unwind, and task cancellation
//! while the owner process is still running. They cannot run after the test
//! executable is externally killed (for example by a harness timeout or machine
//! failure), so this is not a zero-residue guarantee for those events. The
//! fixtures' long-lived commands are bounded, but external termination can leave
//! them until their own command lifetime expires; residue must be audited and
//! cleaned by the harness/operator using the recorded test-owned group identity.
//!
//! Because a FIFO is a filesystem rendezvous rather than an inherited
//! descriptor, the test can install a witness into a group it does not spawn:
//! the production launcher owns the child's stdin, stdout and stderr, and every
//! other descriptor is close-on-exec, so the fixture is what opens the witness.
//! That is the only spelling that works on every platform regardless of how the
//! runtime passes descriptors across `exec`.
//!
//! # Readiness is a handshake, not a race
//!
//! End-of-file is only meaningful once a writer has actually held the
//! descriptor, so a fixture that has not run yet looks identical to a group that
//! was cleaned up perfectly. Each fixture therefore publishes a single handshake
//! byte on the witness as its first action, and no assertion about cleanup is
//! judged before that byte has been observed. This removes the V2.1.2 dependency
//! on a fixture publishing its process identifier before the launcher's own run
//! deadline elapsed: the deadline is a property of the run under test and must
//! not be made to depend on how quickly the test's own fixture is scheduled.
//!
//! # Fixtures own what they create
//!
//! The unrelated group and the fixture root are both owned by guards that clean
//! up on drop, so an assertion that panics unwinding through a test body cannot
//! leave a `sleep 300` behind or a temporary directory behind. V2.1.2 leaked the
//! child with `mem::forget` and killed the group on the last line of the body,
//! so every failure between the two leaked both a process and a directory. A
//! guard never signals any group it does not own: it signals the negative group
//! identifier only while it still holds the leader unreaped, and it drops the
//! child afterwards, which is the only moment at which the kernel is allowed to
//! reuse the identifier.
//!
//! Covered: normal zero exit, nonzero exit, timeout, blocked stdin, retained
//! stdout/stderr pipes, descendants, a leader that exits before its descendants,
//! cancellation of an in-flight run, many repeated short-lived groups, and high
//! parallelism. Verified for each case: the expected lifecycle outcome, that the
//! unrelated group is still live at the end, that no live process the launcher
//! owned survived cleanup, and that the harnesses themselves leave nothing
//! behind.

#![cfg(all(test, unix))]

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::process::Command;

use crate::agent::{AgentError, run_bounded_host_process};

/// Safety bound for the cases that must complete on their own.
///
/// It exists only to turn a hang into a failure; no assertion below depends on
/// it. These runs complete in milliseconds, so the bound is deliberately far
/// above anything the machine needs, because a bound that can be reached by
/// scheduling pressure on a loaded host is a bound that measures the host and
/// not the lease.
const FAST_DEADLINE: Duration = Duration::from_secs(120);

/// Safety bound on how long a run that must hit its deadline may take in total.
///
/// The deadline itself is handed to the launcher as [`RUN_DEADLINE`] or
/// [`WITNESS_RUN_DEADLINE`]; this only asserts that the run ended because that
/// deadline was reached rather than by hanging.
const RUN_UPPER_BOUND: Duration = Duration::from_secs(20);

/// Deadline handed to the launcher by the case that measures the launcher's own
/// timing and nothing else.
///
/// That case carries no witness, so it has no readiness dependency and no race:
/// it asserts only that a run given a short deadline is cancelled by that
/// deadline. The value is the 400 ms V2.1.2 used, kept unchanged so the property
/// it covers is not lost when the cases below move to a deadline their fixtures
/// can actually start within.
const RUN_DEADLINE: Duration = Duration::from_millis(400);

/// Deadline handed to the launcher by the cases that measure cleanup with a
/// lifetime witness.
///
/// This is deliberately **not** a readiness proxy, and it is not derived from
/// [`RUN_DEADLINE`]. A run cannot be cancelled before its process exists, so a
/// deadline that a fixture cannot be scheduled inside measures the host's
/// process-creation cost and nothing else. V2.1.2 made that mistake in reverse:
/// it used a 400 ms deadline as the budget for a fixture to publish a readiness
/// file, and on a host where a single `exec` costs a few hundred milliseconds --
/// which is this one, `endpointsecurityd` is consulted on every spawn -- no
/// fixture could ever be ready in time, so the cases failed on host speed while
/// reporting a cleanup verdict.
///
/// The fixture's readiness is therefore established separately and causally, by
/// the handshake above, on its own budget. A fixture that still never takes hold
/// is reported as the host problem it is. The timing assertion for these cases
/// remains [`RUN_UPPER_BOUND`]; this value only has to be able to contain the
/// fixture's own startup.
const WITNESS_RUN_DEADLINE: Duration = Duration::from_secs(3);

/// How long a terminated group is given to release the witness.
const TEARDOWN_SETTLE: Duration = Duration::from_secs(5);

/// How long a live group is given to be seen still holding the witness.
const LIVENESS_SETTLE: Duration = Duration::from_millis(500);

/// Safety bound on the fixture's readiness handshake.
///
/// This bounds the host, not the lease: it turns "the fixture never got far
/// enough to publish its handshake" into a clearly attributable failure instead
/// of a hang. It is deliberately not derived from [`RUN_DEADLINE`], because the
/// launcher's deadline is the behaviour under test and must not be made to
/// depend on how quickly the test's own fixture is scheduled.
const READY_SETTLE: Duration = Duration::from_secs(20);

/// How often the witness is polled while waiting for a state change.
const POLL: Duration = Duration::from_millis(5);

/// The shell descriptor a fixture holds its witness on.
const WITNESS_FD: u8 = 9;

/// Quote `value` for use as a single `/bin/sh` word.
fn shell_quote(value: &Path) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', r"'\''"))
}

/// A fixture root that removes itself on drop, including on unwind.
struct FixtureRoot {
    path: PathBuf,
}

impl FixtureRoot {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "local-mcp-pgroup-stress-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).expect("the fixture root must be creatable");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FixtureRoot {
    fn drop(&mut self) {
        // Runs on ordinary return and panic unwind only. External SIGKILL of the
        // test executable bypasses Drop and is not covered by this guard.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn fixture(root: &Path, name: &str, body: &str) -> PathBuf {
    let path = root.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

/// What one non-blocking read of the witness reported.
enum Read {
    /// A live process holds the write end and has sent more data.
    Data,
    /// No live process holds the write end any more.
    EndOfFile,
    /// A live process holds the write end and has sent nothing more.
    Pending,
}

/// A non-blocking read of `read_end`, reporting data, end-of-file, or "a live
/// process is still holding it".
///
/// The descriptor is opened `O_NONBLOCK`, so "would block" is the ordinary
/// answer while any live process holds the write end, and it is a measurement
/// rather than an error.
fn poll_read(read_end: &OwnedFd) -> Read {
    let mut buffer = [0_u8; 64];
    loop {
        // Safety: reads at most `buffer.len()` bytes into `buffer`, from a
        // descriptor the caller owns and keeps open.
        let count = unsafe {
            libc::read(
                read_end.as_raw_fd(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if count > 0 {
            return Read::Data;
        }
        if count == 0 {
            return Read::EndOfFile;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        // `EWOULDBLOCK` is an alias of `EAGAIN` on every supported Unix, but
        // naming both keeps the intent readable without an unreachable arm.
        let would_block = matches!(error.raw_os_error(), Some(libc::EAGAIN))
            || matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK));
        if would_block {
            return Read::Pending;
        }
        panic!("the witness read end must stay readable: {error}");
    }
}

/// Resolve to `true` once no live process holds the witness's write end.
///
/// `false` means the write end was still open after `settle`, so `settle` must
/// comfortably exceed the time a group kill needs to reach every member. Group
/// kills are issued synchronously by the code under test, so the observed
/// teardown is immediate and this bound is not a race window.
///
/// The caller must have observed the handshake first: before a writer exists,
/// end-of-file means "nothing has started", not "everything is finished".
async fn released_within(read_end: &OwnedFd, settle: Duration) -> bool {
    let deadline = Instant::now() + settle;
    loop {
        match poll_read(read_end) {
            Read::EndOfFile => return true,
            // Any data the group wrote is not the measurement; drain to EOF.
            Read::Data => continue,
            Read::Pending => {}
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// A kernel-backed witness of whether any *live* process still holds a
/// descriptor.
///
/// The read end is opened before the run starts, so a fixture that opens the
/// write end never blocks on the rendezvous, and the descriptor is close-on-exec
/// so this test binary's other children cannot hold it and delay end-of-file.
struct LivenessWitness {
    path: PathBuf,
    read_end: OwnedFd,
    handshake_seen: bool,
}

impl LivenessWitness {
    fn new(root: &Path, name: &str) -> Self {
        let path = root.join(name);
        let c_path =
            CString::new(path.as_os_str().as_bytes()).expect("the witness path must be NUL-free");
        // Safety: `c_path` is a valid NUL-terminated path and the mode is valid
        // for a FIFO.
        if unsafe { libc::mkfifo(c_path.as_ptr(), 0o700) } == -1 {
            panic!(
                "the witness FIFO must be creatable: {}",
                std::io::Error::last_os_error()
            );
        }
        // Safety: opening a FIFO for reading with O_NONBLOCK never blocks, even
        // though no writer exists yet, and O_CLOEXEC keeps the descriptor out of
        // every other process this test binary spawns.
        let read_end = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if read_end == -1 {
            panic!(
                "the witness read end must be openable: {}",
                std::io::Error::last_os_error()
            );
        }
        // Safety: `read_end` is an open descriptor this function owns, and
        // ownership transfers to the `OwnedFd` that closes it on drop.
        Self {
            path,
            read_end: unsafe { OwnedFd::from_raw_fd(read_end) },
            handshake_seen: false,
        }
    }

    /// The line a fixture must emit to take hold of the witness.
    ///
    /// The fixture is a plain `/bin/sh` script, so the descriptor is opened with
    /// the shell's own `exec N>` redirect. The launcher owns the child's stdin,
    /// stdout and stderr and every other descriptor is close-on-exec, so this is
    /// the only way to install a witness into a group this test does not spawn.
    fn open_writer(&self) -> String {
        format!(
            "exec {fd}>{fifo}\n",
            fd = WITNESS_FD,
            fifo = shell_quote(&self.path)
        )
    }

    /// The line a fixture must emit to publish its readiness.
    fn publish_handshake(&self) -> String {
        format!("printf 'x' 1>&{fd}\n", fd = WITNESS_FD)
    }

    /// The lines a fixture with no descendants must emit, in that order.
    ///
    /// For a fixture whose whole group is the leader, taking hold and announcing
    /// it are the same instant, so the handshake may come first.
    fn fixture_prelude(&self) -> String {
        format!("{}{}", self.open_writer(), self.publish_handshake())
    }

    /// The lines a fixture with a descendant must emit, given the body that
    /// creates it.
    ///
    /// The descendant is created **before** the handshake is published, so a
    /// readiness handshake from this shape is a proof that the group is already
    /// fully populated, not merely that a leader has started. That matters for
    /// every case that terminates the group: a group kill is a traversal of the
    /// group's members, so a child forked at the same instant as the kill can be
    /// missed and survive. Publishing readiness only once the descendant exists
    /// puts a poll interval between the fork and the termination, and it means
    /// the cleanup assertion is about a populated group rather than about a group
    /// still being built.
    fn fixture_prelude_with_descendant(&self, descendant: &str) -> String {
        format!(
            "{}{}{}",
            self.open_writer(),
            descendant,
            self.publish_handshake()
        )
    }

    /// A second, independently owned reader for the same witness.
    ///
    /// Duplicating the read end is safe because end-of-file is a property of the
    /// writers, not of the number of readers. This lets a test keep observing the
    /// witness after the value that owned the original reader has been dropped.
    fn duplicate_read_end(&self) -> OwnedFd {
        // Safety: the descriptor is open, and the new one is immediately marked
        // close-on-exec so nothing inherits it.
        let duplicate = unsafe { libc::fcntl(self.read_end.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        assert!(duplicate != -1, "the witness read end must be duplicable");
        // Safety: ownership of the new descriptor transfers to the `OwnedFd`.
        unsafe { OwnedFd::from_raw_fd(duplicate) }
    }

    /// Wait for the fixture to prove it has taken hold of the witness.
    ///
    /// Resolves to `false` if the handshake has not arrived within `settle`. No
    /// assertion about cleanup may be judged before this has returned `true`.
    ///
    /// End-of-file is ignored until the handshake arrives, and it has to be: a
    /// FIFO that no writer has opened yet reads as end-of-file, so before the
    /// handshake that state means "the fixture has not run yet" and is
    /// indistinguishable from "the group released the witness". Only the
    /// handshake byte makes the later end-of-file a measurement.
    async fn await_handshake(&mut self, settle: Duration) -> bool {
        let deadline = Instant::now() + settle;
        loop {
            if let Read::Data = poll_read(&self.read_end) {
                self.handshake_seen = true;
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Resolve to `true` once no live process holds the witness any more.
    async fn released_within(&self, settle: Duration) -> bool {
        assert!(
            self.handshake_seen,
            "cleanup must never be judged before the fixture is known to hold the witness"
        );
        released_within(&self.read_end, settle).await
    }

    /// Assert that no live process the launcher owned survived cleanup.
    async fn assert_released(&self, described: &str) {
        assert!(
            self.released_within(TEARDOWN_SETTLE).await,
            "a live process survived cleanup of {described}"
        );
    }
}

/// The single-element argv that runs a fixture.
fn command_of(path: &Path) -> [String; 1] {
    [path.to_string_lossy().into_owned()]
}

/// An unrelated, test-owned group that must survive everything under test.
///
/// The guard owns the child, so the group identifier stays provably this test's
/// for as long as the guard exists: the leader is never reaped, and the
/// identifier is released to the kernel only after the guard has stopped using
/// it.
struct UnrelatedGroup {
    group_id: libc::pid_t,
    child: Option<tokio::process::Child>,
    witness: LivenessWitness,
}

impl UnrelatedGroup {
    async fn spawn(root: &Path, name: &str) -> Self {
        let mut witness = LivenessWitness::new(root, "unrelated.witness");
        let path = fixture(
            root,
            name,
            &format!("{}exec /bin/sleep 300\n", witness.fixture_prelude()),
        );
        let mut command = Command::new(path);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = command.spawn().expect("unrelated group must spawn");
        let group_id = child.id().expect("unrelated group pid") as libc::pid_t;
        assert!(
            witness.await_handshake(READY_SETTLE).await,
            "the unrelated group must be observably live before the run under test starts"
        );
        Self {
            group_id,
            child: Some(child),
            witness,
        }
    }

    /// Assert the unrelated group is still live.
    async fn assert_alive(&self) {
        assert!(
            !self.witness.released_within(LIVENESS_SETTLE).await,
            "SECURITY REGRESSION: the unrelated process group under test was terminated"
        );
    }

    /// An independent reader, so this group's liveness can still be observed
    /// after this value has been dropped.
    fn observer(&self) -> OwnedFd {
        self.witness.duplicate_read_end()
    }
}

impl Drop for UnrelatedGroup {
    fn drop(&mut self) {
        // The child is still held and therefore still unreaped, so the kernel
        // cannot have reassigned the identifier and the signal below can only
        // reach the group this test created. Nothing outside this value is ever
        // signalled.
        let _ = unsafe { libc::kill(-self.group_id, libc::SIGKILL) };
        // Dropping the child reaps the leader, which is the only point at which
        // the identifier may be handed to an unrelated process. The group is
        // already dead, so this cannot leave anything running.
        drop(self.child.take());
    }
}

/// A group this test spawned, kept owned for as long as the value exists.
struct MeasuredGroup {
    group_id: libc::pid_t,
    child: Option<tokio::process::Child>,
}

impl MeasuredGroup {
    fn terminate(&mut self) {
        // Only ever signals a group whose leader this value holds unreaped.
        let _ = unsafe { libc::kill(-self.group_id, libc::SIGKILL) };
    }
}

impl Drop for MeasuredGroup {
    fn drop(&mut self) {
        self.terminate();
        drop(self.child.take());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_and_nonzero_exit_release_the_group_without_residue() {
    let root = FixtureRoot::new("exit");
    let unrelated = UnrelatedGroup::spawn(root.path(), "unrelated.sh").await;

    for (name, body, status) in [
        ("zero", "/bin/cat > /dev/null\nprintf done\n", 0),
        ("nonzero", "/bin/cat > /dev/null\nexit 9\n", 9),
    ] {
        let witness = LivenessWitness::new(root.path(), &format!("{name}.witness"));
        let path = fixture(
            root.path(),
            &format!("{name}.fixture"),
            &format!("{}{body}", witness.fixture_prelude()),
        );
        let command = command_of(&path);
        let mut witness = witness;
        let (result, ready) = tokio::join!(
            run_bounded_host_process(&command, b"prompt", root.path(), FAST_DEADLINE),
            witness.await_handshake(READY_SETTLE),
        );
        // A fixture that never took hold of its group has not exercised
        // anything, so this is reported as the host problem it is rather than as
        // a cleanup result.
        assert!(ready, "the {name} fixture must take hold of its group");
        let output = result.expect("a clean fixture must complete");
        assert_eq!(
            output.status, status,
            "{name} must report its own exit status"
        );
        if status == 0 {
            assert_eq!(output.stdout, "done", "{name} must report its own output");
        }
        // A completed run still releases its lease, so no live process may remain
        // in the group it owned.
        witness
            .assert_released(&format!("the {name} fixture"))
            .await;
    }

    unrelated.assert_alive().await;
    // The unrelated group and the fixture root are both released by drop, on this
    // path and on the unwinding path of a failing assertion.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_short_deadline_cancels_a_run_that_would_never_finish() {
    // The V2.1.2 timing case, kept verbatim and deliberately witness-free.
    //
    // It carries no witness, so it has no readiness dependency: it asserts only
    // that the launcher honours the deadline it was given, for a run that would
    // otherwise never end. Nothing here has to be scheduled inside the deadline,
    // which is exactly what made the witness cases below unreliable at 400 ms.
    let root = FixtureRoot::new("deadline");
    let path = fixture(root.path(), "short.fixture", "exec /bin/sleep 300\n");
    let command = command_of(&path);
    let started = Instant::now();
    let result = run_bounded_host_process(&command, b"prompt", root.path(), RUN_DEADLINE).await;
    assert_eq!(result.expect_err("must time out"), AgentError::Timeout);
    assert!(started.elapsed() < RUN_UPPER_BOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_terminates_the_group_and_no_descendant_survives() {
    let root = FixtureRoot::new("timeout");
    let unrelated = UnrelatedGroup::spawn(root.path(), "unrelated.sh").await;
    let witness = LivenessWitness::new(root.path(), "timeout.witness");
    let path = fixture(
        root.path(),
        "timeout.fixture",
        &format!(
            "{}exec /bin/sleep 300\n",
            witness.fixture_prelude_with_descendant("/bin/sleep 300 &\n")
        ),
    );
    let started = Instant::now();
    let command = command_of(&path);
    let mut witness = witness;
    let (result, ready) = tokio::join!(
        run_bounded_host_process(&command, b"prompt", root.path(), WITNESS_RUN_DEADLINE),
        witness.await_handshake(READY_SETTLE),
    );
    assert!(ready, "the fixture must take hold of its group");
    assert_eq!(result.expect_err("must time out"), AgentError::Timeout);
    assert!(started.elapsed() < RUN_UPPER_BOUND);
    // The descendant outlives the leader, so this is the case where cleanup has to
    // reach past the leader the group is named after.
    witness.assert_released("the timed-out fixture").await;
    unrelated.assert_alive().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_stdin_write_is_bounded_and_terminates_the_group() {
    let root = FixtureRoot::new("stdin");
    let unrelated = UnrelatedGroup::spawn(root.path(), "unrelated.sh").await;
    let witness = LivenessWitness::new(root.path(), "stdin.witness");
    // Never reads stdin and outlives the deadline.
    let path = fixture(
        root.path(),
        "blocked.fixture",
        &format!("{}exec /bin/sleep 300\n", witness.fixture_prelude()),
    );
    let started = Instant::now();
    let command = command_of(&path);
    let mut witness = witness;
    let blocked_prompt = vec![b'x'; 4 * 1024 * 1024];
    let (result, ready) = tokio::join!(
        run_bounded_host_process(&command, &blocked_prompt, root.path(), WITNESS_RUN_DEADLINE),
        witness.await_handshake(READY_SETTLE),
    );
    assert!(ready, "the fixture must take hold of its group");
    assert_eq!(result.expect_err("must time out"), AgentError::Timeout);
    assert!(started.elapsed() < RUN_UPPER_BOUND);
    witness.assert_released("the stdin-blocked fixture").await;
    unrelated.assert_alive().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leader_that_exits_before_its_descendant_still_cleans_up() {
    let root = FixtureRoot::new("early-exit");
    let unrelated = UnrelatedGroup::spawn(root.path(), "unrelated.sh").await;
    let witness = LivenessWitness::new(root.path(), "early.witness");
    // The leader exits at once, leaving a descendant inside the group that keeps
    // the inherited witness open. This is the case V2.1.1 signalled by using a
    // group identifier whose leader it had already reaped, and it is also the
    // state in which the leader is briefly an unreaped zombie: the witness must
    // call this cleaned up only once the descendant is really gone.
    let path = fixture(
        root.path(),
        "early.fixture",
        &format!(
            "{}exec /bin/sleep 0\n",
            witness.fixture_prelude_with_descendant("/bin/sleep 300 &\n")
        ),
    );
    let command = command_of(&path);
    let mut witness = witness;
    let (result, ready) = tokio::join!(
        run_bounded_host_process(&command, b"prompt", root.path(), WITNESS_RUN_DEADLINE),
        witness.await_handshake(READY_SETTLE),
    );
    assert!(ready, "the fixture must take hold of its group");
    assert_eq!(result.expect_err("must time out"), AgentError::Timeout);
    witness.assert_released("the early-exit fixture").await;
    unrelated.assert_alive().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_an_in_flight_run_terminates_the_group() {
    let root = FixtureRoot::new("cancel");
    let unrelated = UnrelatedGroup::spawn(root.path(), "unrelated.sh").await;
    let witness = LivenessWitness::new(root.path(), "cancel.witness");
    let path = fixture(
        root.path(),
        "cancel.fixture",
        &format!(
            "{}exec /bin/sleep 300\n",
            witness.fixture_prelude_with_descendant("/bin/sleep 300 &\n")
        ),
    );
    let mut witness = witness;
    let command = vec![path.to_string_lossy().into_owned()];
    let cwd = root.path().to_path_buf();
    let handle = tokio::spawn(async move {
        run_bounded_host_process(&command, b"prompt", &cwd, Duration::from_secs(300)).await
    });
    // Cancelling is only meaningful once the run really is in flight, and the
    // fixture publishes its handshake only after its descendant exists. So this
    // is a proof that the group is populated when the run is aborted, rather
    // than a race between the abort and a fork that has not happened yet. This
    // is the causal readiness the run deadline must not be asked to imply.
    assert!(
        witness.await_handshake(READY_SETTLE).await,
        "the cancelled run must take hold of its witness before it is aborted"
    );
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
    witness.assert_released("the cancelled run").await;
    unrelated.assert_alive().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_short_lived_groups_in_parallel_leave_no_residue() {
    const ROUNDS: usize = 16;
    const PARALLEL: usize = 8;
    let root = FixtureRoot::new("parallel");
    let unrelated = UnrelatedGroup::spawn(root.path(), "unrelated.sh").await;
    let completed = Arc::new(AtomicUsize::new(0));

    for round in 0..ROUNDS {
        let mut tasks = Vec::new();
        for index in 0..PARALLEL {
            let root = root.path().to_path_buf();
            let completed = Arc::clone(&completed);
            tasks.push(tokio::spawn(async move {
                let witness = LivenessWitness::new(&root, &format!("r{round}-{index}.witness"));
                let path = fixture(
                    &root,
                    &format!("r{round}-{index}.fixture"),
                    &format!(
                        "{}/bin/cat > /dev/null\nprintf done\n",
                        witness.fixture_prelude()
                    ),
                );
                let command = command_of(&path);
                let mut witness = witness;
                let (result, ready) = tokio::join!(
                    run_bounded_host_process(&command, b"prompt", &root, FAST_DEADLINE),
                    witness.await_handshake(READY_SETTLE),
                );
                assert!(ready, "a fixture must take hold of its group");
                let output = result.expect("a short-lived group must complete");
                assert_eq!(output.status, 0);
                assert_eq!(output.stdout, "done");
                witness.assert_released("a short-lived group").await;
                completed.fetch_add(1, Ordering::Relaxed);
            }));
        }
        for task in tasks {
            task.await.expect("no parallel group task may panic");
        }
    }
    assert_eq!(completed.load(Ordering::Relaxed), ROUNDS * PARALLEL);
    unrelated.assert_alive().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_witness_separates_a_live_process_from_a_released_group() {
    // The measurement the rest of this file depends on. Without it a harness
    // could pass for the wrong reason: end-of-file must be absent while a live
    // process holds the witness's write end, and must arrive the moment that
    // process is gone.
    let root = FixtureRoot::new("witness");
    let mut witness = LivenessWitness::new(root.path(), "measure.witness");
    let path = fixture(
        root.path(),
        "measure.fixture",
        &format!("{}exec /bin/sleep 300\n", witness.fixture_prelude()),
    );
    let mut command = Command::new(path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let child = command.spawn().expect("the measured group must spawn");
    let group_id = child.id().expect("the measured group pid") as libc::pid_t;
    // Owned for the whole test, so a failing assertion cannot leave the `sleep`
    // behind.
    let mut measured = MeasuredGroup {
        group_id,
        child: Some(child),
    };

    assert!(
        witness.await_handshake(READY_SETTLE).await,
        "the measured group must take hold of its witness"
    );
    assert!(
        !witness.released_within(LIVENESS_SETTLE).await,
        "a live descendant must keep the witness open"
    );

    measured.terminate();
    assert!(
        witness.released_within(TEARDOWN_SETTLE).await,
        "terminating the group must release the witness"
    );
    // With every live holder gone, end-of-file is a stable answer. However many
    // unreaped zombies the leader leaves behind must not change it.
    assert!(
        witness.released_within(LIVENESS_SETTLE).await,
        "end-of-file must stay observable once every live holder is gone"
    );
    drop(measured);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fixtures_are_released_even_when_an_assertion_unwinds() {
    // V2.1.2 leaked the unrelated group with `mem::forget` and killed it on the
    // last line of the body, so a panic anywhere in between left a `sleep 300`
    // and a temporary directory behind. Both are released by drop instead, and
    // this is the only test here that deliberately unwinds.
    let root = FixtureRoot::new("unwind");
    let unrelated = UnrelatedGroup::spawn(root.path(), "unrelated.sh").await;
    let observer = unrelated.observer();
    unrelated.assert_alive().await;

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        // A dedicated thread keeps the deliberate panic away from the test
        // harness's own reporting, so the only thing this test proves is that the
        // guard is released by the unwind.
        std::thread::spawn(move || {
            // Dropped while the panic below unwinds out of this frame.
            let _unrelated = unrelated;
            panic!("deliberate assertion failure exercising fixture cleanup");
        })
        .join()
    }));
    let joined = outcome.expect("the unwinding thread must finish");
    assert!(joined.is_err(), "the guard-owning thread must have unwound");

    // The guard is gone. An independent reader proves the group it owned is gone
    // too, because end-of-file can only arrive once no live process still holds
    // the write end. The handshake precondition already holds: it was observed
    // above, before the guard was dropped.
    assert!(
        released_within(&observer, TEARDOWN_SETTLE).await,
        "a test that unwinds must still release the group it created"
    );
    // The fixture root is owned too, and is removed when this frame ends. The
    // assertion above is the one that matters; the directory is the reason
    // `FixtureRoot` has a `Drop` at all.
    assert!(
        root.path().is_dir(),
        "the fixture root exists until it is dropped"
    );
}
