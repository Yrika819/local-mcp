//! Durable publication quarantine (H5.1-B).
//!
//! # The defect this closes
//!
//! Before this module, "is a publication still able to write?" was derived from
//! the mutation journal: an intent in `APPLYING` meant unresolved, and recovery
//! rewrote that intent from *file observations* to `ReconciledPerformed`,
//! `Partial`, `Unknown` or `ReconciledNotPerformed`. The moment it did, the
//! journal predicate became false and revocation could finalize even though no
//! one had proved the helper process was gone. File state is not process
//! lifetime.
//!
//! This module stores the two facts separately and durably:
//!
//! * **Mutation outcome** stays in the existing `MutationIntent` journal:
//!   `NOT_PERFORMED` / `PERFORMED` / `PARTIAL` / `UNKNOWN`.
//! * **Publication quiescence** lives here, at Goal level, and is changed only
//!   by a process-liveness witness (the helper's OS file lock), never by a file
//!   observation.
//!
//! `Active` and `Unproven` are unresolved. Recovery may move `Active` to
//! `QuiescentVerified` only after it acquires the helper lock and durably writes
//! `ABORTED`; it may never move `Unproven` to `NotStarted`, and it never clears
//! the record because a file happens to match a preimage or a postimage.
//!
//! # What is intentionally absent
//!
//! There is no "clear quarantine" operation. An `Unproven` record with no lock
//! witness (for example a Goal migrated from schema 6 with an `APPLYING` intent)
//! cannot be proven quiescent from inside the same OS user's state, so it stays
//! unresolved. The Owner-facing procedure for such a Goal is investigation and a
//! fresh Goal, not a flag that declares an unknown process safe.

// Some accessors exist for tests, diagnostics, and the frozen evidence surface;
// the host deliberately reads only the authority-bearing fields on each path.
#![allow(dead_code)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::orchestrator_error::OrchestratorError;
use crate::publication_lock::{self, QuiescenceProbe};
use crate::task::{AttemptId, TaskId};

pub(crate) const PUBLICATION_QUARANTINE_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_PUBLICATION_QUARANTINE_HISTORY: usize = 64;
pub(crate) const MAX_QUARANTINE_ID_BYTES: usize = 128;
pub(crate) const MAX_QUARANTINE_DETAIL_BYTES: usize = 512;
pub(crate) const MAX_QUARANTINE_REASON_BYTES: usize = 256;
pub(crate) const MAX_HOST_GENERATION_BYTES: usize = 128;

/// Whether a publication can still write, independent of what the files look
/// like.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum PublicationQuiescence {
    /// No publication has ever been activated for this Goal.
    NotStarted,
    /// A helper was armed and may still be alive. No termination proof exists.
    Active,
    /// A helper ran (or may have run) and the host cannot prove it stopped.
    /// This is the fail-closed state after a crash or a failed probe.
    Unproven,
    /// The host acquired the helper's lock and durably aborted its token. No
    /// helper is in its publication critical section and no late helper can
    /// enter it.
    QuiescentVerified,
}

impl PublicationQuiescence {
    pub(crate) fn is_unresolved(self) -> bool {
        matches!(self, Self::Active | Self::Unproven)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "NOT_STARTED",
            Self::Active => "ACTIVE",
            Self::Unproven => "UNPROVEN",
            Self::QuiescentVerified => "QUIESCENT_VERIFIED",
        }
    }
}

/// Why quiescence was accepted. Only process-liveness evidence appears here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum PublicationTerminationKind {
    /// The host could prove the helper never entered its critical section.
    HelperNotStarted,
    /// The helper exited and the host acquired its lock (normal completion).
    HelperTreeQuiescent,
    /// A restarted or resumed host acquired the helper's lock and aborted it.
    RecoveryQuiescenceProbe,
    /// The probe could not prove quiescence.
    ProbeFailed,
    /// A schema-6 Goal carried an APPLYING intent and no lock witness existed.
    LegacyApplyingIntent,
}

