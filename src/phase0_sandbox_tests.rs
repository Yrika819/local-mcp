use uuid::Uuid;

use crate::sandbox;

#[tokio::test]
async fn sandbox_prestart_and_missing_executable_lifecycle_are_frozen() {
    let cwd = std::env::temp_dir().join(format!("local-mcp-phase0-sandbox-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let missing = cwd.join("definitely-not-present");
    let command = vec![missing.to_string_lossy().into_owned()];

    // Unix launches a sandbox wrapper first, so a missing target completes
    // as a nonzero sandbox attempt. Windows is host-native and therefore
    // reports the missing executable as a genuine pre-start failure.
    #[cfg(not(windows))]
    {
        let output = sandbox::run_tracked(&command, &cwd, &[], None)
            .await
            .expect("sandbox wrapper should complete the missing-target attempt");
        assert_ne!(output.status, 0);
    }
    #[cfg(windows)]
    {
        let error = sandbox::run_tracked(&command, &cwd, &[], None)
            .await
            .err()
            .expect("host-native missing target must fail before process start");
        assert!(!error.command_started);
        assert!(!error.command_finished);
    }

    // A failure while constructing the sandbox process remains genuinely
    // pre-start and must retain false/false lifecycle evidence.
    let missing_cwd = cwd.join("missing-cwd");
    #[cfg(unix)]
    let no_op = "/bin/true";
    #[cfg(windows)]
    let no_op = "cmd.exe";
    let error = sandbox::run_tracked(&[no_op.into()], &missing_cwd, &[], None)
        .await
        .err()
        .expect("unresolvable cwd must fail before the command starts");
    assert!(!error.command_started);
    assert!(!error.command_finished);

    let _ = tokio::fs::remove_dir_all(cwd).await;
}
