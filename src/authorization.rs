//! Authorization B — GoalWriteGrantV1.
//!
//! This module contains the **model and pure rules** for a Goal-scoped write
//! grant. It deliberately contains no IPC and no task-store mutation: the
//! durable ledger I/O lives in [`crate::authorization_store`], and the Owner
//! Control Surface (console) lives in [`crate::authorization_owner`].
//!
//! Authority rules pinned here:
//!
//! 1. A grant is an authority record, never a request field. Nothing in this
//!    module deserializes a grant from a model response or an MCP argument.
//! 2. Every field that can redirect a write is bound to a durable identity:
//!    session, Goal, backend, Goal identity digest, and workspace identity.
//! 3. States are never collapsed into a boolean. `REVOKE_REQUESTED` and
//!    `REVOKED` are distinct, and an expired or revoked grant is never revived.
//! 4. Budget is charged before the side effect, idempotently per operation id,
//!    and is never refunded for an operation that may have occurred.
//! 5. An unreadable or disagreeing record fails closed. Absence of a grant is
//!    never permission.

#![allow(
    dead_code,
    reason = "A1 lands the model before A2 wires every accessor; A2 removes this when the gate uses all of it."
)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config;
use crate::goal::Goal;
use crate::managed_worktree::{ManagedWorktreeLifecycle, WorkspaceMode};
use crate::orchestrator_error::OrchestratorError;
use crate::workspace_snapshot::NormalizedWorkspacePath;

/// Durable schema of the authorization ledger.
pub(crate) const AUTHORIZATION_SCHEMA_VERSION: u32 = 1;

/// Backend identity a grant is issued for. EXTERNAL is the only backend that can
/// carry a write grant in V1.
pub(crate) const BACKEND_IDENTITY_EXTERNAL_WRITER_V1: &str = "EXTERNAL/writer-v1";

/// Maximum number of path rules the Owner may approve for one Goal.
pub(crate) const MAX_GRANT_PATH_RULES: usize = 64;
/// Maximum bytes in one root-relative path rule.
pub(crate) const MAX_GRANT_PATH_BYTES: usize = 4096;
/// Maximum operations a single grant may authorize.
pub(crate) const MAX_GRANT_OPERATIONS: u32 = 64;
/// Maximum content bytes a single grant may authorize.
pub(crate) const MAX_GRANT_BYTES: u64 = 16 * 1024 * 1024;
/// Minimum grant lifetime (seconds).
pub(crate) const MIN_GRANT_TTL_SECONDS: u64 = 60;
/// Maximum grant lifetime (seconds).
pub(crate) const MAX_GRANT_TTL_SECONDS: u64 = 7 * 24 * 60 * 60;
/// A pending request a human has not answered within this window is stale.
pub(crate) const PENDING_GRANT_TTL_SECONDS: u64 = 15 * 60;
/// Bounded retained grant generations per Goal.
pub(crate) const MAX_RETAINED_GRANTS: usize = 8;
/// Bounded retained Owner decision records per Goal.
pub(crate) const MAX_RETAINED_DECISIONS: usize = 16;
/// Bound for the redacted deny reason.
pub(crate) const MAX_DENY_REASON_BYTES: usize = 256;
/// The publication/revocation fence wait. Bounded so a revocation request is
/// never blocked indefinitely by a writer.
pub(crate) const AUTHORIZATION_FENCE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Characters that may not reach the Owner console or a display-bound name.
///
/// Covers C0/C1/DEL (`char::is_control`), the Unicode bidi controls (including
/// ALM/LRM/RLM, embeddings/overrides, isolates and deprecated formatting
/// characters), the line/paragraph separators, the BOM, and the tag block.
pub(crate) fn is_console_unsafe(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{00ad}'
                | '\u{0600}'..='\u{0605}'
                | '\u{061c}'
                | '\u{06dd}'
                | '\u{070f}'
                | '\u{0890}'..='\u{0891}'
                | '\u{08e2}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{110bd}'
                | '\u{110cd}'
                | '\u{13430}'..='\u{1343f}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0000}'..='\u{e007f}'
        )
}

/// The only write operation kind V1 can grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum GrantOperation {
    WriteUtf8,
}

impl GrantOperation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::WriteUtf8 => "WRITE_UTF8",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "WRITE_UTF8" => Some(Self::WriteUtf8),
            _ => None,
        }
    }
}

/// Whether a path rule authorizes one exact file or a whole subtree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum GrantPathKind {
    File,
    Tree,
}

/// One Owner-approved path rule, stored root-relative and `/`-separated.
///
/// Storing the rule relative to the *execution root* is what makes the same
/// grant usable for PRIMARY and for the host-created managed worktree while
/// still refusing a workspace transplant: the workspace identity (absolute root,
/// worktree id, base commit) is bound separately.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrantPathRule {
    path: String,
    kind: GrantPathKind,
}

impl GrantPathRule {
    pub(crate) fn new(path: impl Into<String>, kind: GrantPathKind) -> Result<Self, GrantError> {
        let rule = Self {
            path: path.into(),
            kind,
        };
        rule.validate()?;
        Ok(rule)
    }

    /// Parse one Owner-facing rule spelling.
    ///
    /// `src/**` and `src/` are subtrees; `docs/design.md` is one exact file.
    /// `**` alone authorizes the whole workspace root and must be typed
    /// explicitly by the Owner.
    pub(crate) fn parse(value: &str) -> Result<Self, GrantError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(GrantError::InvalidRequest("empty path rule"));
        }
        if value.len() > MAX_GRANT_PATH_BYTES {
            return Err(GrantError::InvalidRequest("path rule is too long"));
        }
        if value == "**" {
            return Self::new("", GrantPathKind::Tree);
        }
        if let Some(prefix) = value.strip_suffix("/**") {
            return Self::new(prefix.trim_end_matches('/'), GrantPathKind::Tree);
        }
        if value.contains("**") {
            return Err(GrantError::InvalidRequest(
                "wildcards are only allowed as a trailing /**",
            ));
        }
        if value.ends_with('/') {
            return Self::new(value.trim_end_matches('/'), GrantPathKind::Tree);
        }
        Self::new(value, GrantPathKind::File)
    }

    fn validate(&self) -> Result<(), GrantError> {
        if self.path.len() > MAX_GRANT_PATH_BYTES {
            return Err(GrantError::InvalidRequest("path rule is too long"));
        }
        // The Owner reads this spelling on the console before approving it, so a
        // control character or a bidi override is an approval-UI injection
        // primitive, not merely a strange filename.
        if self.path.chars().any(is_console_unsafe) {
            return Err(GrantError::InvalidRequest(
                "path rule contains a control or bidi character",
            ));
        }
        if self.path.is_empty() {
            if self.kind != GrantPathKind::Tree {
                return Err(GrantError::InvalidRequest(
                    "the workspace root rule must be a subtree rule",
                ));
            }
            return Ok(());
        }
        NormalizedWorkspacePath::parse(self.path.clone()).map_err(|_| {
            GrantError::InvalidRequest("path rule is not a normalized relative path")
        })?;
        let lowered = self.path.to_ascii_lowercase();
        if lowered == ".git"
            || lowered.starts_with(".git/")
            || lowered.contains("/.git/")
            || lowered.ends_with("/.git")
        {
            return Err(GrantError::InvalidRequest(
                "path rule targets Git administrative data",
            ));
        }
        Ok(())
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn kind(&self) -> GrantPathKind {
        self.kind
    }

    /// Whether this rule authorizes the normalized operation path.
    pub(crate) fn permits(&self, candidate: &NormalizedWorkspacePath) -> bool {
        match self.kind {
            GrantPathKind::File => self.path == candidate.as_str(),
            GrantPathKind::Tree => {
                if self.path.is_empty() {
                    return true;
                }
                candidate.as_str() == self.path
                    || candidate
                        .as_str()
                        .strip_prefix(self.path.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            }
        }
    }
}

/// Bounded write budget for one grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteBudgetV1 {
    max_operations: u32,
    max_bytes: u64,
}

impl WriteBudgetV1 {
    pub(crate) fn new(max_operations: u32, max_bytes: u64) -> Result<Self, GrantError> {
        if max_operations == 0 || max_operations > MAX_GRANT_OPERATIONS {
            return Err(GrantError::InvalidRequest("max_operations is out of range"));
        }
        if max_bytes == 0 || max_bytes > MAX_GRANT_BYTES {
            return Err(GrantError::InvalidRequest("max_bytes is out of range"));
        }
        Ok(Self {
            max_operations,
            max_bytes,
        })
    }

    pub(crate) fn max_operations(&self) -> u32 {
        self.max_operations
    }

    pub(crate) fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Whether adding `operations`/`bytes` stays within budget.
    pub(crate) fn allows(
        &self,
        used_operations: u32,
        used_bytes: u64,
        operations: u32,
        bytes: u64,
    ) -> bool {
        used_operations
            .checked_add(operations)
            .is_some_and(|total| total <= self.max_operations)
            && used_bytes
                .checked_add(bytes)
                .is_some_and(|total| total <= self.max_bytes)
    }
}

/// Durable usage accounting. `charged_operation_ids` is the idempotency table:
/// one operation id is charged at most once, and a charge is never removed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrantUsage {
    operations: u32,
    bytes: u64,
    charged_operation_ids: Vec<String>,
}