impl PublicationTerminationKind {
    pub(crate) fn proves_quiescence(self) -> bool {
        matches!(
            self,
            Self::HelperNotStarted | Self::HelperTreeQuiescent | Self::RecoveryQuiescenceProbe
        )
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::HelperNotStarted => "HELPER_NOT_STARTED",
            Self::HelperTreeQuiescent => "HELPER_TREE_QUIESCENT",
            Self::RecoveryQuiescenceProbe => "RECOVERY_QUIESCENCE_PROBE",
            Self::ProbeFailed => "PROBE_FAILED",
            Self::LegacyApplyingIntent => "LEGACY_APPLYING_INTENT",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationActivation {
    goal_revision: u64,
    host_generation: String,
    activated_at: String,
}

impl PublicationActivation {
    pub(crate) fn goal_revision(&self) -> u64 {
        self.goal_revision
    }
    pub(crate) fn host_generation(&self) -> &str {
        &self.host_generation
    }
    pub(crate) fn activated_at(&self) -> &str {
        &self.activated_at
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationTerminationEvidence {
    kind: PublicationTerminationKind,
    observed_at: String,
    host_generation: String,
    detail: String,
}

impl PublicationTerminationEvidence {
    pub(crate) fn new(
        kind: PublicationTerminationKind,
        observed_at: impl Into<String>,
        host_generation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            observed_at: observed_at.into(),
            host_generation: host_generation.into(),
            detail: detail.into(),
        }
    }

    pub(crate) fn kind(&self) -> PublicationTerminationKind {
        self.kind
    }
    pub(crate) fn observed_at(&self) -> &str {
        &self.observed_at
    }
    pub(crate) fn host_generation(&self) -> &str {
        &self.host_generation
    }
    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationQuarantineTransition {
    state: PublicationQuiescence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation_id: Option<String>,
    at: String,
    reason: String,
}

impl PublicationQuarantineTransition {
    pub(crate) fn state(&self) -> PublicationQuiescence {
        self.state
    }
    pub(crate) fn operation_id(&self) -> Option<&str> {
        self.operation_id.as_deref()
    }
    pub(crate) fn at(&self) -> &str {
        &self.at
    }
    pub(crate) fn reason(&self) -> &str {
        &self.reason
    }
}

/// The durable quarantine record.
///
/// The authority-bearing fields have no serde default: a schema-7 Goal that
/// omits one is corrupt, not a weaker-but-valid Goal. Only the optional
/// identity/evidence fields default to `None`, because they are absent exactly
/// in `NOT_STARTED`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationQuarantine {
    schema_version: u32,
    state: PublicationQuiescence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attempt_id: Option<AttemptId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lock_path: Option<PathBuf>,
    /// Digest of the armed token. The raw token is only in the helper frame and
    /// in the host-derived lock file; it is not authority and is not copied into
    /// every Goal snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    activation: Option<PublicationActivation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    termination: Option<PublicationTerminationEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    history: Vec<PublicationQuarantineTransition>,
}

impl PublicationQuarantine {
    pub(crate) fn not_started() -> Self {
        Self {
            schema_version: PUBLICATION_QUARANTINE_SCHEMA_VERSION,
            state: PublicationQuiescence::NotStarted,
            operation_id: None,
            task_id: None,
            attempt_id: None,
            lock_path: None,
            token_sha256: None,
            activation: None,
            termination: None,
            history: Vec::new(),
        }
    }

    /// Migration constructor for a schema-6 Goal whose journal still says
    /// `APPLYING`.
    ///
    /// The old binary armed no lock, so there is no process-liveness witness and
    /// this record must stay unresolved. It is deliberately not converted to
    /// `NOT_STARTED`: the Goal may still have a live helper.
    pub(crate) fn legacy_applying(
        operation_id: String,
        task_id: TaskId,
        attempt_id: AttemptId,
        goal_revision: u64,
        at: String,
    ) -> Self {
        let mut record = Self::not_started();
        record.state = PublicationQuiescence::Unproven;
        record.operation_id = Some(operation_id.clone());
        record.task_id = Some(task_id);
        record.attempt_id = Some(attempt_id);
        record.activation = Some(PublicationActivation {
            goal_revision,
            host_generation: "migrated-schema-6".to_owned(),
            activated_at: at.clone(),
        });
        record.termination = Some(PublicationTerminationEvidence::new(
            PublicationTerminationKind::LegacyApplyingIntent,
            at.clone(),
            "migrated-schema-6",
            "schema-6 journal carried an APPLYING intent with no process-liveness witness",
        ));
        record.push_transition(
            PublicationQuiescence::Unproven,
            Some(operation_id),
            at,
            "migrated from schema 6 with an unresolved publication",
        );
        record
    }

    pub(crate) fn state(&self) -> PublicationQuiescence {
        self.state
    }

    pub(crate) fn is_unresolved(&self) -> bool {
        self.state.is_unresolved()
    }

    pub(crate) fn operation_id(&self) -> Option<&str> {
        self.operation_id.as_deref()
    }

    pub(crate) fn task_id(&self) -> Option<&TaskId> {
        self.task_id.as_ref()
    }

    pub(crate) fn attempt_id(&self) -> Option<&AttemptId> {
        self.attempt_id.as_ref()
    }

    pub(crate) fn lock_path(&self) -> Option<&PathBuf> {
        self.lock_path.as_ref()
    }

    pub(crate) fn token_sha256(&self) -> Option<&str> {
        self.token_sha256.as_deref()
    }

    pub(crate) fn activation(&self) -> Option<&PublicationActivation> {
        self.activation.as_ref()
    }

    pub(crate) fn termination(&self) -> Option<&PublicationTerminationEvidence> {
        self.termination.as_ref()
    }

    pub(crate) fn history(&self) -> &[PublicationQuarantineTransition] {
        &self.history
    }

    fn push_transition(
        &mut self,
        state: PublicationQuiescence,
        operation_id: Option<String>,
        at: String,
        reason: impl Into<String>,
    ) {
        self.history.push(PublicationQuarantineTransition {
            state,
            operation_id,
            at,
            reason: reason.into(),
        });
        if self.history.len() > MAX_PUBLICATION_QUARANTINE_HISTORY {
            self.history.remove(0);
        }
    }

    /// Activate a new publication. Only a Goal with no unresolved publication
    /// may do this: starting a second helper while an earlier one is unproven
    /// would destroy the only record that names the first.
    #[expect(
        clippy::too_many_arguments,
        reason = "The identity channels are independent authority bindings."
    )]
    pub(crate) fn activate(
        &mut self,
        operation_id: String,
        task_id: TaskId,
        attempt_id: AttemptId,
        lock_path: PathBuf,
        token_sha256: String,
        goal_revision: u64,
        host_generation: String,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        if self.is_unresolved() {
            return Err(OrchestratorError::CorruptGoal(
                "a publication cannot start while an earlier publication quarantine is unresolved"
                    .to_owned(),
            ));
        }
        if self.state != PublicationQuiescence::NotStarted
            && self.state != PublicationQuiescence::QuiescentVerified
        {
            return Err(OrchestratorError::CorruptGoal(
                "publication quarantine is not in an activatable state".to_owned(),
            ));
        }
        self.state = PublicationQuiescence::Active;
        self.operation_id = Some(operation_id.clone());
        self.task_id = Some(task_id);
        self.attempt_id = Some(attempt_id);
        self.lock_path = Some(lock_path);
        self.token_sha256 = Some(token_sha256);
        self.activation = Some(PublicationActivation {
            goal_revision,
            host_generation,
            activated_at: now.to_owned(),
        });
        self.termination = None;
        self.push_transition(
            PublicationQuiescence::Active,
            Some(operation_id),
            now.to_owned(),
            "publication armed before the helper was spawned",
        );
        self.validate()
    }

    /// Record process-liveness evidence for the active operation.
    pub(crate) fn record_termination(
        &mut self,
        operation_id: &str,
        evidence: PublicationTerminationEvidence,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        if self.operation_id.as_deref() != Some(operation_id) {
            return Err(OrchestratorError::CorruptGoal(
                "publication termination evidence names a different operation".to_owned(),
            ));
        }
        if !matches!(
            self.state,
            PublicationQuiescence::Active | PublicationQuiescence::Unproven
        ) {
            return Err(OrchestratorError::CorruptGoal(
                "publication termination evidence requires an unresolved quarantine".to_owned(),
            ));
        }
        let quiescent = evidence.kind.proves_quiescence();
        self.state = if quiescent {
            PublicationQuiescence::QuiescentVerified
        } else {
            PublicationQuiescence::Unproven
        };
        let reason = evidence.detail.clone();
        self.termination = Some(evidence);
        self.push_transition(
            self.state,
            Some(operation_id.to_owned()),
            now.to_owned(),
            reason,
        );
        self.validate()
    }

    /// Re-probe the lock witness. This is the *only* recovery transition.
    ///
    /// It never consults file contents or the mutation journal, and it never
    /// produces `NotStarted`.
    pub(crate) fn reconcile(
        &mut self,
        now: &str,
        host_generation: &str,
    ) -> Result<bool, OrchestratorError> {
        if !self.is_unresolved() {
            return Ok(false);
        }
        let Some(operation_id) = self.operation_id.clone() else {
            return Err(OrchestratorError::CorruptGoal(
                "unresolved publication quarantine has no operation identity".to_owned(),
            ));
        };
        let Some(lock_path) = self.lock_path.clone() else {
            if self.state != PublicationQuiescence::Unproven {
                self.state = PublicationQuiescence::Unproven;
                self.termination = Some(PublicationTerminationEvidence::new(
                    PublicationTerminationKind::ProbeFailed,
                    now,
                    host_generation,
                    "no publication lock witness exists for this operation",
                ));
                self.push_transition(
                    PublicationQuiescence::Unproven,
                    Some(operation_id),
                    now.to_owned(),
                    "no publication lock witness exists for this operation",
                );
                self.validate()?;
                return Ok(true);
            }
            return Ok(false);
        };
        match publication_lock::probe_and_abort(&lock_path) {
            QuiescenceProbe::Quiescent => {
                self.record_termination(
                    &operation_id,
                    PublicationTerminationEvidence::new(
                        PublicationTerminationKind::RecoveryQuiescenceProbe,
                        now,
                        host_generation,
                        "the host acquired the helper lock and aborted its token",
                    ),
                    now,
                )?;
                Ok(true)
            }
            QuiescenceProbe::Active => {
                // Still alive. Keep ACTIVE (or the existing UNPROVEN); never
                // clear, never rewrite to a weaker state.
                if self.state != PublicationQuiescence::Unproven {
                    self.push_transition(
                        PublicationQuiescence::Active,
                        Some(operation_id),
                        now.to_owned(),
                        "the publication lock is still held by a live process",
                    );
                    self.validate()?;
                    return Ok(true);
                }
                Ok(false)
            }
            QuiescenceProbe::Unproven(detail) => {
                if self.state == PublicationQuiescence::Unproven
                    && self
                        .termination
                        .as_ref()
                        .is_some_and(|evidence| evidence.detail == detail)
                {
                    return Ok(false);
                }
                self.state = PublicationQuiescence::Unproven;
                self.termination = Some(PublicationTerminationEvidence::new(
                    PublicationTerminationKind::ProbeFailed,
                    now,
                    host_generation,
                    detail,
                ));
                self.push_transition(
                    PublicationQuiescence::Unproven,
                    Some(operation_id),
                    now.to_owned(),
                    detail,
                );
                self.validate()?;
                Ok(true)
            }
        }
    }

    pub(crate) fn validate(&self) -> Result<(), OrchestratorError> {
        if self.schema_version != PUBLICATION_QUARANTINE_SCHEMA_VERSION {
            return Err(OrchestratorError::UnsupportedSchema(u64::from(
                self.schema_version,
            )));
        }
        if self.history.len() > MAX_PUBLICATION_QUARANTINE_HISTORY {
            return Err(OrchestratorError::CorruptGoal(
                "publication quarantine history exceeds its durable bound".to_owned(),
            ));
        }
        for transition in &self.history {
            validate_text(transition.at(), "quarantine transition time", 64)?;
            validate_text(
                transition.reason(),
                "quarantine transition reason",
                MAX_QUARANTINE_REASON_BYTES,
            )?;
            if let Some(operation_id) = transition.operation_id() {
                validate_text(
                    operation_id,
                    "quarantine transition operation",
                    MAX_QUARANTINE_ID_BYTES,
                )?;
            }
        }
        if let Some(termination) = &self.termination {
            validate_text(termination.observed_at(), "quarantine termination time", 64)?;
            validate_text(
                termination.host_generation(),
                "quarantine host generation",
                MAX_HOST_GENERATION_BYTES,
            )?;
            validate_text(
                termination.detail(),
                "quarantine termination detail",
                MAX_QUARANTINE_DETAIL_BYTES,
            )?;
        }
        if let Some(activation) = &self.activation {
            if activation.goal_revision() == 0 {
                return Err(OrchestratorError::CorruptGoal(
                    "publication activation has no Goal revision".to_owned(),
                ));
            }
            validate_text(
                activation.host_generation(),
                "publication activation host generation",
                MAX_HOST_GENERATION_BYTES,
            )?;
            validate_text(activation.activated_at(), "publication activation time", 64)?;
        }
        match self.state {
            PublicationQuiescence::NotStarted => {
                if self.operation_id.is_some()
                    || self.task_id.is_some()
                    || self.attempt_id.is_some()
                    || self.lock_path.is_some()
                    || self.token_sha256.is_some()
                    || self.activation.is_some()
                    || self.termination.is_some()
                    || !self.history.is_empty()
                {
                    return Err(OrchestratorError::CorruptGoal(
                        "NOT_STARTED publication quarantine carries publication state".to_owned(),
                    ));
                }
            }
            PublicationQuiescence::Active
            | PublicationQuiescence::Unproven
            | PublicationQuiescence::QuiescentVerified => {
                let operation_id = self.operation_id.as_deref().ok_or_else(|| {
                    OrchestratorError::CorruptGoal(
                        "publication quarantine has no operation identity".to_owned(),
                    )
                })?;
                validate_text(
                    operation_id,
                    "publication quarantine operation",
                    MAX_QUARANTINE_ID_BYTES,
                )?;
                self.task_id.as_ref().ok_or_else(|| {
                    OrchestratorError::CorruptGoal(
                        "publication quarantine has no Task identity".to_owned(),
                    )
                })?;
                self.attempt_id.as_ref().ok_or_else(|| {
                    OrchestratorError::CorruptGoal(
                        "publication quarantine has no Attempt identity".to_owned(),
                    )
                })?;
                let lock_path = self.lock_path.as_ref().ok_or_else(|| {
                    OrchestratorError::CorruptGoal(
                        "publication quarantine has no lock path".to_owned(),
                    )
                })?;
                if !lock_path.is_absolute() {
                    return Err(OrchestratorError::CorruptGoal(
                        "publication quarantine lock path is not absolute".to_owned(),
                    ));
                }
                let digest = self.token_sha256.as_deref().ok_or_else(|| {
                    OrchestratorError::CorruptGoal(
                        "publication quarantine has no token digest".to_owned(),
                    )
                })?;
                if digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(OrchestratorError::CorruptGoal(
                        "publication quarantine token digest is malformed".to_owned(),
                    ));
                }
                self.activation.as_ref().ok_or_else(|| {
                    OrchestratorError::CorruptGoal(
                        "publication quarantine has no activation record".to_owned(),
                    )
                })?;
                match self.state {
                    PublicationQuiescence::Active => {
                        if self.termination.is_some() {
                            return Err(OrchestratorError::CorruptGoal(
                                "ACTIVE publication quarantine carries termination evidence"
                                    .to_owned(),
                            ));
                        }
                    }
                    PublicationQuiescence::Unproven => {
                        let evidence = self.termination.as_ref().ok_or_else(|| {
                            OrchestratorError::CorruptGoal(
                                "UNPROVEN publication quarantine has no evidence".to_owned(),
                            )
                        })?;
                        if evidence.kind().proves_quiescence() {
                            return Err(OrchestratorError::CorruptGoal(
                                "UNPROVEN publication quarantine carries quiescence evidence"
                                    .to_owned(),
                            ));
                        }
                    }
                    PublicationQuiescence::QuiescentVerified => {
                        let evidence = self.termination.as_ref().ok_or_else(|| {
                            OrchestratorError::CorruptGoal(
                                "QUIESCENT_VERIFIED publication quarantine has no evidence"
                                    .to_owned(),
                            )
                        })?;
                        if !evidence.kind().proves_quiescence() {
                            return Err(OrchestratorError::CorruptGoal(
                                "QUIESCENT_VERIFIED publication quarantine lacks quiescence evidence"
                                    .to_owned(),
                            ));
                        }
                    }
                    PublicationQuiescence::NotStarted => unreachable!(),
                }
            }
        }
        Ok(())
    }
}

