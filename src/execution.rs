use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{approvals, config, fallback, sandbox};

const FOREGROUND_TIMEOUT: Duration = Duration::from_secs(30);
const HOST_FOREGROUND_TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) struct BackgroundExecution {
    pub(crate) rendered_command: String,
    pub(crate) handle: JoinHandle<Result<String>>,
    pub(crate) activity: &'static str,
}

pub(crate) enum ExecutionOutcome {
    Completed(String),
    Background(BackgroundExecution),
}

pub(crate) async fn write_file_content(
    absolute: &Path,
    parent: &Path,
    content: &str,
) -> Result<sandbox::Output> {
    #[cfg(unix)]
    {
        let command = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "cat > \"$1\"".to_owned(),
            "local-mcp-write".to_owned(),
            absolute.display().to_string(),
        ];
        let root = parent.to_owned();
        sandbox::run(
            &command,
            parent,
            std::slice::from_ref(&root),
            Some(content.as_bytes()),
        )
        .await
    }
    #[cfg(windows)]
    {
        // Windows has no application sandbox here, so avoid depending on a
        // shell utility for the file-edit operation.
        tokio::fs::write(absolute, content).await?;
        Ok(sandbox::Output {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

pub(crate) async fn execute(
    args: &Value,
    session: &config::Session,
) -> Result<ExecutionOutcome> {
    let (rendered_command, mut handle) = spawn_sandboxed_command("execute", args, session).await?;

    match tokio::time::timeout(FOREGROUND_TIMEOUT, &mut handle).await {
        Ok(joined) => Ok(ExecutionOutcome::Completed(
            joined.context("command task failed")??,
        )),
        Err(_) => Ok(ExecutionOutcome::Background(BackgroundExecution {
            rendered_command,
            handle,
            activity: "Backgrounded",
        })),
    }
}

pub(crate) async fn start_command(
    args: &Value,
    session: &config::Session,
) -> Result<BackgroundExecution> {
    let (rendered_command, handle) = spawn_sandboxed_command("start_command", args, session).await?;
    Ok(BackgroundExecution {
        rendered_command,
        handle,
        activity: "Started",
    })
}

pub(crate) fn resolve_path(session_cwd: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        session_cwd.join(path)
    }
}

pub(crate) fn cwd(args: &Value, session_cwd: &Path) -> Result<PathBuf> {
    let path = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .map(|path| resolve_path(session_cwd, path))
        .unwrap_or_else(|| session_cwd.to_owned());
    std::fs::canonicalize(&path).with_context(|| format!("cannot resolve cwd {}", path.display()))
}

pub(crate) fn required_command(args: &Value) -> Result<Vec<String>> {
    args.get("command")
        .and_then(Value::as_array)
        .context("missing command")?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .context("command entries must be strings")
        })
        .collect()
}

#[derive(Clone)]
pub(crate) struct ExecutionPolicy {
    pub(crate) request_id: String,
    pub(crate) accepted_exit_codes: Vec<i32>,
    pub(crate) operation: Option<fallback::OperationContract>,
    pub(crate) fallback_depth: u8,
    pub(crate) primary_execution_mode: fallback::PrimaryExecutionMode,
}

pub(crate) fn primary_execution_mode() -> fallback::PrimaryExecutionMode {
    #[cfg(windows)]
    {
        fallback::PrimaryExecutionMode::HostNative
    }
    #[cfg(not(windows))]
    {
        fallback::PrimaryExecutionMode::Sandboxed
    }
}

pub(crate) fn primary_execution_requires_approval(mode: fallback::PrimaryExecutionMode) -> bool {
    mode == fallback::PrimaryExecutionMode::HostNative
}

pub(crate) fn ensure_primary_execution_authorized(
    mode: fallback::PrimaryExecutionMode,
    approved: bool,
    operation: &str,
) -> Result<()> {
    if primary_execution_requires_approval(mode) && !approved {
        anyhow::bail!("user denied {operation}");
    }
    Ok(())
}

