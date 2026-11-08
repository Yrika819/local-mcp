//! External Agent Backend V1 — durable host-authored model work.
//!
//! Everything in this module is **host-owned durable state**. The external
//! agent never authors any of it:
//!
//! * [`ModelWorkItem`] is the bounded contract the host hands to an external
//!   model ("produce exactly this output contract for this revision").
//! * [`SubmissionReceipt`] records the *fact* and *verdict* of one submission,
//!   so a replay is idempotent and a conflicting duplicate cannot overwrite a
//!   decision.
//! * [`ExternalModelState`] is the single durable aggregate stored on the Goal.
//!   It is written only through [`crate::external_model_store`], which reuses the
//!   existing `TaskStore` session lock and optimistic revision check.
//!
//! Authority rules pinned here and by tests:
//!
//! 1. A `work_id` is an opaque handle, never a capability. Every lookup is
//!    re-bound to the owning Goal and session.
//! 2. At most one *active* WorkItem (PENDING or SUBMITTED) exists per Goal, and
//!    a WorkItem is handed to a client only after it is durably committed
//!    (ADR-10, TM-05).
//! 3. Raw model responses are never persisted. Only ids, digests, decisions, and
//!    a bounded redacted detail survive (B-07 / TM-10).
//! 4. Receipt capacity exhaustion refuses further submissions. Nothing is
//!    evicted, so the `submission_id + payload` idempotency table stays complete
//!    (B-07: "if no safe compaction is established, refuse at the cap").
//! 5. Accepting a submission is not success: the receipt's decision is at most
//!    `ACCEPTED_FOR_HOST_PROCESSING` and never a Goal transition (ADR-07).

// E3-A lands this durable contract before the `goal_next` / `goal_submit` API
// layer (E3-B) consumes it. Until that layer exists the surface is reachable only
// from the E3-A tests, so the attribute is scoped to this module and removed by
// E3-B rather than leaving a permanent blanket silence.
#![allow(
    dead_code,
    reason = "E3-A durable contract surface; E3-B consumes it and removes this attribute"
)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::agent::ModelRole;
use crate::orchestrator_error::OrchestratorError;

/// B-07: one Goal may have at most one active WorkItem.
pub(crate) const MAX_ACTIVE_WORK_ITEMS_PER_GOAL: usize = 1;

/// B-07: lifetime host-issued WorkItems for one Goal.
pub(crate) const MAX_MODEL_WORK_ITEMS_PER_GOAL: u32 = 64;

/// B-07: per-role attempt budget.
pub(crate) const MAX_ROLE_ATTEMPTS: u32 = 8;

/// B-07: bounded WorkItem request payload (matches the Replanner request cap
/// family; the transport prompt cap is 256 KiB).
pub(crate) const MAX_REQUEST_PAYLOAD_BYTES: usize = 256 * 1024;

/// B-08: bounded model response (matches `agent::MODEL_STDOUT_LIMIT`).
pub(crate) const MAX_MODEL_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

/// B-07: bounded host-only steps per `goal_next` call.
pub(crate) const MAX_HOST_STEPS_PER_CALL: u32 = 8;

/// B-08: bounded JSON nesting depth for a submitted response.
pub(crate) const MAX_JSON_NESTING_DEPTH: usize = 32;

/// Durable receipt set cap. Reaching it refuses new submissions rather than
/// evicting an idempotency row.
pub(crate) const MAX_SUBMISSION_RECEIPTS: usize = 64;

/// Bound for the redacted receipt detail. Receipts never carry model content.
pub(crate) const MAX_RECEIPT_DETAIL_BYTES: usize = 512;

/// Bound for a stop/diagnostic code.
pub(crate) const MAX_STOP_CODE_BYTES: usize = 128;

pub(crate) const EXTERNAL_MODEL_SCHEMA_VERSION: u32 = 1;

/// Output contract for the Planner role in V1.
pub(crate) const OUTPUT_CONTRACT_PLANNER_V1: &str = "planner-proposal-v1";

/// Output contract for the Readonly role in V1.
pub(crate) const OUTPUT_CONTRACT_READONLY_V1: &str = "readonly-investigation-v1";

/// Output contract for the Writer role in V1.
pub(crate) const OUTPUT_CONTRACT_WRITER_V1: &str = "writer-proposal-v1";

/// Output contract for the Reviewer role in V1.
pub(crate) const OUTPUT_CONTRACT_REVIEWER_V1: &str = "reviewer-verdict-v1";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct WorkId(String);

