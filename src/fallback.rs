use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FailureClass {
    Success,
    ExpectedState,
    SandboxPermission,
    HostEnvironment,
    ToolMissing,
    NetworkRemote,
    SemanticFailure,
    PlatformSafety,
    TransportFailure,
    Unknown,
}

impl FailureClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "SUCCESS",
            Self::ExpectedState => "EXPECTED_STATE",
            Self::SandboxPermission => "SANDBOX_PERMISSION",
            Self::HostEnvironment => "HOST_ENVIRONMENT",
            Self::ToolMissing => "TOOL_MISSING",
            Self::NetworkRemote => "NETWORK_REMOTE",
            Self::SemanticFailure => "SEMANTIC_FAILURE",
            Self::PlatformSafety => "PLATFORM_SAFETY",
            Self::TransportFailure => "TRANSPORT_FAILURE",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SideEffectClass {
    None,
    LocalMutation,
    RemoteMutation,
    Unknown,
}

impl SideEffectClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::LocalMutation => "LOCAL_MUTATION",
            Self::RemoteMutation => "REMOTE_MUTATION",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SideEffectState {
    ConfirmedNotPerformed,
    ConfirmedPerformed,
    Unknown,
}

impl SideEffectState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConfirmedNotPerformed => "CONFIRMED_NOT_PERFORMED",
            Self::ConfirmedPerformed => "CONFIRMED_PERFORMED",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FallbackAction {
    None,
    Diagnose,
    Execute,
    Block,
}

impl FallbackAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Diagnose => "DIAGNOSE",
            Self::Execute => "EXECUTE",
            Self::Block => "BLOCK",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FallbackMode {
    DiagnoseOnly,
    ExecuteAuthorizedOperation,
}

impl FallbackMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DiagnoseOnly => "DIAGNOSE_ONLY",
            Self::ExecuteAuthorizedOperation => "EXECUTE_AUTHORIZED_OPERATION",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReasonCode {
    NoFallbackSuccess,
    NoFallbackExpectedState,
    NoFallbackSemanticFailure,
    NoFallbackPlatformBlock,
    NoFallbackRemoteAmbiguous,
    NoFallbackRemotePerformed,
    NoFallbackSafetySignal,
    FallbackDiagnoseHostEnv,
    FallbackDiagnoseToolMissing,
    FallbackDiagnoseTransport,
    FallbackDiagnoseUnknown,
    FallbackExecuteSandboxPermission,
    FallbackDeniedNoAuthority,
    FallbackDeniedNotAllowlisted,
    FallbackDeniedScopeDrift,
    FallbackDeniedBudgetExhausted,
    FallbackDeniedSideEffectUnknown,
    FallbackDeniedSideEffectPerformed,
    FallbackDeniedMaxDepth,
    FallbackDeniedAutoExecuteDisabled,
    FallbackDeniedLifecycle,
    FallbackDeniedPrimaryExecutionMode,
}

impl ReasonCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoFallbackSuccess => "NO_FALLBACK_SUCCESS",
            Self::NoFallbackExpectedState => "NO_FALLBACK_EXPECTED_STATE",
            Self::NoFallbackSemanticFailure => "NO_FALLBACK_SEMANTIC_FAILURE",
            Self::NoFallbackPlatformBlock => "NO_FALLBACK_PLATFORM_BLOCK",
            Self::NoFallbackRemoteAmbiguous => "NO_FALLBACK_REMOTE_AMBIGUOUS",
            Self::NoFallbackRemotePerformed => "NO_FALLBACK_REMOTE_ALREADY_PERFORMED",
            Self::NoFallbackSafetySignal => "NO_FALLBACK_SAFETY_SIGNAL",
            Self::FallbackDiagnoseHostEnv => "FALLBACK_DIAGNOSE_HOST_ENV",
            Self::FallbackDiagnoseToolMissing => "FALLBACK_DIAGNOSE_TOOL_MISSING",
            Self::FallbackDiagnoseTransport => "FALLBACK_DIAGNOSE_TRANSPORT",
            Self::FallbackDiagnoseUnknown => "FALLBACK_DIAGNOSE_UNKNOWN",
            Self::FallbackExecuteSandboxPermission => "FALLBACK_EXECUTE_SANDBOX_PERMISSION",
            Self::FallbackDeniedNoAuthority => "FALLBACK_DENIED_NO_AUTHORITY",
            Self::FallbackDeniedNotAllowlisted => "FALLBACK_DENIED_NOT_ALLOWLISTED",
            Self::FallbackDeniedScopeDrift => "FALLBACK_DENIED_SCOPE_DRIFT",
            Self::FallbackDeniedBudgetExhausted => "FALLBACK_DENIED_BUDGET_EXHAUSTED",
            Self::FallbackDeniedSideEffectUnknown => "FALLBACK_DENIED_SIDE_EFFECT_UNKNOWN",
            Self::FallbackDeniedSideEffectPerformed => "FALLBACK_DENIED_SIDE_EFFECT_PERFORMED",
            Self::FallbackDeniedMaxDepth => "FALLBACK_DENIED_MAX_DEPTH",
            Self::FallbackDeniedAutoExecuteDisabled => "FALLBACK_DENIED_AUTO_EXECUTE_DISABLED",
            Self::FallbackDeniedLifecycle => "FALLBACK_DENIED_LIFECYCLE",
            Self::FallbackDeniedPrimaryExecutionMode => "FALLBACK_DENIED_PRIMARY_EXECUTION_MODE",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationType {
    ReadOnlyCommand,
    GitStagePaths,
    GitCreateRef,
    GitCommit,
    GitPushRef,
    GithubWorkflowDispatch,
    OtherRemoteMutation,
    Unstructured,
}

impl OperationType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnlyCommand => "read_only_command",
            Self::GitStagePaths => "git_stage_paths",
            Self::GitCreateRef => "git_create_ref",
            Self::GitCommit => "git_commit",
            Self::GitPushRef => "git_push_ref",
            Self::GithubWorkflowDispatch => "github_workflow_dispatch",
            Self::OtherRemoteMutation => "other_remote_mutation",
            Self::Unstructured => "unstructured",
        }
    }

    pub fn side_effect_class(self) -> SideEffectClass {
        match self {
            Self::ReadOnlyCommand => SideEffectClass::None,
            Self::GitStagePaths | Self::GitCreateRef | Self::GitCommit => {
                SideEffectClass::LocalMutation
            }
            Self::GitPushRef | Self::GithubWorkflowDispatch | Self::OtherRemoteMutation => {
                SideEffectClass::RemoteMutation
            }
            Self::Unstructured => SideEffectClass::Unknown,
        }
    }
}

