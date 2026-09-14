include!("mcp.rs");

#[test]
fn relative_and_absolute_path_resolution_is_frozen() {
    let cwd = PathBuf::from("/tmp/local-mcp-phase0-cwd");
    assert_eq!(
        resolve_path(&cwd, PathBuf::from("child.txt")),
        cwd.join("child.txt")
    );
    let absolute = PathBuf::from("/tmp/local-mcp-phase0-absolute.txt");
    assert_eq!(resolve_path(&cwd, absolute.clone()), absolute);
}

#[test]
fn command_and_job_argument_validation_is_frozen() {
    let missing = required_command(&json!({})).unwrap_err();
    assert!(missing.to_string().contains("missing command"));

    let wrong_type = required_command(&json!({"command": ["ok", 7]})).unwrap_err();
    assert!(wrong_type.to_string().contains("command entries must be strings"));

    let invalid_job = required_job_id(&json!({"job_id": "not-a-uuid"})).unwrap_err();
    assert!(invalid_job.to_string().contains("invalid job_id"));
}

#[test]
fn default_execution_policy_keeps_zero_as_the_only_accepted_exit_code() {
    let policy = execution_policy(&json!({})).unwrap();
    assert_eq!(policy.accepted_exit_codes, vec![0]);
    assert_eq!(policy.fallback_depth, 0);
}