impl WorkId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub(crate) fn parse(value: &str) -> Result<Self, OrchestratorError> {
        let uuid = Uuid::parse_str(value)
            .map_err(|_| OrchestratorError::UnsafeIdentifier("WorkId".to_owned()))?;
        if uuid.to_string() != value {
            return Err(OrchestratorError::UnsafeIdentifier("WorkId".to_owned()));
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn validate(&self) -> Result<(), OrchestratorError> {
        Self::parse(&self.0).map(|_| ())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct SubmissionId(String);

impl SubmissionId {
    pub(crate) fn parse(value: &str) -> Result<Self, OrchestratorError> {
        let uuid = Uuid::parse_str(value)
            .map_err(|_| OrchestratorError::UnsafeIdentifier("SubmissionId".to_owned()))?;
        if uuid.to_string() != value {
            return Err(OrchestratorError::UnsafeIdentifier(
                "SubmissionId".to_owned(),
            ));
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn validate(&self) -> Result<(), OrchestratorError> {
        Self::parse(&self.0).map(|_| ())
    }
}

/// Wire spelling of a model role for external work.
///
/// This mirrors `agent::ModelRole` but owns its serde representation so the
/// durable `SCREAMING_SNAKE_CASE` form cannot drift from the transport enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum WorkRole {
    Planner,
    Readonly,
    Writer,
    Reviewer,
    Replanner,
}

impl WorkRole {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Planner => "PLANNER",
            Self::Readonly => "READONLY",
            Self::Writer => "WRITER",
            Self::Reviewer => "REVIEWER",
            Self::Replanner => "REPLANNER",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, OrchestratorError> {
        match value {
            "PLANNER" => Ok(Self::Planner),
            "READONLY" => Ok(Self::Readonly),
            "WRITER" => Ok(Self::Writer),
            "REVIEWER" => Ok(Self::Reviewer),
            "REPLANNER" => Ok(Self::Replanner),
            _ => Err(OrchestratorError::UnsafeIdentifier("WorkRole".to_owned())),
        }
    }

    pub(crate) fn as_model_role(self) -> ModelRole {
        match self {
            Self::Planner => ModelRole::Planner,
            Self::Readonly => ModelRole::Readonly,
            Self::Writer => ModelRole::Writer,
            Self::Reviewer => ModelRole::Reviewer,
            Self::Replanner => ModelRole::Replanner,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum WorkItemState {
    /// Issued to the host and awaiting an external response.
    Pending,
    /// A response was accepted for host processing. Not applied, not success.
    Submitted,
    /// A response was refused; the WorkItem is terminal.
    Rejected,
    /// The host finished its own processing phase.
    Committed,
    /// Invalidated by cancellation or authority loss.
    Cancelled,
}

impl WorkItemState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Submitted => "SUBMITTED",
            Self::Rejected => "REJECTED",
            Self::Committed => "COMMITTED",
            Self::Cancelled => "CANCELLED",
        }
    }

    /// Whether this WorkItem still occupies the single active slot.
    pub(crate) fn is_active(self) -> bool {
        matches!(self, Self::Pending | Self::Submitted)
    }

    pub(crate) fn is_terminal(self) -> bool {
        !self.is_active()
    }
}

/// The closed decision set for one submission (B-02).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum SubmissionDecision {
    AcceptedForHostProcessing,
    /// The host validated the response *and* applied its effect in the same
    /// durable commit (E4). This is the only decision that carries a
    /// `committed_revision`, and it still never means "Goal completed".
    Committed,
    RejectedSchema,
    RejectedStaleRevision,
    RejectedAuthority,
    RejectedPreimage,
    ReplayOfAcceptedSubmission,
    ConflictingDuplicate,
    CancelledStale,
    BudgetExhausted,
}

impl SubmissionDecision {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AcceptedForHostProcessing => "ACCEPTED_FOR_HOST_PROCESSING",
            Self::Committed => "COMMITTED",
            Self::RejectedSchema => "REJECTED_SCHEMA",
            Self::RejectedStaleRevision => "REJECTED_STALE_REVISION",
            Self::RejectedAuthority => "REJECTED_AUTHORITY",
            Self::RejectedPreimage => "REJECTED_PREIMAGE",
            Self::ReplayOfAcceptedSubmission => "REPLAY_OF_ACCEPTED_SUBMISSION",
            Self::ConflictingDuplicate => "CONFLICTING_DUPLICATE",
            Self::CancelledStale => "CANCELLED_STALE",
            Self::BudgetExhausted => "BUDGET_EXHAUSTED",
        }
    }
}

/// Why a new WorkItem cannot be issued right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IssueBlock {
    PendingWorkItemExists,
    LifetimeWorkItemBudgetExhausted,
    RoleAttemptBudgetExhausted,
}

impl IssueBlock {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::PendingWorkItemExists => "PENDING_WORK_ITEM_EXISTS",
            Self::LifetimeWorkItemBudgetExhausted => "MODEL_WORK_ITEM_BUDGET_EXHAUSTED",
            Self::RoleAttemptBudgetExhausted => "ROLE_ATTEMPT_BUDGET_EXHAUSTED",
        }
    }

    pub(crate) fn detail(self) -> &'static str {
        match self {
            Self::PendingWorkItemExists => {
                "the Goal already has an active ModelWorkItem; the same work_id is returned"
            }
            Self::LifetimeWorkItemBudgetExhausted => {
                "the Goal reached its lifetime host-issued ModelWorkItem budget"
            }
            Self::RoleAttemptBudgetExhausted => "the role reached its attempt budget for this Goal",
        }
    }
}

/// The bounded request contract handed to an external model.
///
/// `request_payload` is host-built from existing role request builders. An
/// external caller can never supply it, and it is bounded at write time so a
/// durable Goal cannot grow past [`MAX_REQUEST_PAYLOAD_BYTES`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelWorkItem {
    work_id: WorkId,
    session_id: String,
    goal_id: String,
    role: WorkRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attempt_id: Option<String>,
    goal_revision: u64,
    plan_revision: u32,
    request_digest: String,
    request_payload: String,
    output_contract: String,
    state: WorkItemState,
    issue_counter: u32,
    created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    submitted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_code: Option<String>,
}

