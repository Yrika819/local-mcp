//! External Agent Backend V1 — host-owned model backend policy.
//!
//! `ModelBackend` is the *host* decision of who is allowed to produce model
//! work for a Goal:
//!
//! * `CODEX` — the existing synchronous Codex CLI path. This is the default and
//!   stays byte-for-byte compatible with every Goal written before this module
//!   existed.
//! * `EXTERNAL` — External Agent Backend V1. The host issues bounded model work
//!   over `goal_next` / `goal_submit` and **never** launches a Codex CLI
//!   process for the Goal (ADR-08).
//!
//! `HYBRID` is deliberately *not* representable: the architecture lists it as a
//! future mode behind a separate ADR, so V1 fails closed on the wire instead of
//! carrying a variant it cannot honor.
//!
//! The mode is an immutable, creation-time host policy. It is never derived from
//! a Goal's runtime state and it is never changed after the Goal exists: the
//! external-agent architecture forbids an implicit backend change
//! (ADR-01 / ADR-11). `Goal::with_creation_backend` is the only writer, and the
//! only caller is `goal_start`, and the E3-B API layer is the only reader of the
//! accessors.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The wire/durable spelling of a backend mode, including modes V1 refuses.
pub(crate) const REQUESTED_HYBRID: &str = "HYBRID";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ModelBackend {
    /// Existing Codex CLI orchestration. Default for every Goal.
    #[default]
    Codex,
    /// External Agent Backend V1: MCP client-driven model work, zero Codex
    /// spawns.
    External,
}

impl ModelBackend {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "CODEX",
            Self::External => "EXTERNAL",
        }
    }

    /// Serde `skip_serializing_if`: the default is omitted so a CODEX Goal
    /// remains readable by binaries that predate this field.
    pub(crate) fn is_default(&self) -> bool {
        matches!(self, Self::Codex)
    }

    pub(crate) fn is_codex(self) -> bool {
        matches!(self, Self::Codex)
    }

    pub(crate) fn is_external(self) -> bool {
        matches!(self, Self::External)
    }

    /// Parse a caller-supplied `model_backend` for `goal_start`.
    ///
    /// Unknown spellings and the not-yet-implemented `HYBRID` mode are refused
    /// rather than silently downgraded to `CODEX`: a caller that asked for a
    /// different execution model must never be handed the Codex path by
    /// accident (ADR-01).
    pub(crate) fn parse_requested(value: &str) -> Result<Self, ModelBackendError> {
        match value {
            "CODEX" => Ok(Self::Codex),
            "EXTERNAL" => Ok(Self::External),
            REQUESTED_HYBRID => Err(ModelBackendError::UnsupportedMode(value.to_owned())),
            _ => Err(ModelBackendError::UnknownMode(value.to_owned())),
        }
    }
}

impl fmt::Display for ModelBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ModelBackendError {
    UnknownMode(String),
    UnsupportedMode(String),
}

impl ModelBackendError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::UnknownMode(_) => "UNKNOWN_MODEL_BACKEND",
            Self::UnsupportedMode(_) => "UNSUPPORTED_MODEL_BACKEND",
        }
    }

    /// Redacted, bounded diagnostic. Echoes the caller's own value only, never
    /// durable Goal content, and never a list of accepted values that could be
    /// used to probe another session.
    pub(crate) fn message(&self) -> String {
        match self {
            Self::UnknownMode(value) => {
                format!("model_backend {value:?} is not a supported mode in V1")
            }
            Self::UnsupportedMode(value) => {
                format!("model_backend {value:?} is a future mode and is not implemented in V1")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_codex_and_is_omitted_from_serialization() {
        assert_eq!(ModelBackend::default(), ModelBackend::Codex);
        assert!(ModelBackend::Codex.is_default());
        assert!(!ModelBackend::External.is_default());
        assert!(!ModelBackend::Codex.is_external());
        assert!(ModelBackend::External.is_external());
    }

    #[test]
    fn requested_modes_round_trip_and_hybrid_is_refused() {
        assert_eq!(
            ModelBackend::parse_requested("CODEX").unwrap(),
            ModelBackend::Codex
        );
        assert_eq!(
            ModelBackend::parse_requested("EXTERNAL").unwrap(),
            ModelBackend::External
        );
        let hybrid = ModelBackend::parse_requested("HYBRID").unwrap_err();
        assert_eq!(hybrid.code(), "UNSUPPORTED_MODEL_BACKEND");
        let unknown = ModelBackend::parse_requested("codex").unwrap_err();
        assert_eq!(unknown.code(), "UNKNOWN_MODEL_BACKEND");
        // Near misses must never be silently accepted as CODEX.
        assert!(ModelBackend::parse_requested("external ").is_err());
        assert!(ModelBackend::parse_requested("").is_err());
        assert!(unknown.message().contains("codex"));
    }

    #[test]
    fn wire_spelling_is_stable() {
        assert_eq!(ModelBackend::Codex.as_str(), "CODEX");
        assert_eq!(ModelBackend::External.as_str(), "EXTERNAL");
        assert_eq!(
            serde_json::to_string(&ModelBackend::External).unwrap(),
            "\"EXTERNAL\""
        );
        assert_eq!(
            serde_json::from_str::<ModelBackend>("\"CODEX\"").unwrap(),
            ModelBackend::Codex
        );
    }
}
