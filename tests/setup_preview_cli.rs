use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "goallatch-setup-preview-cli-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&root).expect("create isolated Setup Preview test root");
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

fn snapshot(root: &Path) -> Vec<(String, u64, Option<Vec<u8>>)> {
    fn visit(root: &Path, directory: &Path, entries: &mut Vec<(String, u64, Option<Vec<u8>>)>) {
        for entry in fs::read_dir(directory).expect("read test-owned fixture directory") {
            let path = entry.expect("read directory entry").path();
            let metadata = fs::symlink_metadata(&path).expect("inspect test-owned fixture");
            let file_type = metadata.file_type();
            let contents = file_type
                .is_file()
                .then(|| fs::read(&path).expect("read fixture file"));
            entries.push((
                path.strip_prefix(root)
                    .expect("fixture path stays under its root")
                    .to_string_lossy()
                    .into_owned(),
                metadata.len(),
                contents,
            ));
            if file_type.is_dir() {
                visit(root, &path, entries);
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort();
    entries
}

fn setup_command(root: &TestRoot, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_local-mcp"));
    command
        .args(arguments)
        .current_dir(root.path())
        .env_clear()
        .env("PATH", root.path())
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
            root.path().join("missing-codex"),
        )
        .env("CANARY_SECRET_SENTINEL", "AKIA-SETUPPREVIEW-CANARY-0000")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command
}

fn run(root: &TestRoot, arguments: &[&str]) -> Output {
    setup_command(root, arguments)
        .output()
        .expect("spawn local-mcp")
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

#[test]
fn bare_setup_prints_help_to_stderr_and_exits_64() {
    let root = TestRoot::new();
    let output = run(&root, &["setup"]);
    assert_eq!(output.status.code(), Some(64));
    assert!(
        output.stdout.is_empty(),
        "bare setup must not write to stdout"
    );
    let stderr = stderr_text(&output);
    assert!(stderr.contains("local-mcp setup"));
    assert!(stderr.contains("preview"));
}

#[test]
fn setup_help_exits_zero() {
    let root = TestRoot::new();
    let output = run(&root, &["setup", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    assert!(stdout.contains("preview"));
}

#[test]
fn setup_preview_help_exits_zero() {
    let root = TestRoot::new();
    let output = run(&root, &["setup", "preview", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    assert!(stdout.contains("--json"));
    assert!(stdout.contains("--manifest"));
    assert!(stdout.contains("--archive"));
}

#[test]
fn setup_preview_rejects_install_like_flags_with_64() {
    let root = TestRoot::new();
    for flag in ["--install", "--yes", "--apply", "--force", "--confirm"] {
        let output = run(&root, &["setup", "preview", flag]);
        assert_eq!(
            output.status.code(),
            Some(64),
            "{flag} must be rejected as a usage error"
        );
        assert!(!stdout_text(&output).contains("VERIFIED_ARCHIVE"));
    }
}

#[test]
fn setup_preview_unknown_flag_exits_64() {
    let root = TestRoot::new();
    let output = run(&root, &["setup", "preview", "--bogus-flag"]);
    assert_eq!(output.status.code(), Some(64));
}

#[test]
fn setup_preview_human_output_is_bounded_plain_and_disclaims_authorization() {
    let root = TestRoot::new();
    let output = run(&root, &["setup", "preview"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = stdout_text(&output);
    assert!(
        stdout.len() <= 64 * 1024,
        "human output must stay within 64 KiB"
    );
    assert!(stdout.contains("DRY RUN"));
    assert!(stdout.contains("PINNED_UNVERIFIED"));
    assert!(stdout.contains("A Doctor PASS is not an installation authorization."));
    assert!(!stdout.contains("AKIA-SETUPPREVIEW-CANARY-0000"));
    assert!(stderr_text(&output).is_empty());
}

#[test]
fn setup_preview_json_envelope_is_semantically_consistent_with_doctor_json() {
    let root = TestRoot::new();
    let doctor = run(&root, &["doctor", "--json"]);
    assert!(matches!(doctor.status.code(), Some(0..=2)));
    let doctor_value: serde_json::Value =
        serde_json::from_slice(&doctor.stdout).expect("doctor --json emits parseable JSON");

    let preview = run(&root, &["setup", "preview", "--json"]);
    assert_eq!(preview.status.code(), Some(0));
    let envelope: serde_json::Value =
        serde_json::from_slice(&preview.stdout).expect("preview --json emits parseable JSON");

    assert_eq!(envelope["schema_version"], "1.0.0");
    assert_eq!(envelope["preview_contract_version"], 1);
    assert_eq!(envelope["goallatch_version"], "0.1.2");
    assert_eq!(
        envelope["artifact_verification"]["status"],
        "PINNED_UNVERIFIED"
    );
    assert_eq!(envelope["artifact_verification"]["manifest_present"], true);
    assert_eq!(envelope["artifact_verification"]["pin_consistent"], true);
    assert_eq!(envelope["artifact_verification"]["archive_present"], false);
    assert!(envelope["artifact_verification"]["archive_sha256"].is_null());
    assert_eq!(envelope["evidence"]["doctor_embedded"], true);
    assert_eq!(envelope["evidence"]["manifest_source"], "embedded");
    assert_eq!(
        envelope["evidence"]["double_read_consistent"],
        serde_json::Value::Null
    );
    assert_eq!(envelope["evidence"]["archive_hashed_in_run"], false);
    assert!(envelope["residual_warnings"].is_array());
    assert_eq!(envelope["install_root_trust"]["status"], "UNKNOWN");

    // Parsed-value semantic equality with `local-mcp doctor --json`.
    assert_eq!(envelope["doctor"], doctor_value);

    // No field may read as an install authorization.
    fn assert_no_authorization_keys(value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, nested) in map {
                    let lowered = key.to_ascii_lowercase();
                    for banned in [
                        "installable",
                        "authorized",
                        "approved",
                        "proceed",
                        "install_authorization",
                    ] {
                        assert!(
                            !lowered.contains(banned),
                            "envelope key {key} reads as an authorization"
                        );
                    }
                    assert_no_authorization_keys(nested);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    assert_no_authorization_keys(item);
                }
            }
            _ => {}
        }
    }
    assert_no_authorization_keys(&envelope);
    assert!(!stdout_text(&preview).contains("AKIA-SETUPPREVIEW-CANARY-0000"));
}

#[test]
fn setup_preview_json_reports_pinned_sha256_for_this_platform() {
    let root = TestRoot::new();
    let preview = run(&root, &["setup", "preview", "--json"]);
    assert_eq!(preview.status.code(), Some(0));
    let envelope: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
    let sha = envelope["release"]["sha256"].as_str().unwrap();
    const PINNED: [&str; 6] = [
        "34bfccf8a6bc1a258b3d841234d445f6e7db9db0fc9a5895dddffa95affe43e6",
        "f048eddfd17b9fe9bdccaee7d05418d31282d5642c2a709c27738fba58c9ece0",
        "12d24cb82501153527422f54639e3f5332c7d84b741dbdbb4b9a5a7dd6c8d9a8",
        "21400b2fb61e865484055e470ca78f3d289882fda335ed6e5e4cbc37eef8a787",
        "f5d2e3e20242956ca34b1fe2d9de479ca1a38fad84b99ddb619204cf0cd4342a",
        "4288ba681bd47fdfc8c08d923c5870bd1693c2dadac17f975d661024b522841c",
    ];
    assert!(
        PINNED.contains(&sha),
        "release sha256 must be one of the six pinned v0.1.2 digests"
    );
}

#[test]
fn setup_preview_does_not_modify_test_owned_state() {
    let root = TestRoot::new();
    let before = snapshot(root.path());
    let _ = run(&root, &["setup", "preview"]);
    let _ = run(&root, &["setup", "preview", "--json"]);
    let _ = run(&root, &["setup"]);
    let after = snapshot(root.path());
    assert_eq!(before, after, "preview must not modify any file");
}

#[test]
fn setup_preview_archive_mismatch_fails_closed_with_64() {
    let root = TestRoot::new();
    let archive = root.path().join("wrong-bytes.tar.gz");
    fs::write(&archive, b"definitely not the pinned archive").expect("write fixture archive");
    let output = run(
        &root,
        &[
            "setup",
            "preview",
            "--json",
            "--archive",
            archive.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(64));
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["overall_preview_result"], "FAILED");
    assert_eq!(envelope["failure"]["check"], "archive.sha256");
    assert!(
        envelope.get("doctor").is_none(),
        "failure envelopes embed no doctor payload"
    );
    assert!(
        envelope.get("artifact_verification").is_none(),
        "failure envelopes carry no artifact verification section"
    );
}

#[test]
fn setup_preview_missing_archive_path_fails_with_64() {
    let root = TestRoot::new();
    let missing = root.path().join("does-not-exist.tar.gz");
    let output = run(
        &root,
        &["setup", "preview", "--archive", missing.to_str().unwrap()],
    );
    assert_eq!(output.status.code(), Some(64));
}

#[test]
fn setup_preview_manifest_with_duplicate_keys_fails_with_64() {
    let root = TestRoot::new();
    let manifest = root.path().join("duplicate.json");
    fs::write(
        &manifest,
        br#"{ "schema_version": "1.0.0", "schema_version": "1.0.0", "version": "0.1.2",
             "assets": {} }"#,
    )
    .expect("write fixture manifest");
    let output = run(
        &root,
        &["setup", "preview", "--manifest", manifest.to_str().unwrap()],
    );
    assert_eq!(output.status.code(), Some(64));
}

#[test]
fn setup_preview_manifest_with_invalid_utf8_fails_with_64() {
    let root = TestRoot::new();
    let manifest = root.path().join("invalid-utf8.json");
    fs::write(&manifest, [0xff, 0xfe, 0x00, 0x01, 0x7f]).expect("write fixture manifest");
    let output = run(
        &root,
        &["setup", "preview", "--manifest", manifest.to_str().unwrap()],
    );
    assert_eq!(output.status.code(), Some(64));
}

#[test]
fn setup_preview_manifest_version_mismatch_fails_with_64() {
    let root = TestRoot::new();
    let manifest = root.path().join("wrong-version.json");
    fs::write(
        &manifest,
        br#"{ "schema_version": "1.0.0", "version": "0.2.0",
             "assets": {
                "linux-x86_64": { "name": "local-mcp-v0.1.2-linux-x86_64.tar.gz", "sha256": "34bfccf8a6bc1a258b3d841234d445f6e7db9db0fc9a5895dddffa95affe43e6" },
                "linux-aarch64": { "name": "local-mcp-v0.1.2-linux-aarch64.tar.gz", "sha256": "f048eddfd17b9fe9bdccaee7d05418d31282d5642c2a709c27738fba58c9ece0" },
                "macos-x86_64": { "name": "local-mcp-v0.1.2-macos-x86_64.tar.gz", "sha256": "12d24cb82501153527422f54639e3f5332c7d84b741dbdbb4b9a5a7dd6c8d9a8" },
                "macos-aarch64": { "name": "local-mcp-v0.1.2-macos-aarch64.tar.gz", "sha256": "21400b2fb61e865484055e470ca78f3d289882fda335ed6e5e4cbc37eef8a787" },
                "windows-x86_64": { "name": "local-mcp-v0.1.2-windows-x86_64.zip", "sha256": "f5d2e3e20242956ca34b1fe2d9de479ca1a38fad84b99ddb619204cf0cd4342a" },
                "windows-aarch64": { "name": "local-mcp-v0.1.2-windows-aarch64.zip", "sha256": "4288ba681bd47fdfc8c08d923c5870bd1693c2dadac17f975d661024b522841c" }
             } }"#,
    )
    .expect("write fixture manifest");
    let output = run(
        &root,
        &["setup", "preview", "--manifest", manifest.to_str().unwrap()],
    );
    assert_eq!(output.status.code(), Some(64));
}

#[test]
fn setup_preview_valid_manifest_matches_embedded_pin_and_exits_zero() {
    let root = TestRoot::new();
    let manifest = root.path().join("valid.json");
    fs::write(
        &manifest,
        br#"{ "schema_version": "1.0.0", "version": "0.1.2",
             "assets": {
                "linux-x86_64": { "name": "local-mcp-v0.1.2-linux-x86_64.tar.gz", "sha256": "34bfccf8a6bc1a258b3d841234d445f6e7db9db0fc9a5895dddffa95affe43e6" },
                "linux-aarch64": { "name": "local-mcp-v0.1.2-linux-aarch64.tar.gz", "sha256": "f048eddfd17b9fe9bdccaee7d05418d31282d5642c2a709c27738fba58c9ece0" },
                "macos-x86_64": { "name": "local-mcp-v0.1.2-macos-x86_64.tar.gz", "sha256": "12d24cb82501153527422f54639e3f5332c7d84b741dbdbb4b9a5a7dd6c8d9a8" },
                "macos-aarch64": { "name": "local-mcp-v0.1.2-macos-aarch64.tar.gz", "sha256": "21400b2fb61e865484055e470ca78f3d289882fda335ed6e5e4cbc37eef8a787" },
                "windows-x86_64": { "name": "local-mcp-v0.1.2-windows-x86_64.zip", "sha256": "f5d2e3e20242956ca34b1fe2d9de479ca1a38fad84b99ddb619204cf0cd4342a" },
                "windows-aarch64": { "name": "local-mcp-v0.1.2-windows-aarch64.zip", "sha256": "4288ba681bd47fdfc8c08d923c5870bd1693c2dadac17f975d661024b522841c" }
             } }"#,
    )
    .expect("write fixture manifest");
    let output = run(
        &root,
        &[
            "setup",
            "preview",
            "--json",
            "--manifest",
            manifest.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["evidence"]["manifest_source"], "operator");
    assert_eq!(envelope["evidence"]["double_read_consistent"], true);
}

#[test]
fn existing_cli_surface_is_unchanged() {
    let root = TestRoot::new();
    for arguments in [
        &["--version"][..],
        &["--help"][..],
        &["doctor", "--help"][..],
    ] {
        let output = run(&root, arguments);
        assert_eq!(output.status.code(), Some(0), "{arguments:?}");
    }
    let doctor = run(&root, &["doctor", "--json"]);
    assert!(matches!(doctor.status.code(), Some(0..=2)));
    let raw = stdout_text(&doctor);
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("doctor --json parses");
    assert_eq!(parsed["diagnostic_contract_version"], 1);
}