impl ModelWorkItem {
    #[expect(
        clippy::too_many_arguments,
        reason = "Identity, revision, contract, and audit fields are independent authority bindings."
    )]
    #[allow(
        dead_code,
        reason = "Constructor is the host-owned issuance surface; exercised by E3 tests."
    )]
    pub(crate) fn pending(
        work_id: WorkId,
        session_id: impl Into<String>,
        goal_id: impl Into<String>,
        role: WorkRole,
        task_id: Option<String>,
        attempt_id: Option<String>,
        goal_revision: u64,
        plan_revision: u32,
        request_digest: String,
        request_payload: String,
        output_contract: impl Into<String>,
        issue_counter: u32,
        created_at: impl Into<String>,
    ) -> Result<Self, OrchestratorError> {
        let item = Self {
            work_id,
            session_id: session_id.into(),
            goal_id: goal_id.into(),
            role,
            task_id,
            attempt_id,
            goal_revision,
            plan_revision,
            request_digest,
            request_payload,
            output_contract: output_contract.into(),
            state: WorkItemState::Pending,
            issue_counter,
            created_at: created_at.into(),
            submitted_at: None,
            terminal_code: None,
        };
        item.validate()?;
        Ok(item)
    }

    pub(crate) fn validate(&self) -> Result<(), OrchestratorError> {
        self.work_id.validate()?;
        if self.goal_id.trim().is_empty() || self.session_id.trim().is_empty() {
            return Err(OrchestratorError::CorruptGoal(
                "external work item has an empty owner binding".to_owned(),
            ));
        }
        if self.request_digest.len() != 64
            || !self
                .request_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(OrchestratorError::CorruptGoal(
                "external work item request digest is not a SHA-256 hex digest".to_owned(),
            ));
        }
        if self.request_payload.len() > MAX_REQUEST_PAYLOAD_BYTES {
            return Err(OrchestratorError::CorruptGoal(
                "external work item request payload exceeds the durable bound".to_owned(),
            ));
        }
        if self.output_contract.trim().is_empty() || self.output_contract.len() > 128 {
            return Err(OrchestratorError::CorruptGoal(
                "external work item has an invalid output contract".to_owned(),
            ));
        }
        if self.issue_counter == 0 || self.issue_counter > MAX_MODEL_WORK_ITEMS_PER_GOAL {
            return Err(OrchestratorError::CorruptGoal(
                "external work item issue counter is out of range".to_owned(),
            ));
        }
        if let Some(code) = self.terminal_code.as_deref()
            && (code.trim().is_empty() || code.len() > MAX_STOP_CODE_BYTES)
        {
            return Err(OrchestratorError::CorruptGoal(
                "external work item terminal code is not bounded".to_owned(),
            ));
        }
        Ok(())
    }

    pub(crate) fn work_id(&self) -> &WorkId {
        &self.work_id
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn goal_id(&self) -> &str {
        &self.goal_id
    }

    pub(crate) fn role(&self) -> WorkRole {
        self.role
    }

    pub(crate) fn task_id(&self) -> Option<&str> {
        self.task_id.as_deref()
    }

    pub(crate) fn attempt_id(&self) -> Option<&str> {
        self.attempt_id.as_deref()
    }

    pub(crate) fn goal_revision(&self) -> u64 {
        self.goal_revision
    }

    pub(crate) fn plan_revision(&self) -> u32 {
        self.plan_revision
    }

    pub(crate) fn request_digest(&self) -> &str {
        &self.request_digest
    }

    pub(crate) fn request_payload(&self) -> &str {
        &self.request_payload
    }

    pub(crate) fn output_contract(&self) -> &str {
        &self.output_contract
    }

    pub(crate) fn state(&self) -> WorkItemState {
        self.state
    }

    pub(crate) fn issue_counter(&self) -> u32 {
        self.issue_counter
    }

    pub(crate) fn created_at(&self) -> &str {
        &self.created_at
    }

    pub(crate) fn submitted_at(&self) -> Option<&str> {
        self.submitted_at.as_deref()
    }

    pub(crate) fn terminal_code(&self) -> Option<&str> {
        self.terminal_code.as_deref()
    }

    pub(crate) fn summary(&self) -> ModelWorkItemSummary {
        ModelWorkItemSummary {
            work_id: self.work_id.clone(),
            role: self.role,
            state: self.state,
            goal_revision: self.goal_revision,
            plan_revision: self.plan_revision,
            request_digest: self.request_digest.clone(),
            issue_counter: self.issue_counter,
            created_at: self.created_at.clone(),
            terminal_code: self.terminal_code.clone(),
        }
    }

    fn transition(&mut self, next: WorkItemState, code: &str) {
        self.state = next;
        self.terminal_code = Some(code.to_owned());
    }

    /// PENDING -> SUBMITTED. Acceptance only; the host phase is separate.
    fn mark_submitted(&mut self, now: &str) {
        self.state = WorkItemState::Submitted;
        self.submitted_at = Some(now.to_owned());
        self.terminal_code = None;
    }

    /// PENDING -> REJECTED (terminal).
    fn mark_rejected(&mut self, code: &str) {
        self.transition(WorkItemState::Rejected, code);
    }

    /// PENDING -> COMMITTED. The host finished its own application phase in the
    /// same durable commit that recorded this transition.
    fn mark_committed(&mut self, now: &str) {
        self.state = WorkItemState::Committed;
        self.submitted_at = Some(now.to_owned());
        self.terminal_code = None;
    }

    /// PENDING/SUBMITTED -> CANCELLED (terminal).
    fn mark_cancelled(&mut self, code: &str) {
        self.transition(WorkItemState::Cancelled, code);
    }
}

/// Bounded audit copy of a WorkItem. The request payload is deliberately absent:
/// history keeps identity, role, revisions, digest, and outcome only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelWorkItemSummary {
    work_id: WorkId,
    role: WorkRole,
    state: WorkItemState,
    goal_revision: u64,
    plan_revision: u32,
    request_digest: String,
    issue_counter: u32,
    created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal_code: Option<String>,
}

#[allow(
    dead_code,
    reason = "Audit accessors are consumed by tests and future reporting surfaces."
)]
impl ModelWorkItemSummary {
    pub(crate) fn work_id(&self) -> &WorkId {
        &self.work_id
    }

    pub(crate) fn role(&self) -> WorkRole {
        self.role
    }

    pub(crate) fn state(&self) -> WorkItemState {
        self.state
    }

    pub(crate) fn request_digest(&self) -> &str {
        &self.request_digest
    }

    pub(crate) fn issue_counter(&self) -> u32 {
        self.issue_counter
    }
}

/// One durable verdict for one submission attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmissionReceipt {
    work_id: WorkId,
    submission_id: SubmissionId,
    payload_sha256: String,
    decision: SubmissionDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    committed_revision: Option<u64>,
    code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    redacted_detail: Option<String>,
    received_at: String,
}