fn default_budget() -> u32 {
    1
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationContract {
    #[serde(rename = "type")]
    pub kind: OperationType,
    #[serde(default)]
    pub authorized: bool,
    #[serde(default)]
    pub operation_id: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub create_only: bool,
    #[serde(default)]
    pub force: bool,
    #[serde(default = "default_budget")]
    pub attempt_budget_remaining: u32,
    #[serde(default = "default_budget")]
    pub side_effect_budget_remaining: u32,
    #[serde(default)]
    pub side_effect_state: Option<SideEffectState>,
}

impl OperationContract {
    pub fn validate(&self) -> Result<()> {
        if let Some(id) = &self.operation_id {
            anyhow::ensure!(!id.is_empty() && id.len() <= 128, "invalid operation_id");
        }
        match self.kind {
            OperationType::GitStagePaths => {
                anyhow::ensure!(!self.paths.is_empty(), "git_stage_paths requires paths");
                for path in &self.paths {
                    anyhow::ensure!(
                        exact_relative_path(path),
                        "git_stage_paths path is not an exact safe relative path: {path:?}"
                    );
                }
            }
            OperationType::ReadOnlyCommand => {
                anyhow::ensure!(!self.argv.is_empty(), "read_only_command requires argv");
            }
            OperationType::GitPushRef => {
                anyhow::ensure!(
                    self.source
                        .as_deref()
                        .is_some_and(|value| !value.is_empty())
                        && self
                            .destination
                            .as_deref()
                            .is_some_and(|value| !value.is_empty()),
                    "git_push_ref requires source and destination"
                );
            }
            _ => {}
        }
        Ok(())
    }

    pub fn scope_matches(&self, command: &[String]) -> bool {
        match self.kind {
            OperationType::GitStagePaths => {
                command.len() == self.paths.len() + 3
                    && executable_name(command) == Some("git")
                    && command.get(1).is_some_and(|value| value == "add")
                    && command.get(2).is_some_and(|value| value == "--")
                    && command[3..] == self.paths
            }
            OperationType::ReadOnlyCommand => command == self.argv,
            _ => false,
        }
    }

    pub fn auto_execute_allowlisted(&self) -> bool {
        matches!(
            self.kind,
            OperationType::GitStagePaths | OperationType::ReadOnlyCommand
        )
    }

    pub fn side_effect_class(&self) -> SideEffectClass {
        self.kind.side_effect_class()
    }

    pub fn build_exact_command(&self, original: &[String]) -> Result<Vec<String>> {
        anyhow::ensure!(
            self.scope_matches(original),
            "operation scope does not match command"
        );
        match self.kind {
            OperationType::GitStagePaths => {
                let mut command = vec![original[0].clone(), "add".to_owned(), "--".to_owned()];
                command.extend(self.paths.clone());
                Ok(command)
            }
            OperationType::ReadOnlyCommand => Ok(self.argv.clone()),
            _ => anyhow::bail!("operation type is not executable fallback allowlisted"),
        }
    }
}

fn exact_relative_path(value: &str) -> bool {
    if value.is_empty()
        || value.starts_with('-')
        || value.starts_with(":(")
        || value.contains('\0')
        || value.contains('*')
        || value.contains('?')
        || value.contains('[')
    {
        return false;
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return false;
    }
    let mut saw_normal = false;
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                if part == ".git" {
                    return false;
                }
                saw_normal = true;
            }
            _ => return false,
        }
    }
    saw_normal
}