impl GrantUsage {
    pub(crate) fn operations(&self) -> u32 {
        self.operations
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    pub(crate) fn is_charged(&self, operation_id: &str) -> bool {
        self.charged_operation_ids
            .iter()
            .any(|id| id == operation_id)
    }

    pub(crate) fn charge(&mut self, operation_id: &str, bytes: u64) -> Result<(), GrantError> {
        if operation_id.trim().is_empty() || operation_id.len() > 128 {
            return Err(GrantError::InvalidRequest("operation id is not bounded"));
        }
        if self.is_charged(operation_id) {
            return Ok(());
        }
        if self.charged_operation_ids.len() >= MAX_GRANT_OPERATIONS as usize {
            return Err(GrantError::BudgetExhausted);
        }
        self.operations = self
            .operations
            .checked_add(1)
            .ok_or(GrantError::BudgetExhausted)?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(GrantError::BudgetExhausted)?;
        self.charged_operation_ids.push(operation_id.to_owned());
        Ok(())
    }

    fn validate(&self, budget: &WriteBudgetV1) -> Result<(), GrantError> {
        // `operations` may exceed the id table after a ledger restore: the Goal
        // anchor keeps the spent count while the restored ledger lost the ids.
        // It may never be below the table, and it may never exceed the budget.
        if self.operations < self.charged_operation_ids.len() as u32 {
            return Err(GrantError::Corrupt(
                "grant usage count is below its charge table",
            ));
        }
        if self.charged_operation_ids.len() > MAX_GRANT_OPERATIONS as usize
            || self.operations > budget.max_operations
            || self.bytes > budget.max_bytes
        {
            return Err(GrantError::Corrupt("grant usage exceeds its own budget"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for id in &self.charged_operation_ids {
            if id.trim().is_empty() || id.len() > 128 || !seen.insert(id) {
                return Err(GrantError::Corrupt(
                    "grant charge table contains an invalid or duplicate id",
                ));
            }
        }
        Ok(())
    }
}

/// Revocation is a state, not a boolean.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum GrantRevocationState {
    NotRevoked,
    RevokeRequested,
    Revoked,
}

impl GrantRevocationState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NotRevoked => "NOT_REVOKED",
            Self::RevokeRequested => "REVOKE_REQUESTED",
            Self::Revoked => "REVOKED",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrantRevocation {
    state: GrantRevocationState,
    requested_at: Option<String>,
    revoked_at: Option<String>,
    owner_decision_id: Option<String>,
}

impl GrantRevocation {
    fn not_revoked() -> Self {
        Self {
            state: GrantRevocationState::NotRevoked,
            requested_at: None,
            revoked_at: None,
            owner_decision_id: None,
        }
    }

    pub(crate) fn state(&self) -> GrantRevocationState {
        self.state
    }

    pub(crate) fn requested_at(&self) -> Option<&str> {
        self.requested_at.as_deref()
    }

    pub(crate) fn revoked_at(&self) -> Option<&str> {
        self.revoked_at.as_deref()
    }

    pub(crate) fn validate(&self) -> Result<(), GrantError> {
        match self.state {
            GrantRevocationState::NotRevoked => {
                if self.requested_at.is_some()
                    || self.revoked_at.is_some()
                    || self.owner_decision_id.is_some()
                {
                    return Err(GrantError::Corrupt(
                        "a non-revoked grant carries revocation evidence",
                    ));
                }
            }
            GrantRevocationState::RevokeRequested => {
                if self.requested_at.is_none() || self.revoked_at.is_some() {
                    return Err(GrantError::Corrupt(
                        "a revocation request must carry only its request time",
                    ));
                }
            }
            GrantRevocationState::Revoked => {
                if self.requested_at.is_none()
                    || self.revoked_at.is_none()
                    || self.owner_decision_id.is_none()
                {
                    return Err(GrantError::Corrupt(
                        "a revoked grant must carry request, revocation, and decision evidence",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// How the Owner decision was obtained. Deliberately typed, and deliberately not
/// named "authenticated": see the ADR's residual risk R-1.
///
/// `TestFixtureV1` (H5-C) is **not an Owner decision**. It is the marker a
/// disposable test fixture stamps on a grant so that every authority-granting
/// code path refuses it outside the state root that minted it. It exists so the
/// Writer E2E does not have to fabricate a production approval, and so a test
/// grant can never be confused with one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum OwnerAttestation {
    ConsoleLiveNonceV1,
    /// H5-C test-only attestation. Usable only in a state root carrying a
    /// matching fixture marker (see `config::test_grant_fixture_id`).
    TestFixtureV1,
}

impl OwnerAttestation {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ConsoleLiveNonceV1 => "CONSOLE_LIVE_NONCE_V1",
            Self::TestFixtureV1 => "TEST_FIXTURE_V1",
        }
    }

    /// Whether this attestation proves the signer is outside the agent's OS
    /// principal. It does not, and the value must be reported honestly.
    pub(crate) const fn same_uid_process_resistance(self) -> bool {
        false
    }

    /// Whether this grant came from the disposable test fixture rather than a
    /// live Owner decision.
    pub(crate) const fn is_test_fixture(self) -> bool {
        matches!(self, Self::TestFixtureV1)
    }
}

/// The workspace a grant is bound to.
///
/// `root` is the canonical execution root (the primary checkout or the host
/// managed worktree). `worktree_id` and `base_commit` are present only for a
/// managed workspace, so a grant can never be replayed from a primary checkout
/// into a managed worktree or vice versa.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrantWorkspaceIdentity {
    mode: WorkspaceMode,
    root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worktree_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_commit: Option<String>,
}

impl GrantWorkspaceIdentity {
    pub(crate) fn from_goal(goal: &Goal) -> Result<Self, GrantError> {
        if goal.workspace_mode().is_primary() {
            let root = std::fs::canonicalize(goal.cwd()).map_err(|_| {
                GrantError::WorkspaceUnavailable("the primary workspace cannot be resolved")
            })?;
            return Self::new(WorkspaceMode::Primary, root, None, None);
        }
        let record = goal
            .managed_worktree()
            .ok_or(GrantError::WorkspaceUnavailable(
                "the managed Goal has no durable worktree record",
            ))?;
        if record.lifecycle() == ManagedWorktreeLifecycle::Blocked {
            return Err(GrantError::WorkspaceUnavailable(
                "the managed worktree is blocked",
            ));
        }
        // The managed root may not exist yet (REQUESTED). `canonical_future_path`
        // resolves the existing ancestors and keeps the compact Git-compatible
        // spelling that `validated_execution_root` and the writer paths use.
        let root = config::canonical_future_path(record.worktree_root()).map_err(|_| {
            GrantError::WorkspaceUnavailable("the managed worktree root cannot be resolved")
        })?;
        Self::new(
            WorkspaceMode::ManagedWorktree,
            root,
            Some(record.worktree_id().as_str().to_owned()),
            Some(record.base_commit().to_owned()),
        )
    }

    fn new(
        mode: WorkspaceMode,
        root: PathBuf,
        worktree_id: Option<String>,
        base_commit: Option<String>,
    ) -> Result<Self, GrantError> {
        let identity = Self {
            mode,
            root,
            worktree_id,
            base_commit,
        };
        identity.validate()?;
        Ok(identity)
    }

    fn validate(&self) -> Result<(), GrantError> {
        if !self.root.is_absolute() || self.root.to_string_lossy().contains("..") {
            return Err(GrantError::Corrupt("workspace root is not canonical"));
        }
        match self.mode {
            WorkspaceMode::Primary => {
                if self.worktree_id.is_some() || self.base_commit.is_some() {
                    return Err(GrantError::Corrupt(
                        "a primary workspace identity carries managed fields",
                    ));
                }
            }
            WorkspaceMode::ManagedWorktree => {
                let id = self
                    .worktree_id
                    .as_deref()
                    .ok_or(GrantError::Corrupt("managed workspace has no worktree id"))?;
                crate::managed_worktree::WorktreeId::parse(id)
                    .map_err(|_| GrantError::Corrupt("managed workspace id is malformed"))?;
                let base = self
                    .base_commit
                    .as_deref()
                    .ok_or(GrantError::Corrupt("managed workspace has no base commit"))?;
                crate::managed_worktree::validate_object_id(base, "grant base commit")
                    .map_err(|_| GrantError::Corrupt("managed base commit is malformed"))?;
            }
        }
        Ok(())
    }

    pub(crate) fn mode(&self) -> WorkspaceMode {
        self.mode
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn digest(&self) -> String {
        let mut framed = Vec::new();
        framed.extend_from_slice(b"local-mcp/workspace-identity/v1\0");
        push_framed(
            &mut framed,
            if self.mode.is_primary() {
                b"PRIMARY"
            } else {
                b"MANAGED_WORKTREE"
            },
        );
        push_framed(&mut framed, self.root.to_string_lossy().as_bytes());
        push_framed(
            &mut framed,
            self.worktree_id.as_deref().unwrap_or("").as_bytes(),
        );
        push_framed(
            &mut framed,
            self.base_commit.as_deref().unwrap_or("").as_bytes(),
        );
        sha256_hex(&framed)
    }
}

/// Untrusted Owner scope proposal, persisted by `goal_start` and shown to the
/// Owner. It is data, never authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestedWriteScopeV1 {
    allowed_paths: Vec<String>,
    allowed_operations: Vec<GrantOperation>,
    max_operations: u32,
    max_bytes: u64,
    expires_in_seconds: u64,
}

impl RequestedWriteScopeV1 {
    /// Validate an MCP-supplied proposal into a bounded host record.
    pub(crate) fn from_mcp_value(value: &serde_json::Value) -> Result<Self, GrantError> {
        let parsed: Self = serde_json::from_value(value.clone())
            .map_err(|_| GrantError::InvalidRequest("write_scope has an invalid shape"))?;
        parsed.validate()?;
        Ok(parsed)
    }

    pub(crate) fn validate(&self) -> Result<(), GrantError> {
        if self.allowed_paths.is_empty() || self.allowed_paths.len() > MAX_GRANT_PATH_RULES {
            return Err(GrantError::InvalidRequest(
                "allowed_paths must contain between 1 and 64 rules",
            ));
        }
        if self.allowed_operations.is_empty() {
            return Err(GrantError::InvalidRequest(
                "allowed_operations must not be empty",
            ));
        }
        if self.allowed_operations.len() > 1
            || self
                .allowed_operations
                .iter()
                .any(|operation| *operation != GrantOperation::WriteUtf8)
        {
            return Err(GrantError::InvalidRequest(
                "WRITE_UTF8 is the only operation V1 can authorize",
            ));
        }
        WriteBudgetV1::new(self.max_operations, self.max_bytes)?;
        if !(MIN_GRANT_TTL_SECONDS..=MAX_GRANT_TTL_SECONDS).contains(&self.expires_in_seconds) {
            return Err(GrantError::InvalidRequest(
                "expires_in_seconds is out of range",
            ));
        }
        for rule in &self.allowed_paths {
            GrantPathRule::parse(rule)?;
        }
        Ok(())
    }

    pub(crate) fn path_rules(&self) -> Result<Vec<GrantPathRule>, GrantError> {
        self.allowed_paths
            .iter()
            .map(|value| GrantPathRule::parse(value))
            .collect()
    }

    pub(crate) fn budget(&self) -> Result<WriteBudgetV1, GrantError> {
        WriteBudgetV1::new(self.max_operations, self.max_bytes)
    }

    pub(crate) fn expires_in_seconds(&self) -> u64 {
        self.expires_in_seconds
    }

    pub(crate) fn allowed_paths(&self) -> &[String] {
        &self.allowed_paths
    }

    pub(crate) fn allowed_operations(&self) -> &[GrantOperation] {
        &self.allowed_operations
    }
}

/// A durable request waiting for an Owner decision. The live nonce is stored
/// only as a digest, so a restarted process cannot approve its own request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingGrantRequestV1 {
    request_id: String,
    session_id: String,
    goal_id: String,
    backend_identity: String,
    goal_identity_digest: String,
    workspace: GrantWorkspaceIdentity,
    path_rules: Vec<GrantPathRule>,
    allowed_operations: Vec<GrantOperation>,
    budget: WriteBudgetV1,
    requested_at: String,
    expires_at: String,
    /// Lifetime the Owner is approving for the *grant*, measured from approval.
    /// The pending request has its own shorter TTL in `expires_at`.
    #[serde(default = "default_grant_lifetime_seconds")]
    requested_lifetime_seconds: u64,
    nonce_sha256: String,
}

fn default_grant_lifetime_seconds() -> u64 {
    3_600
}

impl PendingGrantRequestV1 {
    #[expect(
        clippy::too_many_arguments,
        reason = "Every field is a distinct durable authority binding for one request."
    )]
    pub(crate) fn new(
        request_id: String,
        session_id: String,
        goal_id: String,
        backend_identity: String,
        goal_identity_digest: String,
        workspace: GrantWorkspaceIdentity,
        path_rules: Vec<GrantPathRule>,
        allowed_operations: Vec<GrantOperation>,
        budget: WriteBudgetV1,
        requested_at: String,
        expires_at: String,
        requested_lifetime_seconds: u64,
        nonce_sha256: String,
    ) -> Result<Self, GrantError> {
        let request = Self {
            request_id,
            session_id,
            goal_id,
            backend_identity,
            goal_identity_digest,
            workspace,
            path_rules,
            allowed_operations,
            budget,
            requested_at,
            expires_at,
            requested_lifetime_seconds,
            nonce_sha256,
        };
        request.validate()?;
        Ok(request)
    }

    pub(crate) fn request_id(&self) -> &str {
        &self.request_id
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn goal_id(&self) -> &str {
        &self.goal_id
    }

    pub(crate) fn workspace(&self) -> &GrantWorkspaceIdentity {
        &self.workspace
    }

    pub(crate) fn path_rules(&self) -> &[GrantPathRule] {
        &self.path_rules
    }

    pub(crate) fn budget(&self) -> WriteBudgetV1 {
        self.budget
    }

    pub(crate) fn nonce_sha256(&self) -> &str {
        &self.nonce_sha256
    }

    pub(crate) fn expires_at(&self) -> &str {
        &self.expires_at
    }

    pub(crate) fn requested_lifetime_seconds(&self) -> u64 {
        self.requested_lifetime_seconds
    }

    pub(crate) fn validate(&self) -> Result<(), GrantError> {
        if uuid::Uuid::parse_str(&self.request_id).is_err()
            || uuid::Uuid::parse_str(&self.goal_id).is_err()
        {
            return Err(GrantError::Corrupt("pending grant request id is malformed"));
        }
        if self.session_id.trim().is_empty()
            || self.backend_identity != BACKEND_IDENTITY_EXTERNAL_WRITER_V1
        {
            return Err(GrantError::Corrupt(
                "pending grant request has an invalid owner binding",
            ));
        }
        if !is_sha256_hex(&self.goal_identity_digest) || !is_sha256_hex(&self.nonce_sha256) {
            return Err(GrantError::Corrupt(
                "pending grant request carries an invalid digest",
            ));
        }
        self.workspace.validate()?;
        if self.path_rules.is_empty() || self.path_rules.len() > MAX_GRANT_PATH_RULES {
            return Err(GrantError::Corrupt(
                "pending grant request has an invalid path rule set",
            ));
        }
        for rule in &self.path_rules {
            rule.validate()?;
        }
        if self.allowed_operations.len() != 1
            || self.allowed_operations[0] != GrantOperation::WriteUtf8
        {
            return Err(GrantError::Corrupt(
                "pending grant request has an invalid operation set",
            ));
        }
        WriteBudgetV1::new(self.budget.max_operations, self.budget.max_bytes)?;
        let requested = parse_utc_rfc3339_seconds(&self.requested_at)
            .ok_or(GrantError::Corrupt("pending request time is malformed"))?;
        let expires = parse_utc_rfc3339_seconds(&self.expires_at)
            .ok_or(GrantError::Corrupt("pending request expiry is malformed"))?;
        if expires <= requested || expires - requested > PENDING_GRANT_TTL_SECONDS as i64 {
            return Err(GrantError::Corrupt(
                "pending request lifetime is outside the host bound",
            ));
        }
        if !(MIN_GRANT_TTL_SECONDS..=MAX_GRANT_TTL_SECONDS)
            .contains(&self.requested_lifetime_seconds)
        {
            return Err(GrantError::Corrupt(
                "approved grant lifetime is outside the host bound",
            ));
        }
        Ok(())
    }
}

/// The durable grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GoalWriteGrantV1 {
    schema_version: u32,
    grant_id: String,
    owner_decision_id: String,
    session_id: String,
    goal_id: String,
    backend_identity: String,
    goal_identity_digest: String,
    workspace: GrantWorkspaceIdentity,
    path_rules: Vec<GrantPathRule>,
    allowed_operations: Vec<GrantOperation>,
    budget: WriteBudgetV1,
    issued_at: String,
    expires_at: String,
    authorization_epoch: u64,
    revocation: GrantRevocation,
    usage: GrantUsage,
    owner_attestation: OwnerAttestation,
    /// Present only on a `TEST_FIXTURE_V1` grant: the fixture root identity the
    /// grant is confined to. Absent for every Owner-issued grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    test_fixture_id: Option<String>,
}

impl GoalWriteGrantV1 {
    pub(crate) fn grant_id(&self) -> &str {
        &self.grant_id
    }

    pub(crate) fn authorization_epoch(&self) -> u64 {
        self.authorization_epoch
    }

    pub(crate) fn goal_identity_digest(&self) -> &str {
        &self.goal_identity_digest
    }

    pub(crate) fn workspace(&self) -> &GrantWorkspaceIdentity {
        &self.workspace
    }

    pub(crate) fn budget(&self) -> WriteBudgetV1 {
        self.budget
    }

    pub(crate) fn usage(&self) -> &GrantUsage {
        &self.usage
    }

    pub(crate) fn revocation(&self) -> &GrantRevocation {
        &self.revocation
    }

    pub(crate) fn expires_at(&self) -> &str {
        &self.expires_at
    }

    pub(crate) fn issued_at(&self) -> &str {
        &self.issued_at
    }

    pub(crate) fn owner_decision_id(&self) -> &str {
        &self.owner_decision_id
    }

    pub(crate) fn owner_attestation(&self) -> OwnerAttestation {
        self.owner_attestation
    }

    pub(crate) fn test_fixture_id(&self) -> Option<&str> {
        self.test_fixture_id.as_deref()
    }

    /// H5-C: stamp a grant as a disposable test-fixture grant.
    ///
    /// This grants nothing by itself: grant resolution refuses a
    /// `TEST_FIXTURE_V1` grant unless the running state root carries a marker
    /// with the same fixture id, and only the feature-gated harness can create
    /// the marker. There is no MCP surface that reaches this.
    pub(crate) fn stamp_test_fixture(mut self, fixture_id: String) -> Self {
        self.owner_attestation = OwnerAttestation::TestFixtureV1;
        self.test_fixture_id = Some(fixture_id);
        self
    }

    pub(crate) fn allowed_operations(&self) -> &[GrantOperation] {
        &self.allowed_operations
    }

    pub(crate) fn permits_operation(&self, operation: GrantOperation) -> bool {
        self.allowed_operations.contains(&operation)
    }

    pub(crate) fn permits_path(&self, candidate: &NormalizedWorkspacePath) -> bool {
        self.path_rules.iter().any(|rule| rule.permits(candidate))
    }

    pub(crate) fn validate(&self) -> Result<(), GrantError> {
        if self.schema_version != AUTHORIZATION_SCHEMA_VERSION {
            return Err(GrantError::UnsupportedSchema);
        }
        if uuid::Uuid::parse_str(&self.grant_id).is_err()
            || uuid::Uuid::parse_str(&self.owner_decision_id).is_err()
            || uuid::Uuid::parse_str(&self.goal_id).is_err()
        {
            return Err(GrantError::Corrupt("grant identity is malformed"));
        }
        if self.session_id.trim().is_empty()
            || self.backend_identity != BACKEND_IDENTITY_EXTERNAL_WRITER_V1
        {
            return Err(GrantError::Corrupt("grant has an invalid owner binding"));
        }
        if !is_sha256_hex(&self.goal_identity_digest) {
            return Err(GrantError::Corrupt(
                "grant Goal identity digest is malformed",
            ));
        }
        self.workspace.validate()?;
        if self.path_rules.is_empty() || self.path_rules.len() > MAX_GRANT_PATH_RULES {
            return Err(GrantError::Corrupt("grant has an invalid path rule set"));
        }
        for rule in &self.path_rules {
            rule.validate()?;
        }
        if self.allowed_operations.len() != 1
            || self.allowed_operations[0] != GrantOperation::WriteUtf8
        {
            return Err(GrantError::Corrupt("grant has an invalid operation set"));
        }
        WriteBudgetV1::new(self.budget.max_operations, self.budget.max_bytes)?;
        match (self.owner_attestation, self.test_fixture_id.as_deref()) {
            (OwnerAttestation::TestFixtureV1, Some(id)) if !id.is_empty() && id.len() <= 64 => {}
            (OwnerAttestation::TestFixtureV1, _) => {
                return Err(GrantError::Corrupt(
                    "a test-fixture grant must name its fixture root",
                ));
            }
            (_, Some(_)) => {
                return Err(GrantError::Corrupt(
                    "an Owner-issued grant must not carry a fixture identity",
                ));
            }
            (_, None) => {}
        }
        let issued = parse_utc_rfc3339_seconds(&self.issued_at)
            .ok_or(GrantError::Corrupt("grant issue time is malformed"))?;
        let expires = parse_utc_rfc3339_seconds(&self.expires_at)
            .ok_or(GrantError::Corrupt("grant expiry is malformed"))?;
        if expires <= issued || expires - issued > MAX_GRANT_TTL_SECONDS as i64 {
            return Err(GrantError::Corrupt(
                "grant lifetime is outside the host bound",
            ));
        }
        if self.authorization_epoch == 0 {
            return Err(GrantError::Corrupt("grant epoch must be non-zero"));
        }
        self.revocation.validate()?;
        self.usage.validate(&self.budget)?;
        Ok(())
    }

    fn remaining_budget_allows(&self, operations: u32, bytes: u64) -> bool {
        self.budget
            .allows(self.usage.operations, self.usage.bytes, operations, bytes)
    }
}

/// One durable Owner decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum OwnerDecisionKind {
    Approved,
    Denied,
    RevocationRequested,
    RevocationFinalized,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnerDecisionRecord {
    decision_id: String,
    request_id: String,
    kind: OwnerDecisionKind,
    decided_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nonce_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    owner_attestation: OwnerAttestation,
}

impl OwnerDecisionRecord {
    pub(crate) fn kind(&self) -> OwnerDecisionKind {
        self.kind
    }

    pub(crate) fn request_id(&self) -> &str {
        &self.request_id
    }

    fn validate(&self) -> Result<(), GrantError> {
        if uuid::Uuid::parse_str(&self.decision_id).is_err()
            || uuid::Uuid::parse_str(&self.request_id).is_err()
        {
            return Err(GrantError::Corrupt("Owner decision id is malformed"));
        }
        if parse_utc_rfc3339_seconds(&self.decided_at).is_none() {
            return Err(GrantError::Corrupt("Owner decision time is malformed"));
        }
        if let Some(reason) = self.reason.as_deref()
            && (reason.len() > MAX_DENY_REASON_BYTES || reason.contains('\0'))
        {
            return Err(GrantError::Corrupt("Owner decision reason is not bounded"));
        }
        Ok(())
    }
}

/// Ledger lifecycle. `EXPIRED` is derived from `expires_at`, never stored as an
/// independent state that could be edited back to ACTIVE.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum AuthorizationState {
    Pending,
    Active,
    Denied,
    Revoked,
}

impl AuthorizationState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Active => "ACTIVE",
            Self::Denied => "DENIED",
            Self::Revoked => "REVOKED",
        }
    }
}