impl SubmissionReceipt {
    #[expect(
        clippy::too_many_arguments,
        reason = "Receipt fields are the frozen idempotency key plus its verdict."
    )]
    #[allow(
        dead_code,
        reason = "Constructor is the host-owned receipt surface; exercised by E3 tests."
    )]
    pub(crate) fn new(
        work_id: WorkId,
        submission_id: SubmissionId,
        payload_sha256: String,
        decision: SubmissionDecision,
        committed_revision: Option<u64>,
        code: impl Into<String>,
        redacted_detail: Option<String>,
        received_at: impl Into<String>,
    ) -> Self {
        Self {
            work_id,
            submission_id,
            payload_sha256,
            decision,
            committed_revision,
            code: code.into(),
            redacted_detail,
            received_at: received_at.into(),
        }
    }

    pub(crate) fn work_id(&self) -> &WorkId {
        &self.work_id
    }

    pub(crate) fn submission_id(&self) -> &SubmissionId {
        &self.submission_id
    }

    pub(crate) fn payload_sha256(&self) -> &str {
        &self.payload_sha256
    }

    pub(crate) fn decision(&self) -> SubmissionDecision {
        self.decision
    }

    pub(crate) fn committed_revision(&self) -> Option<u64> {
        self.committed_revision
    }

    pub(crate) fn code(&self) -> &str {
        &self.code
    }

    pub(crate) fn redacted_detail(&self) -> Option<&str> {
        self.redacted_detail.as_deref()
    }

    pub(crate) fn received_at(&self) -> &str {
        &self.received_at
    }

    fn validate(&self) -> Result<(), OrchestratorError> {
        self.work_id.validate()?;
        self.submission_id.validate()?;
        if self.payload_sha256.len() != 64
            || !self
                .payload_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(OrchestratorError::CorruptGoal(
                "submission receipt payload digest is not a SHA-256 hex digest".to_owned(),
            ));
        }
        if self.code.trim().is_empty() || self.code.len() > MAX_STOP_CODE_BYTES {
            return Err(OrchestratorError::CorruptGoal(
                "submission receipt code is not bounded".to_owned(),
            ));
        }
        if let Some(detail) = self.redacted_detail.as_deref()
            && (detail.len() > MAX_RECEIPT_DETAIL_BYTES || detail.contains('\0'))
        {
            return Err(OrchestratorError::CorruptGoal(
                "submission receipt detail is not bounded or contains a control character"
                    .to_owned(),
            ));
        }
        Ok(())
    }
}

/// A bounded, redacted reason why external progress stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalStop {
    code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    at: String,
}

impl ExternalStop {
    pub(crate) fn new(code: &str, detail: Option<&str>, at: &str) -> Self {
        let detail = detail.map(|value| truncate_on_char_boundary(value, MAX_RECEIPT_DETAIL_BYTES));
        Self {
            code: truncate_on_char_boundary(code, MAX_STOP_CODE_BYTES),
            detail,
            at: at.to_owned(),
        }
    }

    pub(crate) fn code(&self) -> &str {
        &self.code
    }

    pub(crate) fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    pub(crate) fn at(&self) -> &str {
        &self.at
    }
}

/// The durable External Agent Backend state of one Goal.
///
/// `schema_version` is the sub-record version. The containing Goal keeps its own
/// schema version, and the field is additive so a Goal written before the
/// External backend existed still decodes as `None` (CODEX).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalModelState {
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_work: Option<ModelWorkItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    work_history: Vec<ModelWorkItemSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    receipts: Vec<SubmissionReceipt>,
    issue_counter: u32,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    role_attempts: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_stop: Option<ExternalStop>,
}

impl ExternalModelState {
    pub(crate) fn new() -> Self {
        Self {
            schema_version: EXTERNAL_MODEL_SCHEMA_VERSION,
            pending_work: None,
            work_history: Vec::new(),
            receipts: Vec::new(),
            issue_counter: 0,
            role_attempts: BTreeMap::new(),
            last_stop: None,
        }
    }

    pub(crate) fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub(crate) fn active_work(&self) -> Option<&ModelWorkItem> {
        self.pending_work.as_ref()
    }

    pub(crate) fn work_history(&self) -> &[ModelWorkItemSummary] {
        &self.work_history
    }

    pub(crate) fn receipts(&self) -> &[SubmissionReceipt] {
        &self.receipts
    }

    pub(crate) fn issue_counter(&self) -> u32 {
        self.issue_counter
    }

    pub(crate) fn role_attempts(&self, role: WorkRole) -> u32 {
        self.role_attempts.get(role.as_str()).copied().unwrap_or(0)
    }

    pub(crate) fn last_stop(&self) -> Option<&ExternalStop> {
        self.last_stop.as_ref()
    }

    /// Whether the active slot holds a pre-E4 accepted-but-unapplied WorkItem.
    ///
    /// E3 recorded `ACCEPTED_FOR_HOST_PROCESSING` without persisting the model
    /// artifact, so such a WorkItem cannot be applied later: the response is
    /// gone by design. The host must report that honestly instead of guessing or
    /// discarding the receipt.
    pub(crate) fn active_work_awaits_unrecoverable_application(&self) -> Option<&ModelWorkItem> {
        self.pending_work
            .as_ref()
            .filter(|item| item.state == WorkItemState::Submitted)
    }

    pub(crate) fn find_receipt(&self, submission_id: &SubmissionId) -> Option<&SubmissionReceipt> {
        self.receipts
            .iter()
            .find(|receipt| receipt.submission_id == *submission_id)
    }

    pub(crate) fn receipt_capacity_available(&self) -> bool {
        self.receipts.len() < MAX_SUBMISSION_RECEIPTS
    }

