//! Shared, fail-closed Bubblewrap support gate.
//!
//! The Linux sandbox helper searches `PATH` for a system `bwrap` binary.
//! CVE-2026-87766 is fixed upstream in bubblewrap 0.12.0; older releases can
//! follow attacker-controlled symlinks while constructing mount targets. Any
//! release older than 0.12.0 must therefore be refused.
//!
//! This module is the single implementation of that rule. It is compiled into
//! both the trusted parent (which refuses to start the sandbox helper at all)
//! and the `codex-linux-sandbox` helper binary (which keeps its own gate as
//! defence in depth). The two call sites must never disagree about the minimum
//! supported version, so the rule lives here once.

/// Minimum supported upstream Bubblewrap release.
///
/// Bubblewrap 0.12.0 is the first release that fixes CVE-2026-87766.
pub const MINIMUM_SUPPORTED_VERSION: (u64, u64, u64) = (0, 12, 0);

/// Human readable form of [`MINIMUM_SUPPORTED_VERSION`].
pub const MINIMUM_SUPPORTED_VERSION_TEXT: &str = "0.12.0";

/// Why a Bubblewrap runtime cannot be used for a sandboxed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BubblewrapSupport {
    /// A usable `bwrap` was found and it is new enough.
    Supported,
    /// A `bwrap` was found, but it predates the security fix required by
    /// CVE-2026-87766. Continuing would run the requested command against a
    /// knowingly vulnerable sandbox, so this is a platform safety refusal.
    UnsupportedVersion,
    /// No usable `bwrap` could be found or interrogated, so the sandbox
    /// environment cannot be established. This is an environment fault, not a
    /// statement about the host's safety posture.
    Unavailable,
}

impl BubblewrapSupport {
    /// The upstream bubblewrap version string parsed from `bwrap --version`.
    ///
    /// Returns `None` when the runtime could not be interrogated at all.
    fn probe(path: &std::ffi::OsStr) -> (BubblewrapSupport, Option<String>) {
        let output = match std::process::Command::new(path).arg("--version").output() {
            Ok(output) => output,
            Err(_) => return (BubblewrapSupport::Unavailable, None),
        };
        if !output.status.success() {
            return (BubblewrapSupport::Unavailable, None);
        }
        let text = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let version = match text.split_whitespace().last() {
            Some(version) => version.to_owned(),
            None => return (BubblewrapSupport::Unavailable, None),
        };
        match require_supported_version(&version) {
            Ok(()) => (BubblewrapSupport::Supported, Some(version)),
            Err(_) => (BubblewrapSupport::UnsupportedVersion, Some(version)),
        }
    }
}

/// Resolve the first `bwrap` on `path` that can be executed, mirroring the
/// lookup the sandbox helper performs.
pub fn resolve_bubblewrap(path: &std::ffi::OsStr) -> Option<std::path::PathBuf> {
    std::env::split_paths(path)
        .map(|directory| directory.join("bwrap"))
        .find(|candidate| candidate.is_file())
}

/// Decide whether the sandbox runtime available on `path` is safe to use.
///
/// The caller must treat [`BubblewrapSupport::UnsupportedVersion`] and
/// [`BubblewrapSupport::Unavailable`] as a refusal to run any command, and must
/// not retry the requested command outside the sandbox as a result.
pub fn check(path: &std::ffi::OsStr) -> BubblewrapSupport {
    let Some(program) = resolve_bubblewrap(path) else {
        return BubblewrapSupport::Unavailable;
    };
    BubblewrapSupport::probe(program.as_os_str()).0
}