/// Rollback anchor stored in the Goal's EXTERNAL state and written under Goal
/// CAS. The ledger cannot authorize a grant older than this anchor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthorizationAnchor {
    #[serde(default)]
    highest_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revoked_at_epoch: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_grant_id: Option<String>,
    /// Monotonic usage floor. Charges are mirrored here under Goal CAS so a
    /// restored (rolled-back) ledger cannot regain spent budget.
    #[serde(default)]
    usage_operations: u32,
    #[serde(default)]
    usage_bytes: u64,
}

impl AuthorizationAnchor {
    pub(crate) fn highest_epoch(&self) -> u64 {
        self.highest_epoch
    }

    pub(crate) fn revoked_at_epoch(&self) -> Option<u64> {
        self.revoked_at_epoch
    }

    pub(crate) fn last_grant_id(&self) -> Option<&str> {
        self.last_grant_id.as_deref()
    }

    pub(crate) fn record_grant(&mut self, grant_id: &str, epoch: u64) {
        // A stale-generation issuance must never displace a newer generation
        // the Goal has already observed.
        if epoch >= self.highest_epoch {
            self.highest_epoch = epoch;
            self.last_grant_id = Some(grant_id.to_owned());
        }
    }

    pub(crate) fn usage_operations(&self) -> u32 {
        self.usage_operations
    }