    /// Whether a new WorkItem may be issued for `role`.
    ///
    /// All three limits are checked before any state change, so a refusal never
    /// consumes budget.
    pub(crate) fn check_issue_budget(&self, role: WorkRole) -> Result<(), IssueBlock> {
        if self
            .pending_work
            .as_ref()
            .is_some_and(|item| item.state.is_active())
        {
            return Err(IssueBlock::PendingWorkItemExists);
        }
        if self.issue_counter >= MAX_MODEL_WORK_ITEMS_PER_GOAL {
            return Err(IssueBlock::LifetimeWorkItemBudgetExhausted);
        }
        if self.role_attempts(role) >= MAX_ROLE_ATTEMPTS {
            return Err(IssueBlock::RoleAttemptBudgetExhausted);
        }
        Ok(())
    }

    /// Durably record a freshly issued WorkItem.
    ///
    /// The caller must have checked [`Self::check_issue_budget`] on this same
    /// state revision; the counter invariant is re-checked anyway so a bug can
    /// not persist two active WorkItems.
    pub(crate) fn record_issued(&mut self, item: ModelWorkItem) -> Result<(), OrchestratorError> {
        if self
            .pending_work
            .as_ref()
            .is_some_and(|existing| existing.state.is_active())
        {
            return Err(OrchestratorError::CorruptGoal(
                "a Goal may have at most one active external work item".to_owned(),
            ));
        }
        if item.state != WorkItemState::Pending {
            return Err(OrchestratorError::CorruptGoal(
                "a newly issued external work item must be PENDING".to_owned(),
            ));
        }
        let expected_counter = self.issue_counter.checked_add(1).ok_or_else(|| {
            OrchestratorError::CorruptGoal("external work item counter overflow".to_owned())
        })?;
        if item.issue_counter != expected_counter {
            return Err(OrchestratorError::CorruptGoal(
                "external work item issue counter is not monotonic".to_owned(),
            ));
        }
        let attempts = self
            .role_attempts
            .entry(item.role.as_str().to_owned())
            .or_insert(0);
        *attempts = attempts.checked_add(1).ok_or_else(|| {
            OrchestratorError::CorruptGoal("external role attempt counter overflow".to_owned())
        })?;
        self.issue_counter = expected_counter;
        self.pending_work = Some(item);
        Ok(())
    }

    /// Refuse a submission without moving the WorkItem out of PENDING.
    ///
    /// This is the "nothing was accepted" path: the WorkItem stays re-issuable
    /// and no receipt is written, so the client can correct its input.
    pub(crate) fn record_refusal(&mut self, stop: ExternalStop) {
        self.last_stop = Some(stop);
    }

    /// Apply an accepted-or-rejected submission as one durable transition.
    ///
    /// `decision` must be either `ACCEPTED_FOR_HOST_PROCESSING` (WorkItem ->
    /// SUBMITTED, receipt committed revision is `None` because no host phase has
    /// run) or a `REJECTED_SCHEMA`-class refusal (WorkItem -> REJECTED).
    pub(crate) fn record_submission(
        &mut self,
        receipt: SubmissionReceipt,
        now: &str,
    ) -> Result<(), OrchestratorError> {
        if !self.receipt_capacity_available() {
            return Err(OrchestratorError::CorruptGoal(
                "submission receipt capacity is exhausted".to_owned(),
            ));
        }
        receipt.validate()?;
        // All read-only preconditions are checked before the mutable borrow so a
        // refusal can never leave the slot partially transitioned.
        {
            let pending = self.pending_work.as_ref().ok_or_else(|| {
                OrchestratorError::CorruptGoal(
                    "a submission cannot be recorded without an active external work item"
                        .to_owned(),
                )
            })?;
            if pending.work_id != receipt.work_id {
                return Err(OrchestratorError::CorruptGoal(
                    "submission receipt is bound to a different external work item".to_owned(),
                ));
            }
            if pending.state != WorkItemState::Pending {
                return Err(OrchestratorError::CorruptGoal(
                    "a submission requires a PENDING external work item".to_owned(),
                ));
            }
        }
        if self.find_receipt(&receipt.submission_id).is_some() {
            return Err(OrchestratorError::CorruptGoal(
                "submission receipt identifiers must be unique per Goal".to_owned(),
            ));
        }
        let item = self.pending_work.as_mut().ok_or_else(|| {
            OrchestratorError::CorruptGoal(
                "a submission cannot be recorded without an active external work item".to_owned(),
            )
        })?;
        match receipt.decision {
            SubmissionDecision::AcceptedForHostProcessing => {
                item.mark_submitted(now);
                self.receipts.push(receipt);
            }
            SubmissionDecision::Committed => {
                if receipt.committed_revision.is_none() {
                    return Err(OrchestratorError::CorruptGoal(
                        "a committed submission receipt must carry the revision it committed"
                            .to_owned(),
                    ));
                }
                item.mark_committed(now);
                self.work_history.push(item.summary());
                self.pending_work = None;
                self.receipts.push(receipt);
            }
            SubmissionDecision::RejectedSchema
            | SubmissionDecision::RejectedAuthority
            | SubmissionDecision::RejectedPreimage
            | SubmissionDecision::RejectedStaleRevision
            | SubmissionDecision::BudgetExhausted => {
                item.mark_rejected(receipt.code());
                self.work_history.push(item.summary());
                self.pending_work = None;
                self.receipts.push(receipt);
            }
            SubmissionDecision::ReplayOfAcceptedSubmission
            | SubmissionDecision::ConflictingDuplicate
            | SubmissionDecision::CancelledStale => {
                return Err(OrchestratorError::CorruptGoal(
                    "replay and conflict decisions are derived, never recorded".to_owned(),
                ));
            }
        }
        self.last_stop = None;
        Ok(())
    }

    /// SUBMITTED -> COMMITTED for the active WorkItem.
    ///
    /// Used by the two-phase Writer: the acceptance receipt is the submission
    /// verdict, and *this* transition is the durable fact that the host applied
    /// the prepared intent. No second receipt is invented for the same
    /// submission id.
    pub(crate) fn complete_active_work(&mut self, code: &str) -> Result<(), OrchestratorError> {
        let item = self.pending_work.as_mut().ok_or_else(|| {
            OrchestratorError::CorruptGoal(
                "completing external work requires an active work item".to_owned(),
            )
        })?;
        if item.state != WorkItemState::Submitted {
            return Err(OrchestratorError::CorruptGoal(
                "only a SUBMITTED external work item may be completed".to_owned(),
            ));
        }
        item.state = WorkItemState::Committed;
        item.terminal_code = Some(code.to_owned());
        self.work_history.push(item.summary());
        self.pending_work = None;
        Ok(())
    }

