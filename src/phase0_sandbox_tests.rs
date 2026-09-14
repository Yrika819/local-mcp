use uuid::Uuid;

use crate::sandbox;

#[tokio::test]
async fn sandbox_prestart_and_missing_executable_lifecycle_are_frozen() {
    let cwd = std::env::temp_dir().join(format!("local-mcp-phase0-sandbox-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let missing = cwd.join("definitely-not-present");
    let command = vec![missing.to_string_lossy().into_owned()];

    // On macOS the Seatbelt wrapper itself starts successfully. A missing
    // target executable therefore completes as a nonzero sandbox attempt
    // rather than producing pre-start RunError lifecycle evidence.
    let output = sandbox::run_tracked(&command, &cwd, &[], None)
        .await
        .expect("sandbox wrapper should complete the missing-target attempt");
    assert_ne!(output.status, 0);

    // A failure while constructing the sandbox process remains genuinely
    // pre-start and must retain false/false lifecycle evidence.
    let missing_cwd = cwd.join("missing-cwd");
    let error = sandbox::run_tracked(&["/bin/true".into()], &missing_cwd, &[], None)
        .await
        .err()
        .expect("unresolvable cwd must fail before the command starts");
    assert!(!error.command_started);
    assert!(!error.command_finished);

    let _ = tokio::fs::remove_dir_all(cwd).await;
}
