//! Cross-process publication liveness latch shared by the host and the
//! host-owned `atomic-publish` helper.
//!
//! # Why a file lock and not a PID
//!
//! The H5 independent review found that a publication helper could outlive the
//! host's idea of it: a deadline cleanup could time out without observing the
//! leader's exit, and a host crash could leave the helper running. Recording a
//! PID is not evidence, because a PID is reused and a descendant may have a
//! different PID entirely.
//!
//! This module makes the helper itself the witness. Before the helper may touch
//! the workspace it must take an exclusive OS file lock on a host-derived lock
//! file and keep it for its whole lifetime. The host proves quiescence by
//! *acquiring that same lock*: success means no helper currently holds it.
//!
//! # The abort latch
//!
//! A bare lock has a race. If the host dies between spawning the helper and the
//! helper acquiring the lock, a restarted host could acquire the free lock and
//! conclude "quiescent" moments before the helper starts writing.
//!
//! The file therefore carries a one-way token:
//!
//! * the host arms it with `ARMED <token>` before spawning;
//! * the helper acquires the lock, reads the file, and refuses unless the token
//!   matches the token in its frame;
//! * the host proves quiescence by acquiring the lock and rewriting the file as
//!   `ABORTED` before releasing it.
//!
//! Because the host only rewrites the file while holding the lock, and the helper
//! only reads it while holding the same lock, the two cannot interleave. After
//! `ABORTED` is durable, a helper that has not yet acquired the lock can only
//! acquire it later, read `ABORTED`, and refuse. A helper that already acquired
//! the lock prevents the host from acquiring it, so the host reports "active"
//! rather than "quiescent".
//!
//! # Scope
//!
//! This proves the *helper process* is no longer in its publication critical
//! section. It is not a general process-tree containment proof: a deliberately
//! detached descendant that closes the inherited descriptor is the POSIX design
//! constraint recorded as D1. The publication helper spawns no descendants, and
//! a descendant that inherits the descriptor keeps the lock held, which is the
//! safe direction (the quarantine stays set).
//!
//! The lock is advisory and the state root is owned by the same OS user. A
//! same-user attacker can already replace the helper or the state file, so this
//! is not, and does not claim to be, the C3 trust boundary.

// The module is compiled into both binaries. The host uses the probe and the
// helper uses the claim; each binary sees the other's items as unused.
#![allow(
    dead_code,
    reason = "The host and the host-owned helper compile this module separately and each uses only its own half."
)]

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

/// The current host process generation.
///
/// Stable for the lifetime of the process and different after a restart. It is
/// evidence provenance only: it lets durable records distinguish "this host
/// proved quiescence" from "an earlier host armed the helper", and it is never
/// treated as proof that a helper is gone.
pub(crate) fn host_generation() -> &'static str {
    static GENERATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    GENERATION.get_or_init(|| uuid::Uuid::new_v4().to_string())
}
const ARMED_PREFIX: &str = "ARMED ";
const ABORTED: &str = "ABORTED";
const CLAIM_TIMEOUT: Duration = Duration::from_secs(5);
const CLAIM_POLL: Duration = Duration::from_millis(10);

/// Result of the host's quiescence probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QuiescenceProbe {
    /// The host acquired the lock and durably wrote `ABORTED`. No helper is in
    /// its critical section, and no future helper can enter it.
    Quiescent,
    /// Another live process holds the lock. The helper (or a descendant that
    /// inherited the descriptor) may still be able to write.
    Active,
    /// The host could not prove either state. Treated as unresolved.
    Unproven(&'static str),
}

/// Create and arm a fresh lock file. `create_new` is deliberate: an existing
/// file belongs to another operation, and reusing it would let one operation's
/// quiescence evidence clear another's.
pub(crate) fn arm(path: &Path, token: &str) -> io::Result<()> {
    if token.is_empty() || token.len() > 128 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "publication lock token is out of bounds",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    writeln!(file, "{ARMED_PREFIX}{token}")?;
    file.sync_all()?;
    Ok(())
}