    /// Invalidate the active WorkItem. Idempotent: a Goal with no active work
    /// item records nothing.
    pub(crate) fn cancel_active_work(&mut self, code: &str) {
        if let Some(item) = self.pending_work.as_mut()
            && item.state.is_active()
        {
            item.mark_cancelled(code);
            self.work_history.push(item.summary());
        }
        self.pending_work = None;
    }

    /// Durably record why a `goal_next` call could not advance.
    pub(crate) fn set_stop(&mut self, stop: ExternalStop) {
        self.last_stop = Some(stop);
    }

    /// Full re-validation against the containing Goal.
    ///
    /// A corrupt or cross-bound external record must fail the Goal load rather
    /// than be silently ignored, so revision and ownership evidence cannot be
    /// forged or lost by a partial write.
    pub(crate) fn validate_for_goal(
        &self,
        goal_id: &str,
        session_id: &str,
    ) -> Result<(), OrchestratorError> {
        if self.schema_version != EXTERNAL_MODEL_SCHEMA_VERSION {
            return Err(OrchestratorError::UnsupportedSchema(u64::from(
                self.schema_version,
            )));
        }
        if self.issue_counter > MAX_MODEL_WORK_ITEMS_PER_GOAL {
            return Err(OrchestratorError::CorruptGoal(
                "external work item counter exceeds the lifetime budget".to_owned(),
            ));
        }
        if self.work_history.len() > MAX_MODEL_WORK_ITEMS_PER_GOAL as usize {
            return Err(OrchestratorError::CorruptGoal(
                "external work item history exceeds the durable bound".to_owned(),
            ));
        }
        if self.receipts.len() > MAX_SUBMISSION_RECEIPTS {
            return Err(OrchestratorError::CorruptGoal(
                "external submission receipt set exceeds the durable bound".to_owned(),
            ));
        }
        let mut work_ids = std::collections::BTreeSet::new();
        let mut history_count = 0u32;
        for summary in &self.work_history {
            summary.work_id.validate()?;
            if summary.request_digest.len() != 64
                || !summary
                    .request_digest
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit())
            {
                return Err(OrchestratorError::CorruptGoal(
                    "external work history carries an invalid request digest".to_owned(),
                ));
            }
            if !work_ids.insert(summary.work_id.clone()) {
                return Err(OrchestratorError::CorruptGoal(
                    "external work history contains a duplicate work_id".to_owned(),
                ));
            }
            history_count = history_count.saturating_add(1);
        }
        if let Some(item) = self.pending_work.as_ref() {
            item.validate()?;
            if item.goal_id != goal_id || item.session_id != session_id {
                return Err(OrchestratorError::CorruptGoal(
                    "external work item owner binding does not match its Goal".to_owned(),
                ));
            }
            if item.state.is_terminal() {
                return Err(OrchestratorError::CorruptGoal(
                    "the active external work slot holds a terminal work item".to_owned(),
                ));
            }
            if !work_ids.insert(item.work_id.clone()) {
                return Err(OrchestratorError::CorruptGoal(
                    "the active external work item duplicates a history entry".to_owned(),
                ));
            }
            history_count = history_count.saturating_add(1);
        }
        if history_count > self.issue_counter {
            return Err(OrchestratorError::CorruptGoal(
                "external work history exceeds the issued counter".to_owned(),
            ));
        }
        let mut submission_ids = std::collections::BTreeSet::new();
        for receipt in &self.receipts {
            receipt.validate()?;
            let issued_by_this_goal = self
                .pending_work
                .as_ref()
                .is_some_and(|item| item.work_id == receipt.work_id)
                || self
                    .work_history
                    .iter()
                    .any(|summary| summary.work_id == receipt.work_id);
            if !issued_by_this_goal {
                return Err(OrchestratorError::CorruptGoal(
                    "submission receipt references a work item this Goal never issued".to_owned(),
                ));
            }
            if !submission_ids.insert(receipt.submission_id.clone()) {
                return Err(OrchestratorError::CorruptGoal(
                    "external receipts contain a duplicate submission_id".to_owned(),
                ));
            }
        }
        for (role, attempts) in &self.role_attempts {
            WorkRole::parse(role)?;
            if *attempts > MAX_ROLE_ATTEMPTS {
                return Err(OrchestratorError::CorruptGoal(
                    "external role attempt counter exceeds its budget".to_owned(),
                ));
            }
        }
        if let Some(stop) = self.last_stop.as_ref()
            && (stop.code.trim().is_empty() || stop.code.len() > MAX_STOP_CODE_BYTES)
        {
            return Err(OrchestratorError::CorruptGoal(
                "external stop code is not bounded".to_owned(),
            ));
        }
        Ok(())
    }
}

/// SHA-256 hex digest of arbitrary bytes.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Canonical JSON used for `request_digest` and `payload_sha256`.
///
/// `serde_json::Map` is a `BTreeMap` without the `preserve_order` feature, so
/// serialization already emits keys in lexical order and the digest is
/// independent of the caller's key order. This function exists to make that a
/// tested, single place rather than an implicit assumption.
pub(crate) fn canonical_json(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned())
}

/// Canonical digest of a submitted response.
pub(crate) fn payload_digest(value: &serde_json::Value) -> String {
    sha256_hex(canonical_json(value).as_bytes())
}

/// Why a submitted response is outside the V1 contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResponseBoundViolation {
    NotAnObject,
    TooLarge { bytes: usize, limit: usize },
    TooDeep { depth: usize, limit: usize },
}