pub(crate) fn execution_policy(args: &Value) -> Result<ExecutionPolicy> {
    let fallback_depth = args
        .get("fallback_depth")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    anyhow::ensure!(
        fallback_depth <= fallback::max_depth() as u64,
        "fallback_depth exceeds configured maximum"
    );
    Ok(ExecutionPolicy {
        request_id: Uuid::new_v4().to_string(),
        accepted_exit_codes: fallback::accepted_exit_codes(args.get("accepted_exit_codes"))?,
        operation: fallback::operation_from_value(args.get("operation"))?,
        fallback_depth: fallback_depth as u8,
        primary_execution_mode: primary_execution_mode(),
    })
}

async fn spawn_sandboxed_command(
    operation: &str,
    args: &Value,
    session: &config::Session,
) -> Result<(String, JoinHandle<Result<String>>)> {
    let command = required_command(args)?;
    let cwd = cwd(args, &session.cwd)?;
    let policy = execution_policy(args)?;
    if primary_execution_requires_approval(policy.primary_execution_mode) {
        let approved = approvals::request(
            &session.id,
            operation,
            format!("argv: {command:?}"),
            cwd.clone(),
        )
        .await?;
        ensure_primary_execution_authorized(policy.primary_execution_mode, approved, operation)?;
    }
    let mut roots = session.permitted_directories.clone();
    if !roots.iter().any(|root| cwd.starts_with(root)) {
        roots.push(cwd.clone());
    }

    let pre_index_snapshot =
        capture_index_snapshot_if_needed(&command, &cwd, policy.operation.as_ref()).await;

    let rendered_command = render_command(&command);
    approvals::activity(
        &session.id,
        format!("Running {rendered_command}"),
        Some(format!("└ request_id={}", policy.request_id)),
    )
    .await;
    let session_id = session.id.clone();
    let task_command = rendered_command.clone();
    let handle = tokio::spawn(async move {
        let attempt = sandbox::run_tracked(&command, &cwd, &roots, None).await;
        let result = process_sandboxed_attempt(
            &session_id,
            &command,
            &cwd,
            &policy,
            pre_index_snapshot,
            attempt,
        )
        .await;
        report_command_finished(session_id, &task_command, &result).await;
        result
    });
    Ok((rendered_command, handle))
}

