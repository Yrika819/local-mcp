//! E2: Model Accounting Foundation — host-side measurement of model usage.
//!
//! Purpose: make Codex consumption observable and provide the measurement
//! groundwork that later lets the External Backend *prove* zero Codex spawns
//! (ADR-08). This module is a pure observation layer:
//!
//! * It never changes control flow. Recording must not fail an invocation, and
//!   a poisoned or busy registry must never weaken an authority boundary.
//! * It never stores prompts, responses, or secrets — only counts, byte
//!   lengths, and durations.
//! * It does not estimate token counts. Call/response bytes and durations are
//!   host-observed facts; token usage is not observable here.
//!
//! Definitions (pinned by tests):
//!
//! * `codex_spawn_count`: a Codex process launch that passed the approval gate
//!   (counted in `joined_model_invocation` right before the process run). A
//!   denied or pre-flight-failed call contributes 0.
//! * `calls`: completed transport invocations per role, counted at
//!   `GoalModelAgent::invoke` (the only production entrypoint to
//!   `ModelTransport`). The prompt-limit rejection never reaches a transport
//!   and is not a call.
//! * `successes` / `failures`: transport-level outcome. `timeouts` and
//!   `approval_denied` are separate failure kinds and are also counted inside
//!   `failures`. Semantic validation of model output is the role validators'
//!   job and is not duplicated here.
//! * `retries`: a completed invocation whose immediately preceding completed
//!   invocation of the same role failed. This is a transport-level retry proxy;
//!   scheduler-driven Task retries each produce their own invocation and are
//!   visible as such.
//!
//! Test isolation: the process-wide registry is the production surface. In test
//! builds every recording is additionally mirrored into a thread-local
//! registry, and the tests assert against that mirror only, so they stay
//! deterministic while unrelated tests record into the shared registry in
//! parallel.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;

use crate::agent::{AgentError, ModelInvocationOutput, ModelRole};

/// How one model invocation ended, at the transport boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CallOutcome {
    Success,
    Timeout,
    ApprovalDenied,
    Failure,
}

impl CallOutcome {
    fn classify(result: &Result<ModelInvocationOutput, AgentError>) -> Self {
        match result {
            Ok(_) => Self::Success,
            Err(AgentError::Timeout) => Self::Timeout,
            Err(AgentError::ApprovalDenied) => Self::ApprovalDenied,
            Err(_) => Self::Failure,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "E2 measurement surface: tests consume it today; production reporting integration is a later Owner decision"
    )
)]
pub(crate) struct RoleMetrics {
    pub(crate) calls: u64,
    pub(crate) successes: u64,
    pub(crate) failures: u64,
    pub(crate) timeouts: u64,
    pub(crate) approval_denied: u64,
    pub(crate) retries: u64,
    pub(crate) request_bytes: u64,
    pub(crate) response_bytes: u64,
    pub(crate) duration_ms_total: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "E2 measurement surface: tests consume it today; production reporting integration is a later Owner decision"
    )
)]
pub(crate) struct ModelAccountingSnapshot {
    pub(crate) codex_spawn_count: u64,
    pub(crate) roles: BTreeMap<String, RoleMetrics>,
}