impl ResponseBoundViolation {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::NotAnObject => "RESPONSE_NOT_AN_OBJECT",
            Self::TooLarge { .. } => "RESPONSE_TOO_LARGE",
            Self::TooDeep { .. } => "RESPONSE_TOO_DEEP",
        }
    }

    pub(crate) fn detail(self) -> String {
        match self {
            Self::NotAnObject => "response must be a single JSON object".to_owned(),
            Self::TooLarge { bytes, limit } => {
                format!("response is {bytes} bytes, above the {limit} byte host limit")
            }
            Self::TooDeep { depth, limit } => {
                format!("response nests {depth} levels, above the host limit of {limit}")
            }
        }
    }
}

/// Enforce B-08 on a submitted response *before* it is handed to a role
/// validator. Returns the serialized size for the audit record.
///
/// The check is deliberately cheap and bounded: it never allocates a copy of the
/// response beyond the serialization `serde_json` already needs.
pub(crate) fn check_response_bounds(
    value: &serde_json::Value,
) -> Result<usize, ResponseBoundViolation> {
    if !value.is_object() {
        return Err(ResponseBoundViolation::NotAnObject);
    }
    let bytes = serde_json::to_vec(value).map_or(0, |bytes| bytes.len());
    if bytes > MAX_MODEL_RESPONSE_BYTES {
        return Err(ResponseBoundViolation::TooLarge {
            bytes,
            limit: MAX_MODEL_RESPONSE_BYTES,
        });
    }
    let depth = json_depth(value);
    if depth > MAX_JSON_NESTING_DEPTH {
        return Err(ResponseBoundViolation::TooDeep {
            depth,
            limit: MAX_JSON_NESTING_DEPTH,
        });
    }
    Ok(bytes)
}