fn validate_text(value: &str, label: &'static str, max: usize) -> Result<(), OrchestratorError> {
    if value.trim().is_empty() || value.len() > max {
        return Err(OrchestratorError::CorruptGoal(format!(
            "{label} is out of bounds"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_model_work::sha256_hex;
    use uuid::Uuid;

    const NOW: &str = "2026-01-01T00:00:00Z";

    fn task_id() -> TaskId {
        TaskId::parse(&Uuid::new_v4().to_string()).unwrap()
    }

    fn attempt_id() -> AttemptId {
        AttemptId::parse(&Uuid::new_v4().to_string()).unwrap()
    }

    fn lock_path() -> PathBuf {
        std::env::temp_dir().join(format!("goallatch-quarantine-{}.lock", Uuid::new_v4()))
    }

    fn active() -> (PublicationQuarantine, String) {
        let operation_id = Uuid::new_v4().to_string();
        let token = Uuid::new_v4().to_string();
        let mut quarantine = PublicationQuarantine::not_started();
        quarantine
            .activate(
                operation_id.clone(),
                task_id(),
                attempt_id(),
                lock_path(),
                sha256_hex(token.as_bytes()),
                7,
                "host-1".to_owned(),
                NOW,
            )
            .unwrap();
        (quarantine, operation_id)
    }

    #[test]
    fn not_started_is_valid_and_unresolved_states_are_separate() {
        let quarantine = PublicationQuarantine::not_started();
        quarantine.validate().unwrap();
        assert!(!quarantine.is_unresolved());
    }

    #[test]
    fn activation_records_identity_before_the_helper_runs() {
        let (quarantine, operation_id) = active();
        assert_eq!(quarantine.state(), PublicationQuiescence::Active);
        assert!(quarantine.is_unresolved());
        assert_eq!(quarantine.operation_id(), Some(operation_id.as_str()));
        assert!(quarantine.termination().is_none());
        assert_eq!(
            quarantine.history().last().unwrap().state(),
            PublicationQuiescence::Active
        );
    }

    #[test]
    fn a_second_activation_is_refused_while_unresolved() {
        let (mut quarantine, _) = active();
        let error = quarantine
            .activate(
                Uuid::new_v4().to_string(),
                task_id(),
                attempt_id(),
                lock_path(),
                sha256_hex(b"token"),
                8,
                "host-1".to_owned(),
                NOW,
            )
            .unwrap_err();
        assert!(error.to_string().contains("unresolved"));
    }

    #[test]
    fn quiescence_evidence_moves_to_verified_and_cannot_rewrite_to_not_started() {
        let (mut quarantine, operation_id) = active();
        quarantine
            .record_termination(
                &operation_id,
                PublicationTerminationEvidence::new(
                    PublicationTerminationKind::HelperTreeQuiescent,
                    NOW,
                    "host-1",
                    "the helper exited and the lock was free",
                ),
                NOW,
            )
            .unwrap();
        assert_eq!(quarantine.state(), PublicationQuiescence::QuiescentVerified);
        assert!(!quarantine.is_unresolved());
        quarantine.validate().unwrap();
    }

    #[test]
    fn a_failed_probe_is_unproven_and_never_quiescent() {
        let (mut quarantine, operation_id) = active();
        quarantine
            .record_termination(
                &operation_id,
                PublicationTerminationEvidence::new(
                    PublicationTerminationKind::ProbeFailed,
                    NOW,
                    "host-1",
                    "lock probe failed",
                ),
                NOW,
            )
            .unwrap();
        assert_eq!(quarantine.state(), PublicationQuiescence::Unproven);
        assert!(quarantine.is_unresolved());
    }

    #[test]
    fn recovery_probes_a_held_lock_and_keeps_the_quarantine() {
        let (mut quarantine, _) = active();
        let path = quarantine.lock_path().unwrap().clone();
        publication_lock::arm(&path, "token").unwrap();
        let held = publication_lock::claim_for_helper(&path, "token").unwrap();
        assert!(quarantine.reconcile(NOW, "host-2").unwrap());
        assert_eq!(quarantine.state(), PublicationQuiescence::Active);
        assert!(quarantine.is_unresolved());
        drop(held);
        assert!(quarantine.reconcile(NOW, "host-2").unwrap());
        assert_eq!(quarantine.state(), PublicationQuiescence::QuiescentVerified);
        assert!(!quarantine.is_unresolved());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recovery_without_a_lock_witness_stays_unproven() {
        let mut quarantine = PublicationQuarantine::legacy_applying(
            "legacy-operation".to_owned(),
            task_id(),
            attempt_id(),
            4,
            NOW.to_owned(),
        );
        assert_eq!(quarantine.state(), PublicationQuiescence::Unproven);
        assert!(!quarantine.reconcile(NOW, "host-2").unwrap());
        assert_eq!(quarantine.state(), PublicationQuiescence::Unproven);
        assert!(quarantine.is_unresolved());
    }

    #[test]
    fn corrupt_quarantine_records_fail_validation() {
        let mut value = serde_json::to_value(PublicationQuarantine::not_started()).unwrap();
        value["state"] = serde_json::json!("ACTIVE");
        let decoded: PublicationQuarantine = serde_json::from_value(value).unwrap();
        assert!(decoded.validate().is_err());

        let mut unknown = serde_json::to_value(PublicationQuarantine::not_started()).unwrap();
        unknown["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PublicationQuarantine>(unknown).is_err());
    }
}
