//! Starting a program whose file may still be open for writing elsewhere.
//!
//! The kernel refuses to `execve` a file that any process still has open for
//! writing, and reports it as `ETXTBSY`. That refusal is transient and
//! side-effect free: no process is started and nothing about the requested
//! program is decided, so retrying cannot double-start anything and cannot turn
//! a refusal into a weaker one.
//!
//! It is worth tolerating rather than reporting as a missing or unusable
//! program, because writing an executable and then running it is ordinary: a
//! package manager replacing `bwrap` is exactly the moment a host would be
//! upgrading to pick up a sandbox security fix, and that is the worst possible
//! time for a security gate to change its mind about whether the runtime is
//! present.
//!
//! Only `ETXTBSY` is retried. Every other error, including a missing,
//! non-executable or malformed program, is returned immediately and unchanged,
//! so a genuinely unusable program still fails closed and is still classified
//! as a pre-start refusal.
//!
//! This module is deliberately free of async and of process-group machinery, so
//! the synchronous Bubblewrap gate in the sandbox helper can share it with the
//! asynchronous launcher in the parent.

use std::io;
use std::time::{Duration, Instant};

/// How long a start may be retried while the program is momentarily busy.
const BUSY_RETRY_BUDGET: Duration = Duration::from_millis(100);

/// Gap between retries of a momentarily busy program.
const BUSY_RETRY_INTERVAL: Duration = Duration::from_millis(1);

/// Whether `error` is the kernel's transient refusal to start a program whose
/// file some other process still has open for writing.
#[cfg(unix)]
fn is_busy(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ETXTBSY)
}

#[cfg(not(unix))]
fn is_busy(_error: &io::Error) -> bool {
    false
}

/// The retry state for one start attempt.
///
/// Held across the attempts of a single start so the budget bounds that start
/// rather than each attempt.
pub(crate) struct BusyProgramRetry {
    started: Instant,
}

impl BusyProgramRetry {
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }

    /// Whether the caller should make another attempt after `error`.
    ///
    /// Returns `false` for every error that is not a momentary busy refusal, and
    /// once the budget for this start is used up, so the original error is then
    /// reported to the caller unchanged.
    pub(crate) fn retry(&self, error: &io::Error) -> bool {
        if !is_busy(error) {
            return false;
        }
        if self.started.elapsed() >= BUSY_RETRY_BUDGET {
            return false;
        }
        std::thread::sleep(BUSY_RETRY_INTERVAL);
        true
    }
}
