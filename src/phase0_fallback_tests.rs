use std::path::Path;

use crate::fallback;

fn stage_operation(attempts: u32, side_effects: u32) -> fallback::OperationContract {
    fallback::OperationContract {
        kind: fallback::OperationType::GitStagePaths,
        authorized: true,
        operation_id: Some("phase0-stage".into()),
        paths: vec!["src/a.rs".into()],
        argv: vec![],
        source: None,
        destination: None,
        target: None,
        create_only: false,
        force: false,
        attempt_budget_remaining: attempts,
        side_effect_budget_remaining: side_effects,
        side_effect_state: Some(fallback::SideEffectState::ConfirmedNotPerformed),
    }
}

fn decide(
    operation: &fallback::OperationContract,
    state: fallback::SideEffectState,
    budget: fallback::Budget,
) -> fallback::FallbackDecision {
    fallback::decide(fallback::DecisionInput {
        failure_class: fallback::FailureClass::SandboxPermission,
        safety_signal: false,
        primary_execution_mode: fallback::PrimaryExecutionMode::Sandboxed,
        lifecycle: fallback::LifecycleEvidence::completed(),
        operation: Some(operation),
        side_effect_class: fallback::SideEffectClass::LocalMutation,
        side_effect_state: state,
        fallback_depth: 0,
        max_depth: 1,
        budget,
        scope_valid: true,
        automatic_enabled: true,
        auto_execute_enabled: true,
    })
}

#[test]
fn attempt_and_side_effect_budgets_are_independently_binding() {
    for operation in [stage_operation(0, 1), stage_operation(1, 0)] {
        let decision = decide(
            &operation,
            fallback::SideEffectState::ConfirmedNotPerformed,
            fallback::Budget::from_operation(Some(&operation)),
        );
        assert_eq!(decision.action, fallback::FallbackAction::Block);
        assert_eq!(
            decision.reason_code,
            fallback::ReasonCode::FallbackDeniedBudgetExhausted
        );
    }
}

#[test]
fn unknown_and_performed_side_effects_block_replay() {
    let operation = stage_operation(1, 1);

    let unknown_budget = fallback::Budget::from_operation(Some(&operation))
        .after_state(fallback::SideEffectState::Unknown);
    assert!(unknown_budget.locked);
    let unknown = decide(
        &operation,
        fallback::SideEffectState::Unknown,
        unknown_budget,
    );
    assert_eq!(unknown.action, fallback::FallbackAction::Block);
    assert_eq!(
        unknown.reason_code,
        fallback::ReasonCode::FallbackDeniedSideEffectUnknown
    );

    let performed_budget = fallback::Budget::from_operation(Some(&operation))
        .after_state(fallback::SideEffectState::ConfirmedPerformed);
    assert_eq!(performed_budget.attempt_remaining, 0);
    assert_eq!(performed_budget.side_effect_remaining, 0);
    let performed = decide(
        &operation,
        fallback::SideEffectState::ConfirmedPerformed,
        performed_budget,
    );
    assert_eq!(performed.action, fallback::FallbackAction::Block);
    assert_eq!(
        performed.reason_code,
        fallback::ReasonCode::FallbackDeniedSideEffectPerformed
    );
}