pub(crate) async fn process_sandboxed_attempt(
    session_id: &str,
    command: &[String],
    cwd: &Path,
    policy: &ExecutionPolicy,
    pre_index_snapshot: Option<String>,
    attempt: std::result::Result<sandbox::Output, sandbox::RunError>,
) -> Result<String> {
    let (lifecycle, exit_code, stdout, stderr, execution_error) = match attempt {
        Ok(output) => (
            fallback::LifecycleEvidence::completed(),
            Some(output.status),
            output.stdout,
            output.stderr,
            None,
        ),
        Err(error) => (
            fallback::LifecycleEvidence {
                host_reached: true,
                command_started: error.command_started,
                command_finished: error.command_finished,
            },
            None,
            String::new(),
            String::new(),
            Some(format!("{:#}", error.error)),
        ),
    };

    let side_effect_class = fallback::infer_side_effect_class(command, policy.operation.as_ref());
    let classification = fallback::classify(fallback::ClassificationInput {
        command,
        accepted_exit_codes: &policy.accepted_exit_codes,
        primary_execution_mode: policy.primary_execution_mode,
        lifecycle,
        exit_code,
        stdout: &stdout,
        stderr: &stderr,
        execution_error: execution_error.as_deref(),
        side_effect_class,
        authoritative_platform_safety: false,
    });

    let local_not_performed_proof = if policy
        .operation
        .as_ref()
        .is_some_and(|operation| operation.kind == fallback::OperationType::GitStagePaths)
        && classification.failure_class != fallback::FailureClass::Success
    {
        match (
            pre_index_snapshot.as_ref(),
            capture_git_index_snapshot(command, cwd).await.as_ref(),
        ) {
            (Some(before), Some(after)) => Some(before == after),
            _ => None,
        }
    } else {
        None
    };

    let side_effect_state = fallback::infer_side_effect_state(
        side_effect_class,
        lifecycle,
        classification.failure_class,
        local_not_performed_proof,
    );
    let budget_before = fallback::Budget::from_operation(policy.operation.as_ref());
    let budget_after_primary = budget_before.after_state(side_effect_state);
    let scope_valid = policy
        .operation
        .as_ref()
        .is_some_and(|operation| operation.scope_matches(command));
    let decision = fallback::decide(fallback::DecisionInput {
        failure_class: classification.failure_class,
        safety_signal: classification.safety_signal,
        primary_execution_mode: policy.primary_execution_mode,
        lifecycle,
        operation: policy.operation.as_ref(),
        side_effect_class,
        side_effect_state,
        fallback_depth: policy.fallback_depth,
        max_depth: fallback::max_depth(),
        budget: budget_after_primary,
        scope_valid,
        automatic_enabled: fallback::automatic_enabled(),
        auto_execute_enabled: fallback::auto_execute_enabled(),
    });

    emit_fallback_trace(
        session_id,
        policy,
        lifecycle,
        exit_code,
        classification.failure_class,
        side_effect_class,
        side_effect_state,
        &decision,
        budget_before,
        budget_after_primary,
        None,
    )
    .await;

    if decision.action == fallback::FallbackAction::Execute {
        let operation = policy
            .operation
            .as_ref()
            .context("execute fallback missing operation contract")?;
        match execute_authorized_operation_fallback(
            session_id,
            command,
            cwd,
            policy,
            operation,
            pre_index_snapshot.as_deref(),
            budget_after_primary,
        )
        .await
        {
            Ok((fallback_output, budget_after_fallback, verification_passed)) => {
                emit_fallback_trace(
                    session_id,
                    policy,
                    lifecycle,
                    Some(fallback_output.status),
                    classification.failure_class,
                    side_effect_class,
                    fallback::SideEffectState::ConfirmedPerformed,
                    &decision,
                    budget_before,
                    budget_after_fallback,
                    Some(verification_passed),
                )
                .await;
                return Ok(execution_payload(
                    policy,
                    lifecycle,
                    Some(fallback_output.status),
                    &fallback_output.stdout,
                    &fallback_output.stderr,
                    None,
                    classification.failure_class,
                    side_effect_class,
                    fallback::SideEffectState::ConfirmedPerformed,
                    &decision,
                    budget_after_fallback,
                    Some(verification_passed),
                    None,
                ));
            }
            Err(fallback_error) => {
                emit_fallback_trace(
                    session_id,
                    policy,
                    lifecycle,
                    exit_code,
                    classification.failure_class,
                    side_effect_class,
                    side_effect_state,
                    &decision,
                    budget_before,
                    budget_after_primary,
                    Some(false),
                )
                .await;
                let payload = execution_payload(
                    policy,
                    lifecycle,
                    exit_code,
                    &stdout,
                    &stderr,
                    execution_error.as_deref(),
                    classification.failure_class,
                    side_effect_class,
                    side_effect_state,
                    &decision,
                    budget_after_primary,
                    Some(false),
                    Some(&format!("{fallback_error:#}")),
                );
                anyhow::bail!(payload);
            }
        }
    }

    let payload = execution_payload(
        policy,
        lifecycle,
        exit_code,
        &stdout,
        &stderr,
        execution_error.as_deref(),
        classification.failure_class,
        side_effect_class,
        side_effect_state,
        &decision,
        budget_after_primary,
        None,
        None,
    );
    if matches!(
        classification.failure_class,
        fallback::FailureClass::Success | fallback::FailureClass::ExpectedState
    ) {
        Ok(payload)
    } else {
        anyhow::bail!(payload)
    }
}

async fn capture_index_snapshot_if_needed(
    command: &[String],
    cwd: &Path,
    operation: Option<&fallback::OperationContract>,
) -> Option<String> {
    if operation.is_some_and(|operation| {
        operation.kind == fallback::OperationType::GitStagePaths && operation.scope_matches(command)
    }) {
        capture_git_index_snapshot(command, cwd).await
    } else {
        None
    }
}

async fn capture_git_index_snapshot(command: &[String], cwd: &Path) -> Option<String> {
    let git = command.first()?.clone();
    let output = sandbox::run_unrestricted(
        &[git, "ls-files".into(), "--stage".into(), "-z".into()],
        cwd,
        None,
    )
    .await
    .ok()?;
    (output.status == 0).then_some(output.stdout)
}

