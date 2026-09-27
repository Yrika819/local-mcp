//! Host-owned structured failure classification.
//!
//! Recovery policy must never depend on human-readable error prose. Every
//! durable attempt therefore carries an optional [`FailureClass`] produced at
//! the narrowest trustworthy host boundary (the worker/model adapter that
//! observed the failure), and every recovery decision reads that value.
//!
//! The legacy report constants below are the *only* string comparison in this
//! module. They exist exclusively so that Goals durably written before the
//! structured field existed can still be classified. They are exact-equality
//! matches against host-authored constants, never substring matching, and a
//! structured value always takes precedence over them.

use serde::{Deserialize, Serialize};

use crate::agent::AgentError;

/// Legacy read-only model timeout report, retained so pre-structured durable
/// Goals remain classifiable. See [`LEGACY_READONLY_TIMEOUT_REPORT`].
pub(crate) const LEGACY_READONLY_RESPONSE_LIMIT_REPORT: &str = "READONLY_BACKEND_ERROR: readonly model invocation failed: model response exceeded the host limit";

/// Legacy read-only model timeout report, retained so pre-structured durable
/// Goals remain classifiable. See [`FailureClass::for_attempt`].
pub(crate) const LEGACY_READONLY_TIMEOUT_REPORT: &str =
    "READONLY_BACKEND_ERROR: readonly model invocation failed: model invocation timed out";

/// Deterministic, host-owned classification of why an attempt failed.
///
/// The variants are deliberately structural rather than textual: recovery
/// policy branches on the class, never on the diagnostic detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum FailureClass {
    /// A bounded, unchanged replay of the identical Task shape may remain valid.
    TransientModelFailure,
    /// The model produced more output than the host will accept. The Task shape
    /// is deterministic-too-large; unchanged retry cannot succeed.
    HostOutputLimit,
    /// The model or host input exceeded the available context budget.
    ContextLimit,
    /// The declared Task scope is materially broader than one bounded worker can
    /// hold, regardless of the specific failure that surfaced it.
    TaskScopeTooBroad,
    /// The work ran but the produced result did not satisfy the Task contract.
    SemanticFailure,
    /// The host refused the invocation itself (approval, configuration). No
    /// decomposition can resolve an authority decision.
    AuthorityFailure,
    /// Platform or sandbox safety refused the work.
    PlatformSafety,
    /// The host cannot deterministically classify this failure.
    Unknown,
}

impl FailureClass {
    /// Classify a model-invocation failure at the worker boundary, where the
    /// typed [`AgentError`] is still in hand and no prose parsing is required.
    pub(crate) fn from_agent_error(error: AgentError) -> Self {
        match error {
            AgentError::ResponseTooLarge => Self::HostOutputLimit,
            AgentError::Timeout
            | AgentError::SpawnFailed
            | AgentError::NonZeroExit
            | AgentError::TransportFailure => Self::TransientModelFailure,
            AgentError::EmptyResponse => Self::SemanticFailure,
            AgentError::ApprovalDenied | AgentError::InvalidConfiguration => Self::AuthorityFailure,
            AgentError::ExecutableUnavailable | AgentError::Cancelled => Self::PlatformSafety,
        }
    }

    /// Classify a host-authored legacy worker report written before the
    /// structured field existed. Anything unrecognised is [`Self::Unknown`], so
    /// unclassifiable legacy evidence fails closed.
    pub(crate) fn from_legacy_worker_report(summary: &str) -> Self {
        if summary == LEGACY_READONLY_RESPONSE_LIMIT_REPORT {
            Self::HostOutputLimit
        } else if summary == LEGACY_READONLY_TIMEOUT_REPORT {
            Self::TransientModelFailure
        } else {
            Self::Unknown
        }
    }

    /// Whether replaying the *identical* Task shape may remain a valid option.
    ///
    /// A Task's scope and objective are immutable, so a Task whose failure is
    /// caused by its own size can never have a materially reduced scope on an
    /// unchanged retry.
    pub(crate) fn allows_unchanged_retry(self) -> bool {
        matches!(self, Self::TransientModelFailure | Self::SemanticFailure)
    }

    /// Whether this class means the Task *shape* is structurally invalid and
    /// must be decomposed rather than retried or merely reassessed.
    pub(crate) fn requires_decomposition(self) -> bool {
        matches!(
            self,
            Self::HostOutputLimit | Self::ContextLimit | Self::TaskScopeTooBroad
        )
    }

    /// Whether a generic failed-task replacement may be proposed for a trigger
    /// that failed this way.
    ///
    /// Only deterministic size/budget failures qualify. A transient or semantic
    /// failure keeps its bounded unchanged retry: routing it into replacement
    /// would manufacture a decomposition that repairs nothing, and authority,
    /// platform-safety, and unclassified failures can never be repaired by
    /// decomposition at all.
    pub(crate) fn allows_task_replacement(self) -> bool {
        self.requires_decomposition()
    }