#[test]
fn lifecycle_and_auto_execute_gates_are_binding() {
    let operation = stage_operation(1, 1);
    let budget = fallback::Budget::from_operation(Some(&operation));

    let lifecycle = fallback::decide(fallback::DecisionInput {
        failure_class: fallback::FailureClass::SandboxPermission,
        safety_signal: false,
        primary_execution_mode: fallback::PrimaryExecutionMode::Sandboxed,
        lifecycle: fallback::LifecycleEvidence::host_received(),
        operation: Some(&operation),
        side_effect_class: fallback::SideEffectClass::LocalMutation,
        side_effect_state: fallback::SideEffectState::ConfirmedNotPerformed,
        fallback_depth: 0,
        max_depth: 1,
        budget,
        scope_valid: true,
        automatic_enabled: true,
        auto_execute_enabled: true,
    });
    assert_eq!(lifecycle.action, fallback::FallbackAction::Block);
    assert_eq!(
        lifecycle.reason_code,
        fallback::ReasonCode::FallbackDeniedLifecycle
    );

    let disabled = fallback::decide(fallback::DecisionInput {
        failure_class: fallback::FailureClass::SandboxPermission,
        safety_signal: false,
        primary_execution_mode: fallback::PrimaryExecutionMode::Sandboxed,
        lifecycle: fallback::LifecycleEvidence::completed(),
        operation: Some(&operation),
        side_effect_class: fallback::SideEffectClass::LocalMutation,
        side_effect_state: fallback::SideEffectState::ConfirmedNotPerformed,
        fallback_depth: 0,
        max_depth: 1,
        budget,
        scope_valid: true,
        automatic_enabled: true,
        auto_execute_enabled: false,
    });
    assert_eq!(disabled.action, fallback::FallbackAction::Block);
    assert_eq!(
        disabled.reason_code,
        fallback::ReasonCode::FallbackDeniedAutoExecuteDisabled
    );
}

#[test]
fn host_native_permission_failure_is_not_labeled_sandbox_permission() {
    let command = vec!["git".into(), "add".into(), "--".into(), "src/a.rs".into()];
    let classification = fallback::classify(fallback::ClassificationInput {
        command: &command,
        accepted_exit_codes: &[0],
        primary_execution_mode: fallback::PrimaryExecutionMode::HostNative,
        lifecycle: fallback::LifecycleEvidence::completed(),
        exit_code: Some(128),
        stdout: "",
        stderr: "permission denied",
        execution_error: None,
        side_effect_class: fallback::SideEffectClass::LocalMutation,
        authoritative_platform_safety: false,
    });
    assert_eq!(
        classification.failure_class,
        fallback::FailureClass::HostEnvironment
    );
}

#[test]
fn read_only_command_never_grants_executable_fallback() {
    let operation = fallback::OperationContract {
        kind: fallback::OperationType::ReadOnlyCommand,
        authorized: true,
        operation_id: None,
        paths: vec![],
        argv: vec!["/bin/sh".into(), "-c".into(), "touch outside".into()],
        source: None,
        destination: None,
        target: None,
        create_only: false,
        force: false,
        attempt_budget_remaining: 1,
        side_effect_budget_remaining: 1,
        side_effect_state: Some(fallback::SideEffectState::ConfirmedNotPerformed),
    };
    assert!(!operation.auto_execute_allowlisted());
    let decision = fallback::decide(fallback::DecisionInput {
        failure_class: fallback::FailureClass::SandboxPermission,
        safety_signal: false,
        primary_execution_mode: fallback::PrimaryExecutionMode::Sandboxed,
        lifecycle: fallback::LifecycleEvidence::completed(),
        operation: Some(&operation),
        side_effect_class: fallback::SideEffectClass::None,
        side_effect_state: fallback::SideEffectState::ConfirmedNotPerformed,
        fallback_depth: 0,
        max_depth: 1,
        budget: fallback::Budget::from_operation(Some(&operation)),
        scope_valid: true,
        automatic_enabled: true,
        auto_execute_enabled: true,
    });
    assert_ne!(decision.action, fallback::FallbackAction::Execute);
}

#[test]
fn codex_command_contract_is_read_only_ephemeral_and_cwd_scoped() {
    let cwd = std::env::temp_dir();
    let command = fallback::codex_read_only_command(&cwd, fallback::Effort::Low).unwrap();
    assert!(command.windows(2).any(|pair| pair == ["-s", "read-only"]));
    assert!(command.iter().any(|arg| arg == "--ephemeral"));
    assert!(command.iter().any(|arg| arg == "--ignore-user-config"));
    assert!(
        command
            .windows(2)
            .any(|pair| { pair[0] == "-C" && Path::new(&pair[1]) == cwd.as_path() })
    );
}
