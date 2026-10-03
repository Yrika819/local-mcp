use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::fallback::SideEffectClass;
use crate::orchestrator_error::OrchestratorError;
use crate::task::ReplaySafety;

pub(crate) const MAX_MUTATION_ID_BYTES: usize = 128;
pub(crate) const MAX_MUTATION_PATH_BYTES: usize = 4096;
pub(crate) const MAX_MUTATION_OPERATIONS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum MutationIntentState {
    Prepared,
    Applying,
    Applied,
    ReconciledNotPerformed,
    ReconciledPerformed,
    Partial,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum MutationOperationState {
    Prepared,
    Applying,
    Applied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MutationIntentUpdate {
    BeginOperation { index: u32 },
    CompleteOperation { index: u32 },
    Reconcile { state: MutationIntentState },
    BeginReviewer { invocation_id: String },
    CompleteReviewer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ReviewerInvocationState {
    NotStarted,
    Invoking,
    Persisted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub(crate) enum MutationPreimage {
    Absent,
    Sha256 { sha256: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileObservation {
    exists: bool,
    size: Option<u64>,
    sha256: Option<String>,
}

impl FileObservation {
    pub(crate) fn absent() -> Self {
        Self {
            exists: false,
            size: None,
            sha256: None,
        }
    }

    pub(crate) fn exists(size: u64, sha256: String) -> Self {
        Self {
            exists: true,
            size: Some(size),
            sha256: Some(sha256),
        }
    }

    pub(crate) fn is_exists(&self) -> bool {
        self.exists
    }

    pub(crate) fn size(&self) -> Option<u64> {
        self.size
    }

    pub(crate) fn sha256(&self) -> Option<&str> {
        self.sha256.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationOperationIntent {
    index: u32,
    path: PathBuf,
    expected_preimage: MutationPreimage,
    observed_before: FileObservation,
    intended_after_sha256: String,
    request_id: String,
    state: MutationOperationState,
}

#[expect(
    dead_code,
    reason = "Frozen mutation-operation evidence accessors are retained for staged writer/recovery consumers."
)]
impl MutationOperationIntent {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        index: u32,
        path: PathBuf,
        expected_preimage: MutationPreimage,
        observed_before: FileObservation,
        intended_after_sha256: String,
        request_id: String,
    ) -> Self {
        Self {
            index,
            path,
            expected_preimage,
            observed_before,
            intended_after_sha256,
            request_id,
            state: MutationOperationState::Prepared,
        }
    }

    pub(crate) fn index(&self) -> u32 {
        self.index
    }

    pub(crate) fn path(&self) -> &PathBuf {
        &self.path
    }

    pub(crate) fn expected_preimage(&self) -> &MutationPreimage {
        &self.expected_preimage
    }

    pub(crate) fn observed_before(&self) -> &FileObservation {
        &self.observed_before
    }

    pub(crate) fn intended_after_sha256(&self) -> &str {
        &self.intended_after_sha256
    }

    pub(crate) fn request_id(&self) -> &str {
        &self.request_id
    }

    pub(crate) fn state(&self) -> MutationOperationState {
        self.state
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewerInvocation {
    invocation_id: Option<String>,
    state: ReviewerInvocationState,
}

impl ReviewerInvocation {
    fn not_started() -> Self {
        Self {
            invocation_id: None,
            state: ReviewerInvocationState::NotStarted,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MutationIntent {
    operation_id: String,
    scope_identity: String,
    side_effect_class: SideEffectClass,
    replay_safety: ReplaySafety,
    state: MutationIntentState,
    reviewer: ReviewerInvocation,
    operations: Vec<MutationOperationIntent>,
}

#[expect(
    dead_code,
    reason = "Frozen mutation-intent reviewer accessor is retained for staged writer/reviewer integration."
)]
impl MutationIntent {
    pub(crate) fn new(
        operation_id: String,
        scope_identity: String,
        operations: Vec<MutationOperationIntent>,
    ) -> Result<Self, OrchestratorError> {
        let intent = Self {
            operation_id,
            scope_identity,
            side_effect_class: SideEffectClass::LocalMutation,
            replay_safety: ReplaySafety::VerifyBeforeRetry,
            state: MutationIntentState::Prepared,
            reviewer: ReviewerInvocation::not_started(),
            operations,
        };
        intent.validate()?;
        Ok(intent)
    }

    pub(crate) fn state(&self) -> MutationIntentState {
        self.state
    }

    pub(crate) fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub(crate) fn scope_identity(&self) -> &str {
        &self.scope_identity
    }

    pub(crate) fn operations(&self) -> &[MutationOperationIntent] {
        &self.operations
    }

    pub(crate) fn reviewer_state(&self) -> ReviewerInvocationState {
        self.reviewer.state
    }

    pub(crate) fn apply_update(
        &mut self,
        update: MutationIntentUpdate,
    ) -> Result<(), OrchestratorError> {
        match update {
            MutationIntentUpdate::BeginOperation { index } => {
                if self.state == MutationIntentState::Prepared {
                    self.state = MutationIntentState::Applying;
                }
                let operation = self.operation_mut(index)?;
                if operation.state != MutationOperationState::Prepared {
                    return Err(corrupt("operation cannot enter APPLYING twice"));
                }
                operation.state = MutationOperationState::Applying;
            }
            MutationIntentUpdate::CompleteOperation { index } => {
                if self.state != MutationIntentState::Applying {
                    return Err(corrupt("operation completion requires APPLYING intent"));
                }
                let operation = self.operation_mut(index)?;
                if operation.state != MutationOperationState::Applying {
                    return Err(corrupt("operation completion requires APPLYING operation"));
                }
                operation.state = MutationOperationState::Applied;
                if self
                    .operations
                    .iter()
                    .all(|operation| operation.state == MutationOperationState::Applied)
                {
                    self.state = MutationIntentState::Applied;
                }
            }
            MutationIntentUpdate::Reconcile { state } => {
                if !matches!(
                    state,
                    MutationIntentState::ReconciledNotPerformed
                        | MutationIntentState::ReconciledPerformed
                        | MutationIntentState::Partial
                        | MutationIntentState::Unknown
                ) {
                    return Err(corrupt("invalid MutationIntent reconciliation state"));
                }
                if matches!(
                    self.state,
                    MutationIntentState::Partial | MutationIntentState::Unknown
                ) && self.state != state
                {
                    return Err(corrupt("reconciliation state cannot be rewritten"));
                }
                if state == MutationIntentState::ReconciledPerformed {
                    for operation in &mut self.operations {
                        operation.state = MutationOperationState::Applied;
                    }
                }
                self.state = state;
            }
            MutationIntentUpdate::BeginReviewer { invocation_id } => {
                validate_id(&invocation_id, "Reviewer invocation identity")?;
                if self.reviewer.state != ReviewerInvocationState::NotStarted {
                    return Err(corrupt("Reviewer invocation cannot be started twice"));
                }
                self.reviewer.invocation_id = Some(invocation_id);
                self.reviewer.state = ReviewerInvocationState::Invoking;
            }
            MutationIntentUpdate::CompleteReviewer => {
                if self.reviewer.state != ReviewerInvocationState::Invoking {
                    return Err(corrupt("Reviewer completion requires INVOKING state"));
                }
                self.reviewer.state = ReviewerInvocationState::Persisted;
            }
        }
        self.validate()
    }

    fn operation_mut(
        &mut self,
        index: u32,
    ) -> Result<&mut MutationOperationIntent, OrchestratorError> {
        self.operations
            .iter_mut()
            .find(|operation| operation.index == index)
            .ok_or_else(|| corrupt("MutationIntent operation index is missing"))
    }

    pub(crate) fn reviewer(&self) -> &ReviewerInvocation {
        &self.reviewer
    }

    pub(crate) fn validate(&self) -> Result<(), OrchestratorError> {
        validate_id(&self.operation_id, "operation identity")?;
        validate_id(&self.scope_identity, "scope identity")?;
        if self.side_effect_class != SideEffectClass::LocalMutation
            || self.replay_safety != ReplaySafety::VerifyBeforeRetry
        {
            return Err(corrupt(
                "MutationIntent has an invalid Writer authority classification",
            ));
        }
        if self.operations.is_empty() || self.operations.len() > MAX_MUTATION_OPERATIONS {
            return Err(corrupt("MutationIntent operation count is out of bounds"));
        }
        let mut indices = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for operation in &self.operations {
            if !indices.insert(operation.index) {
                return Err(corrupt("MutationIntent has duplicate operation index"));
            }
            if operation.path.as_os_str().is_empty()
                || operation.path.as_os_str().len() > MAX_MUTATION_PATH_BYTES
                || !operation.path.is_absolute()
            {
                return Err(corrupt("MutationIntent operation path is invalid"));
            }
            // De-duplicate on the lossless path byte representation. `to_string_lossy`
            // maps every distinct non-UTF-8 path onto the same replacement text, so
            // using it here would conflate two different host paths into one identity.
            let path = crate::workspace_publish::path_identity_bytes(&operation.path);
            if !paths.insert(path) {
                return Err(corrupt("MutationIntent has duplicate operation path"));
            }
            validate_digest(
                &operation.intended_after_sha256,
                "intended postimage digest",
            )?;
            validate_id(&operation.request_id, "request identity")?;
            validate_observation(&operation.observed_before)?;
            if let MutationPreimage::Sha256 { sha256 } = &operation.expected_preimage {
                validate_digest(sha256, "expected preimage digest")?;
            }
            if !matches!(
                self.state,
                MutationIntentState::Prepared | MutationIntentState::Applying
            ) && operation.state != MutationOperationState::Applied
                && matches!(
                    self.state,
                    MutationIntentState::Applied | MutationIntentState::ReconciledPerformed
                )
            {
                return Err(corrupt(
                    "completed MutationIntent has incomplete operation state",
                ));
            }
        }
        match self.reviewer.state {
            ReviewerInvocationState::NotStarted => {
                if self.reviewer.invocation_id.is_some() {
                    return Err(corrupt("unstarted Reviewer invocation has an identity"));
                }
            }
            ReviewerInvocationState::Invoking | ReviewerInvocationState::Persisted => {
                let id =
                    self.reviewer.invocation_id.as_deref().ok_or_else(|| {
                        corrupt("active Reviewer invocation is missing its identity")
                    })?;
                validate_id(id, "Reviewer invocation identity")?;
            }
        }
        Ok(())
    }
}

fn validate_id(value: &str, label: &str) -> Result<(), OrchestratorError> {
    if value.is_empty() || value.len() > MAX_MUTATION_ID_BYTES {
        return Err(corrupt(format!("{label} is out of bounds")));
    }
    Ok(())
}

fn validate_digest(value: &str, label: &str) -> Result<(), OrchestratorError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(corrupt(format!("{label} is not a SHA-256 digest")));
    }
    Ok(())
}

fn validate_observation(observation: &FileObservation) -> Result<(), OrchestratorError> {
    if observation.exists != observation.sha256.is_some() {
        return Err(corrupt("file observation existence and digest disagree"));
    }
    if !observation.exists && observation.size.is_some() {
        return Err(corrupt("absent file observation has a size"));
    }
    if let Some(digest) = observation.sha256.as_deref() {
        validate_digest(digest, "observed file digest")?;
    }
    Ok(())
}

fn corrupt(message: impl Into<String>) -> OrchestratorError {
    OrchestratorError::CorruptGoal(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_intent_round_trips_without_replacement_contents() {
        let target_path = std::env::temp_dir().join("local-mcp-mutation-target.txt");
        let intent = MutationIntent::new(
            "operation-1".to_owned(),
            "scope-1".to_owned(),
            vec![MutationOperationIntent::new(
                0,
                target_path,
                MutationPreimage::Absent,
                FileObservation::absent(),
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                "request-1".to_owned(),
            )],
        )
        .unwrap();
        let value = serde_json::to_value(&intent).unwrap();
        assert_eq!(value["state"], "PREPARED");
        assert!(value.get("content").is_none());
        let decoded: MutationIntent = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, intent);
    }

    #[test]
    fn malformed_and_duplicate_intents_are_rejected() {
        let path_a = std::env::temp_dir().join("local-mcp-mutation-a");
        let path_b = std::env::temp_dir().join("local-mcp-mutation-b");
        let duplicate = serde_json::json!({
            "operation_id": "operation-1",
            "scope_identity": "scope-1",
            "side_effect_class": "LOCAL_MUTATION",
            "replay_safety": "VERIFY_BEFORE_RETRY",
            "state": "PREPARED",
            "reviewer": {"invocation_id": null, "state": "NOT_STARTED"},
            "operations": [
                {"index": 0, "path": path_a.to_string_lossy(), "expected_preimage": "ABSENT", "observed_before": {"exists": false, "size": null, "sha256": null}, "intended_after_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "request_id": "r1", "state": "PREPARED"},
                {"index": 0, "path": path_b.to_string_lossy(), "expected_preimage": "ABSENT", "observed_before": {"exists": false, "size": null, "sha256": null}, "intended_after_sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "request_id": "r2", "state": "PREPARED"}
            ]
        });
        let decoded = serde_json::from_value::<MutationIntent>(duplicate).unwrap();
        let error = decoded.validate().unwrap_err();
        assert!(error.to_string().contains("duplicate"));
    }
}
