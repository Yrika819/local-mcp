//! Stress for the Unix process-group ownership lease.
//!
//! Every case drives a real process group through the production launcher. The
//! group identifier is the fixture's own process identifier, published by the
//! fixture itself before it does anything else, so the test never has to guess
//! which group it is talking about and never has to reuse a stale identifier.
//! Liveness of a group is then read from the kernel with a negative-group
//! existence check, and every case additionally keeps an unrelated,
//! test-owned group alive and requires that it is still alive at the end.
//!
//! Covered: normal zero exit, nonzero exit, timeout, blocked stdin, retained
//! stdout/stderr pipes, descendants, a leader that exits before its descendants,
//! cancellation of an in-flight run, many repeated short-lived groups, and high
//! parallelism. Verified for each case: the expected lifecycle outcome, that no
//! unrelated group was signalled, that no group the launcher owned survived
//! cleanup, and that no unreaped group owner was left behind.

#![cfg(all(test, unix))]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::process::Command;

use crate::agent::{AgentError, run_bounded_host_process};

/// Deadline handed to the launcher for the cases that must complete on their own.
const FAST_DEADLINE: Duration = Duration::from_secs(20);

/// Deadline handed to the launcher for the cases that must time out.
const RUN_DEADLINE: Duration = Duration::from_millis(400);

/// How long a group is given to disappear after cleanup.
const TEARDOWN_SETTLE: Duration = Duration::from_secs(5);

/// How long a group is given to be seen still alive.
const LIVENESS_SETTLE: Duration = Duration::from_millis(500);

fn temporary_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "local-mcp-pgroup-stress-{label}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn fixture(root: &Path, name: &str, body: &str) -> PathBuf {
    let path = root.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

/// The identifier a fixture publishes for itself. Because the launcher places
/// every child in its own group, this is also the group identifier.
async fn wait_for_group_id(path: &Path) -> libc::pid_t {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(contents) = std::fs::read_to_string(path)
                && let Ok(id) = contents.trim().parse::<libc::pid_t>()
                && id > 0
            {
                break id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("fixture never published its process-group identifier"))
}

fn group_is_alive(group_id: libc::pid_t) -> bool {
    unsafe { libc::kill(-group_id, 0) == 0 }
}

/// The single-element argv that runs a fixture.
fn command_of(path: &Path) -> [String; 1] {
    [path.to_string_lossy().into_owned()]
}