fn executable_name(command: &[String]) -> Option<&str> {
    Path::new(command.first()?)
        .file_name()
        .and_then(|value| value.to_str())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleEvidence {
    pub host_reached: bool,
    pub command_started: bool,
    pub command_finished: bool,
}

impl LifecycleEvidence {
    pub fn host_received() -> Self {
        Self {
            host_reached: true,
            command_started: false,
            command_finished: false,
        }
    }

    pub fn completed() -> Self {
        Self {
            host_reached: true,
            command_started: true,
            command_finished: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PrimaryExecutionMode {
    Sandboxed,
    HostNative,
}

impl PrimaryExecutionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sandboxed => "SANDBOXED",
            Self::HostNative => "HOST_NATIVE",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classification {
    pub failure_class: FailureClass,
    pub safety_signal: bool,
}

pub struct ClassificationInput<'a> {
    pub command: &'a [String],
    pub accepted_exit_codes: &'a [i32],
    pub primary_execution_mode: PrimaryExecutionMode,
    pub lifecycle: LifecycleEvidence,
    pub exit_code: Option<i32>,
    pub stdout: &'a str,
    pub stderr: &'a str,
    pub execution_error: Option<&'a str>,
    pub side_effect_class: SideEffectClass,
    pub authoritative_platform_safety: bool,
}

pub fn classify(input: ClassificationInput<'_>) -> Classification {
    if input.authoritative_platform_safety {
        return Classification {
            failure_class: FailureClass::PlatformSafety,
            safety_signal: true,
        };
    }

    if !input.lifecycle.host_reached {
        return Classification {
            failure_class: FailureClass::TransportFailure,
            safety_signal: false,
        };
    }

    if let Some(exit_code) = input.exit_code
        && input.accepted_exit_codes.contains(&exit_code)
    {
        return Classification {
            failure_class: if exit_code == 0 {
                FailureClass::Success
            } else {
                FailureClass::ExpectedState
            },
            safety_signal: false,
        };
    }

    let combined = format!(
        "{}\n{}\n{}",
        input.stdout,
        input.stderr,
        input.execution_error.unwrap_or_default()
    )
    .to_ascii_lowercase();

    let safety_signal = contains_any(
        &combined,
        &[
            "safety policy",
            "policy violation",
            "disallowed by policy",
            "not allowed by policy",
            "refused by policy",
            "content policy",
        ],
    );
    if safety_signal {
        return Classification {
            failure_class: FailureClass::Unknown,
            safety_signal: true,
        };
    }

    if strong_semantic_failure(input.command, &combined) {
        return Classification {
            failure_class: FailureClass::SemanticFailure,
            safety_signal: false,
        };
    }

    if input.side_effect_class == SideEffectClass::RemoteMutation
        && contains_any(
            &combined,
            &[
                "connection reset",
                "broken pipe",
                "timed out",
                "timeout",
                "network is unreachable",
                "could not resolve host",
                "connection refused",
                "unexpected eof",
            ],
        )
    {
        return Classification {
            failure_class: FailureClass::NetworkRemote,
            safety_signal: false,
        };
    }

    if contains_any(
        &combined,
        &[
            "operation not permitted",
            "permission denied",
            "unauthorizedaccessexception",
            "socketexception",
            "sandbox",
            "failed to create .git/index.lock",
            "unable to create '.git/index.lock'",
            "cannot assign requested address",
            "network is unreachable",
        ],
    ) {
        return Classification {
            failure_class: if input.primary_execution_mode == PrimaryExecutionMode::Sandboxed {
                FailureClass::SandboxPermission
            } else {
                FailureClass::HostEnvironment
            },
            safety_signal: false,
        };
    }

    if (!input.lifecycle.command_started
        && contains_any(
            &combined,
            &[
                "no such file or directory",
                "not found",
                "failed to start",
                "cannot find the file",
            ],
        ))
        || (input.exit_code == Some(127) && combined.contains("not found"))
    {
        return Classification {
            failure_class: FailureClass::ToolMissing,
            safety_signal: false,
        };
    }

    if contains_any(
        &combined,
        &[
            "address already in use",
            "no space left on device",
            "too many open files",
            "resource temporarily unavailable",
            "read-only file system",
            "input/output error",
        ],
    ) {
        return Classification {
            failure_class: FailureClass::HostEnvironment,
            safety_signal: false,
        };
    }

    if input.lifecycle.command_finished && input.exit_code.is_some() {
        return Classification {
            failure_class: FailureClass::SemanticFailure,
            safety_signal: false,
        };
    }

    Classification {
        failure_class: FailureClass::Unknown,
        safety_signal: false,
    }
}

fn strong_semantic_failure(command: &[String], combined: &str) -> bool {
    let bytes = combined.as_bytes();
    let has_cs_code = bytes.windows(7).any(|window| {
        window[0..2].eq_ignore_ascii_case(b"cs")
            && window[2..6].iter().all(u8::is_ascii_digit)
            && matches!(window[6], b':' | b' ')
    });
    if has_cs_code
        || contains_any(
            combined,
            &[
                "compiler error",
                "compilation error",
                "failed to compile",
                "could not compile",
            ],
        )
    {
        return true;
    }

    let executable = executable_name(command).unwrap_or_default();
    let is_test = matches!(executable, "dotnet" | "cargo" | "gradle" | "gradlew")
        && command
            .iter()
            .any(|arg| matches!(arg.as_str(), "test" | "check"));
    is_test
        && contains_any(
            combined,
            &[
                "tests failed",
                "test failed",
                "failed tests",
                "failures:",
                "test result: failed",
            ],
        )
}

pub fn infer_side_effect_class(
    command: &[String],
    operation: Option<&OperationContract>,
) -> SideEffectClass {
    if let Some(operation) = operation {
        return operation.side_effect_class();
    }

    let executable = executable_name(command).unwrap_or_default();
    if executable == "git" {
        match command.get(1).map(String::as_str) {
            Some("push") | Some("fetch-pack") | Some("send-pack") => {
                return SideEffectClass::RemoteMutation;
            }
            Some("add") | Some("commit") | Some("update-ref") | Some("tag") | Some("branch")
            | Some("reset") | Some("rebase") | Some("merge") => {
                return SideEffectClass::LocalMutation;
            }
            _ => {}
        }
    }
    if executable == "gh" {
        let joined = command
            .iter()
            .skip(1)
            .map(|value| value.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(" ");
        if contains_any(
            &joined,
            &[
                "workflow run",
                "api --method post",
                "api -x post",
                "pr create",
                "release create",
            ],
        ) {
            return SideEffectClass::RemoteMutation;
        }
    }
    SideEffectClass::Unknown
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub attempt_remaining: u32,
    pub side_effect_remaining: u32,
    pub locked: bool,
}

impl Budget {
    pub fn from_operation(operation: Option<&OperationContract>) -> Self {
        match operation {
            Some(operation) => Self {
                attempt_remaining: operation.attempt_budget_remaining,
                side_effect_remaining: operation.side_effect_budget_remaining,
                locked: false,
            },
            None => Self {
                attempt_remaining: 0,
                side_effect_remaining: 0,
                locked: false,
            },
        }
    }

    pub fn after_state(self, state: SideEffectState) -> Self {
        match state {
            SideEffectState::ConfirmedNotPerformed => self,
            SideEffectState::ConfirmedPerformed => Self {
                attempt_remaining: self.attempt_remaining.saturating_sub(1),
                side_effect_remaining: self.side_effect_remaining.saturating_sub(1),
                locked: false,
            },
            SideEffectState::Unknown => Self {
                attempt_remaining: self.attempt_remaining,
                side_effect_remaining: self.side_effect_remaining,
                locked: true,
            },
        }
    }

    pub fn consume_fallback(self) -> Self {
        Self {
            attempt_remaining: self.attempt_remaining.saturating_sub(1),
            side_effect_remaining: self.side_effect_remaining.saturating_sub(1),
            locked: false,
        }
    }
}

pub struct DecisionInput<'a> {
    pub failure_class: FailureClass,
    pub safety_signal: bool,
    pub primary_execution_mode: PrimaryExecutionMode,
    pub lifecycle: LifecycleEvidence,
    pub operation: Option<&'a OperationContract>,
    pub side_effect_class: SideEffectClass,
    pub side_effect_state: SideEffectState,
    pub fallback_depth: u8,
    pub max_depth: u8,
    pub budget: Budget,
    pub scope_valid: bool,
    pub automatic_enabled: bool,
    pub auto_execute_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FallbackDecision {
    pub action: FallbackAction,
    pub reason_code: ReasonCode,
    pub mode: Option<FallbackMode>,
    pub operation_authorized: bool,
    pub side_effect_state: SideEffectState,
    pub attempt_budget_remaining: u32,
    pub side_effect_budget_remaining: u32,
    pub budget_locked: bool,
    pub verification_required: bool,
}

pub fn decide(input: DecisionInput<'_>) -> FallbackDecision {
    let operation_authorized = input.operation.is_some_and(|op| op.authorized);
    let decision = |action, reason_code, mode, verification_required| FallbackDecision {
        action,
        reason_code,
        mode,
        operation_authorized,
        side_effect_state: input.side_effect_state,
        attempt_budget_remaining: input.budget.attempt_remaining,
        side_effect_budget_remaining: input.budget.side_effect_remaining,
        budget_locked: input.budget.locked,
        verification_required,
    };

    if input.safety_signal {
        return decision(
            FallbackAction::Block,
            ReasonCode::NoFallbackSafetySignal,
            None,
            false,
        );
    }

    match input.failure_class {
        FailureClass::Success => {
            return decision(
                FallbackAction::None,
                ReasonCode::NoFallbackSuccess,
                None,
                false,
            );
        }
        FailureClass::ExpectedState => {
            return decision(
                FallbackAction::None,
                ReasonCode::NoFallbackExpectedState,
                None,
                false,
            );
        }
        FailureClass::SemanticFailure => {
            return decision(
                FallbackAction::None,
                ReasonCode::NoFallbackSemanticFailure,
                None,
                false,
            );
        }
        FailureClass::PlatformSafety => {
            return decision(
                FallbackAction::Block,
                ReasonCode::NoFallbackPlatformBlock,
                None,
                false,
            );
        }
        _ => {}
    }

    if input.side_effect_class == SideEffectClass::RemoteMutation {
        return match input.side_effect_state {
            SideEffectState::Unknown => decision(
                FallbackAction::Block,
                ReasonCode::NoFallbackRemoteAmbiguous,
                None,
                false,
            ),
            SideEffectState::ConfirmedPerformed => decision(
                FallbackAction::Block,
                ReasonCode::NoFallbackRemotePerformed,
                None,
                false,
            ),
            SideEffectState::ConfirmedNotPerformed => decision(
                FallbackAction::Diagnose,
                ReasonCode::FallbackDiagnoseHostEnv,
                Some(FallbackMode::DiagnoseOnly),
                false,
            ),
        };
    }

    if input.fallback_depth >= input.max_depth {
        return decision(
            FallbackAction::Block,
            ReasonCode::FallbackDeniedMaxDepth,
            None,
            false,
        );
    }

    match input.failure_class {
        FailureClass::SandboxPermission => {
            if input.primary_execution_mode != PrimaryExecutionMode::Sandboxed {
                return decision(
                    FallbackAction::Block,
                    ReasonCode::FallbackDeniedPrimaryExecutionMode,
                    None,
                    false,
                );
            }
            if !input.lifecycle.host_reached || !input.lifecycle.command_started {
                return decision(
                    FallbackAction::Block,
                    ReasonCode::FallbackDeniedLifecycle,
                    None,
                    false,
                );
            }
            if !input.automatic_enabled || !input.auto_execute_enabled {
                return decision(
                    FallbackAction::Block,
                    ReasonCode::FallbackDeniedAutoExecuteDisabled,
                    None,
                    false,
                );
            }
            let Some(operation) = input.operation else {
                return decision(
                    FallbackAction::Block,
                    ReasonCode::FallbackDeniedNoAuthority,
                    None,
                    false,
                );
            };
            if !operation.authorized {
                return decision(
                    FallbackAction::Block,
                    ReasonCode::FallbackDeniedNoAuthority,
                    None,
                    false,
                );
            }
            if !operation.auto_execute_allowlisted() {
                return decision(
                    FallbackAction::Diagnose,
                    ReasonCode::FallbackDeniedNotAllowlisted,
                    Some(FallbackMode::DiagnoseOnly),
                    false,
                );
            }
            if !input.scope_valid {
                return decision(
                    FallbackAction::Block,
                    ReasonCode::FallbackDeniedScopeDrift,
                    None,
                    false,
                );
            }
            match input.side_effect_state {
                SideEffectState::Unknown => {
                    return decision(
                        FallbackAction::Block,
                        ReasonCode::FallbackDeniedSideEffectUnknown,
                        None,
                        false,
                    );
                }
                SideEffectState::ConfirmedPerformed => {
                    return decision(
                        FallbackAction::Block,
                        ReasonCode::FallbackDeniedSideEffectPerformed,
                        None,
                        false,
                    );
                }
                SideEffectState::ConfirmedNotPerformed => {}
            }
            if input.budget.locked
                || input.budget.attempt_remaining == 0
                || input.budget.side_effect_remaining == 0
            {
                return decision(
                    FallbackAction::Block,
                    ReasonCode::FallbackDeniedBudgetExhausted,
                    None,
                    false,
                );
            }
            decision(
                FallbackAction::Execute,
                ReasonCode::FallbackExecuteSandboxPermission,
                Some(FallbackMode::ExecuteAuthorizedOperation),
                true,
            )
        }
        FailureClass::HostEnvironment => decision(
            FallbackAction::Diagnose,
            ReasonCode::FallbackDiagnoseHostEnv,
            Some(FallbackMode::DiagnoseOnly),
            false,
        ),
        FailureClass::ToolMissing => decision(
            FallbackAction::Diagnose,
            ReasonCode::FallbackDiagnoseToolMissing,
            Some(FallbackMode::DiagnoseOnly),
            false,
        ),
        FailureClass::TransportFailure | FailureClass::NetworkRemote => decision(
            FallbackAction::Diagnose,
            ReasonCode::FallbackDiagnoseTransport,
            Some(FallbackMode::DiagnoseOnly),
            false,
        ),
        FailureClass::Unknown => decision(
            FallbackAction::Diagnose,
            ReasonCode::FallbackDiagnoseUnknown,
            Some(FallbackMode::DiagnoseOnly),
            false,
        ),
        FailureClass::Success
        | FailureClass::ExpectedState
        | FailureClass::SemanticFailure
        | FailureClass::PlatformSafety => unreachable!(),
    }
}

pub fn automatic_enabled() -> bool {
    !matches!(
        std::env::var("LOCAL_MCP_CODEX_FALLBACK")
            .unwrap_or_else(|_| "auto".to_owned())
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "no" | "off" | "disabled"
    )
}

pub fn auto_execute_enabled() -> bool {
    automatic_enabled()
        && !matches!(
            std::env::var("LOCAL_MCP_CODEX_FALLBACK_AUTO_EXECUTE")
                .unwrap_or_else(|_| "on".to_owned())
                .to_ascii_lowercase()
                .as_str(),
            "0" | "false" | "no" | "off" | "disabled"
        )
}

pub fn max_depth() -> u8 {
    std::env::var("LOCAL_MCP_CODEX_FALLBACK_MAX_DEPTH")
        .ok()
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(1)
        .min(1)
}

pub fn model() -> String {
    std::env::var("LOCAL_MCP_CODEX_FALLBACK_MODEL").unwrap_or_else(|_| "gpt-5.6-luna".to_owned())
}

pub fn codex_path() -> PathBuf {
    if let Some(path) = std::env::var_os("LOCAL_MCP_CODEX_CLI_PATH") {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("CODEX_CLI_PATH") {
        return PathBuf::from(path);
    }
    #[cfg(target_os = "macos")]
    {
        let bundled = PathBuf::from("/Applications/ChatGPT.app/Contents/Resources/codex");
        if bundled.is_file() {
            return bundled;
        }
    }
    PathBuf::from("codex")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
        }
    }
}

pub fn codex_read_only_command(cwd: &Path, effort: Effort) -> Result<Vec<String>> {
    codex_read_only_command_with_model(cwd, effort, &model())
}

/// Builds the existing read-only Codex invocation with a host-selected model.
/// The explicit model argument is intentionally kept below the MCP surface so
/// Goal callers cannot select a provider or model per request.
pub fn codex_read_only_command_with_model(
    cwd: &Path,
    effort: Effort,
    model: &str,
) -> Result<Vec<String>> {
    let codex = codex_path();
    if codex.is_absolute() {
        anyhow::ensure!(
            codex.is_file(),
            "Codex CLI not found at {}",
            codex.display()
        );
    }
    Ok(vec![
        codex.to_string_lossy().into_owned(),
        "exec".to_owned(),
        "-m".to_owned(),
        model.to_owned(),
        "-c".to_owned(),
        format!("model_reasoning_effort={:?}", effort.as_str()),
        "--ephemeral".to_owned(),
        "--ignore-user-config".to_owned(),
        "--skip-git-repo-check".to_owned(),
        "-s".to_owned(),
        "read-only".to_owned(),
        "-C".to_owned(),
        cwd.to_string_lossy().into_owned(),
        "--color".to_owned(),
        "never".to_owned(),
        "-".to_owned(),
    ])
}

pub fn diagnose_prompt(
    cwd: &Path,
    request_id: &str,
    failure_class: FailureClass,
    blocker: &str,
) -> String {
    format!(
        r#"LOCAL-MCP CODEX FALLBACK POLICY V2 — DIAGNOSE_ONLY

Request ID: {request_id}
Repository / cwd: {cwd}
Failure class: {failure_class}
Observed blocker:
{blocker}

This is a read-only diagnostic fallback.

Allowed:
- inspect relevant read-only local state;
- classify the likely local execution/environment cause;
- report whether side effects may have occurred;
- recommend a safe next action.

Forbidden:
- edit files;
- mutate Git;
- push or publish;
- dispatch workflows;
- retry a remote mutation;
- install software;
- broaden the original operation;
- attempt to bypass platform, tool, host, or sandbox safety controls.

If evidence is insufficient, report UNKNOWN. Do not guess.
"#,
        request_id = request_id,
        cwd = cwd.display(),
        failure_class = failure_class.as_str(),
        blocker = blocker,
    )
}

pub fn execute_preflight_prompt(
    cwd: &Path,
    request_id: &str,
    operation: &OperationContract,
) -> String {
    format!(
        r#"LOCAL-MCP CODEX FALLBACK POLICY V2 — EXECUTE_AUTHORIZED_OPERATION PREFLIGHT

Request ID: {request_id}
Repository / cwd: {cwd}
Operation type: {operation_type}
Authorized: {authorized}
Fallback depth: 1

This Codex invocation is READ-ONLY BY DESIGN.
Do NOT execute the mutation.

Inspect only enough read-only state to determine whether the exact structured
operation is coherent with the current repository. Do not rewrite, broaden, or
substitute the operation. Do not use Local MCP, alternate shells, or another
execution route to perform it.

The Local MCP host owns the exact deterministic operation execution after this
preflight and independently verifies the postcondition.

Never attempt to bypass platform, tool, host, or sandbox safety controls.
"#,
        request_id = request_id,
        cwd = cwd.display(),
        operation_type = operation.kind.as_str(),
        authorized = operation.authorized,
    )
}

pub fn infer_side_effect_state(
    side_effect_class: SideEffectClass,
    lifecycle: LifecycleEvidence,
    failure_class: FailureClass,
    local_not_performed_proof: Option<bool>,
) -> SideEffectState {
    if side_effect_class == SideEffectClass::None {
        return SideEffectState::ConfirmedNotPerformed;
    }
    if !lifecycle.command_started {
        return SideEffectState::ConfirmedNotPerformed;
    }
    if failure_class == FailureClass::Success {
        return SideEffectState::ConfirmedPerformed;
    }
    if side_effect_class == SideEffectClass::RemoteMutation {
        return SideEffectState::Unknown;
    }
    match local_not_performed_proof {
        Some(true) => SideEffectState::ConfirmedNotPerformed,
        Some(false) | None => SideEffectState::Unknown,
    }
}

pub fn parse_index_snapshot(snapshot: &str) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for record in snapshot.split('\0').filter(|record| !record.is_empty()) {
        if let Some((metadata, path)) = record.split_once('\t') {
            result.insert(path.to_owned(), metadata.to_owned());
        }
    }
    result
}

pub fn verify_index_change_scope(before: &str, after: &str, allowed_paths: &[String]) -> bool {
    let before = parse_index_snapshot(before);
    let after = parse_index_snapshot(after);
    let allowed = allowed_paths.iter().cloned().collect::<BTreeSet<_>>();
    let all_paths = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>();

    all_paths.into_iter().all(|path| {
        if before.get(&path) == after.get(&path) {
            true
        } else {
            allowed.contains(&path)
        }
    })
}

pub fn accepted_exit_codes(values: Option<&serde_json::Value>) -> Result<Vec<i32>> {
    let Some(values) = values else {
        return Ok(vec![0]);
    };
    let array = values
        .as_array()
        .context("accepted_exit_codes must be an array")?;
    anyhow::ensure!(array.len() <= 32, "too many accepted_exit_codes");
    let mut result = Vec::with_capacity(array.len() + 1);
    result.push(0);
    for value in array {
        let code = value
            .as_i64()
            .context("accepted_exit_codes entries must be integers")?;
        let code = i32::try_from(code).context("accepted exit code is out of range")?;
        if !result.contains(&code) {
            result.push(code);
        }
    }
    Ok(result)
}

pub fn operation_from_value(
    value: Option<&serde_json::Value>,
) -> Result<Option<OperationContract>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let operation: OperationContract =
        serde_json::from_value(value.clone()).context("invalid operation contract")?;
    operation.validate()?;
    Ok(Some(operation))
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_stage_operation() -> OperationContract {
        OperationContract {
            kind: OperationType::GitStagePaths,
            authorized: true,
            operation_id: Some("case-b".to_owned()),
            paths: vec!["src/a.rs".to_owned(), "tests/a.rs".to_owned()],
            argv: vec![],
            source: None,
            destination: None,
            target: None,
            create_only: false,
            force: false,
            attempt_budget_remaining: 1,
            side_effect_budget_remaining: 1,
            side_effect_state: None,
        }
    }

    fn decision_for(
        failure_class: FailureClass,
        operation: Option<&OperationContract>,
        side_effect_class: SideEffectClass,
        side_effect_state: SideEffectState,
        depth: u8,
        budget: Budget,
        scope_valid: bool,
    ) -> FallbackDecision {
        decide(DecisionInput {
            primary_execution_mode: PrimaryExecutionMode::Sandboxed,
            failure_class,
            safety_signal: false,
            lifecycle: LifecycleEvidence::completed(),
            operation,
            side_effect_class,
            side_effect_state,
            fallback_depth: depth,
            max_depth: 1,
            budget,
            scope_valid,
            automatic_enabled: true,
            auto_execute_enabled: true,
        })
    }

    #[test]
    fn case_a_expected_absent_ref_does_not_fallback() {
        let command = vec![
            "git".into(),
            "show-ref".into(),
            "--verify".into(),
            "refs/x".into(),
        ];
        let classification = classify(ClassificationInput {
            primary_execution_mode: PrimaryExecutionMode::Sandboxed,
            command: &command,
            accepted_exit_codes: &[0, 1],
            lifecycle: LifecycleEvidence::completed(),
            exit_code: Some(1),
            stdout: "",
            stderr: "",
            execution_error: None,
            side_effect_class: SideEffectClass::None,
            authoritative_platform_safety: false,
        });
        assert_eq!(classification.failure_class, FailureClass::ExpectedState);
        let decision = decision_for(
            classification.failure_class,
            None,
            SideEffectClass::None,
            SideEffectState::ConfirmedNotPerformed,
            0,
            Budget::from_operation(None),
            false,
        );
        assert_eq!(decision.action, FallbackAction::None);
        assert_eq!(decision.reason_code, ReasonCode::NoFallbackExpectedState);
    }

    #[test]
    fn case_b_sandbox_index_lock_allows_one_exact_authorized_fallback() {
        let operation = git_stage_operation();
        let command = vec![
            "git".into(),
            "add".into(),
            "--".into(),
            "src/a.rs".into(),
            "tests/a.rs".into(),
        ];
        assert!(operation.scope_matches(&command));
        let classification = classify(ClassificationInput {
            primary_execution_mode: PrimaryExecutionMode::Sandboxed,
            command: &command,
            accepted_exit_codes: &[0],
            lifecycle: LifecycleEvidence::completed(),
            exit_code: Some(128),
            stdout: "",
            stderr: "fatal: Unable to create '.git/index.lock': Operation not permitted",
            execution_error: None,
            side_effect_class: SideEffectClass::LocalMutation,
            authoritative_platform_safety: false,
        });
        assert_eq!(
            classification.failure_class,
            FailureClass::SandboxPermission
        );
        let budget = Budget::from_operation(Some(&operation));
        let decision = decision_for(
            classification.failure_class,
            Some(&operation),
            SideEffectClass::LocalMutation,
            SideEffectState::ConfirmedNotPerformed,
            0,
            budget,
            true,
        );
        assert_eq!(decision.action, FallbackAction::Execute);
        assert_eq!(
            decision.mode,
            Some(FallbackMode::ExecuteAuthorizedOperation)
        );
        let depth_one = decision_for(
            classification.failure_class,
            Some(&operation),
            SideEffectClass::LocalMutation,
            SideEffectState::ConfirmedNotPerformed,
            1,
            budget,
            true,
        );
        assert_eq!(depth_one.action, FallbackAction::Block);
    }

    #[test]
    fn host_native_permission_text_is_not_a_sandbox_failure() {
        let command = vec!["git".into(), "add".into(), "--".into(), "src/a.rs".into()];
        let classification = classify(ClassificationInput {
            primary_execution_mode: PrimaryExecutionMode::HostNative,
            command: &command,
            accepted_exit_codes: &[0],
            lifecycle: LifecycleEvidence::completed(),
            exit_code: Some(128),
            stdout: "",
            stderr: "fatal: Unable to create '.git/index.lock': Operation not permitted",
            execution_error: None,
            side_effect_class: SideEffectClass::LocalMutation,
            authoritative_platform_safety: false,
        });
        assert_eq!(classification.failure_class, FailureClass::HostEnvironment);
    }

    #[test]
    fn sandbox_permission_label_requires_sandboxed_primary_execution() {
        let operation = git_stage_operation();
        let decision = decide(DecisionInput {
            primary_execution_mode: PrimaryExecutionMode::HostNative,
            failure_class: FailureClass::SandboxPermission,
            safety_signal: false,
            lifecycle: LifecycleEvidence::completed(),
            operation: Some(&operation),
            side_effect_class: SideEffectClass::LocalMutation,
            side_effect_state: SideEffectState::ConfirmedNotPerformed,
            fallback_depth: 0,
            max_depth: 1,
            budget: Budget::from_operation(Some(&operation)),
            scope_valid: true,
            automatic_enabled: true,
            auto_execute_enabled: true,
        });
        assert_eq!(decision.action, FallbackAction::Block);
        assert_eq!(
            decision.reason_code,
            ReasonCode::FallbackDeniedPrimaryExecutionMode
        );
    }

    #[test]
    fn sandbox_permission_requires_host_reached_and_command_started() {
        let operation = git_stage_operation();
        let budget = Budget::from_operation(Some(&operation));

        let executable = decide(DecisionInput {
            primary_execution_mode: PrimaryExecutionMode::Sandboxed,
            failure_class: FailureClass::SandboxPermission,
            safety_signal: false,
            lifecycle: LifecycleEvidence {
                host_reached: true,
                command_started: true,
                command_finished: true,
            },
            operation: Some(&operation),
            side_effect_class: SideEffectClass::LocalMutation,
            side_effect_state: SideEffectState::ConfirmedNotPerformed,
            fallback_depth: 0,
            max_depth: 1,
            budget,
            scope_valid: true,
            automatic_enabled: true,
            auto_execute_enabled: true,
        });
        assert_eq!(executable.action, FallbackAction::Execute);

        let no_host = decide(DecisionInput {
            primary_execution_mode: PrimaryExecutionMode::Sandboxed,
            failure_class: FailureClass::SandboxPermission,
            safety_signal: false,
            lifecycle: LifecycleEvidence {
                host_reached: false,
                command_started: false,
                command_finished: false,
            },
            operation: Some(&operation),
            side_effect_class: SideEffectClass::LocalMutation,
            side_effect_state: SideEffectState::ConfirmedNotPerformed,
            fallback_depth: 0,
            max_depth: 1,
            budget,
            scope_valid: true,
            automatic_enabled: true,
            auto_execute_enabled: true,
        });
        assert_eq!(no_host.action, FallbackAction::Block);
        assert_eq!(no_host.reason_code, ReasonCode::FallbackDeniedLifecycle);

        let not_started = decide(DecisionInput {
            primary_execution_mode: PrimaryExecutionMode::Sandboxed,
            failure_class: FailureClass::SandboxPermission,
            safety_signal: false,
            lifecycle: LifecycleEvidence {
                host_reached: true,
                command_started: false,
                command_finished: false,
            },
            operation: Some(&operation),
            side_effect_class: SideEffectClass::LocalMutation,
            side_effect_state: SideEffectState::ConfirmedNotPerformed,
            fallback_depth: 0,
            max_depth: 1,
            budget,
            scope_valid: true,
            automatic_enabled: true,
            auto_execute_enabled: true,
        });
        assert_eq!(not_started.action, FallbackAction::Block);
        assert_eq!(not_started.reason_code, ReasonCode::FallbackDeniedLifecycle);
    }

    #[test]
    fn sandbox_permission_label_alone_cannot_bypass_lifecycle() {
        let operation = git_stage_operation();
        let decision = decide(DecisionInput {
            primary_execution_mode: PrimaryExecutionMode::Sandboxed,
            failure_class: FailureClass::SandboxPermission,
            safety_signal: false,
            lifecycle: LifecycleEvidence {
                host_reached: false,
                command_started: false,
                command_finished: false,
            },
            operation: Some(&operation),
            side_effect_class: SideEffectClass::LocalMutation,
            side_effect_state: SideEffectState::ConfirmedNotPerformed,
            fallback_depth: 0,
            max_depth: 1,
            budget: Budget::from_operation(Some(&operation)),
            scope_valid: true,
            automatic_enabled: true,
            auto_execute_enabled: true,
        });
        assert_eq!(decision.action, FallbackAction::Block);
        assert_eq!(decision.reason_code, ReasonCode::FallbackDeniedLifecycle);
    }

    #[test]
    fn case_c_authoritative_platform_block_is_terminal() {
        let classification = classify(ClassificationInput {
            primary_execution_mode: PrimaryExecutionMode::Sandboxed,
            command: &["git".into(), "add".into()],
            accepted_exit_codes: &[0],
            lifecycle: LifecycleEvidence {
                host_reached: false,
                command_started: false,
                command_finished: false,
            },
            exit_code: None,
            stdout: "",
            stderr: "",
            execution_error: None,
            side_effect_class: SideEffectClass::LocalMutation,
            authoritative_platform_safety: true,
        });
        assert_eq!(classification.failure_class, FailureClass::PlatformSafety);
        let decision = decision_for(
            classification.failure_class,
            None,
            SideEffectClass::LocalMutation,
            SideEffectState::ConfirmedNotPerformed,
            0,
            Budget::from_operation(None),
            false,
        );
        assert_eq!(decision.action, FallbackAction::Block);
        assert_eq!(decision.reason_code, ReasonCode::NoFallbackPlatformBlock);
    }

    #[test]
    fn case_d_semantic_test_failure_does_not_fallback() {
        let command = vec!["dotnet".into(), "test".into()];
        let classification = classify(ClassificationInput {
            primary_execution_mode: PrimaryExecutionMode::Sandboxed,
            command: &command,
            accepted_exit_codes: &[0],
            lifecycle: LifecycleEvidence::completed(),
            exit_code: Some(1),
            stdout: "Test Run Failed. Failed tests: 2",
            stderr: "",
            execution_error: None,
            side_effect_class: SideEffectClass::None,
            authoritative_platform_safety: false,
        });
        assert_eq!(classification.failure_class, FailureClass::SemanticFailure);
        let decision = decision_for(
            classification.failure_class,
            None,
            SideEffectClass::None,
            SideEffectState::ConfirmedNotPerformed,
            0,
            Budget::from_operation(None),
            false,
        );
        assert_eq!(decision.action, FallbackAction::None);
    }

    #[test]
    fn case_e_ambiguous_remote_push_never_auto_retries() {
        let operation = OperationContract {
            kind: OperationType::GitPushRef,
            authorized: true,
            operation_id: Some("push".into()),
            paths: vec![],
            argv: vec![],
            source: Some("refs/heads/x".into()),
            destination: Some("refs/heads/x".into()),
            target: None,
            create_only: false,
            force: false,
            attempt_budget_remaining: 1,
            side_effect_budget_remaining: 1,
            side_effect_state: None,
        };
        let decision = decision_for(
            FailureClass::NetworkRemote,
            Some(&operation),
            SideEffectClass::RemoteMutation,
            SideEffectState::Unknown,
            0,
            Budget::from_operation(Some(&operation)).after_state(SideEffectState::Unknown),
            false,
        );
        assert_eq!(decision.action, FallbackAction::Block);
        assert_eq!(decision.reason_code, ReasonCode::NoFallbackRemoteAmbiguous);
        assert!(decision.budget_locked);
    }

    #[test]
    fn case_f_remote_already_performed_consumes_budget_and_does_not_retry() {
        let operation = OperationContract {
            kind: OperationType::GitPushRef,
            authorized: true,
            operation_id: Some("push".into()),
            paths: vec![],
            argv: vec![],
            source: Some("refs/heads/x".into()),
            destination: Some("refs/heads/x".into()),
            target: None,
            create_only: false,
            force: false,
            attempt_budget_remaining: 1,
            side_effect_budget_remaining: 1,
            side_effect_state: Some(SideEffectState::ConfirmedPerformed),
        };
        let budget = Budget::from_operation(Some(&operation))
            .after_state(SideEffectState::ConfirmedPerformed);
        assert_eq!(budget.attempt_remaining, 0);
        assert_eq!(budget.side_effect_remaining, 0);
        let decision = decision_for(
            FailureClass::NetworkRemote,
            Some(&operation),
            SideEffectClass::RemoteMutation,
            SideEffectState::ConfirmedPerformed,
            0,
            budget,
            false,
        );
        assert_eq!(decision.action, FallbackAction::Block);
        assert_eq!(decision.reason_code, ReasonCode::NoFallbackRemotePerformed);
    }

    #[test]
    fn case_g_tool_missing_is_diagnose_only_at_most() {
        let classification = classify(ClassificationInput {
            primary_execution_mode: PrimaryExecutionMode::Sandboxed,
            command: &["missing-tool".into()],
            accepted_exit_codes: &[0],
            lifecycle: LifecycleEvidence::host_received(),
            exit_code: None,
            stdout: "",
            stderr: "",
            execution_error: Some("failed to start: No such file or directory"),
            side_effect_class: SideEffectClass::None,
            authoritative_platform_safety: false,
        });
        assert_eq!(classification.failure_class, FailureClass::ToolMissing);
        let decision = decision_for(
            classification.failure_class,
            None,
            SideEffectClass::None,
            SideEffectState::ConfirmedNotPerformed,
            0,
            Budget::from_operation(None),
            false,
        );
        assert_eq!(decision.action, FallbackAction::Diagnose);
        assert_eq!(decision.mode, Some(FallbackMode::DiagnoseOnly));
    }

    #[test]
    fn case_h_unknown_failure_is_not_executable() {
        let decision = decision_for(
            FailureClass::Unknown,
            None,
            SideEffectClass::Unknown,
            SideEffectState::Unknown,
            0,
            Budget::from_operation(None),
            false,
        );
        assert_eq!(decision.action, FallbackAction::Diagnose);
        assert_eq!(decision.mode, Some(FallbackMode::DiagnoseOnly));
    }

    #[test]
    fn case_i_fallback_depth_prevents_recursion() {
        let operation = git_stage_operation();
        let decision = decision_for(
            FailureClass::SandboxPermission,
            Some(&operation),
            SideEffectClass::LocalMutation,
            SideEffectState::ConfirmedNotPerformed,
            1,
            Budget::from_operation(Some(&operation)),
            true,
        );
        assert_eq!(decision.action, FallbackAction::Block);
        assert_eq!(decision.reason_code, ReasonCode::FallbackDeniedMaxDepth);
    }

    #[test]
    fn case_j_scope_drift_is_rejected_before_mutation() {
        let operation = git_stage_operation();
        let drift = vec!["git".into(), "add".into(), "-A".into()];
        assert!(!operation.scope_matches(&drift));
        let decision = decision_for(
            FailureClass::SandboxPermission,
            Some(&operation),
            SideEffectClass::LocalMutation,
            SideEffectState::ConfirmedNotPerformed,
            0,
            Budget::from_operation(Some(&operation)),
            false,
        );
        assert_eq!(decision.action, FallbackAction::Block);
        assert_eq!(decision.reason_code, ReasonCode::FallbackDeniedScopeDrift);
    }

    #[test]
    fn case_k_exhausted_budget_blocks_execution() {
        let mut operation = git_stage_operation();
        operation.attempt_budget_remaining = 0;
        operation.side_effect_budget_remaining = 0;
        let decision = decision_for(
            FailureClass::SandboxPermission,
            Some(&operation),
            SideEffectClass::LocalMutation,
            SideEffectState::ConfirmedNotPerformed,
            0,
            Budget::from_operation(Some(&operation)),
            true,
        );
        assert_eq!(decision.action, FallbackAction::Block);
        assert_eq!(
            decision.reason_code,
            ReasonCode::FallbackDeniedBudgetExhausted
        );
    }

    #[test]
    fn case_l_postcondition_rejects_unauthorized_index_change() {
        let before = concat!("100644 aaaa 0\tsrc/a.rs\0", "100644 bbbb 0\tsrc/other.rs\0");
        let after_ok = concat!("100644 cccc 0\tsrc/a.rs\0", "100644 bbbb 0\tsrc/other.rs\0");
        let after_drift = concat!("100644 cccc 0\tsrc/a.rs\0", "100644 dddd 0\tsrc/other.rs\0");
        assert!(verify_index_change_scope(
            before,
            after_ok,
            &["src/a.rs".into()]
        ));
        assert!(!verify_index_change_scope(
            before,
            after_drift,
            &["src/a.rs".into()]
        ));
    }

    #[test]
    fn rejects_pathspecs_that_can_broaden_git_staging() {
        for path in [".", "../x", "*.rs", ":(glob)**", ".git/config", "/tmp/x"] {
            assert!(!exact_relative_path(path), "{path}");
        }
        assert!(exact_relative_path("src/file name.rs"));
    }

    #[test]
    fn accepted_exit_codes_always_preserve_zero() {
        let value = serde_json::json!([1, 2, 1]);
        assert_eq!(accepted_exit_codes(Some(&value)).unwrap(), vec![0, 1, 2]);
    }
}