    /// Whether a durable failed-task replan request may be recorded for a
    /// trigger that failed this way.
    pub(crate) fn allows_replan_request(self) -> bool {
        self.allows_task_replacement()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_agent_error_maps_to_exactly_one_structured_class() {
        let expected = [
            (AgentError::ApprovalDenied, FailureClass::AuthorityFailure),
            (
                AgentError::ExecutableUnavailable,
                FailureClass::PlatformSafety,
            ),
            (AgentError::SpawnFailed, FailureClass::TransientModelFailure),
            (AgentError::Timeout, FailureClass::TransientModelFailure),
            (AgentError::NonZeroExit, FailureClass::TransientModelFailure),
            (AgentError::EmptyResponse, FailureClass::SemanticFailure),
            (AgentError::ResponseTooLarge, FailureClass::HostOutputLimit),
            (
                AgentError::TransportFailure,
                FailureClass::TransientModelFailure,
            ),
            (AgentError::Cancelled, FailureClass::PlatformSafety),
            (
                AgentError::InvalidConfiguration,
                FailureClass::AuthorityFailure,
            ),
        ];
        for (error, class) in expected {
            assert_eq!(FailureClass::from_agent_error(error), class, "{error}");
        }
    }

    #[test]
    fn host_output_limit_forbids_unchanged_retry_and_requires_decomposition() {
        let class = FailureClass::HostOutputLimit;
        assert!(!class.allows_unchanged_retry());
        assert!(class.requires_decomposition());
        assert!(class.allows_task_replacement());
        assert!(class.allows_replan_request());
    }

    #[test]
    fn transient_model_failure_preserves_bounded_unchanged_retry() {
        let class = FailureClass::TransientModelFailure;
        assert!(class.allows_unchanged_retry());
        assert!(!class.requires_decomposition());
    }

    #[test]
    fn semantic_failure_keeps_bounded_retry_without_forcing_decomposition() {
        let class = FailureClass::SemanticFailure;
        assert!(class.allows_unchanged_retry());
        assert!(!class.requires_decomposition());
    }

    #[test]
    fn context_and_scope_breadth_forbid_unchanged_retry() {
        for class in [FailureClass::ContextLimit, FailureClass::TaskScopeTooBroad] {
            assert!(!class.allows_unchanged_retry(), "{class:?}");
            assert!(class.requires_decomposition(), "{class:?}");
        }
    }

    #[test]
    fn transient_and_semantic_failures_keep_their_retry_and_refuse_replacement() {
        for class in [
            FailureClass::TransientModelFailure,
            FailureClass::SemanticFailure,
        ] {
            assert!(class.allows_unchanged_retry(), "{class:?}");
            assert!(!class.requires_decomposition(), "{class:?}");
            assert!(!class.allows_task_replacement(), "{class:?}");
            assert!(!class.allows_replan_request(), "{class:?}");
        }
    }

    #[test]
    fn only_deterministic_size_failures_admit_replacement() {
        for class in [
            FailureClass::HostOutputLimit,
            FailureClass::ContextLimit,
            FailureClass::TaskScopeTooBroad,
        ] {
            assert!(class.allows_task_replacement(), "{class:?}");
            assert!(class.allows_replan_request(), "{class:?}");
        }
    }

    #[test]
    fn unknown_failure_fails_closed_for_replacement() {
        assert!(!FailureClass::Unknown.allows_unchanged_retry());
        assert!(!FailureClass::Unknown.allows_task_replacement());
        assert!(!FailureClass::Unknown.allows_replan_request());
    }

    #[test]
    fn authority_and_platform_failures_are_never_repaired_by_decomposition() {
        for class in [FailureClass::AuthorityFailure, FailureClass::PlatformSafety] {
            assert!(!class.allows_unchanged_retry(), "{class:?}");
            assert!(!class.requires_decomposition(), "{class:?}");
            assert!(!class.allows_task_replacement(), "{class:?}");
            assert!(!class.allows_replan_request(), "{class:?}");
        }
    }

    #[test]
    fn legacy_reports_classify_by_exact_equality_and_fail_closed() {
        assert_eq!(
            FailureClass::from_legacy_worker_report(LEGACY_READONLY_RESPONSE_LIMIT_REPORT),
            FailureClass::HostOutputLimit
        );
        assert_eq!(
            FailureClass::from_legacy_worker_report(LEGACY_READONLY_TIMEOUT_REPORT),
            FailureClass::TransientModelFailure
        );
        // Near-miss prose must not be promoted into a structural class.
        for hostile in [
            "",
            "READONLY_BACKEND_ERROR",
            "READONLY_BACKEND_ERROR: unrelated failure",
            "prefix READONLY_BACKEND_ERROR: readonly model invocation failed: model response exceeded the host limit",
            "READONLY_BACKEND_ERROR: readonly model invocation failed: model response exceeded the host limit suffix",
        ] {
            assert_eq!(
                FailureClass::from_legacy_worker_report(hostile),
                FailureClass::Unknown,
                "{hostile}"
            );
        }
    }

    #[test]
    fn class_serializes_as_a_stable_screaming_snake_case_enum() {
        for (class, text) in [
            (
                FailureClass::TransientModelFailure,
                "\"TRANSIENT_MODEL_FAILURE\"",
            ),
            (FailureClass::HostOutputLimit, "\"HOST_OUTPUT_LIMIT\""),
            (FailureClass::ContextLimit, "\"CONTEXT_LIMIT\""),
            (FailureClass::TaskScopeTooBroad, "\"TASK_SCOPE_TOO_BROAD\""),
            (FailureClass::SemanticFailure, "\"SEMANTIC_FAILURE\""),
            (FailureClass::AuthorityFailure, "\"AUTHORITY_FAILURE\""),
            (FailureClass::PlatformSafety, "\"PLATFORM_SAFETY\""),
            (FailureClass::Unknown, "\"UNKNOWN\""),
        ] {
            assert_eq!(serde_json::to_string(&class).unwrap(), text);
            assert_eq!(serde_json::from_str::<FailureClass>(text).unwrap(), class);
        }
    }
}