#[derive(Default)]
struct Registry {
    codex_spawn_count: u64,
    roles: BTreeMap<&'static str, RoleMetrics>,
    last_call_failed: BTreeMap<&'static str, bool>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry::new_const());

impl Registry {
    const fn new_const() -> Self {
        Self {
            codex_spawn_count: 0,
            roles: BTreeMap::new(),
            last_call_failed: BTreeMap::new(),
        }
    }
}

/// Lock the process-wide registry with poison recovery.
///
/// Recording is observation only: a panic between two counters must not
/// permanently disable accounting, and must never propagate into the authority
/// path that is being measured.
fn lock_registry() -> MutexGuard<'static, Registry> {
    REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
std::thread_local! {
    // Per-test-thread mirror of the registry. Tests assert against this and
    // stay deterministic while unrelated tests record into `REGISTRY` in
    // parallel.
    static TEST_REGISTRY: std::cell::RefCell<Registry> =
        std::cell::RefCell::new(Registry::default());
}

fn accumulate(
    registry: &mut Registry,
    role: ModelRole,
    request_bytes: usize,
    duration: Duration,
    outcome: CallOutcome,
    response_bytes: usize,
) {
    let is_retry = registry
        .last_call_failed
        .get(role.as_str())
        .copied()
        .unwrap_or(false);
    let metrics = registry.roles.entry(role.as_str()).or_default();
    metrics.calls += 1;
    match outcome {
        CallOutcome::Success => metrics.successes += 1,
        CallOutcome::Timeout => {
            metrics.failures += 1;
            metrics.timeouts += 1;
        }
        CallOutcome::ApprovalDenied => {
            metrics.failures += 1;
            metrics.approval_denied += 1;
        }
        CallOutcome::Failure => metrics.failures += 1,
    }
    if is_retry {
        metrics.retries += 1;
    }
    metrics.request_bytes = metrics.request_bytes.saturating_add(request_bytes as u64);
    metrics.response_bytes = metrics.response_bytes.saturating_add(response_bytes as u64);
    metrics.duration_ms_total = metrics
        .duration_ms_total
        .saturating_add(duration.as_millis() as u64);
    registry
        .last_call_failed
        .insert(role.as_str(), outcome != CallOutcome::Success);
}

fn count_spawn(registry: &mut Registry) {
    registry.codex_spawn_count += 1;
}

fn snapshot_of(registry: &Registry) -> ModelAccountingSnapshot {
    ModelAccountingSnapshot {
        codex_spawn_count: registry.codex_spawn_count,
        roles: registry
            .roles
            .iter()
            .map(|(role, metrics)| ((*role).to_owned(), metrics.clone()))
            .collect(),
    }
}

/// Record one completed model invocation.
///
/// Never panics and never returns an error: accounting must not be able to
/// fail, approve, or deny anything.
pub(crate) fn record_invocation(
    role: ModelRole,
    request_bytes: usize,
    duration: Duration,
    result: &Result<ModelInvocationOutput, AgentError>,
    response_bytes: usize,
) {
    let outcome = CallOutcome::classify(result);
    accumulate(
        &mut lock_registry(),
        role,
        request_bytes,
        duration,
        outcome,
        response_bytes,
    );
    #[cfg(test)]
    TEST_REGISTRY.with(|registry| {
        accumulate(
            &mut registry.borrow_mut(),
            role,
            request_bytes,
            duration,
            outcome,
            response_bytes,
        )
    });
}

/// Record one Codex process launch that passed the approval gate.
pub(crate) fn record_codex_spawn() {
    count_spawn(&mut lock_registry());
    #[cfg(test)]
    TEST_REGISTRY.with(|registry| count_spawn(&mut registry.borrow_mut()));
}

/// A stable, secret-free view for tests, future `goal_status` integration, or
/// reporting surfaces decided later by the Owner.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "E2 measurement surface: tests consume it today; production reporting integration is a later Owner decision"
    )
)]
pub(crate) fn snapshot() -> ModelAccountingSnapshot {
    snapshot_of(&lock_registry())
}

/// The deterministic per-test-thread view. Tests assert against this mirror so
/// unrelated tests recording into the shared registry cannot race them.
#[cfg(test)]
pub(crate) fn snapshot_for_test() -> ModelAccountingSnapshot {
    TEST_REGISTRY.with(|registry| snapshot_of(&registry.borrow()))
}