async fn assert_group_gone(group_id: libc::pid_t) {
    let deadline = Instant::now() + TEARDOWN_SETTLE;
    while Instant::now() < deadline {
        if !group_is_alive(group_id) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _ = unsafe { libc::kill(-group_id, libc::SIGKILL) };
    panic!("process group {group_id} survived cleanup");
}

async fn assert_group_alive(group_id: libc::pid_t) {
    tokio::time::sleep(LIVENESS_SETTLE).await;
    assert!(
        group_is_alive(group_id),
        "SECURITY REGRESSION: unrelated process group {group_id} was terminated"
    );
}

fn group_members(group_id: libc::pid_t) -> bool {
    group_is_alive(group_id)
}

/// An unrelated, test-owned group that must survive everything below.
async fn unrelated_group(root: &Path, name: &str) -> libc::pid_t {
    let path = fixture(root, name, "exec /bin/sleep 300");
    let mut command = Command::new(path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let child = command.spawn().expect("unrelated group must spawn");
    let group_id = child.id().expect("unrelated group pid") as libc::pid_t;
    // Deliberately leaked into the runtime's reaper: it must still be alive
    // when the assertions finish, and must not be mistaken for our own group.
    std::mem::forget(child);
    group_id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_and_nonzero_exit_release_the_group_without_residue() {
    let root = temporary_root("exit");
    let unrelated = unrelated_group(&root, "unrelated.sh").await;

    for (name, body, status) in [
        ("zero.sh", "/bin/cat > /dev/null\nprintf done\nexit 0\n", 0),
        ("nonzero.sh", "/bin/cat > /dev/null\nexit 9\n", 9),
    ] {
        let pid_file = root.join(format!("{name}.pid"));
        let path = fixture(
            &root,
            name,
            &format!("printf '%s' \"$$\" > '{}'\n{body}", pid_file.display()),
        );
        let command = command_of(&path);
        let (result, group_id) = tokio::join!(
            run_bounded_host_process(&command, b"prompt", &root, FAST_DEADLINE),
            wait_for_group_id(&pid_file),
        );
        let output = result.expect("a clean fixture must complete");
        assert_eq!(
            output.status, status,
            "{name} must report its own exit status"
        );
        // A completed run still releases its lease, so nothing may remain in
        // the group it owned.
        assert_group_gone(group_id).await;
    }
    assert_group_alive(unrelated).await;
    let _ = unsafe { libc::kill(-unrelated, libc::SIGKILL) };
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_terminates_the_group_and_no_descendant_survives() {
    let root = temporary_root("timeout");
    let unrelated = unrelated_group(&root, "unrelated.sh").await;
    let pid_file = root.join("timeout.pid");
    let path = fixture(
        &root,
        "timeout.sh",
        &format!(
            "printf '%s' \"$$\" > '{}'\n/bin/sleep 300 &\nexec /bin/sleep 300\n",
            pid_file.display()
        ),
    );
    let started = Instant::now();
    let command = [path.to_string_lossy().into_owned()];
    let (result, group_id) = tokio::join!(
        run_bounded_host_process(&command, b"prompt", &root, RUN_DEADLINE,),
        wait_for_group_id(&pid_file),
    );
    assert_eq!(result.expect_err("must time out"), AgentError::Timeout);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_group_gone(group_id).await;
    assert_group_alive(unrelated).await;
    let _ = unsafe { libc::kill(-unrelated, libc::SIGKILL) };
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_stdin_write_is_bounded_and_terminates_the_group() {
    let root = temporary_root("stdin");
    let unrelated = unrelated_group(&root, "unrelated.sh").await;
    let pid_file = root.join("stdin.pid");
    // Never reads stdin and outlives the deadline.
    let path = fixture(
        &root,
        "blocked.sh",
        &format!(
            "printf '%s' \"$$\" > '{}'\nexec /bin/sleep 300\n",
            pid_file.display()
        ),
    );
    let started = Instant::now();
    let command = [path.to_string_lossy().into_owned()];
    let blocked_prompt = vec![b'x'; 4 * 1024 * 1024];
    let (result, group_id) = tokio::join!(
        run_bounded_host_process(&command, &blocked_prompt, &root, RUN_DEADLINE),
        wait_for_group_id(&pid_file),
    );
    assert_eq!(result.expect_err("must time out"), AgentError::Timeout);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_group_gone(group_id).await;
    assert_group_alive(unrelated).await;
    let _ = unsafe { libc::kill(-unrelated, libc::SIGKILL) };
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leader_that_exits_before_its_descendant_still_cleans_up() {
    let root = temporary_root("early-exit");
    let unrelated = unrelated_group(&root, "unrelated.sh").await;
    let pid_file = root.join("early.pid");
    // The leader exits at once, leaving a descendant inside the group that keeps
    // the inherited stdout and stderr open. This is the case V2.1.1 signalled by
    // using a group identifier whose leader it had already reaped.
    let path = fixture(
        &root,
        "early.sh",
        &format!(
            "printf '%s' \"$$\" > '{}'\n/bin/sleep 300 &\nexec /bin/sleep 0\n",
            pid_file.display()
        ),
    );
    let command = [path.to_string_lossy().into_owned()];
    let (result, group_id) = tokio::join!(
        run_bounded_host_process(&command, b"prompt", &root, RUN_DEADLINE,),
        wait_for_group_id(&pid_file),
    );
    assert_eq!(result.expect_err("must time out"), AgentError::Timeout);
    assert_group_gone(group_id).await;
    assert_group_alive(unrelated).await;
    let _ = unsafe { libc::kill(-unrelated, libc::SIGKILL) };
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_an_in_flight_run_terminates_the_group() {
    let root = temporary_root("cancel");
    let unrelated = unrelated_group(&root, "unrelated.sh").await;
    let pid_file = root.join("cancel.pid");
    let path = fixture(
        &root,
        "cancel.sh",
        &format!(
            "printf '%s' \"$$\" > '{}'\n/bin/sleep 300 &\nexec /bin/sleep 300\n",
            pid_file.display()
        ),
    );
    let command = vec![path.to_string_lossy().into_owned()];
    let cwd = root.clone();
    let handle = tokio::spawn(async move {
        run_bounded_host_process(&command, b"prompt", &cwd, Duration::from_secs(300)).await
    });
    let group_id = wait_for_group_id(&pid_file).await;
    handle.abort();
    assert!(handle.await.unwrap_err().is_cancelled());
    assert_group_gone(group_id).await;
    assert_group_alive(unrelated).await;
    let _ = unsafe { libc::kill(-unrelated, libc::SIGKILL) };
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_short_lived_groups_in_parallel_leave_no_residue() {
    const ROUNDS: usize = 16;
    const PARALLEL: usize = 8;
    let root = temporary_root("parallel");
    let unrelated = unrelated_group(&root, "unrelated.sh").await;
    let completed = Arc::new(AtomicUsize::new(0));
    let groups = Arc::new(std::sync::Mutex::new(Vec::new()));

    for round in 0..ROUNDS {
        let mut tasks = Vec::new();
        for index in 0..PARALLEL {
            let root = root.clone();
            let completed = Arc::clone(&completed);
            let groups = Arc::clone(&groups);
            tasks.push(tokio::spawn(async move {
                let name = format!("r{round}-{index}.sh");
                let pid_file = root.join(format!("r{round}-{index}.pid"));
                let path = fixture(
                    &root,
                    &name,
                    &format!(
                        "printf '%s' \"$$\" > '{}'\n/bin/cat > /dev/null\nprintf done\n",
                        pid_file.display()
                    ),
                );
                let command = [path.to_string_lossy().into_owned()];
                let (result, group_id) = tokio::join!(
                    run_bounded_host_process(&command, b"prompt", &root, FAST_DEADLINE),
                    wait_for_group_id(&pid_file),
                );
                let output = result.expect("a short-lived group must complete");
                assert_eq!(output.status, 0);
                assert_eq!(output.stdout, "done");
                groups.lock().unwrap().push(group_id);
                completed.fetch_add(1, Ordering::Relaxed);
            }));
        }
        for task in tasks {
            task.await.expect("no parallel group task may panic");
        }
        // Every group from this round must already be gone, so nothing can
        // accumulate across rounds.
        for group_id in groups.lock().unwrap().drain(..) {
            assert!(
                !group_members(group_id),
                "group {group_id} survived its run"
            );
        }
    }
    assert_eq!(completed.load(Ordering::Relaxed), ROUNDS * PARALLEL);
    assert_group_alive(unrelated).await;
    let _ = unsafe { libc::kill(-unrelated, libc::SIGKILL) };
    let _ = std::fs::remove_dir_all(&root);
}
