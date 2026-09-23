fn main() -> ! {
    #[cfg(target_os = "linux")]
    {
        // The pinned Codex helper prefers a system bubblewrap binary found on
        // PATH. CVE-2026-87766 is fixed upstream in 0.12.0; older releases can
        // follow attacker-controlled symlinks while constructing mount targets.
        // Check before entering its normal outer setup path so unsupported
        // versions fail before the model-authored command can start.
        let is_inner_stage = std::env::args_os()
            .skip(1)
            .take_while(|arg| arg != "--")
            .any(|arg| arg == "--apply-seccomp-then-exec");
        if !is_inner_stage && let Err(error) = require_supported_bubblewrap() {
            eprintln!("Linux sandbox setup rejected: {error}");
            std::process::exit(126);
        }
        codex_linux_sandbox::run_main();
    }

    #[cfg(not(target_os = "linux"))]
    panic!("codex-linux-sandbox is only supported on Linux");
}

#[cfg(target_os = "linux")]
fn require_supported_bubblewrap() -> Result<(), String> {
    let output = std::process::Command::new("bwrap")
        .arg("--version")
        .output()
        .map_err(|error| format!("bubblewrap >= 0.12.0 is required on PATH ({error})"))?;
    if !output.status.success() {
        return Err(format!(
            "bubblewrap --version failed with {}",
            output.status
        ));
    }
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let version = text
        .split_whitespace()
        .last()
        .ok_or_else(|| "bubblewrap returned an empty version".to_string())?;
    require_supported_version(version)
}

#[cfg(target_os = "linux")]
fn require_supported_version(version: &str) -> Result<(), String> {
    let (major, minor, patch) = parse_version(version)?;
    if (major, minor, patch) < (0, 12, 0) {
        return Err(format!(
            "bubblewrap {version} is unsupported; install upstream bubblewrap 0.12.0 or newer (the first release fixing CVE-2026-87766)"
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{parse_version, require_supported_version};

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
}
