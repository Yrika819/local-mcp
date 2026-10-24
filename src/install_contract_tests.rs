//! Tests for the Install Contract V1 helper-resolution rule.
//!
//! The runtime resolves helpers from `std::env::current_exe()`'s parent. These
//! tests cover the rule with synthetic paths and then observe what
//! `current_exe()` actually returns when the same test binary is launched
//! through a symlink. That second part is deliberately an observation, not an
//! assumption: the answer differs between operating systems, and the helper
//! contract is anchored to whatever the OS reports.

use std::path::{Path, PathBuf};

use crate::install_contract;

const PROBE_ENV: &str = "GOALLATCH_INSTALL_CONTRACT_CURRENT_EXE_PROBE";
const PROBE_MARKER: &str = "goallatch-current-exe=";

fn scratch(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "local-mcp-install-contract-{label}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).expect("create install-contract scratch directory");
    root
}

fn write_executable(path: &Path) {
    std::fs::write(path, b"fixture").expect("write fixture executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)
            .expect("stat fixture executable")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod fixture executable");
    }
}

/// Child half of the symlink observation. It only reports the path the OS says
/// the running executable has; it performs no helper lookup of its own.
#[test]
fn current_exe_probe_child() {
    if std::env::var_os(PROBE_ENV).is_none() {
        return;
    }
    let executable = std::env::current_exe().expect("current_exe in probe child");
    println!("{PROBE_MARKER}{}", executable.display());
}

fn run_probe(executable: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new(executable)
        .arg("--exact")
        .arg("install_contract_tests::current_exe_probe_child")
        .arg("--nocapture")
        .env(PROBE_ENV, "1")
        .output()
        .expect("run current_exe probe");
    assert!(
        output.status.success(),
        "probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(PROBE_MARKER))
        .map(PathBuf::from)
}

#[test]
fn helper_name_matches_the_platform() {
    if cfg!(windows) {
        assert_eq!(install_contract::ATOMIC_PUBLISH, "atomic-publish.exe");
    } else {
        assert_eq!(install_contract::ATOMIC_PUBLISH, "atomic-publish");
    }
    assert_eq!(install_contract::LINUX_SANDBOX, "codex-linux-sandbox");
}

#[test]
fn same_directory_helper_is_found() {
    let root = scratch("same-directory");
    let install = root.join("install");
    std::fs::create_dir_all(&install).unwrap();
    let executable = install.join("local-mcp");
    let helper = install.join(install_contract::ATOMIC_PUBLISH);
    write_executable(&executable);
    write_executable(&helper);

    let candidates =
        install_contract::sibling_candidates(&executable, install_contract::ATOMIC_PUBLISH);
    assert_eq!(candidates, vec![helper.clone()]);
    assert_eq!(install_contract::first_existing(&candidates), Some(helper));
}

#[test]
fn missing_helper_is_not_synthesized() {
    let root = scratch("missing");
    let install = root.join("install");
    std::fs::create_dir_all(&install).unwrap();
    let executable = install.join("local-mcp");
    write_executable(&executable);

    let candidates =
        install_contract::sibling_candidates(&executable, install_contract::ATOMIC_PUBLISH);
    assert_eq!(install_contract::first_existing(&candidates), None);
}

#[test]
fn helper_in_another_directory_is_not_used() {
    let root = scratch("separate-directory");
    let install = root.join("install");
    let elsewhere = root.join("path-or-cwd");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let executable = install.join("local-mcp");
    write_executable(&executable);
    write_executable(&elsewhere.join(install_contract::ATOMIC_PUBLISH));

    let candidates =
        install_contract::sibling_candidates(&executable, install_contract::ATOMIC_PUBLISH);
    assert!(
        !candidates.contains(&elsewhere.join(install_contract::ATOMIC_PUBLISH)),
        "a helper outside the executable directory must never be a candidate"
    );
    assert_eq!(install_contract::first_existing(&candidates), None);
}

/// Observe `current_exe()` through a symlink and prove helper resolution is
/// anchored to the directory the OS reports, never to PATH.
#[cfg(any(unix, windows))]
#[test]
fn symlink_launch_anchors_helper_resolution_to_current_exe() {
    let root = scratch("symlink");
    let install = root.join("install");
    let links = root.join("link-bin");
    let path_only = root.join("path-only");
    std::fs::create_dir_all(&install).unwrap();
    std::fs::create_dir_all(&links).unwrap();
    std::fs::create_dir_all(&path_only).unwrap();

    let real = install.join("local-mcp");
    std::fs::copy(std::env::current_exe().unwrap(), &real).expect("copy test executable");
    let link = links.join("local-mcp");

    #[cfg(unix)]
    let symlink_result = std::os::unix::fs::symlink(&real, &link);
    #[cfg(windows)]
    let symlink_result = std::os::windows::fs::symlink_file(&real, &link);
    if symlink_result.is_err() {
        // Windows requires Developer Mode or SeCreateSymbolicLinkPrivilege.
        // Skipping is honest: this host cannot observe the symlink case.
        return;
    }

    let observed = run_probe(&link).expect("probe printed current_exe");
    println!(
        "current_exe through symlink: launch={} observed={}",
        link.display(),
        observed.display()
    );
    assert!(
        observed.is_file(),
        "current_exe() must name a real file, got {}",
        observed.display()
    );
    let observed_parent = observed
        .parent()
        .expect("current_exe() has a parent")
        .to_path_buf();
    assert!(
        observed_parent.starts_with(&root),
        "observed path {} escaped the scratch root {}",
        observed.display(),
        root.display()
    );

    // A helper anywhere else -- the other side of the symlink, or a PATH-only
    // directory -- is not a candidate. This is the failure mode of a
    // symlink-only install when the OS reports the symlink path.
    let expected = observed_parent.join(install_contract::ATOMIC_PUBLISH);
    let other_side = if observed_parent == links {
        &install
    } else {
        &links
    };
    write_executable(&other_side.join(install_contract::ATOMIC_PUBLISH));
    write_executable(&path_only.join(install_contract::ATOMIC_PUBLISH));
    let candidates =
        install_contract::sibling_candidates(&observed, install_contract::ATOMIC_PUBLISH);
    assert_eq!(candidates, vec![expected.clone()]);
    assert_eq!(install_contract::first_existing(&candidates), None);
    assert!(
        !candidates.contains(&path_only.join(install_contract::ATOMIC_PUBLISH)),
        "PATH must not be searched for helpers"
    );

    // The helper is found only when it is beside the path the OS reported.
    write_executable(&expected);
    assert_eq!(
        install_contract::first_existing(&candidates),
        Some(expected)
    );
}