    pub(crate) fn usage_bytes(&self) -> u64 {
        self.usage_bytes
    }

    pub(crate) fn record_usage_floor(&mut self, operations: u32, bytes: u64) {
        if operations > self.usage_operations {
            self.usage_operations = operations;
        }
        if bytes > self.usage_bytes {
            self.usage_bytes = bytes;
        }
    }

    /// A new grant generation has its own budget; the previous generation's
    /// floor must not exhaust it. Rollback protection for the new generation is
    /// rebuilt by its own charges.
    pub(crate) fn reset_usage_floor(&mut self) {
        self.usage_operations = 0;
        self.usage_bytes = 0;
    }

    pub(crate) fn record_revocation(&mut self, epoch: u64) {
        if epoch > self.highest_epoch {
            self.highest_epoch = epoch;
        }
        self.revoked_at_epoch = Some(match self.revoked_at_epoch {
            Some(existing) if existing >= epoch => existing,
            _ => epoch,
        });
    }

    pub(crate) fn validate(&self) -> Result<(), GrantError> {
        if let Some(id) = self.last_grant_id.as_deref()
            && uuid::Uuid::parse_str(id).is_err()
        {
            return Err(GrantError::Corrupt(
                "authorization anchor grant id is malformed",
            ));
        }
        if self
            .revoked_at_epoch
            .is_some_and(|epoch| epoch > self.highest_epoch)
        {
            return Err(GrantError::Corrupt(
                "authorization anchor revokes an epoch it never observed",
            ));
        }
        Ok(())
    }
}

/// What a WorkItem / stage / journal records about the grant that authorized it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrantBinding {
    grant_id: String,
    authorization_epoch: u64,
    goal_identity_digest: String,
    workspace_digest: String,
}

impl GrantBinding {
    pub(crate) fn from_grant(grant: &GoalWriteGrantV1) -> Self {
        Self {
            grant_id: grant.grant_id.clone(),
            authorization_epoch: grant.authorization_epoch,
            goal_identity_digest: grant.goal_identity_digest.clone(),
            workspace_digest: grant.workspace.digest(),
        }
    }

    pub(crate) fn grant_id(&self) -> &str {
        &self.grant_id
    }

    pub(crate) fn authorization_epoch(&self) -> u64 {
        self.authorization_epoch
    }

    pub(crate) fn goal_identity_digest(&self) -> &str {
        &self.goal_identity_digest
    }

