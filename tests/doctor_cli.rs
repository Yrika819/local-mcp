use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::SystemTime,
};

#[derive(Debug, Eq, PartialEq)]
struct SnapshotEntry {
    relative_path: String,
    is_file: bool,
    is_directory: bool,
    is_symlink: bool,
    length: u64,
    mode: u32,
    modified: Option<SystemTime>,
    contents: Option<Vec<u8>>,
}

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let root = env::temp_dir().join(format!(
            "goallatch-doctor-cli-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&root).expect("create isolated Doctor test root");
        fs::create_dir(root.join("path")).expect("create isolated PATH fixture");
        fs::write(root.join("owned-sentinel"), b"test-owned marker")
            .expect("write test-owned marker");
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn snapshot(root: &Path) -> Vec<SnapshotEntry> {
    fn visit(root: &Path, directory: &Path, entries: &mut Vec<SnapshotEntry>) {
        for entry in fs::read_dir(directory).expect("read only test-owned fixture directory") {
            let path = entry.expect("read test-owned directory entry").path();
            let metadata = fs::symlink_metadata(&path).expect("inspect test-owned fixture");
            let file_type = metadata.file_type();
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode()
            };
            #[cfg(windows)]
            let mode = u32::from(metadata.permissions().readonly());
            #[cfg(not(any(unix, windows)))]
            let mode = u32::from(metadata.permissions().readonly());
            let contents = file_type
                .is_file()
                .then(|| fs::read(&path).expect("read test-owned fixture file"));
            entries.push(SnapshotEntry {
                relative_path: path
                    .strip_prefix(root)
                    .expect("fixture path stays under its test-owned root")
                    .to_string_lossy()
                    .into_owned(),
                is_file: file_type.is_file(),
                is_directory: file_type.is_dir(),
                is_symlink: file_type.is_symlink(),
                length: metadata.len(),
                mode,
                modified: metadata.modified().ok(),
                contents,
            });
            if file_type.is_dir() {
                visit(root, &path, entries);
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    entries
}

fn doctor_command(root: &TestRoot, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_local-mcp"));
    command
        .args(arguments)
        .current_dir(root.path())
        .env_clear()
        .env("PATH", root.path().join("path"))
        .env("HOME", root.path())
        .env("USERPROFILE", root.path())
        .env("APPDATA", root.path())
        .env("LOCALAPPDATA", root.path())
        .env("TEMP", root.path())
        .env("TMP", root.path())
        .env("XDG_CONFIG_HOME", root.path())
        .env("XDG_STATE_HOME", root.path())
        .env("XDG_DATA_HOME", root.path())
        .env("XDG_CACHE_HOME", root.path())
        .env(
            "LOCAL_MCP_CODEX_CLI_PATH",
            root.path().join("private-codex-path-sentinel"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    if let Some(system_root) = env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command
}

fn run(root: &TestRoot, arguments: &[&str]) -> Output {
    doctor_command(root, arguments)
        .output()
        .expect("run the test-built local-mcp binary")
}

#[test]
fn doctor_help_is_early_and_does_not_mutate_test_owned_state() {
    let root = TestRoot::new();
    let before = snapshot(root.path());
    let output = run(
        &root,
        &[
            "doctor",
            "--help",
            "--probe-codex-version",
            "--probe-bubblewrap-version",
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("help is UTF-8");
    assert!(stdout.contains("Usage:") && stdout.contains("doctor"));
    assert!(stdout.contains("--json"));
    assert!(output.stderr.is_empty());
    assert_eq!(snapshot(root.path()), before);
}

#[test]
fn doctor_json_is_pure_redacted_bounded_and_does_not_mutate_state() {
    let root = TestRoot::new();
    let before = snapshot(root.path());
    let output = run(
        &root,
        &[
            "doctor",
            "--json",
            "--probe-codex-version",
            "--probe-bubblewrap-version",
        ],
    );
    assert!(output.stdout.len() <= 64 * 1024);
    assert_eq!(output.stdout.last(), Some(&b'\n'));
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    assert!(output.stderr.is_empty());

    let json = std::str::from_utf8(&output.stdout[..output.stdout.len() - 1])
        .expect("Doctor JSON is UTF-8");
    let report: serde_json::Value = serde_json::from_str(json).expect("one JSON object");
    assert!(report.is_object());
    assert_eq!(report["schema_version"], "1.0.0");
    assert_eq!(report["diagnostic_contract_version"], 1);
    assert_eq!(
        report["summary"]["exit_code"].as_i64(),
        output.status.code().map(i64::from)
    );
    assert_eq!(
        report["dependencies"]["codex_cli"]["version_probe"]["requested"],
        true
    );
    assert_eq!(
        report["dependencies"]["codex_cli"]["version_probe"]["attempted"],
        false
    );
    assert_eq!(
        report["dependencies"]["bubblewrap"]["version_probe"]["attempted"],
        false
    );

    let rendered = String::from_utf8(output.stdout).expect("report is UTF-8");
    assert!(!rendered.contains(&root.path().to_string_lossy().to_string()));
    assert!(!rendered.contains("private-codex-path-sentinel"));
    assert!(!rendered.contains("owned-sentinel"));
    assert_eq!(snapshot(root.path()), before);
}

#[test]
fn doctor_human_output_is_bounded_plain_text_and_redacted() {
    let root = TestRoot::new();
    let before = snapshot(root.path());
    let output = run(&root, &["doctor"]);
    assert!(output.stdout.len() <= 32 * 1024);
    assert!(output.stderr.is_empty());
    let rendered = String::from_utf8(output.stdout).expect("human output is UTF-8");
    assert!(rendered.contains("GoalLatch Doctor Core V1"));
    assert!(rendered.contains("Capabilities:"));
    assert!(!rendered.contains('\u{1b}'));
    assert!(!rendered.contains(&root.path().to_string_lossy().to_string()));
    assert!(!rendered.contains("private-codex-path-sentinel"));
    assert_eq!(snapshot(root.path()), before);
}

#[test]
fn doctor_usage_errors_are_64_sanitized_and_emit_no_report() {
    let root = TestRoot::new();
    let output = run(
        &root,
        &["doctor", "--unknown-option", "private-path-sentinel"],
    );
    assert_eq!(output.status.code(), Some(64));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("usage error is UTF-8");
    assert_eq!(
        stderr.trim(),
        "local-mcp doctor: invalid command-line arguments"
    );
    assert!(!stderr.contains("private-path-sentinel"));
}

#[test]
fn legacy_help_commands_remain_available() {
    let root = TestRoot::new();
    for (arguments, expected) in [
        (&["--help"][..], "start"),
        (&["start", "--help"][..], "Start a session"),
        (&["mcp", "--help"][..], "session-independent MCP server"),
    ] {
        let output = run(&root, arguments);
        assert_eq!(output.status.code(), Some(0));
        let stdout = String::from_utf8(output.stdout).expect("legacy help is UTF-8");
        assert!(stdout.contains(expected), "missing {expected} in {stdout}");
        assert!(output.stderr.is_empty());
    }
}
