//! Host-owned helper that performs one atomic workspace publication.
//!
//! This is a Cargo binary target shipped next to `local-mcp`, exactly like the existing
//! `codex-linux-sandbox` helper. It exists so the commit can run **inside** the platform
//! sandbox with the validated parent as the only writable root, instead of being
//! performed by the unrestricted host process. Performing it in-process would have
//! replaced a kernel-enforced containment boundary with in-process correctness, which
//! is strictly more authority than the Writer had before this branch.
//!
//! It is deliberately *not* model-selectable: the host resolves its path from its own
//! executable directory and passes a structured, framed request on stdin. The model
//! never chooses this program, never supplies its arguments, and never supplies any
//! authority-bearing field.
//!
//! # Wire format
//!
//! All integers are little-endian, and every field is length-prefixed so the framing is
//! unambiguous and no two distinct requests share a byte stream.
//!
//! ```text
//! magic      : b"local-mcp/atomic-publish/v2\n"
//! parent     : u32 len | bytes
//! target     : u32 len | bytes
//! preimage   : u8 kind                  (0 = ABSENT, 1 = SHA256)
//!               kind == 1 => u32 len | lowercase hex digest
//! request id : u32 len | bytes
//! lock path  : u32 len | bytes          (empty = no lock witness)
//! lock token : u32 len | bytes          (empty = no lock witness)
//! content    : u64 len | bytes
//! ```
//!
//! The helper takes the publication lock before it reads the workspace or
//! writes anything, and holds it until this process exits. The host proves the
//! helper is gone by acquiring the same lock; see `publication_lock` for why a
//! PID is not evidence.

// This binary is a thin, test-free entry point: the wire format is covered by
// `atomic_publish_frame`'s own tests and the commit itself by `workspace_publish`'s.
// Both modules are compiled into this binary, so in the `--test` target there is no
// `main` entry point reaching them and the compiler would otherwise report the whole
// publication path as dead code. That is an artifact of where the code is compiled,
// not an unreachable path.
#![cfg_attr(
    test,
    allow(dead_code, reason = "the helper has no entry point in the test target")
)]

#[path = "../workspace_publish.rs"]
mod workspace_publish;

#[path = "../atomic_publish_frame.rs"]
mod atomic_publish_frame;

#[path = "../publication_lock.rs"]
mod publication_lock;

fn main() {
    let mut input = Vec::new();
    if let Err(error) = std::io::Read::read_to_end(&mut std::io::stdin(), &mut input) {
        eprintln!("atomic-publish cannot read its request: {error}");
        std::process::exit(64);
    }
    let request = match crate::atomic_publish_frame::decode(&input) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("atomic-publish rejected its request: {error}");
            std::process::exit(65);
        }
    };

    let expected = match request.preimage_kind {
        crate::atomic_publish_frame::PREIMAGE_ABSENT => workspace_publish::ExpectedPreimage::Absent,
        crate::atomic_publish_frame::PREIMAGE_SHA256 => {
            workspace_publish::ExpectedPreimage::Sha256(request.preimage_digest)
        }
        other => {
            eprintln!("atomic-publish rejected an unknown preimage kind: {other}");
            std::process::exit(65);
        }
    };

    let parent = std::path::PathBuf::from(&request.parent);
    let target = std::path::PathBuf::from(&request.target);

    // Quiescence witness: acquire the host-armed lock *before* touching the
    // workspace, and keep it for the whole publication. A refusal here means the
    // host either aborted this operation or never armed it, so this process must
    // not write anything.
    let _publication_lock = if request.lock_path.is_empty() {
        None
    } else {
        match crate::publication_lock::claim_for_helper(
            std::path::Path::new(&request.lock_path),
            &request.lock_token,
        ) {
            Ok(lock) => Some(lock),
            Err(error) => {
                eprintln!("atomic-publish refused its publication lock: {error}");
                std::process::exit(66);
            }
        }
    };

    // H5.3-C: test-only hold inside the publication critical section.
    //
    // Compiled only under the non-default `test-grant-fixture` feature, so a
    // shipped helper has no such seam. It lets a crash-recovery test observe the
    // helper *while it holds the lock and before it writes*, which is the only
    // state in which "the host died mid-publication" is reproducible.
    #[cfg(feature = "test-grant-fixture")]
    test_hold::hold("in-critical-section", &request.lock_path);

    match workspace_publish::publish(workspace_publish::PublishRequest {
        destination: &target,
        parent: &parent,
        expected_preimage: &expected,
        request_id: &request.request_id,
        content: &request.content,
    }) {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            // The class is emitted as a stable token so the host can tell a refusal
            // (destination provably untouched) from a publication whose outcome cannot
            // be proven. That distinction drives recovery and must not depend on
            // parsing a human-readable sentence.
            eprintln!("goallatch-publish-error:{}", error.class());
            eprintln!("atomic-publish failed: {error}");
            std::process::exit(1);
        }
    }
}

/// H5.3-C crash-recovery harness seam.
///
/// The whole module is compiled out without the `test-grant-fixture` feature, so
/// this cannot exist in a shipped helper. When `LOCAL_MCP_TEST_HELPER_HOLD`
/// names a directory, the helper publishes `<point>.reached` (its own pid and the
/// lock path) and waits for `<point>.release` before it touches the workspace.
///
/// A crash-recovery test uses it to prove the helper was *inside* the critical
/// section — holding the publication lock, having written nothing yet — and only
/// then kills the host. Because it never runs without the test feature, it cannot
/// weaken the production publication path.
#[cfg(feature = "test-grant-fixture")]
mod test_hold {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// How long the helper waits for a release before continuing on its own.
    ///
    /// A bounded wait matters: a harness that dies must not leave the helper
    /// holding the publication fence forever.
    const HOLD_LIMIT: Duration = Duration::from_secs(120);

    pub(super) fn hold(point: &str, lock_path: &str) {
        let Ok(directory) = std::env::var("LOCAL_MCP_TEST_HELPER_HOLD") else {
            return;
        };
        if directory.is_empty() {
            return;
        }
        let directory = PathBuf::from(directory);
        if std::fs::create_dir_all(&directory).is_err() {
            return;
        }
        let reached = directory.join(format!("{point}.reached"));
        let release = directory.join(format!("{point}.release"));
        let record = format!("pid={}\nlock={lock_path}\n", std::process::id());
        if std::fs::write(&reached, record).is_err() {
            return;
        }
        let deadline = Instant::now() + HOLD_LIMIT;
        while Instant::now() < deadline {
            if release.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