    pub(crate) fn workspace_digest(&self) -> &str {
        &self.workspace_digest
    }

    pub(crate) fn validate(&self) -> Result<(), GrantError> {
        if uuid::Uuid::parse_str(&self.grant_id).is_err() {
            return Err(GrantError::Corrupt("grant binding id is malformed"));
        }
        if self.authorization_epoch == 0
            || !is_sha256_hex(&self.goal_identity_digest)
            || !is_sha256_hex(&self.workspace_digest)
        {
            return Err(GrantError::Corrupt("grant binding is malformed"));
        }
        Ok(())
    }

    pub(crate) fn matches(&self, grant: &GoalWriteGrantV1) -> bool {
        self.grant_id == grant.grant_id
            && self.authorization_epoch == grant.authorization_epoch
            && self.goal_identity_digest == grant.goal_identity_digest
            && self.workspace_digest == grant.workspace.digest()
    }
}

/// The durable authorization ledger for one Goal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthorizationLedgerV1 {
    schema_version: u32,
    session_id: String,
    goal_id: String,
    backend_identity: String,
    goal_identity_digest: String,
    authorization_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<PendingGrantRequestV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    grants: Vec<GoalWriteGrantV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    decisions: Vec<OwnerDecisionRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_transition_at: Option<String>,
}

impl AuthorizationLedgerV1 {
    pub(crate) fn new(goal: &Goal) -> Result<Self, GrantError> {
        if !goal.model_backend().is_external() {
            return Err(GrantError::BackendNotExternal);
        }
        Ok(Self {
            schema_version: AUTHORIZATION_SCHEMA_VERSION,
            session_id: goal.session_id().to_owned(),
            goal_id: goal.id().as_str().to_owned(),
            backend_identity: BACKEND_IDENTITY_EXTERNAL_WRITER_V1.to_owned(),
            goal_identity_digest: goal_identity_digest(goal)?,
            authorization_epoch: 0,
            pending: None,
            grants: Vec::new(),
            decisions: Vec::new(),
            last_transition_at: None,
        })
    }