/// Maximum nesting depth, computed iteratively so a hostile payload cannot
/// overflow the stack while the host is measuring it.
pub(crate) fn json_depth(value: &serde_json::Value) -> usize {
    let mut deepest = 0usize;
    let mut stack: Vec<(&serde_json::Value, usize)> = vec![(value, 1)];
    while let Some((current, depth)) = stack.pop() {
        deepest = deepest.max(depth);
        match current {
            serde_json::Value::Array(items) => {
                for item in items {
                    stack.push((item, depth + 1));
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values() {
                    stack.push((item, depth + 1));
                }
            }
            _ => {}
        }
    }
    deepest
}

/// Truncate to a byte bound on a UTF-8 char boundary.
pub(crate) fn truncate_on_char_boundary(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(counter: u32, role: WorkRole) -> ModelWorkItem {
        ModelWorkItem::pending(
            WorkId::new(),
            "session-under-test",
            "11111111-1111-4111-8111-111111111111",
            role,
            None,
            None,
            3,
            0,
            sha256_hex(b"{}"),
            "{}".to_owned(),
            OUTPUT_CONTRACT_PLANNER_V1,
            counter,
            "2026-11-07T00:00:00Z",
        )
        .unwrap()
    }

    fn accepted_receipt(item: &ModelWorkItem, submission: &SubmissionId) -> SubmissionReceipt {
        SubmissionReceipt::new(
            item.work_id().clone(),
            submission.clone(),
            sha256_hex(b"{\"a\":1}"),
            SubmissionDecision::AcceptedForHostProcessing,
            None,
            "OK",
            None,
            "2026-11-07T00:00:01Z",
        )
    }

    #[test]
    fn canonical_digest_is_independent_of_key_order() {
        let left: serde_json::Value = serde_json::from_str(r#"{"b":1,"a":{"d":2,"c":3}}"#).unwrap();
        let right: serde_json::Value =
            serde_json::from_str(r#"{"a":{"c":3,"d":2},"b":1}"#).unwrap();
        assert_eq!(payload_digest(&left), payload_digest(&right));
        assert_eq!(canonical_json(&left), r#"{"a":{"c":3,"d":2},"b":1}"#);
    }

    #[test]
    fn response_bounds_reject_non_object_oversize_and_deep_payloads() {
        assert_eq!(
            check_response_bounds(&json!([1, 2])).unwrap_err(),
            ResponseBoundViolation::NotAnObject
        );
        let deep = {
            let mut value = json!(0);
            for _ in 0..MAX_JSON_NESTING_DEPTH + 2 {
                value = json!([value]);
            }
            json!({ "response": value })
        };
        assert!(matches!(
            check_response_bounds(&deep).unwrap_err(),
            ResponseBoundViolation::TooDeep { .. }
        ));
        let wide = json!({ "response": "x".repeat(MAX_MODEL_RESPONSE_BYTES + 1) });
        assert!(matches!(
            check_response_bounds(&wide).unwrap_err(),
            ResponseBoundViolation::TooLarge { .. }
        ));
        assert!(check_response_bounds(&json!({})).is_ok());
    }

    #[test]
    fn only_one_active_work_item_and_budget_is_consumed_once() {
        let mut state = ExternalModelState::new();
        assert_eq!(state.check_issue_budget(WorkRole::Planner), Ok(()));
        state.record_issued(item(1, WorkRole::Planner)).unwrap();
        assert_eq!(state.issue_counter(), 1);
        assert_eq!(state.role_attempts(WorkRole::Planner), 1);
        assert_eq!(
            state.check_issue_budget(WorkRole::Planner),
            Err(IssueBlock::PendingWorkItemExists)
        );
        // A second active item is refused even if the counter is spoofed.
        assert!(state.record_issued(item(2, WorkRole::Planner)).is_err());
        assert_eq!(state.issue_counter(), 1);
    }

    #[test]
    fn submission_transitions_are_atomic_and_reject_terminal_slots() {
        let mut state = ExternalModelState::new();
        let item = item(1, WorkRole::Planner);
        let work_id = item.work_id().clone();
        state.record_issued(item).unwrap();
        let submission = SubmissionId::parse("22222222-2222-4222-8222-222222222222").unwrap();
        let receipt = SubmissionReceipt::new(
            work_id.clone(),
            submission.clone(),
            sha256_hex(b"{}"),
            SubmissionDecision::AcceptedForHostProcessing,
            None,
            "OK",
            None,
            "2026-11-07T00:00:01Z",
        );
        state
            .record_submission(receipt, "2026-11-07T00:00:01Z")
            .unwrap();
        assert_eq!(
            state.active_work().unwrap().state(),
            WorkItemState::Submitted
        );
        assert_eq!(state.receipts().len(), 1);
        assert!(state.find_receipt(&submission).is_some());
        // The accepted submission is not terminal, so a second submission is
        // refused instead of double-recording.
        let second = SubmissionId::parse("33333333-3333-4333-8333-333333333333").unwrap();
        assert!(
            state
                .record_submission(
                    SubmissionReceipt::new(
                        work_id,
                        second,
                        sha256_hex(b"{}"),
                        SubmissionDecision::AcceptedForHostProcessing,
                        None,
                        "OK",
                        None,
                        "2026-11-07T00:00:02Z",
                    ),
                    "2026-11-07T00:00:02Z",
                )
                .is_err()
        );
    }

    #[test]
    fn rejected_submission_frees_the_active_slot_and_is_audited() {
        let mut state = ExternalModelState::new();
        let item = item(1, WorkRole::Planner);
        let work_id = item.work_id().clone();
        state.record_issued(item).unwrap();
        let submission = SubmissionId::parse("44444444-4444-4444-8444-444444444444").unwrap();
        state
            .record_submission(
                SubmissionReceipt::new(
                    work_id,
                    submission,
                    sha256_hex(b"{}"),
                    SubmissionDecision::RejectedSchema,
                    None,
                    "ROLE_VALIDATION_FAILED",
                    Some("planner proposal shape mismatch".to_owned()),
                    "2026-11-07T00:00:01Z",
                ),
                "2026-11-07T00:00:01Z",
            )
            .unwrap();
        assert!(state.active_work().is_none());
        assert_eq!(state.work_history().len(), 1);
        assert_eq!(state.work_history()[0].state(), WorkItemState::Rejected);
        // Budget is not refunded by a rejection.
        assert_eq!(state.issue_counter(), 1);
        assert_eq!(state.role_attempts(WorkRole::Planner), 1);
    }

    #[test]
    fn receipt_capacity_refuses_rather_than_evicting() {
        let mut state = ExternalModelState::new();
        let item = item(1, WorkRole::Planner);
        let work_id = item.work_id().clone();
        state.record_issued(item).unwrap();
        for index in 0..MAX_SUBMISSION_RECEIPTS {
            let submission = SubmissionId::parse(&Uuid::new_v4().to_string()).unwrap();
            let decision = if index == 0 {
                SubmissionDecision::AcceptedForHostProcessing
            } else {
                SubmissionDecision::RejectedSchema
            };
            let receipt = SubmissionReceipt::new(
                work_id.clone(),
                submission,
                sha256_hex(format!("payload-{index}").as_bytes()),
                decision,
                None,
                "OK",
                None,
                "2026-11-07T00:00:00Z",
            );
            // Only the first acceptance can succeed; later ones are rejected
            // because the slot is no longer PENDING.
            if index == 0 {
                state
                    .record_submission(receipt, "2026-11-07T00:00:00Z")
                    .unwrap();
            } else {
                state.receipts.push(receipt);
            }
        }
        assert!(!state.receipt_capacity_available());
        state
            .validate_for_goal("11111111-1111-4111-8111-111111111111", "session-under-test")
            .unwrap();
    }

    #[test]
    fn validation_rejects_cross_goal_and_cross_session_bindings() {
        let mut state = ExternalModelState::new();
        state.record_issued(item(1, WorkRole::Planner)).unwrap();
        state
            .validate_for_goal("11111111-1111-4111-8111-111111111111", "session-under-test")
            .unwrap();
        assert!(
            state
                .validate_for_goal("11111111-1111-4111-8111-111111111111", "another-session")
                .is_err()
        );
        assert!(
            state
                .validate_for_goal("99999999-9999-4999-8999-999999999999", "session-under-test")
                .is_err()
        );
    }

    #[test]
    fn validation_rejects_unknown_schema_and_corrupt_counters() {
        let mut state = ExternalModelState::new();
        state.schema_version = 99;
        assert!(matches!(
            state.validate_for_goal("1", "s"),
            Err(OrchestratorError::UnsupportedSchema(99))
        ));
        let mut state = ExternalModelState::new();
        state.issue_counter = MAX_MODEL_WORK_ITEMS_PER_GOAL + 1;
        assert!(
            state
                .validate_for_goal("11111111-1111-4111-8111-111111111111", "s")
                .is_err()
        );
    }

    #[test]
    fn ids_are_canonical_lowercase_uuids_only() {
        assert!(WorkId::parse("not-a-uuid").is_err());
        let upper = Uuid::new_v4().to_string().to_uppercase();
        assert!(WorkId::parse(&upper).is_err());
        assert!(SubmissionId::parse(&upper).is_err());
    }

    #[test]
    fn truncation_never_splits_a_utf8_sequence() {
        let text = "あ".repeat(100);
        let truncated = truncate_on_char_boundary(&text, 5);
        assert_eq!(truncated.len(), 3);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[test]
    fn accepted_receipt_helper_is_consistent() {
        let item = item(1, WorkRole::Planner);
        let submission = SubmissionId::parse(&Uuid::new_v4().to_string()).unwrap();
        let receipt = accepted_receipt(&item, &submission);
        assert_eq!(
            receipt.decision(),
            SubmissionDecision::AcceptedForHostProcessing
        );
        assert_eq!(receipt.committed_revision(), None);
        assert_eq!(receipt.work_id(), item.work_id());
    }
}