/// Reset the per-test-thread mirror. Test-only observation support.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    TEST_REGISTRY.with(|registry| *registry.borrow_mut() = Registry::default());
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::agent::{GoalModelAgent, ModelInvocation, ModelTransport};

    use super::*;

    fn ok_output() -> ModelInvocationOutput {
        ModelInvocationOutput::new(b"ok".to_vec(), String::new(), 0)
    }

    /// Serialize-inspect helper: the snapshot must never contain prompt or
    /// response content.
    fn serialized_snapshot() -> String {
        serde_json::to_string(&snapshot_for_test()).unwrap()
    }

    #[test]
    fn records_success_bytes_and_duration_per_role() {
        reset_for_test();
        record_invocation(
            ModelRole::Planner,
            100,
            Duration::from_millis(250),
            &Ok(ok_output()),
            2,
        );
        let snapshot = snapshot_for_test();
        assert_eq!(snapshot.codex_spawn_count, 0);
        let planner = &snapshot.roles["PLANNER"];
        assert_eq!(planner.calls, 1);
        assert_eq!(planner.successes, 1);
        assert_eq!(planner.failures, 0);
        assert_eq!(planner.request_bytes, 100);
        assert_eq!(planner.response_bytes, 2);
        assert_eq!(planner.duration_ms_total, 250);
    }

    #[test]
    fn separates_timeout_and_approval_denied_from_plain_failure() {
        reset_for_test();
        record_invocation(
            ModelRole::Readonly,
            1,
            Duration::from_millis(1),
            &Err(AgentError::Timeout),
            0,
        );
        record_invocation(
            ModelRole::Readonly,
            1,
            Duration::from_millis(1),
            &Err(AgentError::ApprovalDenied),
            0,
        );
        record_invocation(
            ModelRole::Readonly,
            1,
            Duration::from_millis(1),
            &Err(AgentError::NonZeroExit),
            0,
        );
        let readonly = &snapshot_for_test().roles["READONLY"];
        assert_eq!(readonly.calls, 3);
        assert_eq!(readonly.failures, 3);
        assert_eq!(readonly.timeouts, 1);
        assert_eq!(readonly.approval_denied, 1);
        assert_eq!(readonly.successes, 0);
    }

    #[test]
    fn counts_invocations_after_a_failure_as_retries() {
        reset_for_test();
        record_invocation(
            ModelRole::Writer,
            1,
            Duration::from_millis(1),
            &Err(AgentError::TransportFailure),
            0,
        );
        record_invocation(
            ModelRole::Writer,
            1,
            Duration::from_millis(1),
            &Ok(ok_output()),
            2,
        );
        record_invocation(
            ModelRole::Writer,
            1,
            Duration::from_millis(1),
            &Ok(ok_output()),
            2,
        );
        let writer = &snapshot_for_test().roles["WRITER"];
        assert_eq!(writer.calls, 3);
        assert_eq!(
            writer.retries, 1,
            "only the call directly after a failure is a retry"
        );
    }

    #[test]
    fn counts_codex_spawns_independently_of_calls() {
        reset_for_test();
        record_codex_spawn();
        record_codex_spawn();
        record_invocation(
            ModelRole::Reviewer,
            1,
            Duration::from_millis(1),
            &Ok(ok_output()),
            1,
        );
        let snapshot = snapshot_for_test();
        assert_eq!(snapshot.codex_spawn_count, 2);
        assert_eq!(snapshot.roles["REVIEWER"].calls, 1);
    }

    #[test]
    fn snapshot_carries_no_prompt_or_response_content() {
        reset_for_test();
        const SECRET_MARKER: &str = "SECRET_MARKER_DO_NOT_PERSIST";
        let prompt = format!("prompt containing {SECRET_MARKER}");
        record_invocation(
            ModelRole::Planner,
            prompt.len(),
            Duration::from_millis(1),
            &Ok(ModelInvocationOutput::new(
                format!("response containing {SECRET_MARKER}").into_bytes(),
                String::new(),
                0,
            )),
            32,
        );
        let rendered = serialized_snapshot();
        assert!(
            !rendered.contains(SECRET_MARKER),
            "accounting must never persist prompt or response content: {rendered}"
        );
    }

    #[test]
    fn poisoned_registry_still_records_and_never_fails_the_authority_path() {
        reset_for_test();
        // Poison the process-wide lock: a panic between two counters.
        let _ = std::panic::catch_unwind(|| {
            let _guard = lock_registry();
            panic!("poison the accounting registry on purpose");
        });
        // Recording recovers from the poison instead of panicking or disabling.
        record_invocation(
            ModelRole::Planner,
            1,
            Duration::from_millis(1),
            &Err(AgentError::ApprovalDenied),
            0,
        );
        record_codex_spawn();
        // The process-wide snapshot stays readable too (poison recovered).
        let _ = snapshot();
        let snapshot = snapshot_for_test();
        assert_eq!(snapshot.roles["PLANNER"].approval_denied, 1);
        assert_eq!(snapshot.codex_spawn_count, 1);
    }

    struct ScriptedTransport {
        result: Result<ModelInvocationOutput, AgentError>,
    }

    impl ModelTransport for ScriptedTransport {
        fn invoke(&self, _request: &ModelInvocation) -> Result<ModelInvocationOutput, AgentError> {
            self.result.clone()
        }
    }

    fn scripted_agent(result: Result<ModelInvocationOutput, AgentError>) -> GoalModelAgent {
        GoalModelAgent::with_transport(
            "g0e2-mock-transport",
            std::env::temp_dir(),
            Arc::new(ScriptedTransport { result }),
        )
    }

    /// Mock Transport success / failure / timeout counted through the real
    /// `GoalModelAgent::invoke` wiring (the single production entrypoint to a
    /// `ModelTransport`).
    #[test]
    fn mock_transport_invocations_are_counted_through_the_agent() {
        reset_for_test();
        scripted_agent(Ok(ok_output()))
            .invoke_at_session_cwd(ModelRole::Planner, "prompt".to_owned())
            .unwrap();
        assert!(
            scripted_agent(Err(AgentError::Timeout))
                .invoke_at_session_cwd(ModelRole::Readonly, "prompt".to_owned())
                .is_err()
        );
        assert!(
            scripted_agent(Err(AgentError::ApprovalDenied))
                .invoke_at_session_cwd(ModelRole::Writer, "prompt".to_owned())
                .is_err()
        );
        let snapshot = snapshot_for_test();
        assert_eq!(snapshot.roles["PLANNER"].successes, 1);
        assert_eq!(
            snapshot.roles["PLANNER"].request_bytes,
            "prompt".len() as u64
        );
        assert_eq!(snapshot.roles["READONLY"].timeouts, 1);
        assert_eq!(snapshot.roles["WRITER"].approval_denied, 1);
    }

    /// Accounting is observation only: it can never turn an approval denial
    /// into an allow or otherwise change the call outcome.
    #[test]
    fn accounting_cannot_change_call_outcomes() {
        reset_for_test();
        let outcome = scripted_agent(Err(AgentError::ApprovalDenied))
            .invoke_at_session_cwd(ModelRole::Reviewer, "prompt".to_owned());
        assert!(
            matches!(outcome, Err(AgentError::ApprovalDenied)),
            "accounting must never turn a denial into an allow"
        );
    }
}
