use std::path::{Path, PathBuf};

use anyhow::Result;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteKind {
    Operational,
    CompileRepair,
    RecoveryOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub kind: RouteKind,
    pub effort: Effort,
    pub recovery_only: bool,
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

pub fn route(blocker: &str, requires_code_change: bool, remote_side_effect: &str) -> Result<Route> {
    let lower = blocker.to_ascii_lowercase();

    if contains_any(
        &lower,
        &[
            "safety policy",
            "policy violation",
            "disallowed by policy",
            "not allowed by policy",
            "refused by policy",
            "content policy",
        ],
    ) {
        anyhow::bail!("policy/safety refusals are not eligible for Codex fallback");
    }

    if contains_any(
        &lower,
        &[
            "authority mismatch",
            "hash mismatch",
            "remote race",
            "push rejected",
            "workflow failed",
            "ci failed",
            "semantic mismatch",
        ],
    ) {
        anyhow::bail!("semantic/authority/remote failures require a hard stop, not fallback");
    }

    if remote_side_effect == "unknown" {
        return Ok(Route {
            kind: RouteKind::RecoveryOnly,
            effort: Effort::Low,
            recovery_only: true,
        });
    }

    if remote_side_effect != "none" && remote_side_effect != "not_started" {
        anyhow::bail!("remote_side_effect must be one of: none, not_started, unknown");
    }

    if looks_like_compile_error(&lower) {
        return Ok(Route {
            kind: RouteKind::CompileRepair,
            effort: Effort::Medium,
            recovery_only: false,
        });
    }

    if looks_operational(&lower) {
        return Ok(Route {
            kind: RouteKind::Operational,
            effort: if requires_code_change {
                Effort::Medium
            } else {
                Effort::Low
            },
            recovery_only: false,
        });
    }

    anyhow::bail!(
        "blocker is not a recognized operational/compile fallback case; hard-stop for manual classification"
    )
}

pub fn auto_operational_reason(
    command: &[String],
    stdout: &str,
    stderr: &str,
) -> Option<&'static str> {
    auto_operational_reason_with_enabled(command, stdout, stderr, automatic_enabled())
}

fn auto_operational_reason_with_enabled(
    command: &[String],
    stdout: &str,
    stderr: &str,
    enabled: bool,
) -> Option<&'static str> {
    if !enabled || command.is_empty() || has_remote_side_effect(command) {
        return None;
    }

    let executable = Path::new(&command[0])
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&command[0])
        .to_ascii_lowercase();
    if !matches!(
        executable.as_str(),
        "dotnet" | "cargo" | "git" | "gradle" | "gradlew" | "xcodebuild" | "swift"
    ) {
        return None;
    }

    let combined = format!(
        "{stdout}
{stderr}"
    )
    .to_ascii_lowercase();
    if contains_any(
        &combined,
        &[
            "safety policy",
            "policy violation",
            "disallowed by policy",
            "not allowed by policy",
            "refused by policy",
            "content policy",
        ],
    ) {
        return None;
    }

    if contains_any(
        &combined,
        &[
            "socketexception",
            "failed to bind",
            "cannot assign requested address",
            "network is unreachable",
            "operation not permitted",
            "unauthorizedaccessexception",
        ],
    ) {
        return Some("sandbox/environment restriction");
    }

    if combined.contains("permission denied")
        && contains_any(
            &combined,
            &[
                "socket",
                "localhost",
                "127.0.0.1",
                "vstest",
                "testhost",
                "/tmp/",
                "tmpdir",
            ],
        )
    {
        return Some("sandbox permission restriction");
    }

    None
}

pub fn codex_command(cwd: &Path, effort: Effort) -> Result<Vec<String>> {
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
        model(),
        "-c".to_owned(),
        format!("model_reasoning_effort={:?}", effort.as_str()),
        "--approve-for-me".to_owned(),
        "-C".to_owned(),
        cwd.to_string_lossy().into_owned(),
        "--color".to_owned(),
        "never".to_owned(),
        "-".to_owned(),
    ])
}

pub fn exact_command_prompt(cwd: &Path, command: &[String], blocker: &str) -> String {
    format!(
        r#"AUTOMATIC LOCAL-MCP → CODEX CLI OPERATIONAL FALLBACK

This handoff exists only because the local MCP sandbox could not execute a command due to an operational environment restriction.

Repository / cwd:
{cwd}

Blocked command argv:
{command:?}

Observed blocker:
{blocker}

Execution profile:
- Model: {model}
- Reasoning effort: low
- Codex's normal workspace-write sandbox + automatic approval review remains enabled.

Contract:
1. Read the applicable Codex/repository instructions (CODEX_SKILLS.md, AGENTS.md, and other applicable repository-local instructions) before acting.
2. Re-check the current repository identity/state before running anything.
3. Run the exact blocked command, or the minimum semantically-equivalent invocation required by the host environment.
4. Do NOT modify source files.
5. Do NOT commit, push, dispatch CI, publish, reset, rebase, or repeat any remote side effect.
6. If the command itself produces a real compile/test/semantic failure, report that failure and stop. Do not fix it in this execution-only fallback.
7. Do not attempt to bypass any policy or safety refusal.

Return the exact command result, exit status, and whether the fallback itself succeeded.
"#,
        cwd = cwd.display(),
        command = command,
        blocker = blocker,
        model = model(),
    )
}

