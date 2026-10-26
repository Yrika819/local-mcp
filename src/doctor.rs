use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    time::Duration,
};

use clap::Args;
use serde::Serialize;

const JSON_OUTPUT_LIMIT: usize = 64 * 1024;
const HUMAN_OUTPUT_LIMIT: usize = 32 * 1024;
const MAX_CHECKS: usize = 64;
const MAX_MESSAGE_CHARS: usize = 512;
const STATIC_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const DOCTOR_VERSION: &str = "1.0.0";

#[derive(Args, Clone, Copy, Debug, Default)]
pub(crate) struct DoctorOptions {
    /// Emit one versioned JSON report instead of plain text.
    #[arg(long)]
    json: bool,
    /// Request a Bubblewrap version probe (currently refused without verified containment).
    #[arg(long)]
    probe_bubblewrap_version: bool,
    /// Request a Codex CLI version probe (currently refused without verified containment).
    #[arg(long)]
    probe_codex_version: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum DiagnosticStatus {
    Pass,
    Warn,
    Block,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Requirement {
    Required,
    Optional,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EvidenceType {
    StaticObservation,
    FormalContract,
    RuntimeReadOnly,
    NotRun,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[allow(
    dead_code,
    reason = "Stable V1 reason codes include fail-closed probe outcomes that are never spawned by this implementation."
)]
pub(crate) enum ReasonCode {
    #[serde(rename = "CAPABILITY_AVAILABLE")]
    CapabilityAvailable,
    #[serde(rename = "CHECK_NOT_APPLICABLE")]
    CheckNotApplicable,
    #[serde(rename = "PROBE_NOT_RUN")]
    ProbeNotRun,
    #[serde(rename = "RUNTIME_VERIFICATION_NOT_RUN")]
    RuntimeVerificationNotRun,
    #[serde(rename = "DOCTOR_STATIC_CHECK_TIMEOUT")]
    DoctorStaticCheckTimeout,
    #[serde(rename = "HOST_TARGET_UNKNOWN")]
    HostTargetUnknown,
    #[serde(rename = "HOST_TARGET_UNSUPPORTED")]
    HostTargetUnsupported,
    #[serde(rename = "PLATFORM_WINDOWS_EXPERIMENTAL")]
    PlatformWindowsExperimental,
    #[serde(rename = "PLATFORM_LINUX_AARCH64_UNVERIFIED")]
    PlatformLinuxAarch64Unverified,
    #[serde(rename = "PLATFORM_MACOS_AARCH64_CI_ONLY")]
    PlatformMacosAarch64CiOnly,
    #[serde(rename = "INSTALL_CURRENT_EXE_UNAVAILABLE")]
    InstallCurrentExeUnavailable,
    #[serde(rename = "INSTALL_REQUIRED_HELPER_MISSING")]
    InstallRequiredHelperMissing,
    #[serde(rename = "INSTALL_REQUIRED_HELPER_NOT_REGULAR_FILE")]
    InstallRequiredHelperNotRegularFile,
    #[serde(rename = "INSTALL_REQUIRED_HELPER_UNREADABLE")]
    InstallRequiredHelperUnreadable,
    #[serde(rename = "INSTALL_REQUIRED_HELPER_NOT_EXECUTABLE")]
    InstallRequiredHelperNotExecutable,
    #[serde(rename = "INSTALL_FORBIDDEN_HELPER_PRESENT")]
    InstallForbiddenHelperPresent,
    #[serde(rename = "INSTALL_SYMLINK_ORIGIN_UNKNOWN")]
    InstallSymlinkOriginUnknown,
    #[serde(rename = "INSTALL_NIX_UNVERIFIED")]
    InstallNixUnverified,
    #[serde(rename = "CODEX_CLI_NOT_FOUND")]
    CodexCliNotFound,
    #[serde(rename = "CODEX_CLI_PATH_INVALID")]
    CodexCliPathInvalid,
    #[serde(rename = "CODEX_CLI_PATH_CONTEXT_UNKNOWN")]
    CodexCliPathContextUnknown,
    #[serde(rename = "CODEX_CLI_NOT_EXECUTABLE")]
    CodexCliNotExecutable,
    #[serde(rename = "CODEX_CLI_VERSION_PROBE_REFUSED")]
    CodexCliVersionProbeRefused,
    #[serde(rename = "CODEX_CLI_VERSION_TIMEOUT")]
    CodexCliVersionTimeout,
    #[serde(rename = "CODEX_CLI_VERSION_MALFORMED")]
    CodexCliVersionMalformed,
    #[serde(rename = "CODEX_CLI_VERSION_NONZERO")]
    CodexCliVersionNonzero,
    #[serde(rename = "BWRAP_NOT_FOUND")]
    BwrapNotFound,
    #[serde(rename = "BWRAP_VERSION_UNPROBED")]
    BwrapVersionUnprobed,
    #[serde(rename = "BWRAP_VERSION_UNSUPPORTED")]
    BwrapVersionUnsupported,
    #[serde(rename = "BWRAP_VERSION_UNKNOWN")]
    BwrapVersionUnknown,
    #[serde(rename = "BWRAP_RUNTIME_PREREQUISITES_UNKNOWN")]
    BwrapRuntimePrerequisitesUnknown,
    #[serde(rename = "MACOS_SANDBOX_EXEC_NOT_FOUND")]
    MacosSandboxExecNotFound,
    #[serde(rename = "SANDBOX_PARITY_NOT_APPLICABLE")]
    SandboxParityNotApplicable,
}

impl ReasonCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::CapabilityAvailable => "CAPABILITY_AVAILABLE",
            Self::CheckNotApplicable => "CHECK_NOT_APPLICABLE",
            Self::ProbeNotRun => "PROBE_NOT_RUN",
            Self::RuntimeVerificationNotRun => "RUNTIME_VERIFICATION_NOT_RUN",
            Self::DoctorStaticCheckTimeout => "DOCTOR_STATIC_CHECK_TIMEOUT",
            Self::HostTargetUnknown => "HOST_TARGET_UNKNOWN",
            Self::HostTargetUnsupported => "HOST_TARGET_UNSUPPORTED",
            Self::PlatformWindowsExperimental => "PLATFORM_WINDOWS_EXPERIMENTAL",
            Self::PlatformLinuxAarch64Unverified => "PLATFORM_LINUX_AARCH64_UNVERIFIED",
            Self::PlatformMacosAarch64CiOnly => "PLATFORM_MACOS_AARCH64_CI_ONLY",
            Self::InstallCurrentExeUnavailable => "INSTALL_CURRENT_EXE_UNAVAILABLE",
            Self::InstallRequiredHelperMissing => "INSTALL_REQUIRED_HELPER_MISSING",
            Self::InstallRequiredHelperNotRegularFile => "INSTALL_REQUIRED_HELPER_NOT_REGULAR_FILE",
            Self::InstallRequiredHelperUnreadable => "INSTALL_REQUIRED_HELPER_UNREADABLE",
            Self::InstallRequiredHelperNotExecutable => "INSTALL_REQUIRED_HELPER_NOT_EXECUTABLE",
            Self::InstallForbiddenHelperPresent => "INSTALL_FORBIDDEN_HELPER_PRESENT",
            Self::InstallSymlinkOriginUnknown => "INSTALL_SYMLINK_ORIGIN_UNKNOWN",
            Self::InstallNixUnverified => "INSTALL_NIX_UNVERIFIED",
            Self::CodexCliNotFound => "CODEX_CLI_NOT_FOUND",
            Self::CodexCliPathInvalid => "CODEX_CLI_PATH_INVALID",
            Self::CodexCliPathContextUnknown => "CODEX_CLI_PATH_CONTEXT_UNKNOWN",
            Self::CodexCliNotExecutable => "CODEX_CLI_NOT_EXECUTABLE",
            Self::CodexCliVersionProbeRefused => "CODEX_CLI_VERSION_PROBE_REFUSED",
            Self::CodexCliVersionTimeout => "CODEX_CLI_VERSION_TIMEOUT",
            Self::CodexCliVersionMalformed => "CODEX_CLI_VERSION_MALFORMED",
            Self::CodexCliVersionNonzero => "CODEX_CLI_VERSION_NONZERO",
            Self::BwrapNotFound => "BWRAP_NOT_FOUND",
            Self::BwrapVersionUnprobed => "BWRAP_VERSION_UNPROBED",
            Self::BwrapVersionUnsupported => "BWRAP_VERSION_UNSUPPORTED",
            Self::BwrapVersionUnknown => "BWRAP_VERSION_UNKNOWN",
            Self::BwrapRuntimePrerequisitesUnknown => "BWRAP_RUNTIME_PREREQUISITES_UNKNOWN",
            Self::MacosSandboxExecNotFound => "MACOS_SANDBOX_EXEC_NOT_FOUND",
            Self::SandboxParityNotApplicable => "SANDBOX_PARITY_NOT_APPLICABLE",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CapabilityId {
    McpTransport,
    SessionManagement,
    SandboxedExecution,
    AtomicPublication,
    GoalModelExecution,
}

impl CapabilityId {
    fn as_str(self) -> &'static str {
        match self {
            Self::McpTransport => "mcp_transport",
            Self::SessionManagement => "session_management",
            Self::SandboxedExecution => "sandboxed_execution",
            Self::AtomicPublication => "atomic_publication",
            Self::GoalModelExecution => "goal_model_execution",
        }
    }
}

/// Doctor emits only null or booleans for `observed`. Arbitrary JSON values are
/// intentionally not representable here, even though the schema permits them.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct SafeObserved(Option<bool>);

impl SafeObserved {
    const fn none() -> Self {
        Self(None)
    }

    const fn boolean(value: bool) -> Self {
        Self(Some(value))
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DiagnosticCheck {
    id: String,
    status: DiagnosticStatus,
    requirement: Requirement,
    applicability: Applicability,
    evidence_type: EvidenceType,
    observed: SafeObserved,
    reason_code: ReasonCode,
    message: String,
    capability: Option<CapabilityId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Applicability {
    Applicable,
    NotApplicable,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CapabilityStatus {
    requirement: Requirement,
    status: DiagnosticStatus,
    formal_status: DiagnosticStatus,
    runtime_status: DiagnosticStatus,
    reason_codes: Vec<ReasonCode>,
}

#[derive(Clone, Debug, Serialize)]
struct Capabilities {
    mcp_transport: CapabilityStatus,
    session_management: CapabilityStatus,
    sandboxed_execution: CapabilityStatus,
    atomic_publication: CapabilityStatus,
    goal_model_execution: CapabilityStatus,
}

#[derive(Clone, Debug, Serialize)]
struct PlatformDiagnostic {
    operating_system: OperatingSystem,
    architecture: Architecture,
    host_target: Option<String>,
    support_status: SupportStatus,
    experimental: bool,
    unix_sandbox_parity: UnixSandboxParity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum OperatingSystem {
    Linux,
    Macos,
    Windows,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Architecture {
    X86_64,
    Aarch64,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SupportStatus {
    Supported,
    Experimental,
    CiTarget,
    Unsupported,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum UnixSandboxParity {
    Available,
    NotApplicable,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
struct Installation {
    install_contract_version: u8,
    helper_resolution: &'static str,
    current_exe: CurrentExe,
    required_helpers: Vec<Helper>,
    forbidden_helpers: Vec<Helper>,
    symlink_status: DiagnosticStatus,
    nix_validation_status: DiagnosticStatus,
}

#[derive(Clone, Debug, Serialize)]
struct CurrentExe {
    observed: bool,
    path_redacted: bool,
    basename_matches_expected: Option<bool>,
    directory_observed: bool,
    resolution: CurrentExeResolution,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CurrentExeResolution {
    AsObserved,
    Unavailable,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
struct Helper {
    name: String,
    required: bool,
    present: Option<bool>,
    regular_file: Option<bool>,
    readable: Option<bool>,
    executable: Option<bool>,
    resolution: HelperResolution,
    status: DiagnosticStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum HelperResolution {
    CurrentExeSibling,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
struct Dependencies {
    codex_cli: CodexCli,
    bubblewrap: Bubblewrap,
    macos_sandbox_exec: MacosSandboxExec,
}

#[derive(Clone, Debug, Serialize)]
struct CodexCli {
    resolution_source: CodexSource,
    selected: bool,
    path_redacted: bool,
    file_status: CodexFileStatus,
    executable_status: DiagnosticStatus,
    version_probe: VersionProbe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CodexSource {
    LocalMcpCodexCliPath,
    CodexCliPath,
    MacosChatgptBundle,
    Path,
    None,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CodexFileStatus {
    Present,
    Missing,
    NotRegularFile,
    Unreadable,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
struct VersionProbe {
    requested: bool,
    attempted: bool,
    status: DiagnosticStatus,
    version: Option<String>,
    timeout_seconds: u8,
    reason_code: ReasonCode,
}

#[derive(Clone, Debug, Serialize)]
struct Bubblewrap {
    applicable: bool,
    found: Option<bool>,
    version: Option<String>,
    minimum_version: &'static str,
    version_probe: VersionProbe,
}

#[derive(Clone, Debug, Serialize)]
struct MacosSandboxExec {
    applicable: bool,
    present: Option<bool>,
    executable_status: DiagnosticStatus,
    runtime_status: DiagnosticStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExitCode {
    Success,
    RequiredBlock,
    RequiredUnknown,
    UsageError,
    InternalError,
}

impl ExitCode {
    pub(crate) const fn value(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::RequiredBlock => 1,
            Self::RequiredUnknown => 2,
            Self::UsageError => 64,
            Self::InternalError => 70,
        }
    }
}

impl Serialize for ExitCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u8(self.value())
    }
}

#[derive(Clone, Debug, Serialize)]
struct DiagnosticCounts {
    #[serde(rename = "PASS")]
    pass: u8,
    #[serde(rename = "WARN")]
    warn: u8,
    #[serde(rename = "BLOCK")]
    block: u8,
    #[serde(rename = "UNKNOWN")]
    unknown: u8,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DiagnosticSummary {
    status: DiagnosticStatus,
    required_status: DiagnosticStatus,
    advisory_status: DiagnosticStatus,
    runtime_status: DiagnosticStatus,
    counts: DiagnosticCounts,
    checks_total: usize,
    required_blockers: usize,
    required_unknowns: usize,
    exit_code: ExitCode,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DoctorReport {
    schema_version: &'static str,
    diagnostic_contract_version: u8,
    doctor_version: &'static str,
    goallatch_version: &'static str,
    platform: PlatformDiagnostic,
    installation: Installation,
    dependencies: Dependencies,
    capabilities: Capabilities,
    checks: Vec<DiagnosticCheck>,
    summary: DiagnosticSummary,
}

impl DoctorReport {
    fn validate(&self) -> Result<(), ()> {
        if self.checks.is_empty() || self.checks.len() > MAX_CHECKS {
            return Err(());
        }
        if self.checks.iter().any(|check| {
            check.id.is_empty()
                || check.id.len() > 128
                || check.message.chars().count() > MAX_MESSAGE_CHARS
                || check.reason_code.as_str().len() > 64
        }) {
            return Err(());
        }
        for capability in [
            &self.capabilities.mcp_transport,
            &self.capabilities.session_management,
            &self.capabilities.sandboxed_execution,
            &self.capabilities.atomic_publication,
            &self.capabilities.goal_model_execution,
        ] {
            if capability.reason_codes.len() > 16 {
                return Err(());
            }
        }
        let actual = summarize_checks(&self.checks);
        if actual.checks_total != self.summary.checks_total
            || actual.status != self.summary.status
            || actual.required_status != self.summary.required_status
            || actual.advisory_status != self.summary.advisory_status
            || actual.runtime_status != self.summary.runtime_status
            || actual.counts.pass != self.summary.counts.pass
            || actual.counts.warn != self.summary.counts.warn
            || actual.counts.block != self.summary.counts.block
            || actual.counts.unknown != self.summary.counts.unknown
            || actual.required_blockers != self.summary.required_blockers
            || actual.required_unknowns != self.summary.required_unknowns
            || actual.exit_code != self.summary.exit_code
        {
            return Err(());
        }
        let sandbox_requirement = if self.platform.operating_system == OperatingSystem::Windows {
            Requirement::NotApplicable
        } else {
            Requirement::Required
        };
        if self.checks.iter().any(|check| {
            if (check.requirement == Requirement::NotApplicable)
                != (check.applicability == Applicability::NotApplicable)
            {
                return true;
            }
            let Some(id) = check.capability else {
                return false;
            };
            let expected_requirement = match id {
                CapabilityId::McpTransport
                | CapabilityId::SessionManagement
                | CapabilityId::AtomicPublication => Requirement::Required,
                CapabilityId::SandboxedExecution => sandbox_requirement,
                CapabilityId::GoalModelExecution => Requirement::Optional,
            };
            check.requirement != expected_requirement
        }) {
            return Err(());
        }
        for (actual, id, requirement) in [
            (
                &self.capabilities.mcp_transport,
                CapabilityId::McpTransport,
                Requirement::Required,
            ),
            (
                &self.capabilities.session_management,
                CapabilityId::SessionManagement,
                Requirement::Required,
            ),
            (
                &self.capabilities.sandboxed_execution,
                CapabilityId::SandboxedExecution,
                sandbox_requirement,
            ),
            (
                &self.capabilities.atomic_publication,
                CapabilityId::AtomicPublication,
                Requirement::Required,
            ),
            (
                &self.capabilities.goal_model_execution,
                CapabilityId::GoalModelExecution,
                Requirement::Optional,
            ),
        ] {
            let expected = capability(&self.checks, id, requirement);
            if actual.requirement != requirement
                || actual.status != expected.status
                || actual.formal_status != expected.formal_status
                || actual.runtime_status != expected.runtime_status
                || actual.reason_codes != expected.reason_codes
            {
                return Err(());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HostFacts {
    os: OperatingSystem,
    architecture: Architecture,
}

impl HostFacts {
    fn current() -> Self {
        let os = match std::env::consts::OS {
            "linux" => OperatingSystem::Linux,
            "macos" => OperatingSystem::Macos,
            "windows" => OperatingSystem::Windows,
            _ => OperatingSystem::Unknown,
        };
        let architecture = match std::env::consts::ARCH {
            "x86_64" => Architecture::X86_64,
            "aarch64" => Architecture::Aarch64,
            _ => Architecture::Other,
        };
        Self { os, architecture }
    }

    fn platform(self) -> PlatformDiagnostic {
        let target = match (self.os, self.architecture) {
            (OperatingSystem::Linux, Architecture::X86_64) => Some("linux-x86_64"),
            (OperatingSystem::Linux, Architecture::Aarch64) => Some("linux-aarch64"),
            (OperatingSystem::Macos, Architecture::X86_64) => Some("macos-x86_64"),
            (OperatingSystem::Macos, Architecture::Aarch64) => Some("macos-aarch64"),
            (OperatingSystem::Windows, Architecture::X86_64) => Some("windows-x86_64"),
            (OperatingSystem::Windows, Architecture::Aarch64) => Some("windows-aarch64"),
            _ => None,
        };
        let (support_status, experimental, parity) = match (self.os, self.architecture) {
            (OperatingSystem::Linux, Architecture::X86_64) => (
                SupportStatus::Supported,
                false,
                UnixSandboxParity::Available,
            ),
            (OperatingSystem::Linux, Architecture::Aarch64) => (
                SupportStatus::Experimental,
                true,
                UnixSandboxParity::Available,
            ),
            (OperatingSystem::Macos, Architecture::X86_64) => (
                SupportStatus::Supported,
                false,
                UnixSandboxParity::Available,
            ),
            (OperatingSystem::Macos, Architecture::Aarch64) => {
                (SupportStatus::CiTarget, false, UnixSandboxParity::Available)
            }
            (OperatingSystem::Windows, Architecture::X86_64 | Architecture::Aarch64) => (
                SupportStatus::Experimental,
                true,
                UnixSandboxParity::NotApplicable,
            ),
            (OperatingSystem::Unknown, _) => {
                (SupportStatus::Unknown, false, UnixSandboxParity::Unknown)
            }
            _ => (
                SupportStatus::Unsupported,
                false,
                if self.os == OperatingSystem::Windows {
                    UnixSandboxParity::NotApplicable
                } else {
                    UnixSandboxParity::Unknown
                },
            ),
        };
        PlatformDiagnostic {
            operating_system: self.os,
            architecture: self.architecture,
            host_target: target.map(str::to_owned),
            support_status,
            experimental,
            unix_sandbox_parity: parity,
        }
    }

    fn sandbox_requirement(self) -> Requirement {
        if self.os == OperatingSystem::Windows {
            Requirement::NotApplicable
        } else {
            Requirement::Required
        }
    }

    fn is_known_target(self) -> bool {
        self.platform().host_target.is_some()
    }
}

#[derive(Clone, Debug)]
struct DoctorInput {
    host: HostFacts,
    current_exe: Option<PathBuf>,
    codex_source: CodexSource,
    codex_path: Option<PathBuf>,
    path_environment: Option<OsString>,
    bubblewrap_found: Option<bool>,
    bubblewrap_version: Option<String>,
    macos_sandbox_exec: Option<(Option<bool>, Option<bool>)>,
    probe_bubblewrap_requested: bool,
    probe_codex_requested: bool,
}

fn process_input(options: DoctorOptions) -> DoctorInput {
    let current_exe = std::env::current_exe().ok();
    let local_override = std::env::var_os("LOCAL_MCP_CODEX_CLI_PATH");
    let generic_override = std::env::var_os("CODEX_CLI_PATH");
    let path_environment = std::env::var_os("PATH");
    let codex_path = crate::fallback::codex_path();
    #[cfg(target_os = "macos")]
    let macos_bundle_selected =
        codex_path == Path::new("/Applications/ChatGPT.app/Contents/Resources/codex");
    #[cfg(not(target_os = "macos"))]
    let macos_bundle_selected = false;
    let codex_source = choose_codex_source(
        local_override.is_some(),
        generic_override.is_some(),
        macos_bundle_selected,
    );
    #[cfg(target_os = "linux")]
    let bubblewrap_found = path_environment
        .as_deref()
        .map(|path| crate::bubblewrap_support::resolve_bubblewrap(path).is_some());
    #[cfg(not(target_os = "linux"))]
    let bubblewrap_found = None;
    #[cfg(target_os = "macos")]
    let macos_sandbox_exec = {
        let path = Path::new(codex_sandboxing::seatbelt::MACOS_PATH_TO_SEATBELT_EXECUTABLE);
        Some(file_executable_observation(path))
    };
    #[cfg(not(target_os = "macos"))]
    let macos_sandbox_exec = None;
    DoctorInput {
        host: HostFacts::current(),
        current_exe,
        codex_source,
        codex_path: Some(codex_path),
        path_environment,
        bubblewrap_found,
        bubblewrap_version: None,
        macos_sandbox_exec,
        probe_bubblewrap_requested: options.probe_bubblewrap_version,
        probe_codex_requested: options.probe_codex_version,
    }
}

fn choose_codex_source(
    local_override_present: bool,
    generic_override_present: bool,
    macos_bundle_present: bool,
) -> CodexSource {
    if local_override_present {
        CodexSource::LocalMcpCodexCliPath
    } else if generic_override_present {
        CodexSource::CodexCliPath
    } else if macos_bundle_present {
        CodexSource::MacosChatgptBundle
    } else {
        CodexSource::Path
    }
}

pub(crate) fn execute(options: DoctorOptions) -> u8 {
    let host = HostFacts::current();
    let report = match run_with_deadline(STATIC_CHECK_TIMEOUT, move || {
        build_report(process_input(options))
    }) {
        Ok(report) => report,
        Err(DeadlineFailure::TimedOut) => timeout_report(host, options),
        Err(DeadlineFailure::WorkerUnavailable | DeadlineFailure::WorkerFailed) => {
            return internal_error();
        }
    };
    let output = if options.json {
        match json_output(&report) {
            Ok(output) => output,
            Err(()) => return internal_error(),
        }
    } else {
        match human_output(&report) {
            Ok(output) => output.into_bytes(),
            Err(()) => return internal_error(),
        }
    };
    if io::stdout().lock().write_all(&output).is_err() {
        return internal_error();
    }
    report.summary.exit_code.value()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeadlineFailure {
    TimedOut,
    WorkerUnavailable,
    WorkerFailed,
}

fn run_with_deadline<T>(
    timeout: Duration,
    task: impl FnOnce() -> T + Send + 'static,
) -> Result<T, DeadlineFailure>
where
    T: Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("doctor-static-checks".to_owned())
        .spawn(move || {
            let result = task();
            let _ = sender.send(result);
        })
        .map_err(|_| DeadlineFailure::WorkerUnavailable)?;
    match receiver.recv_timeout(timeout) {
        Ok(result) => Ok(result),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(DeadlineFailure::TimedOut),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(DeadlineFailure::WorkerFailed),
    }
}

fn timeout_report(host: HostFacts, options: DoctorOptions) -> DoctorReport {
    let platform = host.platform();
    let known_target = host.is_known_target();
    let mut checks = Vec::new();
    let input = DoctorInput {
        host,
        current_exe: None,
        codex_source: CodexSource::Unknown,
        codex_path: None,
        path_environment: None,
        bubblewrap_found: None,
        bubblewrap_version: None,
        macos_sandbox_exec: None,
        probe_bubblewrap_requested: options.probe_bubblewrap_version,
        probe_codex_requested: options.probe_codex_version,
    };
    add_platform_checks(&input, &platform, &mut checks);

    let current_exe = CurrentExe {
        observed: false,
        path_redacted: true,
        basename_matches_expected: None,
        directory_observed: false,
        resolution: CurrentExeResolution::Unknown,
    };
    checks.push(check(
        "installation.current_exe",
        DiagnosticStatus::Unknown,
        Requirement::Required,
        Applicability::Applicable,
        EvidenceType::NotRun,
        SafeObserved::none(),
        ReasonCode::DoctorStaticCheckTimeout,
        "Static installation checks exceeded the diagnostic deadline.",
        Some(CapabilityId::AtomicPublication),
    ));
    let (required_names, forbidden_names) = helper_contract(host.os);
    let mut required_helpers = Vec::new();
    for (name, capability) in required_names {
        required_helpers.push(unknown_helper(name, true));
        checks.push(check(
            format!("installation.helper.{}", helper_id(name)),
            DiagnosticStatus::Unknown,
            Requirement::Required,
            Applicability::Applicable,
            EvidenceType::NotRun,
            SafeObserved::none(),
            ReasonCode::DoctorStaticCheckTimeout,
            "The required sibling helper could not be inspected before the diagnostic deadline.",
            Some(capability),
        ));
    }
    if required_helpers.is_empty() {
        required_helpers.push(unknown_helper(
            crate::install_contract::ATOMIC_PUBLISH,
            true,
        ));
        checks.push(check(
            "installation.helper.atomic_publish",
            DiagnosticStatus::Unknown,
            Requirement::Required,
            Applicability::Applicable,
            EvidenceType::NotRun,
            SafeObserved::none(),
            ReasonCode::DoctorStaticCheckTimeout,
            "Required helper naming and installation status were not established before the deadline.",
            Some(CapabilityId::AtomicPublication),
        ));
    }

    let mut forbidden_helpers = Vec::new();
    for name in forbidden_names {
        forbidden_helpers.push(unknown_helper(name, false));
        checks.push(check(
            format!("installation.forbidden_helper.{}", helper_id(name)),
            DiagnosticStatus::Unknown,
            Requirement::Required,
            Applicability::Applicable,
            EvidenceType::NotRun,
            SafeObserved::none(),
            ReasonCode::DoctorStaticCheckTimeout,
            "The platform-forbidden sibling helper could not be inspected before the deadline.",
            None,
        ));
    }
    let installation = Installation {
        install_contract_version: 1,
        helper_resolution: "current_exe_sibling",
        current_exe,
        required_helpers,
        forbidden_helpers,
        symlink_status: DiagnosticStatus::Unknown,
        nix_validation_status: DiagnosticStatus::Unknown,
    };

    add_core_contract_checks(known_target, &mut checks);
    add_timeout_sandbox_checks(host.os, &mut checks);
    checks.push(check(
        "goal_model.codex_cli",
        DiagnosticStatus::Unknown,
        Requirement::Optional,
        Applicability::Applicable,
        EvidenceType::NotRun,
        SafeObserved::none(),
        ReasonCode::DoctorStaticCheckTimeout,
        "Codex CLI resolution did not complete before the diagnostic deadline.",
        Some(CapabilityId::GoalModelExecution),
    ));
    checks.push(check(
        "goal_model.execution_not_run",
        DiagnosticStatus::Unknown,
        Requirement::Optional,
        Applicability::Applicable,
        EvidenceType::NotRun,
        SafeObserved::none(),
        ReasonCode::RuntimeVerificationNotRun,
        "Authentication and model execution were not tested.",
        Some(CapabilityId::GoalModelExecution),
    ));
    let codex_probe = VersionProbe {
        requested: options.probe_codex_version,
        attempted: false,
        status: DiagnosticStatus::Unknown,
        version: None,
        timeout_seconds: 2,
        reason_code: if options.probe_codex_version {
            ReasonCode::CodexCliVersionProbeRefused
        } else {
            ReasonCode::ProbeNotRun
        },
    };
    checks.push(check(
        "goal_model.version_probe",
        DiagnosticStatus::Unknown,
        Requirement::Optional,
        Applicability::Applicable,
        EvidenceType::NotRun,
        SafeObserved::none(),
        codex_probe.reason_code,
        if options.probe_codex_version {
            "The opted-in Codex probe was refused before process spawn because containment is unverified."
        } else {
            "The Codex version probe was not requested and no process was started."
        },
        Some(CapabilityId::GoalModelExecution),
    ));
    add_advisory_install_checks(&mut checks);

    let bubblewrap_applicable = host.os == OperatingSystem::Linux;
    let bubblewrap = Bubblewrap {
        applicable: bubblewrap_applicable,
        found: None,
        version: None,
        minimum_version: "0.12.0",
        version_probe: VersionProbe {
            requested: options.probe_bubblewrap_version,
            attempted: false,
            status: DiagnosticStatus::Unknown,
            version: None,
            timeout_seconds: 5,
            reason_code: if bubblewrap_applicable {
                ReasonCode::BwrapVersionUnknown
            } else {
                ReasonCode::CheckNotApplicable
            },
        },
    };
    let macos_sandbox_exec = MacosSandboxExec {
        applicable: host.os == OperatingSystem::Macos,
        present: None,
        executable_status: DiagnosticStatus::Unknown,
        runtime_status: DiagnosticStatus::Unknown,
    };
    let codex_cli = CodexCli {
        resolution_source: CodexSource::Unknown,
        selected: false,
        path_redacted: true,
        file_status: CodexFileStatus::Unknown,
        executable_status: DiagnosticStatus::Unknown,
        version_probe: codex_probe,
    };
    let capabilities = build_capabilities(&checks, host.sandbox_requirement());
    let summary = summarize_checks(&checks);
    DoctorReport {
        schema_version: "1.0.0",
        diagnostic_contract_version: 1,
        doctor_version: DOCTOR_VERSION,
        goallatch_version: env!("CARGO_PKG_VERSION"),
        platform,
        installation,
        dependencies: Dependencies {
            codex_cli,
            bubblewrap,
            macos_sandbox_exec,
        },
        capabilities,
        checks,
        summary,
    }
}

fn add_timeout_sandbox_checks(os: OperatingSystem, checks: &mut Vec<DiagnosticCheck>) {
    match os {
        OperatingSystem::Linux => {
            checks.push(check(
                "sandbox.bubblewrap.presence",
                DiagnosticStatus::Unknown,
                Requirement::Required,
                Applicability::Applicable,
                EvidenceType::NotRun,
                SafeObserved::none(),
                ReasonCode::DoctorStaticCheckTimeout,
                "Bubblewrap presence could not be observed before the diagnostic deadline.",
                Some(CapabilityId::SandboxedExecution),
            ));
            checks.push(check(
                "sandbox.bubblewrap.minimum_version",
                DiagnosticStatus::Unknown,
                Requirement::Required,
                Applicability::Applicable,
                EvidenceType::NotRun,
                SafeObserved::none(),
                ReasonCode::BwrapVersionUnknown,
                "Bubblewrap version was not established; no process was started.",
                Some(CapabilityId::SandboxedExecution),
            ));
            checks.push(check(
                "sandbox.runtime_prerequisites",
                DiagnosticStatus::Unknown,
                Requirement::Required,
                Applicability::Applicable,
                EvidenceType::NotRun,
                SafeObserved::none(),
                ReasonCode::BwrapRuntimePrerequisitesUnknown,
                "Namespace, kernel, and seccomp prerequisites were not tested.",
                Some(CapabilityId::SandboxedExecution),
            ));
        }
        OperatingSystem::Macos => {
            checks.push(check(
                "sandbox.macos.sandbox_exec",
                DiagnosticStatus::Unknown,
                Requirement::Required,
                Applicability::Applicable,
                EvidenceType::NotRun,
                SafeObserved::none(),
                ReasonCode::DoctorStaticCheckTimeout,
                "The Seatbelt executable could not be inspected before the diagnostic deadline.",
                Some(CapabilityId::SandboxedExecution),
            ));
            checks.push(check(
                "sandbox.macos.runtime",
                DiagnosticStatus::Unknown,
                Requirement::Required,
                Applicability::Applicable,
                EvidenceType::NotRun,
                SafeObserved::none(),
                ReasonCode::RuntimeVerificationNotRun,
                "Seatbelt runtime behavior was not exercised.",
                Some(CapabilityId::SandboxedExecution),
            ));
        }
        OperatingSystem::Windows => checks.push(check(
            "sandbox.parity",
            DiagnosticStatus::Unknown,
            Requirement::NotApplicable,
            Applicability::NotApplicable,
            EvidenceType::NotRun,
            SafeObserved::none(),
            ReasonCode::SandboxParityNotApplicable,
            "Windows host-native execution is not Unix-equivalent process sandboxing.",
            Some(CapabilityId::SandboxedExecution),
        )),
        OperatingSystem::Unknown => checks.push(check(
            "sandbox.platform_unknown",
            DiagnosticStatus::Unknown,
            Requirement::Required,
            Applicability::Applicable,
            EvidenceType::NotRun,
            SafeObserved::none(),
            ReasonCode::HostTargetUnknown,
            "Sandbox requirements are unknown for this operating system.",
            Some(CapabilityId::SandboxedExecution),
        )),
    }
}

fn internal_error() -> u8 {
    let _ = writeln!(
        io::stderr().lock(),
        "Doctor could not produce a bounded report."
    );
    ExitCode::InternalError.value()
}

fn json_output(report: &DoctorReport) -> Result<Vec<u8>, ()> {
    report.validate()?;
    let mut output = serde_json::to_vec(report).map_err(|_| ())?;
    output.push(b'\n');
    if output.len() > JSON_OUTPUT_LIMIT {
        return Err(());
    }
    Ok(output)
}

fn human_output(report: &DoctorReport) -> Result<String, ()> {
    report.validate()?;
    let mut output = String::new();
    use std::fmt::Write as FmtWrite;
    writeln!(output, "GoalLatch Doctor Core V1").map_err(|_| ())?;
    writeln!(
        output,
        "Platform: {} {} ({}, target {}; experimental: {})",
        operating_system_label(report.platform.operating_system),
        architecture_label(report.platform.architecture),
        support_label(report.platform.support_status),
        report.platform.host_target.as_deref().unwrap_or("unknown"),
        report.platform.experimental
    )
    .map_err(|_| ())?;
    writeln!(output, "GoalLatch version: {}", report.goallatch_version).map_err(|_| ())?;
    writeln!(
        output,
        "Install Contract version: {}",
        report.installation.install_contract_version
    )
    .map_err(|_| ())?;
    writeln!(output, "Capabilities:").map_err(|_| ())?;
    for (id, capability) in [
        (
            CapabilityId::McpTransport,
            &report.capabilities.mcp_transport,
        ),
        (
            CapabilityId::SessionManagement,
            &report.capabilities.session_management,
        ),
        (
            CapabilityId::SandboxedExecution,
            &report.capabilities.sandboxed_execution,
        ),
        (
            CapabilityId::AtomicPublication,
            &report.capabilities.atomic_publication,
        ),
        (
            CapabilityId::GoalModelExecution,
            &report.capabilities.goal_model_execution,
        ),
    ] {
        let reasons = capability
            .reason_codes
            .iter()
            .map(|reason| reason.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            output,
            "  {}: {} [{}; formal {}; runtime {}] ({})",
            id.as_str(),
            status_label(capability.status),
            requirement_label(capability.requirement),
            status_label(capability.formal_status),
            status_label(capability.runtime_status),
            reasons
        )
        .map_err(|_| ())?;
    }
    writeln!(output, "Checks:").map_err(|_| ())?;
    for check in &report.checks {
        writeln!(
            output,
            "  {}: {} [{} / {}] {} — {}",
            check.id,
            status_label(check.status),
            requirement_label(check.requirement),
            evidence_label(check.evidence_type),
            check.reason_code.as_str(),
            check.message
        )
        .map_err(|_| ())?;
    }
    writeln!(
        output,
        "Summary: {} (required {}; advisory {}; runtime {})",
        status_label(report.summary.status),
        status_label(report.summary.required_status),
        status_label(report.summary.advisory_status),
        status_label(report.summary.runtime_status)
    )
    .map_err(|_| ())?;
    writeln!(
        output,
        "Counts: PASS {} WARN {} BLOCK {} UNKNOWN {}",
        report.summary.counts.pass,
        report.summary.counts.warn,
        report.summary.counts.block,
        report.summary.counts.unknown
    )
    .map_err(|_| ())?;
    writeln!(output, "Exit code: {}", report.summary.exit_code.value()).map_err(|_| ())?;
    if output.len() > HUMAN_OUTPUT_LIMIT {
        return Err(());
    }
    Ok(output)
}

fn operating_system_label(value: OperatingSystem) -> &'static str {
    match value {
        OperatingSystem::Linux => "Linux",
        OperatingSystem::Macos => "macOS",
        OperatingSystem::Windows => "Windows",
        OperatingSystem::Unknown => "unknown OS",
    }
}

fn architecture_label(value: Architecture) -> &'static str {
    match value {
        Architecture::X86_64 => "x86_64",
        Architecture::Aarch64 => "aarch64",
        Architecture::Other => "other architecture",
    }
}

fn support_label(value: SupportStatus) -> &'static str {
    match value {
        SupportStatus::Supported => "supported",
        SupportStatus::Experimental => "experimental",
        SupportStatus::CiTarget => "CI target",
        SupportStatus::Unsupported => "unsupported",
        SupportStatus::Unknown => "unknown",
    }
}

fn status_label(value: DiagnosticStatus) -> &'static str {
    match value {
        DiagnosticStatus::Pass => "PASS",
        DiagnosticStatus::Warn => "WARN",
        DiagnosticStatus::Block => "BLOCK",
        DiagnosticStatus::Unknown => "UNKNOWN",
    }
}

fn requirement_label(value: Requirement) -> &'static str {
    match value {
        Requirement::Required => "required",
        Requirement::Optional => "optional",
        Requirement::NotApplicable => "not_applicable",
    }
}

fn evidence_label(value: EvidenceType) -> &'static str {
    match value {
        EvidenceType::StaticObservation => "static",
        EvidenceType::FormalContract => "formal",
        EvidenceType::RuntimeReadOnly => "runtime",
        EvidenceType::NotRun => "not_run",
    }
}

fn build_report(input: DoctorInput) -> DoctorReport {
    let platform = input.host.platform();
    let known_target = input.host.is_known_target();
    let mut checks = Vec::new();
    add_platform_checks(&input, &platform, &mut checks);
    let (installation, required_helper_checks) = installation_report(&input, known_target);
    checks.extend(required_helper_checks);
    add_core_contract_checks(known_target, &mut checks);
    add_sandbox_checks(&input, &mut checks);
    let (codex, codex_checks) = codex_report(&input);
    checks.extend(codex_checks);
    add_advisory_install_checks(&mut checks);
    let (bubblewrap, macos_sandbox_exec) = dependency_reports(&input);
    let capabilities = build_capabilities(&checks, input.host.sandbox_requirement());
    let summary = summarize_checks(&checks);
    DoctorReport {
        schema_version: "1.0.0",
        diagnostic_contract_version: 1,
        doctor_version: DOCTOR_VERSION,
        goallatch_version: env!("CARGO_PKG_VERSION"),
        platform,
        installation,
        dependencies: Dependencies {
            codex_cli: codex,
            bubblewrap,
            macos_sandbox_exec,
        },
        capabilities,
        checks,
        summary,
    }
}

fn add_platform_checks(
    input: &DoctorInput,
    platform: &PlatformDiagnostic,
    checks: &mut Vec<DiagnosticCheck>,
) {
    let (status, reason_code, message) = match platform.support_status {
        SupportStatus::Supported => (
            DiagnosticStatus::Pass,
            ReasonCode::CapabilityAvailable,
            "This host target is in the supported install matrix.",
        ),
        SupportStatus::Experimental => match input.host {
            HostFacts {
                os: OperatingSystem::Windows,
                ..
            } => (
                DiagnosticStatus::Warn,
                ReasonCode::PlatformWindowsExperimental,
                "Windows is Experimental and has no Unix-equivalent process sandbox.",
            ),
            HostFacts {
                os: OperatingSystem::Linux,
                architecture: Architecture::Aarch64,
            } => (
                DiagnosticStatus::Warn,
                ReasonCode::PlatformLinuxAarch64Unverified,
                "Linux aarch64 sandbox closure is not validated.",
            ),
            _ => (
                DiagnosticStatus::Warn,
                ReasonCode::HostTargetUnsupported,
                "This host target is experimental; platform parity is not claimed.",
            ),
        },
        SupportStatus::CiTarget => (
            DiagnosticStatus::Warn,
            ReasonCode::PlatformMacosAarch64CiOnly,
            "macOS aarch64 is a CI target; local release closure is not established.",
        ),
        SupportStatus::Unsupported => (
            DiagnosticStatus::Unknown,
            ReasonCode::HostTargetUnsupported,
            "This operating system or architecture is outside the verified install matrix.",
        ),
        SupportStatus::Unknown => (
            DiagnosticStatus::Unknown,
            ReasonCode::HostTargetUnknown,
            "The host operating system or architecture could not be established.",
        ),
    };
    checks.push(check(
        "platform.support",
        status,
        Requirement::Required,
        Applicability::Applicable,
        EvidenceType::FormalContract,
        SafeObserved::none(),
        reason_code,
        message,
        None,
    ));
}

fn installation_report(
    input: &DoctorInput,
    known_target: bool,
) -> (Installation, Vec<DiagnosticCheck>) {
    let exe = input.current_exe.as_deref();
    let directory = exe.and_then(Path::parent);
    let expected_name = match input.host.os {
        OperatingSystem::Windows => "local-mcp.exe",
        _ => "local-mcp",
    };
    let basename_matches_expected = exe.and_then(Path::file_name).map(|name| {
        if input.host.os == OperatingSystem::Windows {
            name.to_string_lossy().eq_ignore_ascii_case(expected_name)
        } else {
            name == OsStr::new(expected_name)
        }
    });
    let current_exe = CurrentExe {
        observed: exe.is_some(),
        path_redacted: true,
        basename_matches_expected,
        directory_observed: directory.is_some(),
        resolution: match (exe, directory) {
            (Some(_), Some(_)) => CurrentExeResolution::AsObserved,
            (None, _) => CurrentExeResolution::Unavailable,
            _ => CurrentExeResolution::Unknown,
        },
    };
    let current_status = if directory.is_some() && known_target {
        DiagnosticStatus::Pass
    } else {
        DiagnosticStatus::Unknown
    };
    let current_reason = if current_status == DiagnosticStatus::Pass {
        ReasonCode::CapabilityAvailable
    } else {
        ReasonCode::InstallCurrentExeUnavailable
    };
    let current_message = if current_status == DiagnosticStatus::Pass {
        "The running executable and its sibling directory were observed; the path is redacted."
    } else {
        "The running executable or sibling directory could not be safely observed."
    };
    let mut checks = vec![check(
        "installation.current_exe",
        current_status,
        Requirement::Required,
        Applicability::Applicable,
        EvidenceType::StaticObservation,
        SafeObserved::boolean(current_exe.observed && current_exe.directory_observed),
        current_reason,
        current_message,
        Some(CapabilityId::AtomicPublication),
    )];

    let (required_names, forbidden_names) = helper_contract(input.host.os);
    let mut required_helpers = Vec::new();
    for (name, capability) in required_names {
        let (helper, status, reason, message) =
            inspect_helper(exe, name, true, known_target, input.host.os);
        checks.push(check(
            format!("installation.helper.{}", helper_id(name)),
            status,
            Requirement::Required,
            Applicability::Applicable,
            EvidenceType::StaticObservation,
            helper
                .present
                .map(SafeObserved::boolean)
                .unwrap_or_else(SafeObserved::none),
            reason,
            message,
            Some(capability),
        ));
        required_helpers.push(helper);
    }
    if required_helpers.is_empty() {
        let helper = unknown_helper("atomic-publish", true);
        checks.push(check(
            "installation.helper.atomic_publish".to_owned(),
            DiagnosticStatus::Unknown,
            Requirement::Required,
            Applicability::Applicable,
            EvidenceType::StaticObservation,
            SafeObserved::none(),
            ReasonCode::HostTargetUnsupported,
            "Required helper naming is unknown for this host target.",
            Some(CapabilityId::AtomicPublication),
        ));
        required_helpers.push(helper);
    }

    let mut forbidden_helpers = Vec::new();
    for name in forbidden_names {
        let (helper, status, reason, message) = inspect_forbidden_helper(exe, name);
        checks.push(check(
            format!("installation.forbidden_helper.{}", helper_id(name)),
            status,
            Requirement::Required,
            Applicability::Applicable,
            EvidenceType::StaticObservation,
            helper
                .present
                .map(SafeObserved::boolean)
                .unwrap_or_else(SafeObserved::none),
            reason,
            message,
            None,
        ));
        forbidden_helpers.push(helper);
    }
    (
        Installation {
            install_contract_version: 1,
            helper_resolution: "current_exe_sibling",
            current_exe,
            required_helpers,
            forbidden_helpers,
            symlink_status: DiagnosticStatus::Unknown,
            nix_validation_status: DiagnosticStatus::Unknown,
        },
        checks,
    )
}

fn helper_contract(os: OperatingSystem) -> (Vec<(&'static str, CapabilityId)>, Vec<&'static str>) {
    let atomic_name = if os == OperatingSystem::Windows {
        "atomic-publish.exe"
    } else {
        crate::install_contract::ATOMIC_PUBLISH
    };
    match os {
        OperatingSystem::Linux => (
            vec![
                (atomic_name, CapabilityId::AtomicPublication),
                (
                    crate::install_contract::LINUX_SANDBOX,
                    CapabilityId::SandboxedExecution,
                ),
            ],
            vec![],
        ),
        OperatingSystem::Macos => (
            vec![(atomic_name, CapabilityId::AtomicPublication)],
            vec![crate::install_contract::LINUX_SANDBOX],
        ),
        OperatingSystem::Windows => (
            vec![(atomic_name, CapabilityId::AtomicPublication)],
            vec![
                crate::install_contract::LINUX_SANDBOX,
                "codex-linux-sandbox.exe",
            ],
        ),
        OperatingSystem::Unknown => (vec![], vec![]),
    }
}

fn inspect_helper(
    executable: Option<&Path>,
    name: &'static str,
    required: bool,
    known_target: bool,
    os: OperatingSystem,
) -> (Helper, DiagnosticStatus, ReasonCode, &'static str) {
    let candidates = executable
        .map(|path| crate::install_contract::sibling_candidates(path, name))
        .unwrap_or_default();
    let candidate = candidates.first().map(PathBuf::as_path);
    let (present, regular_file, readable, executable_status, resolution) = match candidate {
        Some(path) => {
            let (present, regular) = observe_regular_file(path);
            let readable =
                regular.and_then(|is_regular| is_regular.then(|| readable(path)).flatten());
            let executable = regular.and_then(|is_regular| {
                is_regular
                    .then(|| executable_permission(path, os))
                    .flatten()
            });
            (
                present,
                regular,
                readable,
                executable,
                HelperResolution::CurrentExeSibling,
            )
        }
        None => (None, None, None, None, HelperResolution::Unknown),
    };
    let helper = Helper {
        name: name.to_owned(),
        required,
        present,
        regular_file,
        readable,
        executable: executable_status,
        resolution,
        status: helper_status(
            known_target,
            present,
            regular_file,
            readable,
            executable_status,
        ),
    };
    let (reason, message) = helper_reason(&helper);
    (helper.clone(), helper.status, reason, message)
}

fn inspect_forbidden_helper(
    executable: Option<&Path>,
    name: &'static str,
) -> (Helper, DiagnosticStatus, ReasonCode, &'static str) {
    let candidate = executable.and_then(|path| {
        crate::install_contract::sibling_candidates(path, name)
            .into_iter()
            .next()
    });
    let present = candidate.as_deref().and_then(path_entry_present);
    let status = match present {
        Some(true) => DiagnosticStatus::Block,
        Some(false) => DiagnosticStatus::Pass,
        None => DiagnosticStatus::Unknown,
    };
    let resolution = if candidate.is_some() {
        HelperResolution::CurrentExeSibling
    } else {
        HelperResolution::Unknown
    };
    let helper = Helper {
        name: name.to_owned(),
        required: false,
        present,
        regular_file: candidate.as_deref().and_then(path_regular_file),
        readable: None,
        executable: None,
        resolution,
        status,
    };
    let (reason, message) = match status {
        DiagnosticStatus::Block => (
            ReasonCode::InstallForbiddenHelperPresent,
            "A platform-forbidden helper entry is beside the observed executable.",
        ),
        DiagnosticStatus::Pass => (
            ReasonCode::CapabilityAvailable,
            "No platform-forbidden helper entry is beside the observed executable.",
        ),
        _ => (
            ReasonCode::InstallCurrentExeUnavailable,
            "The forbidden-helper sibling candidate could not be observed.",
        ),
    };
    (helper, status, reason, message)
}

fn helper_status(
    known_target: bool,
    present: Option<bool>,
    regular: Option<bool>,
    readable: Option<bool>,
    executable: Option<bool>,
) -> DiagnosticStatus {
    if !known_target {
        return DiagnosticStatus::Unknown;
    }
    if present == Some(false)
        || regular == Some(false)
        || readable == Some(false)
        || executable == Some(false)
    {
        DiagnosticStatus::Block
    } else if present == Some(true)
        && regular == Some(true)
        && readable == Some(true)
        && executable == Some(true)
    {
        DiagnosticStatus::Pass
    } else {
        DiagnosticStatus::Unknown
    }
}

fn helper_reason(helper: &Helper) -> (ReasonCode, &'static str) {
    match helper.status {
        DiagnosticStatus::Pass => (
            ReasonCode::CapabilityAvailable,
            "The required regular sibling helper has static execute permission; it was not launched.",
        ),
        DiagnosticStatus::Block if helper.present == Some(false) => (
            ReasonCode::InstallRequiredHelperMissing,
            "The required helper is missing from the executable sibling directory; PATH is not used.",
        ),
        DiagnosticStatus::Block if helper.regular_file == Some(false) => (
            ReasonCode::InstallRequiredHelperNotRegularFile,
            "The sibling helper entry is not a regular file.",
        ),
        DiagnosticStatus::Block if helper.readable == Some(false) => (
            ReasonCode::InstallRequiredHelperUnreadable,
            "The required sibling helper is not readable by this process.",
        ),
        DiagnosticStatus::Block => (
            ReasonCode::InstallRequiredHelperNotExecutable,
            "The required sibling helper is not executable by this process.",
        ),
        _ => (
            ReasonCode::InstallCurrentExeUnavailable,
            "The required sibling helper could not be safely inspected.",
        ),
    }
}

fn unknown_helper(name: &'static str, required: bool) -> Helper {
    Helper {
        name: name.to_owned(),
        required,
        present: None,
        regular_file: None,
        readable: None,
        executable: None,
        resolution: HelperResolution::Unknown,
        status: DiagnosticStatus::Unknown,
    }
}

fn observe_regular_file(path: &Path) -> (Option<bool>, Option<bool>) {
    match fs::symlink_metadata(path) {
        Ok(entry) => {
            let regular = crate::install_contract::is_plain_file(&entry);
            (Some(true), Some(regular))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (Some(false), None),
        Err(_) => (None, None),
    }
}

fn path_entry_present(path: &Path) -> Option<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Some(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

fn path_regular_file(path: &Path) -> Option<bool> {
    fs::symlink_metadata(path)
        .ok()
        .map(|metadata| metadata.file_type().is_file())
}

#[cfg(unix)]
fn readable(path: &Path) -> Option<bool> {
    unix_access_permission(path, libc::R_OK)
}

#[cfg(windows)]
fn readable(path: &Path) -> Option<bool> {
    match std::fs::File::open(path) {
        Ok(_) => Some(true),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Some(false),
        Err(_) => None,
    }
}

#[cfg(not(any(unix, windows)))]
fn readable(_path: &Path) -> Option<bool> {
    None
}

fn executable_permission(path: &Path, os: OperatingSystem) -> Option<bool> {
    match os {
        OperatingSystem::Windows => windows_executable_permission(path),
        OperatingSystem::Linux | OperatingSystem::Macos => unix_executable_permission(path),
        OperatingSystem::Unknown => None,
    }
}

#[cfg(unix)]
fn unix_executable_permission(path: &Path) -> Option<bool> {
    unix_access_permission(path, libc::X_OK)
}

#[cfg(unix)]
fn unix_access_permission(path: &Path, mode: libc::c_int) -> Option<bool> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let (real_uid, effective_uid, real_gid, effective_gid) = unsafe {
        (
            libc::getuid(),
            libc::geteuid(),
            libc::getgid(),
            libc::getegid(),
        )
    };
    // `access(2)` uses real IDs; different effective credentials make the result ambiguous.
    if real_uid != effective_uid || real_gid != effective_gid {
        return None;
    }
    if unsafe { libc::access(path.as_ptr(), mode) } == 0 {
        Some(true)
    } else if io::Error::last_os_error().kind() == io::ErrorKind::PermissionDenied {
        Some(false)
    } else {
        None
    }
}

#[cfg(not(unix))]
fn unix_executable_permission(_path: &Path) -> Option<bool> {
    None
}

#[cfg(windows)]
fn windows_executable_permission(path: &Path) -> Option<bool> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_EXECUTE: u32 = 0x0020;
    const FILE_READ_ATTRIBUTES: u32 = 0x0080;
    match std::fs::OpenOptions::new()
        .access_mode(FILE_EXECUTE | FILE_READ_ATTRIBUTES)
        .open(path)
    {
        Ok(_) => Some(true),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Some(false),
        Err(_) => None,
    }
}

#[cfg(not(windows))]
fn windows_executable_permission(_path: &Path) -> Option<bool> {
    None
}

fn helper_id(name: &str) -> &'static str {
    match name {
        "atomic-publish" | "atomic-publish.exe" => "atomic_publish",
        "codex-linux-sandbox" => "codex_linux_sandbox",
        _ => "helper",
    }
}

fn add_core_contract_checks(known_target: bool, checks: &mut Vec<DiagnosticCheck>) {
    for (id, capability, message) in [
        (
            "capability.mcp_transport_contract",
            CapabilityId::McpTransport,
            "MCP transport is included in this build contract; no server was started.",
        ),
        (
            "capability.session_management_contract",
            CapabilityId::SessionManagement,
            "Session management is included in this build contract; no Session was created.",
        ),
    ] {
        let status = if known_target {
            DiagnosticStatus::Pass
        } else {
            DiagnosticStatus::Unknown
        };
        checks.push(check(
            id,
            status,
            Requirement::Required,
            Applicability::Applicable,
            EvidenceType::FormalContract,
            SafeObserved::none(),
            if known_target {
                ReasonCode::CapabilityAvailable
            } else {
                ReasonCode::HostTargetUnsupported
            },
            message,
            Some(capability),
        ));
    }
}

fn add_sandbox_checks(input: &DoctorInput, checks: &mut Vec<DiagnosticCheck>) {
    match input.host.os {
        OperatingSystem::Linux => {
            add_linux_sandbox_checks(input, checks);
        }
        OperatingSystem::Macos => {
            add_macos_sandbox_checks(input, checks);
        }
        OperatingSystem::Windows => {
            checks.push(check(
                "sandbox.parity",
                DiagnosticStatus::Unknown,
                Requirement::NotApplicable,
                Applicability::NotApplicable,
                EvidenceType::NotRun,
                SafeObserved::none(),
                ReasonCode::SandboxParityNotApplicable,
                "Windows host-native execution is not Unix-equivalent process sandboxing.",
                Some(CapabilityId::SandboxedExecution),
            ));
        }
        OperatingSystem::Unknown => {
            checks.push(check(
                "sandbox.platform_unknown",
                DiagnosticStatus::Unknown,
                Requirement::Required,
                Applicability::Applicable,
                EvidenceType::NotRun,
                SafeObserved::none(),
                ReasonCode::HostTargetUnknown,
                "Sandbox requirements are unknown for this operating system.",
                Some(CapabilityId::SandboxedExecution),
            ));
        }
    }
}

fn add_linux_sandbox_checks(input: &DoctorInput, checks: &mut Vec<DiagnosticCheck>) {
    let found = input.bubblewrap_found;
    let (version_status, version_reason, _version, evidence) = bubblewrap_version_result(
        found,
        input.bubblewrap_version.as_deref(),
        input.probe_bubblewrap_requested,
    );
    checks.push(check(
        "sandbox.bubblewrap.presence",
        match found {
            Some(true) => DiagnosticStatus::Pass,
            Some(false) => DiagnosticStatus::Block,
            None => DiagnosticStatus::Unknown,
        },
        Requirement::Required,
        Applicability::Applicable,
        EvidenceType::StaticObservation,
        found
            .map(SafeObserved::boolean)
            .unwrap_or_else(SafeObserved::none),
        match found {
            Some(true) => ReasonCode::CapabilityAvailable,
            Some(false) => ReasonCode::BwrapNotFound,
            None => ReasonCode::BwrapVersionUnknown,
        },
        if found == Some(true) {
            "Bubblewrap was found by the existing PATH resolver; its version is not inferred."
        } else {
            "Bubblewrap was not found by the existing PATH resolver."
        },
        Some(CapabilityId::SandboxedExecution),
    ));
    checks.push(check(
        "sandbox.bubblewrap.minimum_version",
        version_status,
        Requirement::Required,
        Applicability::Applicable,
        evidence,
        SafeObserved::none(),
        version_reason,
        "Bubblewrap version evidence is reported only after strict shared-gate parsing.",
        Some(CapabilityId::SandboxedExecution),
    ));
    checks.push(check(
        "sandbox.runtime_prerequisites",
        DiagnosticStatus::Unknown,
        Requirement::Required,
        Applicability::Applicable,
        EvidenceType::NotRun,
        SafeObserved::none(),
        ReasonCode::BwrapRuntimePrerequisitesUnknown,
        "Namespace, kernel, and seccomp prerequisites were not tested.",
        Some(CapabilityId::SandboxedExecution),
    ));
}

fn bubblewrap_version_result(
    found: Option<bool>,
    version: Option<&str>,
    requested: bool,
) -> (DiagnosticStatus, ReasonCode, Option<String>, EvidenceType) {
    match found {
        Some(false) => {
            return (
                DiagnosticStatus::Unknown,
                ReasonCode::BwrapVersionUnknown,
                None,
                EvidenceType::NotRun,
            );
        }
        None => {
            return (
                DiagnosticStatus::Unknown,
                ReasonCode::BwrapVersionUnknown,
                None,
                EvidenceType::NotRun,
            );
        }
        Some(true) => {}
    }
    let Some(version) = version else {
        return (
            DiagnosticStatus::Unknown,
            if requested {
                ReasonCode::BwrapVersionUnknown
            } else {
                ReasonCode::BwrapVersionUnprobed
            },
            None,
            EvidenceType::NotRun,
        );
    };
    if version.len() > 32 {
        return (
            DiagnosticStatus::Unknown,
            ReasonCode::BwrapVersionUnknown,
            None,
            EvidenceType::RuntimeReadOnly,
        );
    }
    #[cfg(target_os = "linux")]
    {
        match crate::bubblewrap_support::classify_version(version) {
            Ok(()) => (
                DiagnosticStatus::Pass,
                ReasonCode::CapabilityAvailable,
                Some(version.to_owned()),
                EvidenceType::RuntimeReadOnly,
            ),
            Err(crate::bubblewrap_support::BubblewrapVersionError::Unsupported) => (
                DiagnosticStatus::Block,
                ReasonCode::BwrapVersionUnsupported,
                Some(version.to_owned()),
                EvidenceType::RuntimeReadOnly,
            ),
            Err(crate::bubblewrap_support::BubblewrapVersionError::Malformed) => (
                DiagnosticStatus::Unknown,
                ReasonCode::BwrapVersionUnknown,
                None,
                EvidenceType::RuntimeReadOnly,
            ),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = version;
        (
            DiagnosticStatus::Unknown,
            ReasonCode::BwrapVersionUnknown,
            None,
            EvidenceType::NotRun,
        )
    }
}

fn add_macos_sandbox_checks(input: &DoctorInput, checks: &mut Vec<DiagnosticCheck>) {
    let (present, executable) = input.macos_sandbox_exec.unwrap_or((None, None));
    let (status, reason, message) = match (present, executable) {
        (Some(false), _) => (
            DiagnosticStatus::Block,
            ReasonCode::MacosSandboxExecNotFound,
            "The existing Seatbelt sandbox executable is absent or unusable.",
        ),
        (Some(true), Some(true)) => (
            DiagnosticStatus::Pass,
            ReasonCode::CapabilityAvailable,
            "The existing Seatbelt executable is statically present and has execute permission; it was not launched.",
        ),
        (Some(true), Some(false)) => (
            DiagnosticStatus::Block,
            ReasonCode::MacosSandboxExecNotFound,
            "The existing Seatbelt executable is not executable.",
        ),
        (Some(true), None) | (None, _) => (
            DiagnosticStatus::Unknown,
            ReasonCode::RuntimeVerificationNotRun,
            "Seatbelt executable status could not be established safely.",
        ),
    };
    checks.push(check(
        "sandbox.macos.sandbox_exec",
        status,
        Requirement::Required,
        Applicability::Applicable,
        EvidenceType::StaticObservation,
        present
            .map(SafeObserved::boolean)
            .unwrap_or_else(SafeObserved::none),
        reason,
        message,
        Some(CapabilityId::SandboxedExecution),
    ));
    checks.push(check(
        "sandbox.macos.runtime",
        DiagnosticStatus::Unknown,
        Requirement::Required,
        Applicability::Applicable,
        EvidenceType::NotRun,
        SafeObserved::none(),
        ReasonCode::RuntimeVerificationNotRun,
        "Seatbelt runtime behavior was not exercised.",
        Some(CapabilityId::SandboxedExecution),
    ));
}

#[cfg(target_os = "macos")]
fn file_executable_observation(path: &Path) -> (Option<bool>, Option<bool>) {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => (
            Some(true),
            executable_permission(path, OperatingSystem::Macos),
        ),
        Ok(_) => (Some(false), Some(false)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (Some(false), Some(false)),
        Err(_) => (None, None),
    }
}

fn codex_report(input: &DoctorInput) -> (CodexCli, Vec<DiagnosticCheck>) {
    let selected = input.codex_source != CodexSource::None && input.codex_path.is_some();
    let explicit = matches!(
        input.codex_source,
        CodexSource::LocalMcpCodexCliPath | CodexSource::CodexCliPath
    );
    let mut selected_path = input.codex_path.clone();
    let mut path_context_unknown = false;
    if explicit {
        if let Some(path) = selected_path.as_deref() {
            if path.as_os_str().is_empty() {
                selected_path = None;
            } else if !path.is_absolute() {
                path_context_unknown = true;
                selected_path = None;
            }
        }
    } else if input.codex_source == CodexSource::Path && input.host.os != OperatingSystem::Windows {
        selected_path = resolve_codex_on_path(
            selected_path.as_deref(),
            input.path_environment.as_deref(),
            input.host.os,
        );
    } else if input.codex_source == CodexSource::Path {
        selected_path = None;
    }
    let observation = selected_path.as_deref().map(observe_codex_path);
    let candidate_observed = observation.is_some();
    let (regular, can_read, can_execute) = observation.unwrap_or((None, None, None));
    let (file_status, exec_status, codex_status, reason, message) = if input.codex_source
        == CodexSource::None
    {
        (
            CodexFileStatus::Missing,
            DiagnosticStatus::Block,
            DiagnosticStatus::Block,
            ReasonCode::CodexCliNotFound,
            "No Codex CLI candidate was selected; Goal model execution is unavailable.",
        )
    } else if path_context_unknown {
        (
            CodexFileStatus::Unknown,
            DiagnosticStatus::Unknown,
            DiagnosticStatus::Unknown,
            ReasonCode::CodexCliPathContextUnknown,
            "The selected relative Codex override depends on the launcher context, which standalone Doctor cannot establish.",
        )
    } else if input.codex_source == CodexSource::Path && input.host.os == OperatingSystem::Windows {
        (
            CodexFileStatus::Unknown,
            DiagnosticStatus::Unknown,
            DiagnosticStatus::Unknown,
            ReasonCode::RuntimeVerificationNotRun,
            "Windows process resolution also checks executable-adjacent and system directories; Doctor does not infer the selected candidate from PATH alone.",
        )
    } else if !candidate_observed {
        if explicit {
            (
                CodexFileStatus::NotRegularFile,
                DiagnosticStatus::Block,
                DiagnosticStatus::Block,
                ReasonCode::CodexCliPathInvalid,
                "The selected Codex override is empty or invalid; lower-priority fallback is disabled.",
            )
        } else {
            (
                CodexFileStatus::Missing,
                DiagnosticStatus::Block,
                DiagnosticStatus::Block,
                ReasonCode::CodexCliNotFound,
                "No Codex CLI file was found through the selected resolver source.",
            )
        }
    } else {
        if regular == Some(false) {
            (
                CodexFileStatus::NotRegularFile,
                DiagnosticStatus::Block,
                DiagnosticStatus::Block,
                if explicit {
                    ReasonCode::CodexCliPathInvalid
                } else {
                    ReasonCode::CodexCliNotFound
                },
                "The selected Codex CLI candidate is not a regular file.",
            )
        } else if regular.is_none() {
            (
                CodexFileStatus::Unknown,
                DiagnosticStatus::Unknown,
                DiagnosticStatus::Unknown,
                if explicit {
                    ReasonCode::CodexCliPathInvalid
                } else {
                    ReasonCode::RuntimeVerificationNotRun
                },
                "Codex CLI file metadata could not be established safely.",
            )
        } else if can_read == Some(false) && explicit {
            (
                CodexFileStatus::Unreadable,
                DiagnosticStatus::Block,
                DiagnosticStatus::Block,
                ReasonCode::CodexCliPathInvalid,
                "The selected Codex override is not readable; lower-priority fallback is disabled.",
            )
        } else if can_execute == Some(false) {
            (
                CodexFileStatus::Present,
                DiagnosticStatus::Block,
                DiagnosticStatus::Block,
                ReasonCode::CodexCliNotExecutable,
                "The selected Codex CLI file is statically known not to be executable.",
            )
        } else if can_read == Some(false) || can_read.is_none() || can_execute.is_none() {
            (
                if can_read == Some(false) {
                    CodexFileStatus::Unreadable
                } else {
                    CodexFileStatus::Unknown
                },
                DiagnosticStatus::Unknown,
                DiagnosticStatus::Unknown,
                if explicit {
                    ReasonCode::CodexCliPathInvalid
                } else {
                    ReasonCode::RuntimeVerificationNotRun
                },
                "Codex CLI permissions or file metadata are ambiguous.",
            )
        } else {
            (
                CodexFileStatus::Present,
                DiagnosticStatus::Pass,
                DiagnosticStatus::Pass,
                ReasonCode::CapabilityAvailable,
                "A regular, readable Codex CLI candidate has static execute permission; it was not launched.",
            )
        }
    };
    let version_probe = VersionProbe {
        requested: input.probe_codex_requested,
        attempted: false,
        status: DiagnosticStatus::Unknown,
        version: None,
        timeout_seconds: 2,
        reason_code: if input.probe_codex_requested {
            ReasonCode::CodexCliVersionProbeRefused
        } else {
            ReasonCode::ProbeNotRun
        },
    };
    let mut checks = vec![check(
        "goal_model.codex_cli",
        codex_status,
        Requirement::Optional,
        Applicability::Applicable,
        EvidenceType::StaticObservation,
        SafeObserved::boolean(selected),
        reason,
        message,
        Some(CapabilityId::GoalModelExecution),
    )];
    checks.push(check(
        "goal_model.execution_not_run",
        DiagnosticStatus::Unknown,
        Requirement::Optional,
        Applicability::Applicable,
        EvidenceType::NotRun,
        SafeObserved::none(),
        ReasonCode::RuntimeVerificationNotRun,
        "Authentication and model execution were not tested.",
        Some(CapabilityId::GoalModelExecution),
    ));
    checks.push(check(
        "goal_model.version_probe",
        DiagnosticStatus::Unknown,
        Requirement::Optional,
        Applicability::Applicable,
        EvidenceType::NotRun,
        SafeObserved::none(),
        version_probe.reason_code,
        if input.probe_codex_requested {
            "The opted-in Codex probe was refused before process spawn because containment is unverified."
        } else {
            "The Codex version probe was not requested and no process was started."
        },
        Some(CapabilityId::GoalModelExecution),
    ));
    let _ = exec_status;
    let codex = CodexCli {
        resolution_source: input.codex_source,
        selected,
        path_redacted: true,
        file_status,
        executable_status: exec_status,
        version_probe,
    };
    (codex, std::mem::take(&mut checks))
}

fn observe_codex_path(path: &Path) -> (Option<bool>, Option<bool>, Option<bool>) {
    if path.as_os_str().as_encoded_bytes().contains(&0) {
        return (Some(false), Some(false), Some(false));
    }
    match fs::metadata(path) {
        Ok(metadata) if !metadata.is_file() => (Some(false), Some(false), Some(false)),
        Ok(_) => (
            Some(true),
            readable(path),
            executable_permission(path, HostFacts::current().os),
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (Some(false), None, None),
        Err(_) => (None, None, None),
    }
}

fn codex_search_path_entries(path: &OsStr, os: OperatingSystem) -> Vec<PathBuf> {
    std::env::split_paths(path)
        .filter(|directory| os != OperatingSystem::Windows || !directory.as_os_str().is_empty())
        .collect()
}

fn resolve_codex_on_path(
    command: Option<&Path>,
    path: Option<&OsStr>,
    os: OperatingSystem,
) -> Option<PathBuf> {
    let command = command?;
    let name = command.file_name()?;
    let path = path?;
    let extensions: &[&str] =
        if os == OperatingSystem::Windows && !name.as_encoded_bytes().contains(&b'.') {
            &[".exe"]
        } else {
            &[""]
        };
    for directory in codex_search_path_entries(path, os) {
        for extension in extensions {
            let mut candidate_name = OsString::from(name);
            candidate_name.push(extension);
            let candidate = directory.join(candidate_name);
            if fs::metadata(&candidate).is_ok_and(|metadata| metadata.is_file()) {
                return Some(candidate);
            }
        }
    }
    None
}

fn add_advisory_install_checks(checks: &mut Vec<DiagnosticCheck>) {
    checks.push(check(
        "installation.symlink_origin",
        DiagnosticStatus::Unknown,
        Requirement::Optional,
        Applicability::Applicable,
        EvidenceType::NotRun,
        SafeObserved::none(),
        ReasonCode::InstallSymlinkOriginUnknown,
        "The executable was not relaunched through a symlink; origin behavior is unknown.",
        None,
    ));
    checks.push(check(
        "installation.nix_validation",
        DiagnosticStatus::Unknown,
        Requirement::Optional,
        Applicability::Applicable,
        EvidenceType::NotRun,
        SafeObserved::none(),
        ReasonCode::InstallNixUnverified,
        "Nix build and run layout were not verified.",
        None,
    ));
}

fn dependency_reports(input: &DoctorInput) -> (Bubblewrap, MacosSandboxExec) {
    let applicable = input.host.os == OperatingSystem::Linux;
    let found = if applicable {
        input.bubblewrap_found
    } else {
        None
    };
    let (status, reason_code, version, _evidence) = bubblewrap_version_result(
        found,
        input.bubblewrap_version.as_deref(),
        input.probe_bubblewrap_requested,
    );
    let bubblewrap = Bubblewrap {
        applicable,
        found,
        version: if status == DiagnosticStatus::Pass || status == DiagnosticStatus::Block {
            version.clone()
        } else {
            None
        },
        minimum_version: "0.12.0",
        version_probe: VersionProbe {
            requested: input.probe_bubblewrap_requested,
            attempted: false,
            status: if !applicable {
                DiagnosticStatus::Unknown
            } else {
                status
            },
            version: if status == DiagnosticStatus::Pass || status == DiagnosticStatus::Block {
                version
            } else {
                None
            },
            timeout_seconds: 5,
            reason_code: if !applicable {
                ReasonCode::CheckNotApplicable
            } else {
                reason_code
            },
        },
    };
    let macos_applicable = input.host.os == OperatingSystem::Macos;
    let (present, executable_status, runtime_status) = if macos_applicable {
        match input.macos_sandbox_exec {
            Some((Some(true), Some(true))) => (
                Some(true),
                DiagnosticStatus::Pass,
                DiagnosticStatus::Unknown,
            ),
            Some((Some(false), _)) => (
                Some(false),
                DiagnosticStatus::Block,
                DiagnosticStatus::Unknown,
            ),
            Some((Some(true), Some(false))) => (
                Some(true),
                DiagnosticStatus::Block,
                DiagnosticStatus::Unknown,
            ),
            Some((Some(true), None)) => (
                Some(true),
                DiagnosticStatus::Unknown,
                DiagnosticStatus::Unknown,
            ),
            Some((None, _)) | None => (None, DiagnosticStatus::Unknown, DiagnosticStatus::Unknown),
        }
    } else {
        (None, DiagnosticStatus::Unknown, DiagnosticStatus::Unknown)
    };
    (
        bubblewrap,
        MacosSandboxExec {
            applicable: macos_applicable,
            present,
            executable_status,
            runtime_status,
        },
    )
}

fn build_capabilities(
    checks: &[DiagnosticCheck],
    sandbox_requirement: Requirement,
) -> Capabilities {
    Capabilities {
        mcp_transport: capability(checks, CapabilityId::McpTransport, Requirement::Required),
        session_management: capability(
            checks,
            CapabilityId::SessionManagement,
            Requirement::Required,
        ),
        sandboxed_execution: capability(
            checks,
            CapabilityId::SandboxedExecution,
            sandbox_requirement,
        ),
        atomic_publication: capability(
            checks,
            CapabilityId::AtomicPublication,
            Requirement::Required,
        ),
        goal_model_execution: capability(
            checks,
            CapabilityId::GoalModelExecution,
            Requirement::Optional,
        ),
    }
}

fn capability(
    checks: &[DiagnosticCheck],
    id: CapabilityId,
    requirement: Requirement,
) -> CapabilityStatus {
    if requirement == Requirement::NotApplicable {
        return CapabilityStatus {
            requirement,
            status: DiagnosticStatus::Unknown,
            formal_status: DiagnosticStatus::Unknown,
            runtime_status: DiagnosticStatus::Unknown,
            reason_codes: vec![ReasonCode::SandboxParityNotApplicable],
        };
    }
    let selected: Vec<&DiagnosticCheck> = checks
        .iter()
        .filter(|check| {
            check.capability == Some(id) && check.applicability == Applicability::Applicable
        })
        .collect();
    let status = aggregate_status(selected.iter().map(|check| check.status));
    let formal_status = aggregate_status(selected.iter().filter_map(|check| {
        matches!(
            check.evidence_type,
            EvidenceType::StaticObservation | EvidenceType::FormalContract
        )
        .then_some(check.status)
    }));
    let runtime_status = aggregate_status(selected.iter().filter_map(|check| {
        matches!(
            check.evidence_type,
            EvidenceType::RuntimeReadOnly | EvidenceType::NotRun
        )
        .then_some(check.status)
    }));
    let reason_codes = selected
        .iter()
        .map(|check| check.reason_code)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    CapabilityStatus {
        requirement,
        status,
        formal_status,
        runtime_status,
        reason_codes,
    }
}

fn summarize_checks(checks: &[DiagnosticCheck]) -> DiagnosticSummary {
    let counts = DiagnosticCounts {
        pass: checks
            .iter()
            .filter(|check| check.status == DiagnosticStatus::Pass)
            .count() as u8,
        warn: checks
            .iter()
            .filter(|check| check.status == DiagnosticStatus::Warn)
            .count() as u8,
        block: checks
            .iter()
            .filter(|check| check.status == DiagnosticStatus::Block)
            .count() as u8,
        unknown: checks
            .iter()
            .filter(|check| check.status == DiagnosticStatus::Unknown)
            .count() as u8,
    };
    let applicable_required: Vec<DiagnosticStatus> = checks
        .iter()
        .filter(|check| {
            check.requirement == Requirement::Required
                && check.applicability == Applicability::Applicable
        })
        .map(|check| check.status)
        .collect();
    let applicable_optional: Vec<DiagnosticStatus> = checks
        .iter()
        .filter(|check| {
            check.requirement == Requirement::Optional
                && check.applicability == Applicability::Applicable
        })
        .map(|check| check.status)
        .collect();
    let runtime: Vec<DiagnosticStatus> = checks
        .iter()
        .filter(|check| {
            matches!(
                check.evidence_type,
                EvidenceType::RuntimeReadOnly | EvidenceType::NotRun
            )
        })
        .map(|check| check.status)
        .collect();
    let required_status = aggregate_status(applicable_required.iter().copied());
    let required_blockers = applicable_required
        .iter()
        .filter(|status| **status == DiagnosticStatus::Block)
        .count();
    let required_unknowns = applicable_required
        .iter()
        .filter(|status| **status == DiagnosticStatus::Unknown)
        .count();
    let exit_code = if required_blockers > 0 {
        ExitCode::RequiredBlock
    } else if required_unknowns > 0 || applicable_required.is_empty() {
        ExitCode::RequiredUnknown
    } else {
        ExitCode::Success
    };
    DiagnosticSummary {
        status: required_status,
        required_status,
        advisory_status: aggregate_status(applicable_optional),
        runtime_status: aggregate_status(runtime),
        counts,
        checks_total: checks.len(),
        required_blockers,
        required_unknowns,
        exit_code,
    }
}

fn aggregate_status(statuses: impl IntoIterator<Item = DiagnosticStatus>) -> DiagnosticStatus {
    let mut any = false;
    let mut block = false;
    let mut unknown = false;
    let mut warn = false;
    for status in statuses {
        any = true;
        block |= status == DiagnosticStatus::Block;
        unknown |= status == DiagnosticStatus::Unknown;
        warn |= status == DiagnosticStatus::Warn;
    }
    if block {
        DiagnosticStatus::Block
    } else if unknown || !any {
        DiagnosticStatus::Unknown
    } else if warn {
        DiagnosticStatus::Warn
    } else {
        DiagnosticStatus::Pass
    }
}

#[allow(clippy::too_many_arguments)]
fn check(
    id: impl Into<String>,
    status: DiagnosticStatus,
    requirement: Requirement,
    applicability: Applicability,
    evidence_type: EvidenceType,
    observed: SafeObserved,
    reason_code: ReasonCode,
    message: &'static str,
    capability: Option<CapabilityId>,
) -> DiagnosticCheck {
    DiagnosticCheck {
        id: id.into(),
        status,
        requirement,
        applicability,
        evidence_type,
        observed,
        reason_code,
        message: message.to_owned(),
        capability,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "goallatch-doctor-test-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).expect("create isolated Doctor test root");
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

    fn create_file(path: &Path, contents: &[u8], executable: bool) {
        fs::write(path, contents).expect("write test-owned fixture");
        #[cfg(unix)]
        if executable {
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(path, permissions).unwrap();
        }
        #[cfg(not(unix))]
        let _ = executable;
    }

    fn test_input(host: OperatingSystem, arch: Architecture, root: &TestRoot) -> DoctorInput {
        let exe_name = if host == OperatingSystem::Windows {
            "local-mcp.exe"
        } else {
            "local-mcp"
        };
        let bin = root.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let exe = bin.join(exe_name);
        create_file(&exe, b"test executable sentinel", true);
        let host = HostFacts {
            os: host,
            architecture: arch,
        };
        let (required, forbidden) = helper_contract(host.os);
        for (name, _) in required {
            create_file(&bin.join(name), b"test helper sentinel", true);
        }
        for name in forbidden {
            let _ = fs::remove_file(bin.join(name));
        }
        let codex_file = root.path().join("codex-fixture");
        create_file(&codex_file, b"not launched", true);
        DoctorInput {
            host,
            current_exe: Some(exe),
            codex_source: CodexSource::Path,
            codex_path: Some(codex_file),
            path_environment: None,
            bubblewrap_found: (host.os == OperatingSystem::Linux).then_some(true),
            bubblewrap_version: None,
            macos_sandbox_exec: (host.os == OperatingSystem::Macos)
                .then_some((Some(true), Some(true))),
            probe_bubblewrap_requested: false,
            probe_codex_requested: false,
        }
    }

    fn statuses() -> [DiagnosticStatus; 4] {
        [
            DiagnosticStatus::Pass,
            DiagnosticStatus::Warn,
            DiagnosticStatus::Block,
            DiagnosticStatus::Unknown,
        ]
    }

    fn check_for(status: DiagnosticStatus, requirement: Requirement) -> DiagnosticCheck {
        check(
            "test.status",
            status,
            requirement,
            if requirement == Requirement::NotApplicable {
                Applicability::NotApplicable
            } else {
                Applicability::Applicable
            },
            EvidenceType::FormalContract,
            SafeObserved::none(),
            ReasonCode::CapabilityAvailable,
            "synthetic Doctor contract test",
            Some(CapabilityId::McpTransport),
        )
    }

    #[test]
    fn status_truth_table_covers_every_three_check_combination() {
        let values = statuses();
        for first in values {
            for second in values {
                for third in values {
                    let expected = if [first, second, third].contains(&DiagnosticStatus::Block) {
                        DiagnosticStatus::Block
                    } else if [first, second, third].contains(&DiagnosticStatus::Unknown) {
                        DiagnosticStatus::Unknown
                    } else if [first, second, third].contains(&DiagnosticStatus::Warn) {
                        DiagnosticStatus::Warn
                    } else {
                        DiagnosticStatus::Pass
                    };
                    assert_eq!(aggregate_status([first, second, third]), expected);
                }
            }
        }
        assert_eq!(aggregate_status([]), DiagnosticStatus::Unknown);
    }

    #[test]
    fn optional_only_block_or_unknown_does_not_change_required_exit() {
        for status in statuses() {
            let checks = [
                check_for(DiagnosticStatus::Pass, Requirement::Required),
                check_for(status, Requirement::Optional),
            ];
            let summary = summarize_checks(&checks);
            assert_eq!(summary.required_status, DiagnosticStatus::Pass);
            assert_eq!(summary.exit_code, ExitCode::Success);
        }
    }

    #[test]
    fn required_unknown_and_block_precedence_are_deterministic() {
        let unknown = summarize_checks(&[
            check_for(DiagnosticStatus::Unknown, Requirement::Required),
            check_for(DiagnosticStatus::Warn, Requirement::Required),
        ]);
        assert_eq!(unknown.required_status, DiagnosticStatus::Unknown);
        assert_eq!(unknown.exit_code, ExitCode::RequiredUnknown);
        let block = summarize_checks(&[
            check_for(DiagnosticStatus::Unknown, Requirement::Required),
            check_for(DiagnosticStatus::Block, Requirement::Required),
        ]);
        assert_eq!(block.required_status, DiagnosticStatus::Block);
        assert_eq!(block.exit_code, ExitCode::RequiredBlock);
    }

    #[test]
    fn not_applicable_checks_are_excluded_and_never_pass() {
        let checks = [
            check_for(DiagnosticStatus::Pass, Requirement::Required),
            check_for(DiagnosticStatus::Unknown, Requirement::NotApplicable),
        ];
        let summary = summarize_checks(&checks);
        assert_eq!(summary.required_status, DiagnosticStatus::Pass);
        assert_eq!(summary.exit_code, ExitCode::Success);
        assert_eq!(
            aggregate_status([DiagnosticStatus::Unknown]),
            DiagnosticStatus::Unknown
        );
    }

    #[test]
    fn formal_pass_does_not_promote_runtime_unknown() {
        let checks = [
            check(
                "contract.formal",
                DiagnosticStatus::Pass,
                Requirement::Required,
                Applicability::Applicable,
                EvidenceType::FormalContract,
                SafeObserved::none(),
                ReasonCode::CapabilityAvailable,
                "formal contract is present",
                Some(CapabilityId::McpTransport),
            ),
            check(
                "runtime.not_run",
                DiagnosticStatus::Unknown,
                Requirement::Required,
                Applicability::Applicable,
                EvidenceType::NotRun,
                SafeObserved::none(),
                ReasonCode::RuntimeVerificationNotRun,
                "runtime was not exercised",
                Some(CapabilityId::McpTransport),
            ),
        ];
        let capability = capability(&checks, CapabilityId::McpTransport, Requirement::Required);
        assert_eq!(capability.formal_status, DiagnosticStatus::Pass);
        assert_eq!(capability.runtime_status, DiagnosticStatus::Unknown);
        assert_eq!(capability.status, DiagnosticStatus::Unknown);
    }

    #[cfg(unix)]
    #[test]
    fn unix_read_and_execute_access_observations_use_effective_file_access() {
        let root = TestRoot::new();
        let executable = root.path().join("executable-fixture");
        let non_executable = root.path().join("non-executable-fixture");
        create_file(&executable, b"test executable", true);
        create_file(&non_executable, b"test non-executable", false);
        assert_eq!(readable(&executable), Some(true));
        assert_eq!(
            executable_permission(&executable, OperatingSystem::Linux),
            Some(true)
        );
        assert_eq!(
            executable_permission(&non_executable, OperatingSystem::Linux),
            Some(false)
        );
    }

    #[test]
    fn windows_is_experimental_and_sandbox_parity_is_not_applicable() {
        let root = TestRoot::new();
        let report = build_report(test_input(
            OperatingSystem::Windows,
            Architecture::X86_64,
            &root,
        ));
        assert_eq!(report.platform.support_status, SupportStatus::Experimental);
        assert!(report.platform.experimental);
        assert_eq!(
            report.platform.unix_sandbox_parity,
            UnixSandboxParity::NotApplicable
        );
        assert_eq!(
            report.capabilities.sandboxed_execution.requirement,
            Requirement::NotApplicable
        );
        assert_eq!(
            report.capabilities.sandboxed_execution.status,
            DiagnosticStatus::Unknown
        );
        #[cfg(windows)]
        assert_eq!(report.summary.exit_code, ExitCode::Success);
        #[cfg(not(windows))]
        assert_eq!(report.summary.exit_code, ExitCode::RequiredUnknown);
        assert!(report.checks.iter().any(|check| {
            check.reason_code == ReasonCode::PlatformWindowsExperimental
                && check.status == DiagnosticStatus::Warn
        }));
    }

    #[test]
    fn linux_bubblewrap_missing_is_a_required_sandbox_block() {
        let root = TestRoot::new();
        let mut input = test_input(OperatingSystem::Linux, Architecture::X86_64, &root);
        input.bubblewrap_found = Some(false);
        input.bubblewrap_version = None;
        let report = build_report(input);
        assert_eq!(
            report.capabilities.sandboxed_execution.status,
            DiagnosticStatus::Block
        );
        assert_eq!(report.summary.exit_code, ExitCode::RequiredBlock);
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.reason_code == ReasonCode::BwrapNotFound)
        );
        assert_eq!(
            report.dependencies.bubblewrap.version_probe.status,
            DiagnosticStatus::Unknown
        );
        assert!(!report.dependencies.bubblewrap.version_probe.attempted);
    }

    #[test]
    fn unprobed_bubblewrap_is_unknown_even_when_the_binary_is_present() {
        let root = TestRoot::new();
        let report = build_report(test_input(
            OperatingSystem::Linux,
            Architecture::X86_64,
            &root,
        ));
        assert_eq!(report.dependencies.bubblewrap.found, Some(true));
        assert_eq!(report.dependencies.bubblewrap.version, None);
        assert_eq!(
            report.capabilities.sandboxed_execution.status,
            DiagnosticStatus::Unknown
        );
        assert_eq!(report.summary.exit_code, ExitCode::RequiredUnknown);
        assert!(report.checks.iter().any(|check| {
            check.reason_code == ReasonCode::BwrapVersionUnprobed
                && check.status == DiagnosticStatus::Unknown
        }));
    }

    #[test]
    fn unknown_bubblewrap_presence_never_becomes_missing_or_pass() {
        let root = TestRoot::new();
        let mut input = test_input(OperatingSystem::Linux, Architecture::X86_64, &root);
        input.bubblewrap_found = None;
        input.bubblewrap_version = None;
        let report = build_report(input);
        assert_eq!(
            report.capabilities.sandboxed_execution.status,
            DiagnosticStatus::Unknown
        );
        assert!(report.checks.iter().any(|check| {
            check.id == "sandbox.bubblewrap.presence"
                && check.status == DiagnosticStatus::Unknown
                && check.reason_code == ReasonCode::BwrapVersionUnknown
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bubblewrap_old_version_uses_the_shared_minimum_gate() {
        let old = bubblewrap_version_result(Some(true), Some("0.11.9"), true);
        assert_eq!(old.0, DiagnosticStatus::Block);
        assert_eq!(old.1, ReasonCode::BwrapVersionUnsupported);
        let minimum = bubblewrap_version_result(Some(true), Some("0.12.0"), true);
        assert_eq!(minimum.0, DiagnosticStatus::Pass);
        let malformed = bubblewrap_version_result(Some(true), Some("0.12.0-vendor"), true);
        assert_eq!(malformed.0, DiagnosticStatus::Unknown);
        assert_eq!(malformed.2, None);
        let long = bubblewrap_version_result(Some(true), Some("18446744073709551615.1.1"), true);
        assert_eq!(long.0, DiagnosticStatus::Unknown);
        assert_eq!(long.2, None);
        let unknown = bubblewrap_version_result(None, None, true);
        assert_eq!(unknown.0, DiagnosticStatus::Unknown);
        assert_eq!(unknown.1, ReasonCode::BwrapVersionUnknown);
    }

    #[test]
    fn macos_static_sandbox_detection_does_not_claim_runtime_pass() {
        let root = TestRoot::new();
        let report = build_report(test_input(
            OperatingSystem::Macos,
            Architecture::X86_64,
            &root,
        ));
        assert_eq!(report.dependencies.macos_sandbox_exec.present, Some(true));
        assert_eq!(
            report.dependencies.macos_sandbox_exec.executable_status,
            DiagnosticStatus::Pass
        );
        assert_eq!(
            report.dependencies.macos_sandbox_exec.runtime_status,
            DiagnosticStatus::Unknown
        );
        assert_eq!(
            report.capabilities.sandboxed_execution.status,
            DiagnosticStatus::Unknown
        );
    }

    #[test]
    fn unknown_required_helper_observation_stays_null_in_safe_observed() {
        let root = TestRoot::new();
        let mut input = test_input(OperatingSystem::Windows, Architecture::X86_64, &root);
        input.current_exe = None;
        let report = build_report(input);
        let helper_check = report
            .checks
            .iter()
            .find(|check| check.id == "installation.helper.atomic_publish")
            .unwrap();
        assert_eq!(helper_check.status, DiagnosticStatus::Unknown);
        let value = serde_json::to_value(helper_check).unwrap();
        assert!(value["observed"].is_null());
    }

    #[test]
    fn macos_unknown_metadata_keeps_presence_unknown() {
        let root = TestRoot::new();
        let mut input = test_input(OperatingSystem::Macos, Architecture::X86_64, &root);
        input.macos_sandbox_exec = Some((None, None));
        let report = build_report(input);
        assert_eq!(report.dependencies.macos_sandbox_exec.present, None);
        assert_eq!(
            report.dependencies.macos_sandbox_exec.executable_status,
            DiagnosticStatus::Unknown
        );
        let check = report
            .checks
            .iter()
            .find(|check| check.id == "sandbox.macos.sandbox_exec")
            .unwrap();
        assert_eq!(check.status, DiagnosticStatus::Unknown);
        assert!(serde_json::to_value(check).unwrap()["observed"].is_null());
    }

    #[test]
    fn missing_required_helper_blocks_only_its_capability_and_path_never_falls_back() {
        let root = TestRoot::new();
        let mut input = test_input(OperatingSystem::Windows, Architecture::X86_64, &root);
        let sibling = input.current_exe.as_ref().unwrap().parent().unwrap();
        fs::remove_file(sibling.join("atomic-publish.exe")).unwrap();
        let path_bin = root.path().join("path-only");
        fs::create_dir(&path_bin).unwrap();
        create_file(
            &path_bin.join("atomic-publish.exe"),
            b"not a helper candidate",
            true,
        );
        let child = sibling.join("child");
        fs::create_dir(&child).unwrap();
        create_file(
            &child.join("atomic-publish.exe"),
            b"not a helper candidate",
            true,
        );
        create_file(
            &root.path().join("atomic-publish.exe"),
            b"not a helper candidate",
            true,
        );
        input.path_environment = Some(std::env::join_paths([path_bin]).unwrap());
        let report = build_report(input);
        assert_eq!(
            report.capabilities.atomic_publication.status,
            DiagnosticStatus::Block
        );
        assert_eq!(
            report.capabilities.mcp_transport.status,
            DiagnosticStatus::Pass
        );
        assert!(report.checks.iter().any(|check| {
            check.reason_code == ReasonCode::InstallRequiredHelperMissing
                && check.capability == Some(CapabilityId::AtomicPublication)
        }));
    }

    #[test]
    fn non_regular_required_helper_is_a_specific_installation_block() {
        let root = TestRoot::new();
        let input = test_input(OperatingSystem::Windows, Architecture::X86_64, &root);
        let candidate = input
            .current_exe
            .as_ref()
            .unwrap()
            .parent()
            .unwrap()
            .join("atomic-publish.exe");
        fs::remove_file(&candidate).unwrap();
        fs::create_dir(&candidate).unwrap();
        let report = build_report(input);
        assert_eq!(
            report.capabilities.atomic_publication.status,
            DiagnosticStatus::Block
        );
        assert!(report.checks.iter().any(|check| {
            check.reason_code == ReasonCode::InstallRequiredHelperNotRegularFile
                && check.status == DiagnosticStatus::Block
        }));
    }

    #[test]
    fn runtime_and_doctor_both_reject_helper_symlink_entries() {
        let root = TestRoot::new();
        let bin = root.path().join("bin");
        let external = root.path().join("outside-sibling");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&external).unwrap();
        let executable = bin.join(if cfg!(windows) {
            "local-mcp.exe"
        } else {
            "local-mcp"
        });
        create_file(&executable, b"test-owned executable", true);
        let helper_name = crate::install_contract::ATOMIC_PUBLISH;
        let candidate = bin.join(helper_name);

        create_file(&candidate, b"ordinary helper fixture", true);
        assert!(candidate.is_file());
        assert_eq!(
            crate::install_contract::first_existing(std::slice::from_ref(&candidate)),
            Some(candidate.clone())
        );
        let (ordinary, _, _, _) = inspect_helper(
            Some(&executable),
            helper_name,
            true,
            true,
            HostFacts::current().os,
        );
        assert_eq!(ordinary.present, Some(true));
        assert_eq!(ordinary.regular_file, Some(true));
        fs::remove_file(&candidate).unwrap();
        assert!(!candidate.is_file());
        assert_eq!(
            crate::install_contract::first_existing(std::slice::from_ref(&candidate)),
            None
        );

        let same_directory_target = bin.join("helper-target");
        create_file(&same_directory_target, b"same-directory target", true);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&same_directory_target, &candidate).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&same_directory_target, &candidate)
            .expect("create a test-owned file symlink/reparse point");
        assert!(candidate.is_file());
        assert_eq!(
            crate::install_contract::first_existing(std::slice::from_ref(&candidate)),
            None
        );
        assert!(
            fs::symlink_metadata(&candidate)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let (same_directory_link, same_directory_status, _, _) = inspect_helper(
            Some(&executable),
            helper_name,
            true,
            true,
            HostFacts::current().os,
        );
        assert_eq!(same_directory_link.present, Some(true));
        assert_eq!(same_directory_link.regular_file, Some(false));
        assert_eq!(same_directory_status, DiagnosticStatus::Block);
        fs::remove_file(&candidate).unwrap();

        let outside_target = external.join("helper-target");
        create_file(&outside_target, b"outside-sibling target", true);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_target, &candidate).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&outside_target, &candidate)
            .expect("create a test-owned external file symlink/reparse point");
        assert!(candidate.is_file());
        assert_eq!(
            crate::install_contract::first_existing(std::slice::from_ref(&candidate)),
            None
        );
        let (outside_link, outside_status, _, _) = inspect_helper(
            Some(&executable),
            helper_name,
            true,
            true,
            HostFacts::current().os,
        );
        assert_eq!(outside_link.present, Some(true));
        assert_eq!(outside_link.regular_file, Some(false));
        assert_eq!(outside_status, DiagnosticStatus::Block);
        fs::remove_file(&candidate).unwrap();

        let broken_target = external.join("missing-helper-target");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&broken_target, &candidate).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&broken_target, &candidate)
            .expect("create a test-owned broken file symlink/reparse point");
        assert!(!candidate.is_file());
        assert_eq!(
            crate::install_contract::first_existing(std::slice::from_ref(&candidate)),
            None
        );
        let (broken_link, broken_status, _, _) = inspect_helper(
            Some(&executable),
            helper_name,
            true,
            true,
            HostFacts::current().os,
        );
        assert_eq!(broken_link.present, Some(true));
        assert_eq!(broken_status, DiagnosticStatus::Block);
        fs::remove_file(&candidate).unwrap();

        let directory_target = external.join("directory-target");
        fs::create_dir(&directory_target).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&directory_target, &candidate).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&directory_target, &candidate)
            .expect("create a test-owned directory symlink/reparse point");
        assert!(!candidate.is_file());
        assert_eq!(
            crate::install_contract::first_existing(std::slice::from_ref(&candidate)),
            None
        );
        assert!(
            fs::symlink_metadata(&candidate)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let (directory_link, directory_status, _, _) = inspect_helper(
            Some(&executable),
            helper_name,
            true,
            true,
            HostFacts::current().os,
        );
        assert_eq!(directory_link.present, Some(true));
        assert_eq!(directory_link.regular_file, Some(false));
        assert_eq!(directory_status, DiagnosticStatus::Block);
    }

    #[test]
    fn forbidden_helper_presence_blocks_install_contract_integrity() {
        let root = TestRoot::new();
        let input = test_input(OperatingSystem::Windows, Architecture::X86_64, &root);
        let sibling = input.current_exe.as_ref().unwrap().parent().unwrap();
        create_file(
            &sibling.join("codex-linux-sandbox.exe"),
            b"forbidden helper fixture",
            false,
        );
        let report = build_report(input);
        assert_eq!(report.summary.exit_code, ExitCode::RequiredBlock);
        assert!(report.checks.iter().any(|check| {
            check.reason_code == ReasonCode::InstallForbiddenHelperPresent
                && check.status == DiagnosticStatus::Block
        }));
    }

    #[test]
    fn codex_resolver_precedence_is_exact_and_override_invalidity_does_not_fall_through() {
        assert_eq!(
            choose_codex_source(true, true, true),
            CodexSource::LocalMcpCodexCliPath
        );
        assert_eq!(
            choose_codex_source(false, true, true),
            CodexSource::CodexCliPath
        );
        assert_eq!(
            choose_codex_source(false, false, true),
            CodexSource::MacosChatgptBundle
        );
        assert_eq!(choose_codex_source(false, false, false), CodexSource::Path);

        let root = TestRoot::new();
        let mut input = test_input(OperatingSystem::Windows, Architecture::X86_64, &root);
        let lower_priority_path = input.codex_path.clone().unwrap();
        input.codex_source = CodexSource::LocalMcpCodexCliPath;
        input.codex_path = Some(PathBuf::new());
        let report = build_report(input);
        assert_eq!(
            report.dependencies.codex_cli.resolution_source,
            CodexSource::LocalMcpCodexCliPath
        );
        assert_eq!(
            report.dependencies.codex_cli.executable_status,
            DiagnosticStatus::Block
        );
        assert_eq!(
            report.dependencies.codex_cli.file_status,
            CodexFileStatus::NotRegularFile
        );
        assert!(lower_priority_path.is_file());
    }

    #[test]
    fn windows_default_codex_resolution_is_unknown_and_does_not_block_required_capabilities() {
        let root = TestRoot::new();
        let mut input = test_input(OperatingSystem::Windows, Architecture::X86_64, &root);
        input.codex_source = CodexSource::Path;
        input.codex_path = Some(PathBuf::from("codex"));
        input.path_environment = None;
        let report = build_report(input);
        assert_eq!(
            report.capabilities.goal_model_execution.status,
            DiagnosticStatus::Unknown
        );
        assert_eq!(
            report.capabilities.mcp_transport.status,
            DiagnosticStatus::Pass
        );
        assert_eq!(
            report.capabilities.session_management.status,
            DiagnosticStatus::Pass
        );
        assert!(report.checks.iter().any(|check| {
            check.reason_code == ReasonCode::RuntimeVerificationNotRun
                && check.capability == Some(CapabilityId::GoalModelExecution)
        }));
        assert_eq!(report.summary.exit_code, ExitCode::Success);
    }

    #[test]
    fn non_windows_missing_codex_blocks_only_the_optional_goal_model_capability() {
        let root = TestRoot::new();
        let mut input = test_input(OperatingSystem::Linux, Architecture::X86_64, &root);
        input.codex_source = CodexSource::Path;
        input.codex_path = Some(PathBuf::from("codex"));
        input.path_environment = None;
        let report = build_report(input);
        assert_eq!(
            report.capabilities.goal_model_execution.status,
            DiagnosticStatus::Block
        );
        assert_eq!(
            report.capabilities.mcp_transport.status,
            DiagnosticStatus::Pass
        );
        assert_eq!(
            report.capabilities.session_management.status,
            DiagnosticStatus::Pass
        );
        assert!(report.checks.iter().any(|check| {
            check.reason_code == ReasonCode::CodexCliNotFound
                && check.capability == Some(CapabilityId::GoalModelExecution)
        }));
        assert_ne!(report.summary.exit_code, ExitCode::RequiredBlock);
    }

    #[test]
    fn relative_codex_override_is_unknown_without_consulting_the_current_directory() {
        let root = TestRoot::new();
        let mut input = test_input(OperatingSystem::Windows, Architecture::X86_64, &root);
        input.codex_source = CodexSource::CodexCliPath;
        input.codex_path = Some(PathBuf::from("relative/codex"));
        let report = build_report(input);
        assert_eq!(
            report.dependencies.codex_cli.file_status,
            CodexFileStatus::Unknown
        );
        assert_eq!(
            report.dependencies.codex_cli.executable_status,
            DiagnosticStatus::Unknown
        );
        assert!(report.checks.iter().any(|check| {
            check.reason_code == ReasonCode::CodexCliPathContextUnknown
                && check.status == DiagnosticStatus::Unknown
        }));
    }

    #[test]
    fn path_codex_resolution_is_distinct_from_helper_resolution() {
        let root = TestRoot::new();
        let codex_dir = root.path().join("codex-path");
        fs::create_dir(&codex_dir).unwrap();
        let (host, codex_name) = if cfg!(windows) {
            (OperatingSystem::Windows, "codex.exe")
        } else {
            (HostFacts::current().os, "codex")
        };
        let codex = codex_dir.join(codex_name);
        create_file(&codex, b"not launched", true);
        let resolved =
            resolve_codex_on_path(Some(Path::new("codex")), Some(codex_dir.as_os_str()), host);
        assert_eq!(resolved.as_deref(), Some(codex.as_path()));
    }

    #[test]
    fn windows_bare_codex_uses_only_exe_and_skips_empty_path_entries() {
        let root = TestRoot::new();
        let bin = root.path().join("codex-path");
        fs::create_dir(&bin).unwrap();
        for suffix in ["", ".com", ".cmd", ".bat"] {
            let candidate = bin.join(format!("codex{suffix}"));
            create_file(&candidate, b"not launched", true);
            assert_eq!(
                resolve_codex_on_path(
                    Some(Path::new("codex")),
                    Some(bin.as_os_str()),
                    OperatingSystem::Windows,
                ),
                None,
                "bare Command::new(\"codex\") must not infer {suffix:?}"
            );
            fs::remove_file(candidate).unwrap();
        }
        let exe = bin.join("codex.exe");
        create_file(&exe, b"not launched", true);
        let path_with_empty_entry = std::env::join_paths([PathBuf::new(), bin.clone()]).unwrap();
        assert_eq!(
            codex_search_path_entries(&path_with_empty_entry, OperatingSystem::Windows),
            vec![bin.clone()]
        );
        assert_eq!(
            resolve_codex_on_path(
                Some(Path::new("codex")),
                Some(path_with_empty_entry.as_os_str()),
                OperatingSystem::Windows,
            ),
            Some(exe)
        );
    }

    #[test]
    fn windows_default_codex_path_diagnostic_is_unknown_not_a_path_only_guess() {
        let root = TestRoot::new();
        let mut input = test_input(OperatingSystem::Windows, Architecture::X86_64, &root);
        let path_dir = root.path().join("fake-runtime-path");
        fs::create_dir(&path_dir).unwrap();
        create_file(&path_dir.join("codex.exe"), b"never launched", true);
        input.codex_source = CodexSource::Path;
        input.codex_path = Some(PathBuf::from("codex"));
        input.path_environment = Some(path_dir.clone().into_os_string());
        let report = build_report(input);
        assert_eq!(
            report.dependencies.codex_cli.file_status,
            CodexFileStatus::Unknown
        );
        assert_eq!(
            report.dependencies.codex_cli.executable_status,
            DiagnosticStatus::Unknown
        );
        assert_eq!(
            report.capabilities.goal_model_execution.status,
            DiagnosticStatus::Unknown
        );
        assert_eq!(
            report.capabilities.mcp_transport.status,
            DiagnosticStatus::Pass
        );
        assert_eq!(report.summary.exit_code, ExitCode::Success);
        let serialized = serde_json::to_string(&report).unwrap();
        assert!(!serialized.contains(&path_dir.to_string_lossy().to_string()));
    }

    #[test]
    fn explicit_version_probes_are_refused_without_a_process_attempt() {
        let root = TestRoot::new();
        let mut input = test_input(OperatingSystem::Windows, Architecture::X86_64, &root);
        input.probe_codex_requested = true;
        input.probe_bubblewrap_requested = true;
        let report = build_report(input);
        assert!(report.dependencies.codex_cli.version_probe.requested);
        assert!(!report.dependencies.codex_cli.version_probe.attempted);
        assert_eq!(
            report.dependencies.codex_cli.version_probe.status,
            DiagnosticStatus::Unknown
        );
        assert_eq!(
            report.dependencies.codex_cli.version_probe.reason_code,
            ReasonCode::CodexCliVersionProbeRefused
        );
        assert!(!report.dependencies.bubblewrap.version_probe.attempted);
        assert_eq!(
            report.dependencies.bubblewrap.version_probe.status,
            DiagnosticStatus::Unknown
        );
    }

    #[test]
    fn nul_in_codex_override_is_malformed_and_does_not_fall_through() {
        let root = TestRoot::new();
        let mut input = test_input(OperatingSystem::Windows, Architecture::X86_64, &root);
        input.codex_source = CodexSource::LocalMcpCodexCliPath;
        input.codex_path = Some(root.path().join("invalid\0codex"));
        let report = build_report(input);
        assert_eq!(
            report.dependencies.codex_cli.executable_status,
            DiagnosticStatus::Block
        );
        assert_eq!(
            report
                .checks
                .iter()
                .find(|check| check.id == "goal_model.codex_cli")
                .unwrap()
                .reason_code,
            ReasonCode::CodexCliPathInvalid
        );
    }

    #[test]
    fn malformed_codex_path_is_blocked_without_exposing_the_path() {
        let root = TestRoot::new();
        let mut input = test_input(OperatingSystem::Windows, Architecture::X86_64, &root);
        let secret_path = root.path().join("credential-path-sentinel");
        input.codex_source = CodexSource::LocalMcpCodexCliPath;
        input.codex_path = Some(secret_path.clone());
        let report = build_report(input);
        assert_eq!(
            report.dependencies.codex_cli.executable_status,
            DiagnosticStatus::Block
        );
        let serialized = serde_json::to_string(&report).unwrap();
        assert!(!serialized.contains(&secret_path.to_string_lossy().to_string()));
        assert!(!serialized.contains("credential-path-sentinel"));
    }

    #[test]
    fn required_unknown_plus_optional_block_keeps_exit_two() {
        let checks = [
            check_for(DiagnosticStatus::Unknown, Requirement::Required),
            check_for(DiagnosticStatus::Block, Requirement::Optional),
        ];
        assert_eq!(
            summarize_checks(&checks).exit_code,
            ExitCode::RequiredUnknown
        );
    }

    #[test]
    fn required_block_plus_unknown_keeps_exit_one() {
        let checks = [
            check_for(DiagnosticStatus::Block, Requirement::Required),
            check_for(DiagnosticStatus::Unknown, Requirement::Required),
        ];
        assert_eq!(summarize_checks(&checks).exit_code, ExitCode::RequiredBlock);
    }

    #[test]
    fn exit_code_values_match_the_public_contract() {
        assert_eq!(ExitCode::Success.value(), 0);
        assert_eq!(ExitCode::RequiredBlock.value(), 1);
        assert_eq!(ExitCode::RequiredUnknown.value(), 2);
        assert_eq!(ExitCode::UsageError.value(), 64);
        assert_eq!(ExitCode::InternalError.value(), 70);
    }

    #[test]
    fn static_check_timeout_returns_a_complete_bounded_unknown_report() {
        let result = run_with_deadline(Duration::from_millis(1), || {
            std::thread::sleep(Duration::from_millis(50));
            "late static observation"
        });
        assert_eq!(result, Err(DeadlineFailure::TimedOut));

        let report = timeout_report(
            HostFacts {
                os: OperatingSystem::Windows,
                architecture: Architecture::X86_64,
            },
            DoctorOptions {
                json: true,
                probe_bubblewrap_version: true,
                probe_codex_version: true,
            },
        );
        assert_eq!(report.summary.required_status, DiagnosticStatus::Unknown);
        assert_eq!(report.summary.exit_code, ExitCode::RequiredUnknown);
        assert!(report.checks.iter().any(|check| {
            check.reason_code == ReasonCode::DoctorStaticCheckTimeout
                && check.status == DiagnosticStatus::Unknown
        }));
        assert!(!report.dependencies.codex_cli.version_probe.attempted);
        assert!(!report.dependencies.bubblewrap.version_probe.attempted);
        let output = json_output(&report).expect("timeout still yields a bounded whole report");
        assert!(output.len() <= JSON_OUTPUT_LIMIT);
    }

    #[test]
    fn safe_observed_serialization_never_emits_arbitrary_json_values() {
        let root = TestRoot::new();
        let report = build_report(test_input(
            OperatingSystem::Windows,
            Architecture::X86_64,
            &root,
        ));
        let value = serde_json::to_value(report).unwrap();
        for check in value["checks"].as_array().unwrap() {
            assert!(check["observed"].is_null() || check["observed"].is_boolean());
        }
    }

    #[test]
    fn report_validation_rejects_capability_status_drift_from_checks() {
        let root = TestRoot::new();
        let mut report = build_report(test_input(
            OperatingSystem::Windows,
            Architecture::X86_64,
            &root,
        ));
        assert!(report.validate().is_ok());
        report.capabilities.mcp_transport.status = DiagnosticStatus::Block;
        assert!(report.validate().is_err());
        assert!(json_output(&report).is_err());
    }

    #[test]
    fn report_validation_rejects_check_requirement_drift_that_hides_a_required_blocker() {
        let root = TestRoot::new();
        let mut report = build_report(test_input(
            OperatingSystem::Windows,
            Architecture::X86_64,
            &root,
        ));
        let helper_check = report
            .checks
            .iter_mut()
            .find(|check| check.id == "installation.helper.atomic_publish")
            .unwrap();
        helper_check.status = DiagnosticStatus::Block;
        helper_check.requirement = Requirement::Optional;
        report.capabilities.atomic_publication = capability(
            &report.checks,
            CapabilityId::AtomicPublication,
            Requirement::Required,
        );
        report.summary = summarize_checks(&report.checks);
        assert_eq!(report.summary.exit_code, ExitCode::Success);
        assert_eq!(
            report.capabilities.atomic_publication.status,
            DiagnosticStatus::Block
        );
        assert!(report.validate().is_err());
    }

    #[test]
    fn report_json_is_single_bounded_object_with_trailing_newline() {
        let root = TestRoot::new();
        let report = build_report(test_input(
            OperatingSystem::Windows,
            Architecture::X86_64,
            &root,
        ));
        let output = json_output(&report).unwrap();
        assert!(output.ends_with(b"}\n"));
        assert_eq!(output.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert!(output.len() <= JSON_OUTPUT_LIMIT);
    }

    #[test]
    fn output_limit_refusal_does_not_write_a_partial_buffer() {
        let mut stdout = Vec::new();
        let payload = b"too large";
        let result = write_bounded(&mut stdout, payload, 2);
        assert!(result.is_err());
        assert!(stdout.is_empty());
    }

    #[test]
    fn output_and_check_limits_are_enforced_before_writing() {
        let root = TestRoot::new();
        let mut report = build_report(test_input(
            OperatingSystem::Windows,
            Architecture::X86_64,
            &root,
        ));
        report.checks.resize_with(MAX_CHECKS + 1, || {
            check_for(DiagnosticStatus::Pass, Requirement::Required)
        });
        assert!(json_output(&report).is_err());
    }

    fn fixture_status(value: &serde_json::Value) -> DiagnosticStatus {
        match value.as_str().expect("fixture status string") {
            "PASS" => DiagnosticStatus::Pass,
            "WARN" => DiagnosticStatus::Warn,
            "BLOCK" => DiagnosticStatus::Block,
            "UNKNOWN" => DiagnosticStatus::Unknown,
            invalid => panic!("unknown fixture status: {invalid}"),
        }
    }

    #[test]
    fn fixture_summaries_and_exit_codes_are_semantically_consistent() {
        for (fixture, expected_status, expected_exit) in [
            (
                include_str!("../docs/examples/doctor-v1-pass.json"),
                "PASS",
                0,
            ),
            (
                include_str!("../docs/examples/doctor-v1-warn.json"),
                "WARN",
                0,
            ),
            (
                include_str!("../docs/examples/doctor-v1-block.json"),
                "BLOCK",
                1,
            ),
            (
                include_str!("../docs/examples/doctor-v1-unknown.json"),
                "UNKNOWN",
                2,
            ),
        ] {
            let value: serde_json::Value = serde_json::from_str(fixture).unwrap();
            let checks = value["checks"].as_array().unwrap();
            let summary = &value["summary"];
            let mut counts = [0usize; 4];
            let mut required = Vec::new();
            let mut advisory = Vec::new();
            let mut runtime = Vec::new();
            for check in checks {
                let status = fixture_status(&check["status"]);
                let index = match status {
                    DiagnosticStatus::Pass => 0,
                    DiagnosticStatus::Warn => 1,
                    DiagnosticStatus::Block => 2,
                    DiagnosticStatus::Unknown => 3,
                };
                counts[index] += 1;
                if check["applicability"] == "applicable" {
                    match check["requirement"].as_str() {
                        Some("required") => required.push(status),
                        Some("optional") => advisory.push(status),
                        _ => {}
                    }
                }
                if matches!(
                    check["evidence_type"].as_str(),
                    Some("runtime_read_only" | "not_run")
                ) {
                    runtime.push(status);
                }
            }
            let aggregate = aggregate_status(required.iter().copied());
            let advisory_aggregate = aggregate_status(advisory.iter().copied());
            let runtime_aggregate = aggregate_status(runtime.iter().copied());
            let blockers = required
                .iter()
                .filter(|status| **status == DiagnosticStatus::Block)
                .count();
            let unknowns = required
                .iter()
                .filter(|status| **status == DiagnosticStatus::Unknown)
                .count();
            let exit = if blockers > 0 {
                1
            } else if unknowns > 0 || required.is_empty() {
                2
            } else {
                0
            };
            assert_eq!(summary["status"], expected_status);
            assert_eq!(fixture_status(&summary["status"]), aggregate);
            assert_eq!(fixture_status(&summary["required_status"]), aggregate);
            assert_eq!(
                fixture_status(&summary["advisory_status"]),
                advisory_aggregate
            );
            assert_eq!(
                fixture_status(&summary["runtime_status"]),
                runtime_aggregate
            );
            assert_eq!(summary["exit_code"], expected_exit);
            assert_eq!(exit, expected_exit);
            assert_eq!(summary["checks_total"], checks.len());
            assert_eq!(summary["required_blockers"], blockers);
            assert_eq!(summary["required_unknowns"], unknowns);
            assert_eq!(summary["counts"]["PASS"], counts[0]);
            assert_eq!(summary["counts"]["WARN"], counts[1]);
            assert_eq!(summary["counts"]["BLOCK"], counts[2]);
            assert_eq!(summary["counts"]["UNKNOWN"], counts[3]);
            assert!(checks.len() <= MAX_CHECKS);

            let sandbox_requirement = if value["platform"]["operating_system"] == "windows" {
                "not_applicable"
            } else {
                "required"
            };
            for (capability_name, expected_requirement) in [
                ("mcp_transport", "required"),
                ("session_management", "required"),
                ("sandboxed_execution", sandbox_requirement),
                ("atomic_publication", "required"),
                ("goal_model_execution", "optional"),
            ] {
                let capability = &value["capabilities"][capability_name];
                assert_eq!(capability["requirement"], expected_requirement);
                if expected_requirement == "not_applicable" {
                    assert_eq!(capability["status"], "UNKNOWN");
                    assert_eq!(capability["formal_status"], "UNKNOWN");
                    assert_eq!(capability["runtime_status"], "UNKNOWN");
                    assert_eq!(
                        capability["reason_codes"],
                        serde_json::json!(["SANDBOX_PARITY_NOT_APPLICABLE"])
                    );
                    continue;
                }
                let related: Vec<&serde_json::Value> = checks
                    .iter()
                    .filter(|check| {
                        check["capability"] == capability_name
                            && check["applicability"] == "applicable"
                    })
                    .collect();
                assert_eq!(
                    fixture_status(&capability["status"]),
                    aggregate_status(related.iter().map(|check| fixture_status(&check["status"])))
                );
                assert_eq!(
                    fixture_status(&capability["formal_status"]),
                    aggregate_status(
                        related
                            .iter()
                            .filter(|check| {
                                matches!(
                                    check["evidence_type"].as_str(),
                                    Some("static_observation" | "formal_contract")
                                )
                            })
                            .map(|check| fixture_status(&check["status"])),
                    )
                );
                assert_eq!(
                    fixture_status(&capability["runtime_status"]),
                    aggregate_status(
                        related
                            .iter()
                            .filter(|check| {
                                matches!(
                                    check["evidence_type"].as_str(),
                                    Some("runtime_read_only" | "not_run")
                                )
                            })
                            .map(|check| fixture_status(&check["status"])),
                    )
                );
                let expected_reasons: BTreeSet<String> = related
                    .iter()
                    .map(|check| check["reason_code"].as_str().unwrap().to_owned())
                    .collect();
                let actual_reasons: BTreeSet<String> = capability["reason_codes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|reason| reason.as_str().unwrap().to_owned())
                    .collect();
                assert_eq!(actual_reasons, expected_reasons);
            }
        }
    }

    fn write_bounded(writer: &mut impl Write, output: &[u8], limit: usize) -> Result<(), ()> {
        if output.len() > limit {
            return Err(());
        }
        writer.write_all(output).map_err(|_| ())
    }
}
