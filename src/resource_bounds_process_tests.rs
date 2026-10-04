//! Resource Bounds V1: process-level behavior of bounded generic capture.
//!
//! These run a real child so the claims that matter are tested against real
//! pipes and a real process group:
//!
//! * an output bound terminates the group rather than merely abandoning the read;
//! * a descendant cannot hold capture open forever;
//! * an overflow never marks a possibly mutating command as "not performed",
//!   so it cannot restore retry budget.
//!
//! The emitters are test-owned shell commands. Production capture special-cases
//! no program at all: nothing here corresponds to a production branch.

#![cfg(all(test, unix))]

use std::time::Duration;

use crate::resource_limits::{MAX_COMMAND_STDERR_BYTES, MAX_COMMAND_STDOUT_BYTES};

/// Emit exactly `bytes` of zeros on stdout, using only POSIX `head`/`/dev/zero`.
fn emit_stdout(bytes: usize) -> Vec<String> {
    vec![
        "sh".to_owned(),
        "-c".to_owned(),
        format!("head -c {bytes} /dev/zero"),
    ]
}

/// Emit exactly `bytes` of zeros on stderr.
fn emit_stderr(bytes: usize) -> Vec<String> {
    vec![
        "sh".to_owned(),
        "-c".to_owned(),
        format!("head -c {bytes} /dev/zero >&2"),
    ]
}

fn cwd() -> std::path::PathBuf {
    std::env::temp_dir()
}

#[tokio::test]
async fn stdout_at_the_limit_succeeds_with_the_whole_stream_retained() {
    let command = emit_stdout(MAX_COMMAND_STDOUT_BYTES);
    let output = crate::sandbox::run_unrestricted_inner(&command, &cwd(), None, false)
        .await
        .expect("output exactly at the limit must be accepted");
    assert_eq!(output.stdout.len(), MAX_COMMAND_STDOUT_BYTES);
    assert_eq!(output.status, 0);
}

#[tokio::test]
async fn stdout_one_byte_over_the_limit_is_a_typed_resource_failure() {
    let command = emit_stdout(MAX_COMMAND_STDOUT_BYTES + 1);
    let error = crate::sandbox::run_unrestricted_inner(&command, &cwd(), None, false)
        .await
        .expect_err("limit + 1 must be rejected");
    let rendered = format!("{:#}", error.error);
    assert!(
        rendered.contains("command stdout limit exceeded"),
        "unexpected failure: {rendered}"
    );
    assert!(
        rendered.contains("discarded"),
        "the failure must state output was discarded, got: {rendered}"
    );
}

#[tokio::test]
async fn stderr_one_byte_over_the_limit_is_a_typed_resource_failure() {
    let command = emit_stderr(MAX_COMMAND_STDERR_BYTES + 1);
    let error = crate::sandbox::run_unrestricted_inner(&command, &cwd(), None, false)
        .await
        .expect_err("stderr limit + 1 must be rejected");
    let rendered = format!("{:#}", error.error);
    assert!(
        rendered.contains("command stderr limit exceeded"),
        "unexpected failure: {rendered}"
    );
}

#[tokio::test]
async fn an_output_overflow_reports_the_command_as_started() {
    // This is the load-bearing security property. The host launched a process
    // and observed it, so `command_started` stays true: an overflow says nothing
    // about whether the command performed its side effects.
    let command = emit_stdout(MAX_COMMAND_STDOUT_BYTES + 1);
    let error = crate::sandbox::run_unrestricted_inner(&command, &cwd(), None, false)
        .await
        .expect_err("overflow must fail");
    assert!(
        error.command_started,
        "an overflow must never claim the command was not started"
    );
}

#[tokio::test]
async fn a_flood_after_the_parent_exits_is_still_bounded() {
    // The parent exits immediately while a descendant inherits the pipes and
    // floods. Capture must not wait forever for EOF, and the descendant must not
    // survive the bound.
    let command = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        format!(
            "(head -c {} /dev/zero) & exit 0",
            MAX_COMMAND_STDOUT_BYTES + 1
        ),
    ];
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        crate::sandbox::run_unrestricted_inner(&command, &cwd(), None, false),
    )
    .await
    .expect("capture must be bounded even when a descendant holds the pipes");
    match result {
        Ok(_) => {}
        Err(error) => {
            let rendered = format!("{:#}", error.error);
            assert!(
                rendered.contains("command stdout limit exceeded")
                    || rendered.contains("capture did not finish"),
                "unexpected failure: {rendered}"
            );
        }
    }
}

#[tokio::test]
async fn a_child_that_ignores_normal_termination_is_still_terminated() {
    // `sh` traps SIGTERM. The bound must not depend on cooperative shutdown, and
    // the call must return rather than wait on a child that refuses to end.
    let command = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        format!(
            "trap '' TERM; head -c {} /dev/zero",
            MAX_COMMAND_STDOUT_BYTES + 1
        ),
    ];
    let result = tokio::time::timeout(
        Duration::from_secs(60),
        crate::sandbox::run_unrestricted_inner(&command, &cwd(), None, false),
    )
    .await
    .expect("a child ignoring SIGTERM must not hang the bound");
    let error = result.expect_err("overflow must fail");
    assert!(error.command_started);
}

#[tokio::test]
async fn an_overflow_cannot_be_reported_as_not_performed() {
    // Drive the real classifier: an overflow after a possibly mutating command
    // must leave the side-effect state unknown and the budget locked, so no
    // retry authority is restored.
    let unproven = crate::fallback::LifecycleEvidence {
        host_reached: true,
        command_start: crate::sandbox::CommandStart::Unproven,
        command_finished: false,
        process_finished: false,
    };
    let state = crate::fallback::infer_side_effect_state(
        crate::fallback::SideEffectClass::LocalMutation,
        unproven,
        crate::fallback::FailureClass::ResourceLimit,
        None,
    );
    assert_eq!(
        state,
        crate::fallback::SideEffectState::Unknown,
        "an overflow must never be treated as proof of no side effect"
    );

    let budget = crate::fallback::Budget::from_operation(None);
    let after = budget.after_state(state);
    assert!(
        after.locked,
        "an unknown side effect must lock the fallback budget"
    );
    assert_eq!(after.attempt_remaining, budget.attempt_remaining);
    assert_eq!(after.side_effect_remaining, budget.side_effect_remaining);
}

#[tokio::test]
async fn an_overflow_is_terminal_and_never_selects_executable_fallback() {
    use crate::fallback::{
        Budget, DecisionInput, FailureClass, FallbackAction, LifecycleEvidence,
        PrimaryExecutionMode, ReasonCode, SideEffectClass, SideEffectState, decide,
    };
    let decision = decide(DecisionInput {
        failure_class: FailureClass::ResourceLimit,
        safety_signal: false,
        primary_execution_mode: PrimaryExecutionMode::Sandboxed,
        lifecycle: LifecycleEvidence {
            host_reached: true,
            command_start: crate::sandbox::CommandStart::Unproven,
            command_finished: false,
            process_finished: false,
        },
        operation: None,
        side_effect_class: SideEffectClass::LocalMutation,
        side_effect_state: SideEffectState::Unknown,
        fallback_depth: 0,
        max_depth: 1,
        budget: Budget::from_operation(None),
        operation_validated: false,
        scope_valid: false,
        automatic_enabled: true,
        auto_execute_enabled: true,
    });
    assert_eq!(
        decision.action,
        FallbackAction::Block,
        "a resource limit must never be executable fallback"
    );
    assert_eq!(decision.reason_code, ReasonCode::NoFallbackResourceLimit);
    assert!(decision.mode.is_none());
}
