//! Install Contract V1 helper-resolution rules.
//!
//! The installed runtime never searches `PATH`, the working directory, or a
//! configuration value for its helpers. Both the Linux sandbox helper and the
//! Writer commit helper are resolved as siblings of the running executable, so
//! an install is exactly the directory the executable lives in. Keeping the rule
//! in one place lets the release/install contract test the same candidates the
//! runtime uses without adding a fallback path.

use std::path::{Path, PathBuf};

/// The Writer commit helper's installed name.
pub(crate) const ATOMIC_PUBLISH: &str = if cfg!(windows) {
    "atomic-publish.exe"
} else {
    "atomic-publish"
};

/// The Linux sandbox helper's installed name. It is never shipped on other
/// platforms, but the name is stable for contract and negative tests.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "the Linux helper is not resolved on this platform"
    )
)]
pub(crate) const LINUX_SANDBOX: &str = "codex-linux-sandbox";

/// Candidate paths for a helper resolved next to `executable`.
///
/// There is exactly one production candidate: the helper in the executable's
/// own directory. Returning an empty list (rather than guessing a directory)
/// keeps a host path without a parent a hard failure.
pub(crate) fn sibling_candidates(executable: &Path, helper: &str) -> Vec<PathBuf> {
    executable
        .parent()
        .map(|directory| vec![directory.join(helper)])
        .unwrap_or_default()
}

/// The first candidate that exists as a regular file, if any.
pub(crate) fn first_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
}