    pub(crate) fn validate(&self) -> Result<(), GrantError> {
        if self.schema_version != AUTHORIZATION_SCHEMA_VERSION {
            return Err(GrantError::UnsupportedSchema);
        }
        if self.session_id.trim().is_empty()
            || self.backend_identity != BACKEND_IDENTITY_EXTERNAL_WRITER_V1
        {
            return Err(GrantError::Corrupt("ledger has an invalid owner binding"));
        }
        if uuid::Uuid::parse_str(&self.goal_id).is_err()
            || !is_sha256_hex(&self.goal_identity_digest)
        {
            return Err(GrantError::Corrupt("ledger Goal identity is malformed"));
        }
        if self.grants.len() > MAX_RETAINED_GRANTS || self.decisions.len() > MAX_RETAINED_DECISIONS
        {
            return Err(GrantError::Corrupt("ledger history exceeds its bound"));
        }
        let mut grant_ids = std::collections::BTreeSet::new();
        let mut last_epoch = 0u64;
        for grant in &self.grants {
            grant.validate()?;
            if grant.session_id != self.session_id
                || grant.goal_id != self.goal_id
                || grant.backend_identity != self.backend_identity
            {
                return Err(GrantError::Corrupt(
                    "ledger contains a grant bound to another owner",
                ));
            }
            if grant.goal_identity_digest != self.goal_identity_digest {
                return Err(GrantError::Corrupt(
                    "ledger contains a grant for another Goal identity",
                ));
            }
            if !grant_ids.insert(grant.grant_id.clone()) {
                return Err(GrantError::Corrupt("ledger contains a duplicate grant id"));
            }
            if grant.authorization_epoch < last_epoch {
                return Err(GrantError::Corrupt("ledger grant epochs are not monotonic"));
            }
            last_epoch = grant.authorization_epoch;
        }
        if let Some(pending) = self.pending.as_ref() {
            pending.validate()?;
            if pending.session_id != self.session_id
                || pending.goal_id != self.goal_id
                || pending.backend_identity != self.backend_identity
                || pending.goal_identity_digest != self.goal_identity_digest
            {
                return Err(GrantError::Corrupt(
                    "pending request is bound to another owner",
                ));
            }
        }
        for decision in &self.decisions {
            decision.validate()?;
        }
        if self.grants.len() + self.decisions.len() == 0 && self.authorization_epoch != 0 {
            return Err(GrantError::Corrupt(
                "an empty ledger has a non-zero authorization epoch",
            ));
        }
        if last_epoch > self.authorization_epoch {
            return Err(GrantError::Corrupt(
                "ledger epoch is behind its latest grant",
            ));
        }
        Ok(())
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn goal_id(&self) -> &str {
        &self.goal_id
    }

    pub(crate) fn goal_identity_digest(&self) -> &str {
        &self.goal_identity_digest
    }

    pub(crate) fn authorization_epoch(&self) -> u64 {
        self.authorization_epoch
    }

    pub(crate) fn pending(&self) -> Option<&PendingGrantRequestV1> {
        self.pending.as_ref()
    }

    pub(crate) fn latest_grant(&self) -> Option<&GoalWriteGrantV1> {
        self.grants.last()
    }

    pub(crate) fn grants(&self) -> &[GoalWriteGrantV1] {
        &self.grants
    }

    pub(crate) fn decisions(&self) -> &[OwnerDecisionRecord] {
        &self.decisions
    }

    /// Whether this ledger still matches the Goal it claims to authorize.
    pub(crate) fn matches_goal(&self, goal: &Goal) -> Result<(), GrantError> {
        let digest = goal_identity_digest(goal)?;
        if self.session_id != goal.session_id()
            || self.goal_id != goal.id().as_str()
            || self.goal_identity_digest != digest
        {
            return Err(GrantError::StateMismatch(
                "authorization ledger does not match this Goal",
            ));
        }
        Ok(())
    }

    /// Record one pending request. A newer request supersedes an undecided one.
    pub(crate) fn record_pending(
        &mut self,
        pending: PendingGrantRequestV1,
        now: &str,
    ) -> Result<(), GrantError> {
        if self.pending.is_some() {
            return Err(GrantError::PendingExists);
        }
        self.pending = Some(pending);
        self.last_transition_at = Some(now.to_owned());
        Ok(())
    }

    /// Build the grant an approval would produce, without mutating the ledger.
    ///
    /// The grant id is deterministic in `(request_id, epoch)` so a retry after a
    /// crash between the Goal anchor write and the ledger write produces exactly
    /// the grant the anchor already names.
    pub(crate) fn build_grant(
        &self,
        request_id: &str,
        nonce_sha256: &str,
        decision_id: String,
        now: &str,
    ) -> Result<GoalWriteGrantV1, GrantError> {
        let pending = self.pending.as_ref().ok_or(GrantError::NoPendingRequest)?;
        if pending.request_id != request_id {
            return Err(GrantError::NoPendingRequest);
        }
        if pending.nonce_sha256 != nonce_sha256 {
            return Err(GrantError::NonceMismatch);
        }
        let now_seconds =
            parse_utc_rfc3339_seconds(now).ok_or(GrantError::Corrupt("clock is malformed"))?;
        let expires = parse_utc_rfc3339_seconds(&pending.expires_at)
            .ok_or(GrantError::Corrupt("pending request expiry is malformed"))?;
        if now_seconds >= expires {
            return Err(GrantError::PendingExpired);
        }
        if let Some(previous) = self.latest_grant()
            && previous.revocation.state() == GrantRevocationState::NotRevoked
            && parse_utc_rfc3339_seconds(&previous.expires_at)
                .is_some_and(|value| value > now_seconds)
        {
            return Err(GrantError::AlreadyActive);
        }
        let epoch = self
            .authorization_epoch
            .checked_add(1)
            .ok_or(GrantError::Corrupt("authorization epoch overflow"))?;
        let grant = GoalWriteGrantV1 {
            schema_version: AUTHORIZATION_SCHEMA_VERSION,
            grant_id: deterministic_grant_id(request_id, epoch),
            owner_decision_id: decision_id,
            session_id: self.session_id.clone(),
            goal_id: self.goal_id.clone(),
            backend_identity: self.backend_identity.clone(),
            goal_identity_digest: self.goal_identity_digest.clone(),
            workspace: pending.workspace.clone(),
            path_rules: pending.path_rules.clone(),
            allowed_operations: pending.allowed_operations.clone(),
            budget: pending.budget,
            issued_at: now.to_owned(),
            expires_at: format_utc_rfc3339(now_seconds + pending.requested_lifetime_seconds as i64),
            authorization_epoch: epoch,
            revocation: GrantRevocation::not_revoked(),
            usage: GrantUsage::default(),
            owner_attestation: OwnerAttestation::ConsoleLiveNonceV1,
            test_fixture_id: None,
        };
        grant.validate()?;
        Ok(grant)
    }

    /// Commit a grant built by [`Self::build_grant`].
    pub(crate) fn record_approved(
        &mut self,
        grant: GoalWriteGrantV1,
        request_id: &str,
        decision_id: String,
        nonce_sha256: &str,
        now: &str,
    ) -> Result<(), GrantError> {
        self.record_approved_with_attestation(
            grant,
            request_id,
            decision_id,
            nonce_sha256,
            now,
            OwnerAttestation::ConsoleLiveNonceV1,
        )
    }

    /// [`Self::record_approved`] with an explicit decision attestation.
    ///
    /// The only caller that passes anything but `ConsoleLiveNonceV1` is the
    /// feature-gated H5-C fixture issuance, which stamps `TestFixtureV1` so the
    /// resulting grant is refused outside the disposable fixture root.
    pub(crate) fn record_approved_with_attestation(
        &mut self,
        grant: GoalWriteGrantV1,
        request_id: &str,
        decision_id: String,
        nonce_sha256: &str,
        now: &str,
        attestation: OwnerAttestation,
    ) -> Result<(), GrantError> {
        let pending = self.pending.take().ok_or(GrantError::NoPendingRequest)?;
        if pending.request_id != request_id {
            self.pending = Some(pending);
            return Err(GrantError::NoPendingRequest);
        }
        if grant.authorization_epoch != self.authorization_epoch.saturating_add(1) {
            self.pending = Some(pending);
            return Err(GrantError::Corrupt(
                "approval epoch is not the next generation",
            ));
        }
        self.authorization_epoch = grant.authorization_epoch;
        self.grants.push(grant);
        if self.grants.len() > MAX_RETAINED_GRANTS {
            self.grants.remove(0);
        }
        self.push_decision(OwnerDecisionRecord {
            decision_id,
            request_id: request_id.to_owned(),
            kind: OwnerDecisionKind::Approved,
            decided_at: now.to_owned(),
            nonce_sha256: Some(nonce_sha256.to_owned()),
            reason: None,
            owner_attestation: attestation,
        })?;
        self.last_transition_at = Some(now.to_owned());
        Ok(())
    }

    /// Replace an undecided pending request (used only when the previous one is
    /// already stale, which the caller proves from its expiry).
    pub(crate) fn replace_pending(
        &mut self,
        pending: PendingGrantRequestV1,
        now: &str,
    ) -> Result<(), GrantError> {
        self.pending = Some(pending);
        self.last_transition_at = Some(now.to_owned());
        Ok(())
    }

    /// Apply an Owner denial.
    pub(crate) fn deny(
        &mut self,
        request_id: &str,
        decision_id: String,
        reason: Option<String>,
        now: &str,
    ) -> Result<(), GrantError> {
        let pending = self.pending.take().ok_or(GrantError::NoPendingRequest)?;
        if pending.request_id != request_id {
            self.pending = Some(pending);
            return Err(GrantError::NoPendingRequest);
        }
        self.push_decision(OwnerDecisionRecord {
            decision_id,
            request_id: request_id.to_owned(),
            kind: OwnerDecisionKind::Denied,
            decided_at: now.to_owned(),
            nonce_sha256: None,
            reason,
            owner_attestation: OwnerAttestation::ConsoleLiveNonceV1,
        })?;
        self.last_transition_at = Some(now.to_owned());
        Ok(())
    }

    /// Record a revocation request. New operations are refused from this moment.
    pub(crate) fn request_revocation(
        &mut self,
        grant_id: &str,
        decision_id: String,
        now: &str,
    ) -> Result<(), GrantError> {
        let grant = self
            .grants
            .iter_mut()
            .find(|grant| grant.grant_id == grant_id)
            .ok_or(GrantError::UnknownGrant)?;
        match grant.revocation.state() {
            GrantRevocationState::NotRevoked => {
                grant.revocation.state = GrantRevocationState::RevokeRequested;
                grant.revocation.requested_at = Some(now.to_owned());
            }
            GrantRevocationState::RevokeRequested => {}
            GrantRevocationState::Revoked => return Ok(()),
        }
        self.push_decision(OwnerDecisionRecord {
            decision_id,
            request_id: grant_id.to_owned(),
            kind: OwnerDecisionKind::RevocationRequested,
            decided_at: now.to_owned(),
            nonce_sha256: None,
            reason: None,
            owner_attestation: OwnerAttestation::ConsoleLiveNonceV1,
        })?;
        self.last_transition_at = Some(now.to_owned());
        Ok(())
    }

    /// Finalize a revocation and bump the epoch so every old binding is stale.
    pub(crate) fn finalize_revocation(
        &mut self,
        grant_id: &str,
        decision_id: String,
        now: &str,
    ) -> Result<u64, GrantError> {
        let epoch = self.authorization_epoch;
        {
            let grant = self
                .grants
                .iter_mut()
                .find(|grant| grant.grant_id == grant_id)
                .ok_or(GrantError::UnknownGrant)?;
            if grant.revocation.state() == GrantRevocationState::Revoked {
                return Ok(grant.authorization_epoch);
            }
            if grant.revocation.state() == GrantRevocationState::NotRevoked {
                grant.revocation.requested_at = Some(now.to_owned());
            }
            grant.revocation.state = GrantRevocationState::Revoked;
            grant.revocation.revoked_at = Some(now.to_owned());
            grant.revocation.owner_decision_id = Some(decision_id.clone());
        }
        let next_epoch = epoch
            .checked_add(1)
            .ok_or(GrantError::Corrupt("authorization epoch overflow"))?;
        self.authorization_epoch = next_epoch;
        self.push_decision(OwnerDecisionRecord {
            decision_id,
            request_id: grant_id.to_owned(),
            kind: OwnerDecisionKind::RevocationFinalized,
            decided_at: now.to_owned(),
            nonce_sha256: None,
            reason: None,
            owner_attestation: OwnerAttestation::ConsoleLiveNonceV1,
        })?;
        self.last_transition_at = Some(now.to_owned());
        Ok(epoch)
    }

    fn push_decision(&mut self, decision: OwnerDecisionRecord) -> Result<(), GrantError> {
        self.decisions.push(decision);
        if self.decisions.len() > MAX_RETAINED_DECISIONS {
            self.decisions.remove(0);
        }
        Ok(())
    }

    /// Charge one operation id against the grant, idempotently.
    ///
    /// Returns the grant snapshot after the charge. A charge that already exists
    /// is a no-op, so re-processing one operation cannot double-consume budget.
    pub(crate) fn charge_operation(
        &mut self,
        grant_id: &str,
        operation_id: &str,
        bytes: u64,
        now: &str,
    ) -> Result<bool, GrantError> {
        let grant = self
            .grants
            .iter_mut()
            .find(|grant| grant.grant_id == grant_id)
            .ok_or(GrantError::UnknownGrant)?;
        if grant.revocation.state() != GrantRevocationState::NotRevoked {
            return Err(GrantError::Revoked);
        }
        if grant.usage.is_charged(operation_id) {
            return Ok(false);
        }
        if !grant.remaining_budget_allows(1, bytes) {
            return Err(GrantError::BudgetExhausted);
        }
        grant.usage.charge(operation_id, bytes)?;
        self.last_transition_at = Some(now.to_owned());
        Ok(true)
    }

    /// Test-only: force the latest grant's validity window so expiry and clock
    /// regression can be tested deterministically without sleeping.
    #[cfg(test)]
    pub(crate) fn force_grant_window(&mut self, issued_at: &str, expires_at: &str) {
        if let Some(grant) = self.grants.last_mut() {
            grant.issued_at = issued_at.to_owned();
            grant.expires_at = expires_at.to_owned();
        }
    }

    /// Charge one operation id with an explicit monotonic floor.
    ///
    /// The floor comes from the Goal anchor, so a ledger restore that lost a
    /// charge can never spend the same budget twice: the effective usage is the
    /// maximum of the ledger and the anchor.
    pub(crate) fn charge_operation_with_floor(
        &mut self,
        grant_id: &str,
        operation_id: &str,
        bytes: u64,
        floor_operations: u32,
        floor_bytes: u64,
        now: &str,
    ) -> Result<bool, GrantError> {
        let grant = self
            .grants
            .iter_mut()
            .find(|grant| grant.grant_id == grant_id)
            .ok_or(GrantError::UnknownGrant)?;
        if grant.revocation.state() != GrantRevocationState::NotRevoked {
            return Err(GrantError::Revoked);
        }
        if grant.usage.is_charged(operation_id) {
            return Ok(false);
        }
        let base_operations = grant.usage.operations.max(floor_operations);
        let base_bytes = grant.usage.bytes.max(floor_bytes);
        if !grant.budget.allows(base_operations, base_bytes, 1, bytes) {
            return Err(GrantError::BudgetExhausted);
        }
        if grant.usage.charged_operation_ids.len() >= MAX_GRANT_OPERATIONS as usize {
            return Err(GrantError::BudgetExhausted);
        }
        grant.usage.operations = base_operations
            .checked_add(1)
            .ok_or(GrantError::BudgetExhausted)?;
        grant.usage.bytes = base_bytes
            .checked_add(bytes)
            .ok_or(GrantError::BudgetExhausted)?;
        grant
            .usage
            .charged_operation_ids
            .push(operation_id.to_owned());
        self.last_transition_at = Some(now.to_owned());
        Ok(true)
    }

    /// Resolve the grant that may authorize work right now.
    ///
    /// This is the single state/expiry/anchor decision used by the issuance,
    /// stage and publication gates.
    pub(crate) fn usable_grant(
        &self,
        anchor: &AuthorizationAnchor,
        now: &str,
    ) -> Result<&GoalWriteGrantV1, GrantRefusal> {
        self.resolve_grant(anchor, now, None)
    }

    /// Resolve a grant for one specific operation.
    ///
    /// A crash after the durable charge but before `BeginOperation` leaves the
    /// operation charged while the grant is at its budget ceiling. Re-running
    /// that exact operation id may use its existing charge; a different id may
    /// not.
    pub(crate) fn usable_grant_for_operation(
        &self,
        anchor: &AuthorizationAnchor,
        now: &str,
        operation_id: &str,
    ) -> Result<&GoalWriteGrantV1, GrantRefusal> {
        self.resolve_grant(anchor, now, Some(operation_id))
    }

    fn resolve_grant(
        &self,
        anchor: &AuthorizationAnchor,
        now: &str,
        resume_operation_id: Option<&str>,
    ) -> Result<&GoalWriteGrantV1, GrantRefusal> {
        if anchor.validate().is_err() {
            return Err(GrantRefusal::StateMismatch);
        }
        let Some(grant) = self.latest_grant() else {
            return Err(if self.pending().is_some() {
                GrantRefusal::Pending
            } else if self
                .decisions
                .iter()
                .any(|decision| decision.kind == OwnerDecisionKind::Denied)
            {
                GrantRefusal::Denied
            } else {
                GrantRefusal::Required
            });
        };
        match grant.revocation.state() {
            GrantRevocationState::RevokeRequested => return Err(GrantRefusal::Revoked),
            GrantRevocationState::Revoked => return Err(GrantRefusal::Revoked),
            GrantRevocationState::NotRevoked => {}
        }
        if grant.authorization_epoch != self.authorization_epoch {
            return Err(GrantRefusal::StateMismatch);
        }
        if grant.authorization_epoch < anchor.highest_epoch() {
            // The Goal remembers a newer generation than this ledger, so the
            // ledger was rolled back (or replaced) after an issuance.
            return Err(GrantRefusal::RollbackDetected);
        }
        // The anchor must name *this* grant. An ACTIVE ledger record the Goal
        // never observed is not authority: it is exactly the state a crash
        // between the anchor write and the ledger write leaves behind, and a
        // forged ledger record with no matching anchor.
        if anchor.highest_epoch() < grant.authorization_epoch
            || anchor.last_grant_id() != Some(grant.grant_id.as_str())
        {
            return Err(GrantRefusal::StateMismatch);
        }
        if let Some(revoked_at) = anchor.revoked_at_epoch()
            && revoked_at >= grant.authorization_epoch
        {
            return Err(GrantRefusal::Revoked);
        }
        // H5-C: a fixture grant is authority only inside the disposable state
        // root that minted it. This is the single resolve path used by the
        // issuance, stage and publication gates, so there is no second place to
        // forget it. A fixture grant copied into a real operator state root has
        // no marker to match and is refused here.
        if grant.owner_attestation.is_test_fixture()
            && crate::config::test_grant_fixture_id().as_deref() != grant.test_fixture_id.as_deref()
        {
            return Err(GrantRefusal::StateMismatch);
        }
        let now_seconds = parse_utc_rfc3339_seconds(now).ok_or(GrantRefusal::Expired)?;
        let issued =
            parse_utc_rfc3339_seconds(&grant.issued_at).ok_or(GrantRefusal::StateMismatch)?;
        let expires =
            parse_utc_rfc3339_seconds(&grant.expires_at).ok_or(GrantRefusal::StateMismatch)?;
        if now_seconds < issued || now_seconds >= expires {
            return Err(GrantRefusal::Expired);
        }
        let effective_operations = grant.usage.operations().max(anchor.usage_operations());
        let effective_bytes = grant.usage.bytes().max(anchor.usage_bytes());
        let resume_charged = resume_operation_id.is_some_and(|id| grant.usage.is_charged(id));
        if !resume_charged
            && (effective_operations >= grant.budget.max_operations()
                || effective_bytes >= grant.budget.max_bytes())
        {
            return Err(GrantRefusal::BudgetExhausted);
        }
        Ok(grant)
    }
}

/// Why a grant check refused. Each variant maps to exactly one stable code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GrantRefusal {
    Required,
    Pending,
    Denied,
    Revoked,
    Expired,
    StateMismatch,
    RollbackDetected,
    ScopeMismatch,
    BudgetExhausted,
    Unavailable,
}