async fn execute_authorized_operation_fallback(
    session_id: &str,
    original_command: &[String],
    cwd: &Path,
    policy: &ExecutionPolicy,
    operation: &fallback::OperationContract,
    pre_index_snapshot: Option<&str>,
    budget_before_fallback: fallback::Budget,
) -> Result<(sandbox::Output, fallback::Budget, bool)> {
    anyhow::ensure!(
        policy.fallback_depth == 0,
        "fallback recursion is not allowed"
    );
    anyhow::ensure!(
        operation.authorized,
        "operation is not explicitly authorized"
    );
    anyhow::ensure!(
        operation.auto_execute_allowlisted(),
        "operation is not executable-fallback allowlisted"
    );
    anyhow::ensure!(
        operation.scope_matches(original_command),
        "operation scope drift detected before fallback"
    );
    anyhow::ensure!(
        !budget_before_fallback.locked
            && budget_before_fallback.attempt_remaining > 0
            && budget_before_fallback.side_effect_remaining > 0,
        "fallback budget is exhausted or locked"
    );

    if !approvals::request(
        session_id,
        "codex_fallback_v2_execute",
        format!(
            "request_id={} mode=EXECUTE_AUTHORIZED_OPERATION operation={} paths={} budget={}/{}",
            policy.request_id,
            operation.kind.as_str(),
            operation.paths.len(),
            budget_before_fallback.attempt_remaining,
            budget_before_fallback.side_effect_remaining
        ),
        cwd.to_owned(),
    )
    .await?
    {
        anyhow::bail!("user denied executable Codex fallback");
    }

    // Codex is intentionally read-only in Policy V2. It cannot broaden the
    // mutation. The host executes only the exact structured operation below.
    let codex_command = fallback::codex_read_only_command(cwd, fallback::Effort::Low)?;
    let preflight_prompt = fallback::execute_preflight_prompt(cwd, &policy.request_id, operation);
    let codex_output =
        sandbox::run_unrestricted(&codex_command, cwd, Some(preflight_prompt.as_bytes())).await?;
    anyhow::ensure!(
        codex_output.status == 0,
        "Codex read-only preflight failed: exit={} stderr={}",
        codex_output.status,
        codex_output.stderr
    );

    let exact_command = operation.build_exact_command(original_command)?;
    let output = sandbox::run_unrestricted(&exact_command, cwd, None).await?;

    let verification_passed = match operation.kind {
        fallback::OperationType::GitStagePaths => {
            if output.status != 0 {
                false
            } else {
                let before =
                    pre_index_snapshot.context("missing pre-fallback Git index snapshot")?;
                let after = capture_git_index_snapshot(original_command, cwd)
                    .await
                    .context("failed to capture post-fallback Git index snapshot")?;
                fallback::verify_index_change_scope(before, &after, &operation.paths)
            }
        }
        fallback::OperationType::ReadOnlyCommand => {
            policy.accepted_exit_codes.contains(&output.status)
        }
        _ => false,
    };

    anyhow::ensure!(
        verification_passed,
        "post-fallback verification did not confirm the exact authorized postcondition"
    );

    let budget_after = if operation.side_effect_class() == fallback::SideEffectClass::None {
        fallback::Budget {
            attempt_remaining: budget_before_fallback.attempt_remaining.saturating_sub(1),
            side_effect_remaining: budget_before_fallback.side_effect_remaining,
            locked: false,
        }
    } else {
        budget_before_fallback.consume_fallback()
    };

    Ok((output, budget_after, true))
}

