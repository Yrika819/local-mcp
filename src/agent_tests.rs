use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::agent::{
    AgentError, MODEL_PROMPT_LIMIT, MODEL_STDERR_LIMIT, MODEL_STDOUT_LIMIT, ModelInvocation,
    ModelInvocationOutput, ModelRole, ModelTransport, configured_model_from_for_test,
    require_approval_for_test, run_bounded_process_for_test, safe_diagnostic_for_test,
};

#[derive(Default)]
struct RecordingTransport {
    calls: Mutex<Vec<ModelInvocation>>,
}

impl ModelTransport for RecordingTransport {
    fn invoke(&self, request: &ModelInvocation) -> Result<ModelInvocationOutput, AgentError> {
        self.calls.lock().unwrap().push(request.clone());
        Ok(ModelInvocationOutput::new(
            b"{\"exact\":true}\n".to_vec(),
            String::new(),
            0,
        ))
    }
}

#[test]
fn injectable_transport_preserves_typed_output_and_role_data() {
    let transport = Arc::new(RecordingTransport::default());
    let invocation = ModelInvocation::new(
        "session-1",
        PathBuf::from("/repo"),
        ModelRole::Writer,
        "fixed writer prompt\nREQUEST_JSON\n{\"x\":1}",
    );

    let output = transport.invoke(&invocation).unwrap();
    assert_eq!(output.stdout(), b"{\"exact\":true}\n");
    assert_eq!(output.safe_stderr(), "");
    assert_eq!(output.exit_status(), 0);
    let call = &transport.calls.lock().unwrap()[0];
    assert_eq!(call.session_id(), "session-1");
    assert_eq!(call.cwd(), std::path::Path::new("/repo"));
    assert_eq!(call.role(), ModelRole::Writer);
    assert_eq!(
        call.prompt(),
        "fixed writer prompt\nREQUEST_JSON\n{\"x\":1}"
    );
}

#[test]
fn model_roles_are_fixed_and_provider_free() {
    assert_eq!(ModelRole::Planner.as_str(), "PLANNER");
    assert_eq!(ModelRole::Writer.as_str(), "WRITER");
    assert_eq!(ModelRole::Reviewer.as_str(), "REVIEWER");
    assert_eq!(ModelRole::Replanner.as_str(), "REPLANNER");
}

fn command(program: &str, args: &[&str]) -> Vec<String> {
    std::iter::once(program.to_owned())
        .chain(args.iter().map(|arg| (*arg).to_owned()))
        .collect()
}

#[cfg(unix)]
#[test]
fn bounded_process_maps_success_empty_exit_timeout_and_output_limits() {
    let output = run_bounded_process_for_test(
        &command("/usr/bin/printf", &["exact"]),
        b"prompt",
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(output.stdout(), b"exact");
    assert_eq!(output.exit_status(), 0);
    assert_eq!(output.safe_stderr(), "");

    let safe = safe_diagnostic_for_test(b"\x1b[31mdiagnostic\x1b[0m\n");
    assert!(safe.contains("diagnostic"));
    assert!(!safe.contains('\u{1b}'));

    assert_eq!(
        run_bounded_process_for_test(
            &command("/usr/bin/false", &[]),
            b"prompt",
            Duration::from_secs(2),
        )
        .unwrap_err(),
        AgentError::NonZeroExit
    );
    assert_eq!(
        run_bounded_process_for_test(
            &command("/usr/bin/true", &[]),
            b"prompt",
            Duration::from_secs(2),
        )
        .unwrap_err(),
        AgentError::EmptyResponse
    );
    assert_eq!(
        run_bounded_process_for_test(
            &command("/bin/sleep", &["1"]),
            b"prompt",
            Duration::from_millis(20),
        )
        .unwrap_err(),
        AgentError::Timeout
    );
    assert_eq!(
        run_bounded_process_for_test(
            &command("/usr/bin/yes", &[]),
            b"prompt",
            Duration::from_secs(2),
        )
        .unwrap_err(),
        AgentError::ResponseTooLarge
    );
}

#[test]
fn bounded_process_maps_executable_and_spawn_failures() {
    assert_eq!(
        run_bounded_process_for_test(
            &["/definitely/not/a/codex-executable".into()],
            b"prompt",
            Duration::from_secs(2),
        )
        .unwrap_err(),
        AgentError::ExecutableUnavailable
    );
    #[cfg(unix)]
    assert_eq!(
        run_bounded_process_for_test(
            &[std::env::current_dir().unwrap().display().to_string()],
            b"prompt",
            Duration::from_secs(2),
        )
        .unwrap_err(),
        AgentError::SpawnFailed
    );
}

#[test]
fn model_config_and_approval_are_host_owned_and_typed() {
    assert_eq!(
        configured_model_from_for_test(Some("goal-model"), "fallback-model").unwrap(),
        "goal-model"
    );
    assert_eq!(
        configured_model_from_for_test(None, "fallback-model").unwrap(),
        "fallback-model"
    );
    assert_eq!(
        configured_model_from_for_test(Some(""), "fallback-model").unwrap_err(),
        AgentError::InvalidConfiguration
    );
    assert_eq!(
        configured_model_from_for_test(Some("bad\nmodel"), "fallback-model").unwrap_err(),
        AgentError::InvalidConfiguration
    );
    assert_eq!(require_approval_for_test(false), Err(AgentError::ApprovalDenied));
    assert_eq!(require_approval_for_test(true), Ok(()));
}

#[test]
fn goal_read_only_command_allows_non_git_workspace_without_weakening_sandbox() {
    let cwd = PathBuf::from("/authoritative/non-git-goal");
    let command = crate::fallback::codex_read_only_command_with_model(
        &cwd,
        crate::fallback::Effort::Medium,
        "host-model",
    )
    .unwrap();

    assert!(command.contains(&"--skip-git-repo-check".to_owned()));
    assert!(command.windows(2).any(|pair| pair == ["-s", "read-only"]));
    assert!(command.contains(&"--ephemeral".to_owned()));
    assert!(command.contains(&"--ignore-user-config".to_owned()));
    assert!(!command.contains(&"--dangerously-bypass-approvals-and-sandbox".to_owned()));
    assert_eq!(command.last().map(String::as_str), Some("-"));
}

#[test]
fn model_bounds_and_read_only_command_are_finite_and_fixed() {
    assert_eq!(MODEL_STDOUT_LIMIT, 2 * 1024 * 1024);
    assert_eq!(MODEL_STDERR_LIMIT, 64 * 1024);
    assert_eq!(MODEL_PROMPT_LIMIT, 256 * 1024);
    let cwd = PathBuf::from("/authoritative/goal");
    let command = crate::fallback::codex_read_only_command_with_model(
        &cwd,
        crate::fallback::Effort::Medium,
        "host-model",
    )
    .unwrap();
    for flag in [
        "exec",
        "--ephemeral",
        "--ignore-user-config",
        "--skip-git-repo-check",
        "-s",
        "read-only",
        "-C",
    ] {
        assert!(command.contains(&flag.to_owned()));
    }
    assert!(command.windows(2).any(|pair| pair == ["-m", "host-model"]));
    assert!(
        command
            .windows(2)
            .any(|pair| pair == ["-C", "/authoritative/goal"])
    );
    assert_eq!(command.last().map(String::as_str), Some("-"));
}