impl GrantRefusal {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Required => "WRITE_AUTHORIZATION_REQUIRED",
            Self::Pending => "WRITE_AUTHORIZATION_PENDING",
            Self::Denied => "WRITE_AUTHORIZATION_DENIED",
            Self::Revoked => "WRITE_AUTHORIZATION_REVOKED",
            Self::Expired => "WRITE_AUTHORIZATION_EXPIRED",
            Self::StateMismatch => "WRITE_AUTHORIZATION_STATE_MISMATCH",
            Self::RollbackDetected => "WRITE_AUTHORIZATION_ROLLBACK_DETECTED",
            Self::ScopeMismatch => "WRITE_AUTHORIZATION_SCOPE_MISMATCH",
            Self::BudgetExhausted => "WRITE_BUDGET_EXHAUSTED",
            Self::Unavailable => "WRITE_AUTHORIZATION_UNAVAILABLE",
        }
    }

    pub(crate) fn detail(self) -> &'static str {
        match self {
            Self::Required => {
                "no Owner write grant exists for this Goal; the Writer role is not authorized"
            }
            Self::Pending => "the Owner has not decided the write grant request yet",
            Self::Denied => "the Owner denied the write grant request",
            Self::Revoked => "the Owner revoked the write grant",
            Self::Expired => "the write grant is expired or the system clock regressed",
            Self::StateMismatch => "the durable authorization state disagrees with this Goal",
            Self::RollbackDetected => {
                "the authorization ledger is older than the Goal's durable anchor"
            }
            Self::ScopeMismatch => "the operation is outside the Owner-approved write scope",
            Self::BudgetExhausted => "the Owner-approved write budget is exhausted",
            Self::Unavailable => "the Owner Control Surface is unavailable",
        }
    }
}

/// Errors while reading or mutating the durable authorization state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GrantError {
    InvalidRequest(&'static str),
    Corrupt(&'static str),
    BackendNotExternal,
    WorkspaceUnavailable(&'static str),
    UnsupportedSchema,
    NoPendingRequest,
    PendingExists,
    PendingExpired,
    NonceMismatch,
    AlreadyActive,
    UnknownGrant,
    Revoked,
    BudgetExhausted,
    StateMismatch(&'static str),
    /// The per-Goal fence could not be acquired within its bound.
    FenceTimeout,
    /// A revocation could not take the fence. It is deferred; it is **not**
    /// reported as revoked.
    RevocationDeferred,
    /// H5-A: a Writer publication is unresolved (an operation intent is still
    /// `APPLYING`), so the host cannot state that the write path is closed. The
    /// grant is denied (`REVOKE_REQUESTED`), the revocation is deliberately left
    /// unfinalized, and nothing reports it as revoked.
    RevocationBlockedByPublication,
    Io,
}

impl GrantError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "INVALID_ARGUMENT",
            Self::Corrupt(_) => "AUTHORIZATION_STATE_CORRUPT",
            Self::BackendNotExternal => "MODEL_BACKEND_CODEX",
            Self::WorkspaceUnavailable(_) => "AUTHORIZATION_WORKSPACE_UNAVAILABLE",
            Self::UnsupportedSchema => "AUTHORIZATION_SCHEMA_UNSUPPORTED",
            Self::NoPendingRequest => "AUTHORIZATION_NO_PENDING_REQUEST",
            Self::PendingExists => "AUTHORIZATION_PENDING_EXISTS",
            Self::PendingExpired => "AUTHORIZATION_REQUEST_EXPIRED",
            Self::NonceMismatch => "AUTHORIZATION_OWNER_PROOF_INVALID",
            Self::AlreadyActive => "AUTHORIZATION_ALREADY_ACTIVE",
            Self::UnknownGrant => "AUTHORIZATION_UNKNOWN_GRANT",
            Self::Revoked => "WRITE_AUTHORIZATION_REVOKED",
            Self::BudgetExhausted => "WRITE_BUDGET_EXHAUSTED",
            Self::StateMismatch(_) => "AUTHORIZATION_STATE_MISMATCH",
            Self::FenceTimeout => "AUTHORIZATION_FENCE_BUSY",
            Self::RevocationDeferred => "REVOCATION_DEFERRED",
            Self::RevocationBlockedByPublication => "REVOCATION_BLOCKED_BY_UNRESOLVED_PUBLICATION",
            Self::Io => "AUTHORIZATION_STATE_IO",
        }
    }
}

impl std::fmt::Display for GrantError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(detail)
            | Self::Corrupt(detail)
            | Self::WorkspaceUnavailable(detail) => formatter.write_str(detail),
            Self::StateMismatch(detail) => formatter.write_str(detail),
            Self::FenceTimeout => formatter
                .write_str("the authorization fence is busy; the operation was not changed"),
            Self::RevocationDeferred => formatter.write_str(
                "the revocation fence is busy; the grant was not changed and is still active",
            ),
            Self::RevocationBlockedByPublication => formatter.write_str(
                "a Writer publication is unresolved; the grant is already denied (REVOKE_REQUESTED) but the revocation is not finalized while a helper's lifetime is unproven",
            ),
            Self::Io => formatter.write_str("authorization state I/O failed"),
            other => formatter.write_str(other.code()),
        }
    }
}

impl std::error::Error for GrantError {}