/// Human readable rejection message for an unsupported or unusable runtime.
pub fn rejection_message(support: BubblewrapSupport) -> String {
    match support {
        BubblewrapSupport::Supported => {
            format!("bubblewrap {MINIMUM_SUPPORTED_VERSION_TEXT} or newer is available")
        }
        BubblewrapSupport::UnsupportedVersion => format!(
            "bubblewrap older than {MINIMUM_SUPPORTED_VERSION_TEXT} is unsupported; install \
             upstream bubblewrap {MINIMUM_SUPPORTED_VERSION_TEXT} or newer (the first release \
             fixing CVE-2026-87766)"
        ),
        BubblewrapSupport::Unavailable => format!(
            "a working bubblewrap {MINIMUM_SUPPORTED_VERSION_TEXT} or newer is required on PATH"
        ),
    }
}

/// Fail closed unless `version` is at least [`MINIMUM_SUPPORTED_VERSION`].
pub fn require_supported_version(version: &str) -> Result<(), String> {
    let (major, minor, patch) = parse_version(version)?;
    if (major, minor, patch) < MINIMUM_SUPPORTED_VERSION {
        return Err(format!(
            "bubblewrap {version} is unsupported; install upstream bubblewrap \
             {MINIMUM_SUPPORTED_VERSION_TEXT} or newer (the first release fixing \
             CVE-2026-87766)"
        ));
    }
    Ok(())
}

fn parse_version(version: &str) -> Result<(u64, u64, u64), String> {
    let mut parts = version.split('.');
    let major = parse_version_component(parts.next())?;
    let minor = parse_version_component(parts.next())?;
    let patch = parse_version_component(parts.next())?;
    if parts.next().is_some() {
        return Err(format!("malformed bubblewrap version: {version}"));
    }
    Ok((major, minor, patch))
}

fn parse_version_component(component: Option<&str>) -> Result<u64, String> {
    let component = component.ok_or_else(|| "malformed bubblewrap version".to_string())?;
    if component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!(
            "malformed bubblewrap version component: {component}"
        ));
    }
    component
        .parse()
        .map_err(|_| format!("bubblewrap version component is out of range: {component}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use uuid::Uuid;

    #[test]
    fn parses_numeric_bubblewrap_versions_strictly() {
        assert_eq!(parse_version("0.12.0"), Ok((0, 12, 0)));
        assert_eq!(parse_version("1.0.0"), Ok((1, 0, 0)));
        assert!(require_supported_version("0.11.9").is_err());
        assert!(require_supported_version("0.12.0").is_ok());
        assert!(require_supported_version("0.12.1").is_ok());
        assert!(require_supported_version("1.0.0").is_ok());
        assert!(parse_version("0.12").is_err());
        assert!(parse_version("0.12.0-ubuntu1").is_err());
        assert!(parse_version("0.x.0").is_err());
        assert!(parse_version("0.12.0.1").is_err());
    }

    fn write_fake_bwrap(root: &Path, version: &str) -> std::ffi::OsString {
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let bwrap = bin.join("bwrap");
        std::fs::write(
            &bwrap,
            format!("#!/bin/sh\nprintf 'bubblewrap {version}\\n'\n"),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&bwrap).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&bwrap, permissions).unwrap();
        bin.into_os_string()
    }

    #[test]
    fn unsupported_runtime_is_refused_and_supported_runtime_is_accepted() {
        let root = std::env::temp_dir().join(format!("local-mcp-bwrap-gate-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let old = write_fake_bwrap(&root, "0.11.1");
        assert_eq!(check(&old), BubblewrapSupport::UnsupportedVersion);
        let new = write_fake_bwrap(&root, "0.12.0");
        assert_eq!(check(&new), BubblewrapSupport::Supported);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_or_unusable_runtime_is_unavailable_not_unsupported() {
        let root = std::env::temp_dir().join(format!("local-mcp-bwrap-gone-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        assert_eq!(check(root.as_os_str()), BubblewrapSupport::Unavailable);
        // A non-executable file must not be treated as a usable runtime.
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("bwrap"), "not a program").unwrap();
        assert_eq!(check(bin.as_os_str()), BubblewrapSupport::Unavailable);
        let _ = std::fs::remove_dir_all(root);
    }
}