pub fn handoff_prompt(
    cwd: &Path,
    task: &str,
    blocker: &str,
    phase: Option<&str>,
    route: Route,
) -> String {
    let mode = if route.recovery_only {
        "RECOVERY ONLY"
    } else {
        "CONTINUATION"
    };
    let recovery = if route.recovery_only {
        r#"
A remote side effect may already have happened.
Do NOT repeat commit/push/workflow_dispatch/publication or any other non-idempotent action.
First recover live/local state and determine whether the side effect already occurred.
Only continue beyond recovery if absence is proven and the original task explicitly authorizes that action.
"#
    } else {
        ""
    };

    format!(
        r#"LOCAL-MCP → CODEX CLI FALLBACK HANDOFF

Mode:
{mode}

Repository / cwd:
{cwd}

Original phase:
{phase}

Observed blocker:
{blocker}

Execution profile:
- Model: {model}
- Reasoning effort: {effort}
- Codex's normal workspace-write sandbox + automatic approval review remains enabled.

Original task / continuation contract:
{task}

Mandatory handoff rules:
1. Read the applicable Codex-specific and repository-local instructions first, including CODEX_SKILLS.md and AGENTS.md when present.
2. Re-verify branch, HEAD, upstream/base, worktree, and index before continuing.
3. Adopt already-completed evidence; do not redo work merely for ceremony.
4. Preserve unrelated dirty state.
5. Treat genuine test failures, authority mismatches, hash mismatches, remote races, CI failures, and policy/safety refusals as hard stops.
6. Do not attempt to bypass policy or safety controls.
{recovery}
"#,
        mode = mode,
        cwd = cwd.display(),
        phase = phase.unwrap_or("(unspecified)"),
        blocker = blocker,
        model = model(),
        effort = route.effort.as_str(),
        task = task,
        recovery = recovery,
    )
}

fn looks_like_compile_error(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let has_cs_code = bytes.windows(7).any(|window| {
        window[0..2].eq_ignore_ascii_case(b"cs")
            && window[2..6].iter().all(u8::is_ascii_digit)
            && matches!(window[6], b':' | b' ')
    });
    has_cs_code
        || contains_any(
            lower,
            &[
                "compiler error",
                "compilation error",
                "failed to compile",
                "could not compile",
            ],
        )
}

fn looks_operational(lower: &str) -> bool {
    contains_any(
        lower,
        &[
            "permission denied",
            "operation not permitted",
            "unauthorizedaccessexception",
            "socketexception",
            "failed to bind",
            "sandbox",
            "host-native",
            "host native",
            "timed out",
            "timeout",
            "session ended",
            "connection reset",
            "broken pipe",
        ],
    )
}

fn has_remote_side_effect(command: &[String]) -> bool {
    let joined = command
        .iter()
        .map(|part| part.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    contains_any(
        &joined,
        &[
            " git push",
            "git push",
            "workflow_dispatch",
            "workflow run",
            "gh api --method post",
            "gh api -x post",
            "gh pr create",
        ],
    )
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operational_no_code_change_routes_low() {
        let route = route("SocketException (13): Permission denied", false, "none").unwrap();
        assert_eq!(route.kind, RouteKind::Operational);
        assert_eq!(route.effort, Effort::Low);
    }

    #[test]
    fn operational_with_code_change_routes_medium() {
        let route = route("sandbox blocked write", true, "none").unwrap();
        assert_eq!(route.effort, Effort::Medium);
    }

    #[test]
    fn compiler_error_routes_medium() {
        let route = route("error CS0019: operator cannot be applied", true, "none").unwrap();
        assert_eq!(route.kind, RouteKind::CompileRepair);
        assert_eq!(route.effort, Effort::Medium);
    }

    #[test]
    fn uncertain_remote_side_effect_routes_recovery_only_low() {
        let route = route("connection reset", false, "unknown").unwrap();
        assert!(route.recovery_only);
        assert_eq!(route.effort, Effort::Low);
    }

    #[test]
    fn policy_and_semantic_failures_do_not_route() {
        assert!(route("blocked by safety policy", false, "none").is_err());
        assert!(route("authority mismatch", false, "none").is_err());
        assert!(route("CI failed", false, "none").is_err());
    }

    #[test]
    fn auto_detects_vstest_socket_denial_but_not_push() {
        let dotnet = vec!["/Users/example/.dotnet/dotnet".to_owned(), "test".to_owned()];
        assert_eq!(
            auto_operational_reason_with_enabled(
                &dotnet,
                "",
                "System.Net.Sockets.SocketException (13): Permission denied",
                false,
            ),
            None
        );
        assert_eq!(
            auto_operational_reason_with_enabled(
                &dotnet,
                "",
                "System.Net.Sockets.SocketException (13): Permission denied",
                true,
            ),
            Some("sandbox/environment restriction")
        );

        let push = vec![
            "git".to_owned(),
            "push".to_owned(),
            "origin".to_owned(),
            "main".to_owned(),
        ];
        assert_eq!(
            auto_operational_reason_with_enabled(&push, "", "Operation not permitted", true,),
            None
        );
    }
}