/// Compact authorization projection for API responses.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct AuthorizationView {
    pub(crate) state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) grant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) authorization_epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) allowed_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) allowed_operations: Option<Vec<&'static str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) budget: Option<AuthorizationBudgetView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) usage: Option<AuthorizationUsageView>,
    pub(crate) owner_attestation: &'static str,
    pub(crate) same_uid_process_resistance: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AuthorizationBudgetView {
    pub(crate) max_operations: u32,
    pub(crate) max_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AuthorizationUsageView {
    pub(crate) operations: u32,
    pub(crate) bytes: u64,
}

impl AuthorizationView {
    pub(crate) fn not_requested() -> Self {
        Self {
            state: "NOT_REQUESTED",
            grant_id: None,
            request_id: None,
            authorization_epoch: None,
            expires_at: None,
            allowed_paths: None,
            allowed_operations: None,
            budget: None,
            usage: None,
            owner_attestation: OwnerAttestation::ConsoleLiveNonceV1.as_str(),
            same_uid_process_resistance: OwnerAttestation::ConsoleLiveNonceV1
                .same_uid_process_resistance(),
        }
    }

    pub(crate) fn from_ledger(
        ledger: &AuthorizationLedgerV1,
        anchor: &AuthorizationAnchor,
        now: &str,
    ) -> Self {
        match ledger.usable_grant(anchor, now) {
            Ok(grant) => Self {
                state: "ACTIVE",
                grant_id: Some(grant.grant_id.clone()),
                request_id: None,
                authorization_epoch: Some(grant.authorization_epoch),
                expires_at: Some(grant.expires_at.clone()),
                allowed_paths: Some(
                    grant
                        .path_rules
                        .iter()
                        .map(|rule| match rule.kind {
                            GrantPathKind::Tree if rule.path.is_empty() => "**".to_owned(),
                            GrantPathKind::Tree => format!("{}/**", rule.path),
                            GrantPathKind::File => rule.path.clone(),
                        })
                        .collect(),
                ),
                allowed_operations: Some(
                    grant
                        .allowed_operations
                        .iter()
                        .map(|operation| operation.as_str())
                        .collect(),
                ),
                budget: Some(AuthorizationBudgetView {
                    max_operations: grant.budget.max_operations,
                    max_bytes: grant.budget.max_bytes,
                }),
                // H5-A (independent review F6): report the *effective* usage the
                // host actually enforces (the componentwise maximum of the ledger
                // charge and the Goal anchor's monotonic floor), not the raw
                // ledger counter. A restored ledger could otherwise make spending
                // look smaller than it is while enforcement still used the floor.
                usage: Some(AuthorizationUsageView {
                    operations: grant.usage.operations.max(anchor.usage_operations()),
                    bytes: grant.usage.bytes.max(anchor.usage_bytes()),
                }),
                owner_attestation: grant.owner_attestation.as_str(),
                same_uid_process_resistance: grant.owner_attestation.same_uid_process_resistance(),
            },
            Err(refusal) => Self {
                state: match refusal {
                    GrantRefusal::Pending => "PENDING",
                    GrantRefusal::Denied => "DENIED",
                    // H4-B (independent review, LOW): `REVOKE_REQUESTED` and
                    // `REVOKED` both deny publication, but they are different
                    // durable states and this projection must not claim the
                    // finalization write happened. The permission decision is
                    // unchanged; only the reported state is derived from the
                    // durable grant record.
                    GrantRefusal::Revoked => {
                        match ledger.latest_grant().map(|grant| grant.revocation.state()) {
                            Some(GrantRevocationState::RevokeRequested) => "REVOKE_REQUESTED",
                            _ => "REVOKED",
                        }
                    }
                    GrantRefusal::Expired => "EXPIRED",
                    GrantRefusal::Unavailable => "UNAVAILABLE",
                    GrantRefusal::Required => "REQUIRED",
                    GrantRefusal::StateMismatch | GrantRefusal::RollbackDetected => "INVALID",
                    GrantRefusal::ScopeMismatch | GrantRefusal::BudgetExhausted => "INACTIVE",
                },
                grant_id: ledger.latest_grant().map(|grant| grant.grant_id.clone()),
                request_id: ledger.pending().map(|pending| pending.request_id.clone()),
                authorization_epoch: Some(ledger.authorization_epoch()),
                expires_at: ledger.latest_grant().map(|grant| grant.expires_at.clone()),
                allowed_paths: None,
                allowed_operations: None,
                budget: None,
                usage: None,
                // The attestation still describes the durable grant record, so a
                // refused fixture grant is not silently reported as an Owner
                // decision.
                owner_attestation: ledger
                    .latest_grant()
                    .map(|grant| grant.owner_attestation.as_str())
                    .unwrap_or(OwnerAttestation::ConsoleLiveNonceV1.as_str()),
                same_uid_process_resistance: ledger
                    .latest_grant()
                    .map(|grant| grant.owner_attestation.same_uid_process_resistance())
                    .unwrap_or(false),
            },
        }
    }
}

/// SHA-256 over the authority-relevant Goal identity.
///
/// The digest intentionally includes the Owner-authored objective, constraints
/// and completion criteria: a grant must not survive a Goal being repurposed in
/// place through a state transplant.
pub(crate) fn goal_identity_digest(goal: &Goal) -> Result<String, GrantError> {
    let mut framed = Vec::new();
    framed.extend_from_slice(b"local-mcp/goal-identity/v1\0");
    push_framed(&mut framed, goal.id().as_str().as_bytes());
    push_framed(&mut framed, goal.session_id().as_bytes());
    push_framed(&mut framed, goal.objective().as_bytes());
    push_framed(&mut framed, goal.title().unwrap_or("").as_bytes());
    for constraint in goal.constraints() {
        push_framed(&mut framed, constraint.as_bytes());
    }
    for criterion in goal.completion_criteria() {
        push_framed(&mut framed, criterion.id().as_str().as_bytes());
        push_framed(&mut framed, criterion.description().as_bytes());
    }
    push_framed(&mut framed, goal.created_at().as_bytes());
    Ok(sha256_hex(&framed))
}

/// Convert one canonical absolute operation path to the grant's root-relative
/// spelling. Both inputs must already be canonical.
pub(crate) fn relative_workspace_path(
    path: &Path,
    workspace_root: &Path,
) -> Result<NormalizedWorkspacePath, GrantRefusal> {
    // Match the root's spelling convention so a Windows verbatim operation path
    // and a compact managed root compare correctly.
    let path = config::canonical_path_like(path, workspace_root)
        .map_err(|_| GrantRefusal::ScopeMismatch)?;
    let relative = path
        .strip_prefix(workspace_root)
        .map_err(|_| GrantRefusal::ScopeMismatch)?;
    let mut normalized = String::new();
    for component in relative.components() {
        let std::path::Component::Normal(value) = component else {
            return Err(GrantRefusal::ScopeMismatch);
        };
        let text = value.to_str().ok_or(GrantRefusal::ScopeMismatch)?;
        if text.is_empty() || text == "." || text == ".." || text.contains(['/', '\\', '\0', ':']) {
            return Err(GrantRefusal::ScopeMismatch);
        }
        if !normalized.is_empty() {
            normalized.push('/');
        }
        normalized.push_str(text);
    }
    if normalized.is_empty() {
        return Err(GrantRefusal::ScopeMismatch);
    }
    NormalizedWorkspacePath::parse(normalized).map_err(|_| GrantRefusal::ScopeMismatch)
}

/// Deterministic grant id for one `(request, epoch)` pair, spelled as a UUID so
/// every existing id validator accepts it.
pub(crate) fn deterministic_grant_id(request_id: &str, epoch: u64) -> String {
    let mut framed = Vec::new();
    framed.extend_from_slice(b"local-mcp/grant-id/v1\0");
    push_framed(&mut framed, request_id.as_bytes());
    push_framed(&mut framed, &epoch.to_le_bytes());
    let digest = Sha256::digest(&framed);
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // RFC 4122 version/variant bits so `Uuid::parse_str` and every other id
    // validator treat the value as an ordinary UUID.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn nonce_sha256(nonce: &str) -> String {
    let mut framed = Vec::new();
    framed.extend_from_slice(b"local-mcp/owner-nonce/v1\0");
    framed.extend_from_slice(nonce.as_bytes());
    sha256_hex(&framed)
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn push_framed(target: &mut Vec<u8>, bytes: &[u8]) {
    target.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    target.extend_from_slice(bytes);
}

/// Current UTC time as seconds since the Unix epoch.
pub(crate) fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Format seconds since the Unix epoch in the durable `...Z` spelling.
pub(crate) fn format_utc_rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Parse the exact durable spelling produced by [`format_utc_rfc3339`].
pub(crate) fn parse_utc_rfc3339_seconds(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = &value[range];
        if !slice.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        slice.parse::<i64>().ok()
    };
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    Some(
        days_from_civil(year, month as u32, day as u32) * 86_400
            + hour * 3_600
            + minute * 60
            + second,
    )
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}

/// Map a grant-state decision to an orchestrator error for paths that must stop
/// rather than return a typed protocol result.
pub(crate) fn refusal_error(refusal: GrantRefusal) -> OrchestratorError {
    OrchestratorError::CorruptGoal(format!("{}: {}", refusal.code(), refusal.detail()))
}