#[allow(clippy::too_many_arguments)]
fn execution_payload(
    policy: &ExecutionPolicy,
    lifecycle: fallback::LifecycleEvidence,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    execution_error: Option<&str>,
    failure_class: fallback::FailureClass,
    side_effect_class: fallback::SideEffectClass,
    side_effect_state: fallback::SideEffectState,
    decision: &fallback::FallbackDecision,
    budget_after: fallback::Budget,
    verification_passed: Option<bool>,
    fallback_error: Option<&str>,
) -> String {
    json!({
        "request_id": policy.request_id,
        "operation_type": policy.operation.as_ref().map(|operation| operation.kind.as_str()).unwrap_or("unstructured"),
        "primary_execution_mode": policy.primary_execution_mode.as_str(),
        "host_reached": lifecycle.host_reached,
        "command_started": lifecycle.command_started,
        "command_finished": lifecycle.command_finished,
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "execution_error": execution_error,
        "failure_class": failure_class.as_str(),
        "side_effect_class": side_effect_class.as_str(),
        "side_effect_state": side_effect_state.as_str(),
        "fallback_decision": {
            "action": decision.action.as_str(),
            "reason_code": decision.reason_code.as_str(),
            "operation_authorized": decision.operation_authorized,
            "side_effect_state": decision.side_effect_state.as_str(),
            "attempt_budget_remaining": decision.attempt_budget_remaining,
            "side_effect_budget_remaining": decision.side_effect_budget_remaining,
            "budget_locked": decision.budget_locked,
            "verification_required": decision.verification_required,
        },
        "fallback_mode": decision.mode.map(|mode| mode.as_str()),
        "fallback_depth": policy.fallback_depth,
        "remaining_attempt_budget": budget_after.attempt_remaining,
        "remaining_side_effect_budget": budget_after.side_effect_remaining,
        "budget_locked": budget_after.locked,
        "verification": {
            "required": decision.verification_required,
            "passed": verification_passed,
        },
        "fallback_error": fallback_error,
    })
    .to_string()
}

#[allow(clippy::too_many_arguments)]
async fn emit_fallback_trace(
    session_id: &str,
    policy: &ExecutionPolicy,
    lifecycle: fallback::LifecycleEvidence,
    exit_code: Option<i32>,
    failure_class: fallback::FailureClass,
    side_effect_class: fallback::SideEffectClass,
    side_effect_state: fallback::SideEffectState,
    decision: &fallback::FallbackDecision,
    budget_before: fallback::Budget,
    budget_after: fallback::Budget,
    verification_passed: Option<bool>,
) {
    let trace = json!({
        "request_id": policy.request_id,
        "operation_type": policy.operation.as_ref().map(|operation| operation.kind.as_str()).unwrap_or("unstructured"),
        "primary_execution_mode": policy.primary_execution_mode.as_str(),
        "host_reached": lifecycle.host_reached,
        "command_started": lifecycle.command_started,
        "command_finished": lifecycle.command_finished,
        "exit_code": exit_code,
        "failure_class": failure_class.as_str(),
        "side_effect_class": side_effect_class.as_str(),
        "side_effect_state": side_effect_state.as_str(),
        "fallback_action": decision.action.as_str(),
        "reason_code": decision.reason_code.as_str(),
        "fallback_depth": policy.fallback_depth,
        "budget_before": {
            "attempt": budget_before.attempt_remaining,
            "side_effect": budget_before.side_effect_remaining,
            "locked": budget_before.locked,
        },
        "budget_after": {
            "attempt": budget_after.attempt_remaining,
            "side_effect": budget_after.side_effect_remaining,
            "locked": budget_after.locked,
        },
        "verification_passed": verification_passed,
    });
    approvals::activity(
        session_id,
        format!("Fallback V2 {}", policy.request_id),
        Some(format!("└ {}", trace)),
    )
    .await;
}