/// Prove quiescence and permanently abort the armed token.
///
/// The `ABORTED` write is the durable part: after this returns `Quiescent`, any
/// helper that starts late will read it and refuse before touching the
/// workspace.
pub(crate) fn probe_and_abort(path: &Path) -> QuiescenceProbe {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return QuiescenceProbe::Unproven("publication lock file is missing");
        }
        Err(_) => return QuiescenceProbe::Unproven("publication lock file is unreadable"),
    };
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return QuiescenceProbe::Active,
        Err(std::fs::TryLockError::Error(_)) => {
            return QuiescenceProbe::Unproven("publication lock probe failed");
        }
    }
    let mut content = String::new();
    if file.read_to_string(&mut content).is_err() {
        return QuiescenceProbe::Unproven("publication lock content is unreadable");
    }
    if content.trim_end() == ABORTED {
        // A previous recovery already aborted this token. No helper can enter.
        return QuiescenceProbe::Quiescent;
    }
    if !content.starts_with(ARMED_PREFIX) {
        return QuiescenceProbe::Unproven("publication lock content is not a recognised token");
    }
    if write_aborted(&mut file).is_err() {
        return QuiescenceProbe::Unproven("publication lock abort could not be persisted");
    }
    QuiescenceProbe::Quiescent
}

fn write_aborted(file: &mut File) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{ABORTED}")?;
    file.sync_all()
}

/// A helper-held lock. Dropping it releases the OS lock at process exit.
pub(crate) struct HelperPublicationLock {
    _file: File,
}

/// Acquire the armed lock on the helper side and verify the token.
///
/// Every failure is a refusal to publish. The caller must not fall back to an
/// unlocked write: an unlocked helper would have no quiescence witness at all.
pub(crate) fn claim_for_helper(path: &Path, token: &str) -> io::Result<HelperPublicationLock> {
    if token.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "publication lock token is missing",
        ));
    }
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let deadline = Instant::now() + CLAIM_TIMEOUT;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(CLAIM_POLL);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "timed out waiting for the publication lock",
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    let expected = format!("{ARMED_PREFIX}{token}");
    if content.trim_end() != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "publication lock is not armed for this operation",
        ));
    }
    Ok(HelperPublicationLock { _file: file })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "goallatch-publication-lock-{label}-{}.lock",
            Uuid::new_v4()
        ))
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn a_free_armed_lock_is_quiescent_and_becomes_aborted() {
        let path = path("free");
        arm(&path, "token-1").unwrap();
        assert_eq!(read(&path), "ARMED token-1\n");
        assert_eq!(probe_and_abort(&path), QuiescenceProbe::Quiescent);
        assert_eq!(read(&path), "ABORTED\n");
        // The latch is one-way and idempotent.
        assert_eq!(probe_and_abort(&path), QuiescenceProbe::Quiescent);
        assert_eq!(read(&path), "ABORTED\n");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_held_lock_is_active_and_a_late_helper_refuses_after_abort() {
        let path = path("held");
        arm(&path, "token-2").unwrap();
        // A separate open file description is a real kernel lock witness, not a
        // state flag. It models a helper that is still inside its critical
        // section.
        let helper = claim_for_helper(&path, "token-2").unwrap();
        assert_eq!(probe_and_abort(&path), QuiescenceProbe::Active);
        drop(helper);
        assert_eq!(probe_and_abort(&path), QuiescenceProbe::Quiescent);
        assert!(claim_for_helper(&path, "token-2").is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_helper_with_the_wrong_token_cannot_publish() {
        let path = path("wrong-token");
        arm(&path, "token-3").unwrap();
        assert!(claim_for_helper(&path, "token-4").is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_missing_lock_file_is_unproven_not_quiescent() {
        let path = path("missing");
        assert_eq!(
            probe_and_abort(&path),
            QuiescenceProbe::Unproven("publication lock file is missing")
        );
    }

    #[test]
    fn an_unrecognised_lock_file_is_unproven() {
        let path = path("garbage");
        std::fs::write(&path, "not a token\n").unwrap();
        assert_eq!(
            probe_and_abort(&path),
            QuiescenceProbe::Unproven("publication lock content is not a recognised token")
        );
        let _ = std::fs::remove_file(path);
    }
}
