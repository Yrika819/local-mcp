//! H5.2-D — real subprocess publication-lock witness.
//!
//! This module is compiled only into the `local-mcp` test harness, never into
//! the `atomic-publish` helper. The helper test binary would otherwise re-exec
//! itself while other tests run, and an inherited file description can keep a
//! kernel lock alive across the exec. Keeping this witness in the host test
//! binary removes that interference; the lock semantics it exercises are the
//! production ones in [`crate::publication_lock`].

use std::time::Duration;

use crate::publication_lock::{QuiescenceProbe, arm, claim_for_helper, probe_and_abort};

fn path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "goallatch-publication-lock-subprocess-{label}-{}.lock",
        uuid::Uuid::new_v4()
    ))
}

/// H51-05: a real child process (not an in-process handle) holds the lock.
///
/// The child is this test binary re-executed in an `#[ignore]`d mode. The parent
/// proves `Active` while the child lives, kills it, and proves that only the
/// observed process death releases the lock.
#[test]
#[ignore = "spawned by a_real_subprocess_lock_holder_blocks_quiescence_until_it_dies"]
fn publication_lock_holder_child() {
    let Some(lock_path) = std::env::var_os("GOALLATCH_LOCK_HOLDER_PATH") else {
        return;
    };
    let token = std::env::var("GOALLATCH_LOCK_HOLDER_TOKEN").expect("token");
    let ready = std::path::PathBuf::from(
        std::env::var_os("GOALLATCH_LOCK_HOLDER_READY").expect("ready path"),
    );
    let _held = claim_for_helper(std::path::Path::new(&lock_path), &token)
        .expect("child claims the armed lock");
    std::fs::write(ready, b"ready").expect("child signals readiness");
    std::thread::sleep(Duration::from_secs(120));
}

#[test]
fn a_real_subprocess_lock_holder_blocks_quiescence_until_it_dies() {
    let lock_path = path("holder");
    let ready = lock_path.with_extension("ready");
    arm(&lock_path, "token-subprocess").unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "publication_lock_subprocess_tests::publication_lock_holder_child",
            "--ignored",
        ])
        .env("GOALLATCH_LOCK_HOLDER_PATH", &lock_path)
        .env("GOALLATCH_LOCK_HOLDER_TOKEN", "token-subprocess")
        .env("GOALLATCH_LOCK_HOLDER_READY", &ready)
        .spawn()
        .expect("spawn the lock-holder child");

    for _ in 0..200 {
        if ready.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "the child did not claim the lock");
    assert_eq!(probe_and_abort(&lock_path), QuiescenceProbe::Active);

    child.kill().expect("kill the real lock holder");
    let _ = child.wait();
    let mut quiescent = false;
    for _ in 0..200 {
        if probe_and_abort(&lock_path) == QuiescenceProbe::Quiescent {
            quiescent = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        quiescent,
        "a dead holder must release the lock and allow a durable abort"
    );
    let _ = std::fs::remove_file(&lock_path);
    let _ = std::fs::remove_file(&ready);
}