pub(crate) async fn codex_fallback(args: &Value, session: &config::Session) -> Result<BackgroundExecution> {
    let task = args
        .get("task")
        .and_then(Value::as_str)
        .context("missing task")?;
    anyhow::ensure!(task.len() <= 128 * 1024, "task is too large");
    let blocker = args
        .get("blocker")
        .and_then(Value::as_str)
        .context("missing blocker")?;
    anyhow::ensure!(blocker.len() <= 64 * 1024, "blocker is too large");
    let cwd = cwd(args, &session.cwd)?;
    let request_id = Uuid::new_v4().to_string();
    let mode = args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("DIAGNOSE_ONLY");

    let explicit_failure_class = args
        .get("failure_class")
        .and_then(Value::as_str)
        .map(parse_failure_class)
        .transpose()?;
    let original_host_reached = args
        .get("original_host_reached")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let safety_probe = fallback::classify(fallback::ClassificationInput {
        command: &[],
        accepted_exit_codes: &[0],
        primary_execution_mode: primary_execution_mode(),
        lifecycle: fallback::LifecycleEvidence {
            host_reached: original_host_reached,
            command_started: false,
            command_finished: false,
        },
        exit_code: None,
        stdout: "",
        stderr: "",
        execution_error: Some(blocker),
        side_effect_class: fallback::SideEffectClass::Unknown,
        authoritative_platform_safety: explicit_failure_class
            == Some(fallback::FailureClass::PlatformSafety),
    });

    if safety_probe.safety_signal
        || explicit_failure_class == Some(fallback::FailureClass::PlatformSafety)
    {
        anyhow::bail!("terminal platform/safety classification: Codex fallback is not permitted");
    }

    match mode {
        "DIAGNOSE_ONLY" => {
            let failure_class = explicit_failure_class.unwrap_or(safety_probe.failure_class);
            let requires_code_change = args
                .get("requires_code_change")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let effort = if requires_code_change {
                fallback::Effort::Medium
            } else {
                fallback::Effort::Low
            };
            if !approvals::request(
                &session.id,
                "codex_fallback_v2_diagnose",
                format!(
                    "request_id={} mode=DIAGNOSE_ONLY class={} model={} effort={}",
                    request_id,
                    failure_class.as_str(),
                    fallback::model(),
                    effort.as_str()
                ),
                cwd.clone(),
            )
            .await?
            {
                anyhow::bail!("user denied diagnostic Codex fallback");
            }

            let command = fallback::codex_read_only_command(&cwd, effort)?;
            let prompt = fallback::diagnose_prompt(&cwd, &request_id, failure_class, blocker);
            let session_id = session.id.clone();
            let label = format!(
                "Codex fallback diagnose {}/{}",
                fallback::model(),
                effort.as_str()
            );
            let task_label = label.clone();
            let handle = tokio::spawn(async move {
                let result = sandbox::run_unrestricted(&command, &cwd, Some(prompt.as_bytes()))
                    .await
                    .and_then(render_output);
                report_command_finished(session_id, &task_label, &result).await;
                result
            });
            Ok(BackgroundExecution { rendered_command: label, handle, activity: "Started" })
        }
        "EXECUTE_AUTHORIZED_OPERATION" => {
            let failure_class = explicit_failure_class
                .context("EXECUTE_AUTHORIZED_OPERATION requires failure_class")?;
            anyhow::ensure!(
                failure_class == fallback::FailureClass::SandboxPermission,
                "executable fallback requires SANDBOX_PERMISSION"
            );
            anyhow::ensure!(
                args.get("original_host_reached").and_then(Value::as_bool) == Some(true),
                "executable fallback requires authoritative original_host_reached=true"
            );
            anyhow::ensure!(
                args.get("original_command_started")
                    .and_then(Value::as_bool)
                    == Some(true),
                "executable fallback requires authoritative original_command_started=true"
            );

            let command = required_command(args)?;
            let operation = fallback::operation_from_value(args.get("operation"))?
                .context("EXECUTE_AUTHORIZED_OPERATION requires operation")?;
            anyhow::ensure!(
                operation.side_effect_state
                    == Some(fallback::SideEffectState::ConfirmedNotPerformed),
                "executable fallback requires CONFIRMED_NOT_PERFORMED side-effect state"
            );
            let policy = ExecutionPolicy {
                request_id,
                accepted_exit_codes: vec![0],
                operation: Some(operation.clone()),
                fallback_depth: args
                    .get("fallback_depth")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as u8,
                primary_execution_mode: primary_execution_mode(),
            };
            anyhow::ensure!(
                policy.fallback_depth == 0,
                "fallback recursion is not allowed"
            );

            let lifecycle = fallback::LifecycleEvidence {
                host_reached: true,
                command_started: true,
                command_finished: args
                    .get("original_command_finished")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            let budget = fallback::Budget::from_operation(Some(&operation));
            let decision = fallback::decide(fallback::DecisionInput {
                failure_class,
                safety_signal: false,
                primary_execution_mode: policy.primary_execution_mode,
                lifecycle,
                operation: Some(&operation),
                side_effect_class: operation.side_effect_class(),
                side_effect_state: fallback::SideEffectState::ConfirmedNotPerformed,
                fallback_depth: 0,
                max_depth: fallback::max_depth(),
                budget,
                scope_valid: operation.scope_matches(&command),
                automatic_enabled: fallback::automatic_enabled(),
                auto_execute_enabled: fallback::auto_execute_enabled(),
            });
            anyhow::ensure!(
                decision.action == fallback::FallbackAction::Execute,
                "fallback policy blocked execution: {}",
                decision.reason_code.as_str()
            );

            let pre_index_snapshot =
                capture_index_snapshot_if_needed(&command, &cwd, Some(&operation)).await;
            let session_id = session.id.clone();
            let label = format!(
                "Codex fallback execute {}/{}",
                fallback::model(),
                operation.kind.as_str()
            );
            let task_label = label.clone();
            let handle = tokio::spawn(async move {
                let result = execute_authorized_operation_fallback(
                    &session_id,
                    &command,
                    &cwd,
                    &policy,
                    &operation,
                    pre_index_snapshot.as_deref(),
                    budget,
                )
                .await
                .and_then(|(output, _, verified)| {
                    anyhow::ensure!(verified, "post-fallback verification failed");
                    render_output(output)
                });
                report_command_finished(session_id, &task_label, &result).await;
                result
            });
            Ok(BackgroundExecution { rendered_command: label, handle, activity: "Started" })
        }
        other => anyhow::bail!("unsupported fallback mode: {other}"),
    }
}

fn parse_failure_class(value: &str) -> Result<fallback::FailureClass> {
    serde_json::from_value(Value::String(value.to_owned()))
        .with_context(|| format!("invalid failure_class: {value}"))
}

pub(crate) async fn without_sandbox(args: &Value, session: &config::Session) -> Result<ExecutionOutcome> {
    let command = required_command(args)?;
    let cwd = cwd(args, &session.cwd)?;
    if !approvals::request(
        &session.id,
        "without_sandbox",
        format!("argv: {command:?}"),
        cwd.clone(),
    )
    .await?
    {
        anyhow::bail!("user denied without_sandbox")
    }

    let rendered_command = render_command(&command);
    approvals::activity(&session.id, format!("Running {rendered_command}"), None).await;

    let session_id = session.id.clone();
    let task_command = rendered_command.clone();
    let handle = tokio::spawn(async move {
        let result = sandbox::run_unrestricted(&command, &cwd, None)
            .await
            .and_then(render_output);
        report_command_finished(session_id, &task_command, &result).await;
        result
    });

    let mut handle = handle;
    match tokio::time::timeout(HOST_FOREGROUND_TIMEOUT, &mut handle).await {
        Ok(joined) => Ok(ExecutionOutcome::Completed(
            joined.context("command task failed")??,
        )),
        Err(_) => Ok(ExecutionOutcome::Background(BackgroundExecution {
            rendered_command,
            handle,
            activity: "Backgrounded host command",
        })),
    }
}

pub(crate) fn render_command(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| shell_word(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

async fn report_command_finished(session_id: String, command: &str, result: &Result<String>) {
    let detail = match result {
        Ok(text) => command_summary(text),
        Err(error) => Some(format!("└ Error: {error:#}")),
    };
    approvals::activity(&session_id, format!("Ran {command}"), detail).await;
}

pub(crate) fn shell_word(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:=+".contains(c))
    {
        value.to_owned()
    } else {
        format!("{:?}", value)
    }
}

fn command_summary(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    let stdout = value
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end();
    let stderr = value
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end();
    let output = if stdout.is_empty() { stderr } else { stdout };
    if output.is_empty() {
        None
    } else {
        Some(
            output
                .lines()
                .map(|line| format!("└ {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}

pub(crate) fn render_output(output: sandbox::Output) -> Result<String> {
    let text = json!({"exit_code":output.status,"stdout":output.stdout,"stderr":output.stderr})
        .to_string();
    if output.status == 0 {
        Ok(text)
    } else {
        anyhow::bail!(text)
    }
}
