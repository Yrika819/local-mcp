//! Managed Worktrees V1 — durable state model (Phase 1).
//!
//! Frozen contract: `docs/MANAGED_WORKTREES_V1_DESIGN.md` sections 3, 4, 5, 8,
//! 10, 11, 12, 13, and 21.
//!
//! This module is pure state plus pure validation. It performs no filesystem
//! access, no Git access, and grants no creation, cleanup, merge, publication,
//! fallback, or session-authority capability. No public MCP request type exposes
//! a managed-workspace field, so a model, planner, worker, or caller cannot
//! supply one.
//!
//! `branch_ref` and `lock_reason` are computed from the Goal ID and re-derived
//! during validation, so they are mechanically unforgeable. The remaining
//! identity values (`worktree_root`, `repository_common_dir`, `base_commit`,
//! `source_ref`, and the revision bindings) are supplied by the host's
//! derivation step and are validated here for shape and safety. Pure validation
//! cannot prove an observation was genuine; proving that is the job of the
//! read-only reconciliation phase, not this module.
//!
//! Phase boundary: nothing in this file performs or authorizes
//! `git worktree add`, branch creation, lock/unlock, remove, prune, or any ref
//! mutation, and nothing here widens Session path authority. See
//! `docs/MANAGED_WORKTREES_V1_DESIGN.md` section 26 for the frozen phase order.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::goal::GoalId;
use crate::orchestrator_error::OrchestratorError;

/// Deterministic host-owned branch namespace from design section 8.
///
/// The full Goal UUID is authoritative; a shortened UUID is never acceptable.
pub(crate) const MANAGED_BRANCH_REF_PREFIX: &str = "refs/heads/local-mcp/goal/";

/// Deterministic host-owned lock reason from design section 10/12.
pub(crate) const MANAGED_LOCK_REASON_PREFIX: &str = "local-mcp goal ";

const MAX_REASON_CHARS: usize = 512;
const MAX_REF_CHARS: usize = 1_024;

/// The host-derived managed branch ref for `goal_id`.
///
/// This is a pure function of the Goal identity. There is deliberately no API
/// that accepts a caller-supplied branch name: design sections 2 and 8 freeze
/// branch naming as host-owned.
pub(crate) fn managed_branch_ref(goal_id: &GoalId) -> String {
    format!("{MANAGED_BRANCH_REF_PREFIX}{}", goal_id.as_str())
}

/// The host-derived Git worktree lock reason for `goal_id` (design section 10).
pub(crate) fn managed_lock_reason(goal_id: &GoalId) -> String {
    format!("{MANAGED_LOCK_REASON_PREFIX}{}", goal_id.as_str())
}

/// Host-owned identity of one managed linked worktree (design section 8).
///
/// A worktree may outlive its Goal for review/cleanup, but durable ownership
/// always remains the originating Goal ID (design section 4).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct WorktreeId(String);

#[expect(
    dead_code,
    reason = "Managed Worktrees identity accessors are frozen contract state; Phase 3 creation authority is not authorized yet."
)]
impl WorktreeId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub(crate) fn parse(value: &str) -> Result<Self, OrchestratorError> {
        let uuid = Uuid::parse_str(value)
            .map_err(|_| OrchestratorError::UnsafeIdentifier("WorktreeId".to_owned()))?;
        Ok(Self(uuid.to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn validate(&self) -> Result<(), OrchestratorError> {
        if Self::parse(&self.0)?.0 != self.0 {
            return Err(OrchestratorError::UnsafeIdentifier("WorktreeId".to_owned()));
        }
        Ok(())
    }
}

/// Host-owned identity of one managed-worktree creation operation
/// (design section 11). A retry after a proven no-side-effect failure uses a
/// fresh operation ID so attempt budgets and evidence stay distinguishable.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct WorktreeOperationId(String);

#[expect(
    dead_code,
    reason = "Managed Worktrees operation identity is frozen contract state; Phase 3 creation authority is not authorized yet."
)]
impl WorktreeOperationId {
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub(crate) fn parse(value: &str) -> Result<Self, OrchestratorError> {
        let uuid = Uuid::parse_str(value)
            .map_err(|_| OrchestratorError::UnsafeIdentifier("WorktreeOperationId".to_owned()))?;
        Ok(Self(uuid.to_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn validate(&self) -> Result<(), OrchestratorError> {
        if Self::parse(&self.0)?.0 != self.0 {
            return Err(OrchestratorError::UnsafeIdentifier(
                "WorktreeOperationId".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Workspace selection from design section 3.
///
/// `PRIMARY` is the exact current behavior and is the default, so omitting the
/// field on an existing client or an existing durable Goal is byte- and
/// behavior-compatible with pre-managed-worktree releases.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum WorkspaceMode {
    #[default]
    Primary,
    ManagedWorktree,
}

#[expect(
    dead_code,
    reason = "workspace_mode is frozen contract state; the public opt-in arrives with Phase 3 creation authority."
)]
impl WorkspaceMode {
    pub(crate) fn is_primary(&self) -> bool {
        matches!(self, Self::Primary)
    }

    pub(crate) fn is_managed(&self) -> bool {
        matches!(self, Self::ManagedWorktree)
    }
}

/// Managed-worktree lifecycle from design sections 5, 11, and 19.
///
/// `Prepared` -> `Active` is deliberately unreachable except through the
/// creation-intent state, and `Requested` cannot reach `Active` directly, so
/// "reconcile mechanically before committing ACTIVE" is expressible purely.
/// Phase 1 freezes the edge set; the transition site that consults it belongs to
/// Phase 3 creation authority, which is not authorized here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ManagedWorktreeLifecycle {
    Requested,
    Prepared,
    Active,
    CleanupEligible,
    Blocked,
    Removed,
}

#[expect(
    dead_code,
    reason = "Managed Worktrees lifecycle edges are frozen contract state; no lifecycle transition is authorized in Phase 1 or 2."
)]
impl ManagedWorktreeLifecycle {
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, Self::Removed)
    }

    /// Pure lifecycle edge check (design sections 5, 11, 19).
    ///
    /// A self-transition is not a lifecycle change: a bounded retry after a
    /// proven no-side-effect failure re-persists a `Prepared` intent with a new
    /// operation ID without advancing the record.
    pub(crate) fn can_transition_to(&self, next: Self) -> bool {
        use ManagedWorktreeLifecycle::{
            Active, Blocked, CleanupEligible, Prepared, Removed, Requested,
        };
        matches!(
            (self, next),
            (Requested, Prepared | Blocked)
                | (Prepared, Active | Blocked)
                | (Active, CleanupEligible | Removed | Blocked)
                | (CleanupEligible, Removed | Blocked)
                | (Blocked, Active | CleanupEligible | Removed | Blocked)
        )
    }

    /// Whether this lifecycle state must already carry a mechanical
    /// reconciliation observation (design section 11 reconciles after Git
    /// returns, i.e. strictly after `Prepared`).
    fn requires_reconciled_observation(&self) -> bool {
        matches!(self, Self::Active | Self::CleanupEligible | Self::Removed)
    }

    /// Whether this lifecycle state may not yet carry a reconciliation
    /// observation, because nothing has been invoked yet.
    fn forbids_reconciled_observation(&self) -> bool {
        matches!(self, Self::Requested | Self::Prepared)
    }
}

/// Durable creation intent from design section 11.
///
/// Persisted **before** any Git invocation. The single representable state is
/// `PREPARED`; `ACTIVE` and beyond are not intent states, so an intent can never
/// be mistaken for a committed workspace binding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedWorktreeCreationIntent {
    goal_id: GoalId,
    worktree_id: WorktreeId,
    operation_id: WorktreeOperationId,
    repository_common_dir: PathBuf,
    worktree_root: PathBuf,
    branch_ref: String,
    base_commit: String,
    state: ManagedWorktreeIntentState,
}

/// The only state a creation intent may represent in Phase 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum ManagedWorktreeIntentState {
    #[default]
    Prepared,
}

#[expect(
    dead_code,
    reason = "PREPARED intent construction is frozen contract state; Phase 3 creation authority is not authorized yet."
)]
impl ManagedWorktreeCreationIntent {
    /// Build the host-derived `PREPARED` intent for `goal_id`.
    ///
    /// All identity-bearing values are computed from the Goal ID or supplied by
    /// mechanical observation; none of them is caller- or model-supplied.
    pub(crate) fn prepared(
        goal_id: &GoalId,
        worktree_id: WorktreeId,
        operation_id: WorktreeOperationId,
        repository_common_dir: PathBuf,
        worktree_root: PathBuf,
        base_commit: String,
    ) -> Result<Self, OrchestratorError> {
        let intent = Self {
            goal_id: goal_id.clone(),
            worktree_id,
            operation_id,
            repository_common_dir,
            worktree_root,
            branch_ref: managed_branch_ref(goal_id),
            base_commit,
            state: ManagedWorktreeIntentState::Prepared,
        };
        intent.validate()?;
        Ok(intent)
    }

    pub(crate) fn goal_id(&self) -> &GoalId {
        &self.goal_id
    }

    pub(crate) fn worktree_id(&self) -> &WorktreeId {
        &self.worktree_id
    }

    pub(crate) fn operation_id(&self) -> &WorktreeOperationId {
        &self.operation_id
    }

    pub(crate) fn repository_common_dir(&self) -> &Path {
        &self.repository_common_dir
    }

    pub(crate) fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    pub(crate) fn branch_ref(&self) -> &str {
        &self.branch_ref
    }

    pub(crate) fn base_commit(&self) -> &str {
        &self.base_commit
    }

    pub(crate) fn state(&self) -> ManagedWorktreeIntentState {
        self.state
    }

    /// Pure validation. Performs no filesystem or Git access.
    pub(crate) fn validate(&self) -> Result<(), OrchestratorError> {
        if self.state != ManagedWorktreeIntentState::Prepared {
            return Err(OrchestratorError::CorruptGoal(
                "managed-worktree creation intent must be PREPARED".to_owned(),
            ));
        }
        self.goal_id.validate()?;
        self.worktree_id.validate()?;
        self.operation_id.validate()?;
        validate_canonical_absolute(&self.repository_common_dir, "repository_common_dir")?;
        validate_canonical_absolute(&self.worktree_root, "worktree_root")?;
        validate_expected_branch_ref(&self.goal_id, &self.branch_ref)?;
        validate_object_id(&self.base_commit, "base_commit")?;
        Ok(())
    }

    /// Check the intent describes the same host-owned target as `record`.
    ///
    /// A disagreement means the durable intent and record no longer describe
    /// one worktree, which is corruption rather than a retryable condition.
    pub(crate) fn validate_against_record(
        &self,
        record: &ManagedWorktreeRecord,
    ) -> Result<(), OrchestratorError> {
        self.validate()?;
        record.validate()?;
        let consistent = self.goal_id == record.goal_id
            && self.worktree_id == record.worktree_id
            && self.repository_common_dir == record.repository_common_dir
            && self.worktree_root == record.worktree_root
            && self.branch_ref == record.branch_ref
            && self.base_commit == record.base_commit;
        if !consistent {
            return Err(OrchestratorError::CorruptGoal(
                "managed-worktree creation intent and record describe different targets".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Durable managed-worktree record from design section 8.
///
/// All paths are canonical absolute host paths. `source_ref` is informational
/// only and may be absent for a detached source. The reconciled observation pair
/// is absent before any Git invocation and mandatory once the record is
/// `Active`, because design section 11 requires mechanical reconciliation before
/// `ACTIVE` is committed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManagedWorktreeRecord {
    worktree_id: WorktreeId,
    goal_id: GoalId,
    lifecycle: ManagedWorktreeLifecycle,
    primary_root: PathBuf,
    repository_common_dir: PathBuf,
    worktree_root: PathBuf,
    branch_ref: String,
    base_commit: String,
    source_ref: Option<String>,
    created_goal_revision: u64,
    created_plan_revision: u32,
    lock_reason: String,
    last_reconciled_head: Option<String>,
    last_reconciled_at: Option<String>,
}

#[expect(
    dead_code,
    reason = "Managed Worktrees record accessors are frozen contract state; Phase 3 creation and Phase 5 evidence are not authorized yet."
)]
#[expect(
    clippy::too_many_arguments,
    reason = "Every Managed Worktrees record field is an independent host-owned authority binding; bundling them would hide a required identity field."
)]
impl ManagedWorktreeRecord {
    /// Host-derived `REQUESTED` record for a newly managed-aware Goal.
    ///
    /// `primary_root` must be the Goal's own `cwd`: design section 13 freezes
    /// `Goal.cwd` as the durable identity root.
    pub(crate) fn requested(
        goal_id: &GoalId,
        worktree_id: WorktreeId,
        primary_root: PathBuf,
        repository_common_dir: PathBuf,
        worktree_root: PathBuf,
        base_commit: String,
        source_ref: Option<String>,
        created_goal_revision: u64,
        created_plan_revision: u32,
    ) -> Result<Self, OrchestratorError> {
        let record = Self {
            worktree_id,
            goal_id: goal_id.clone(),
            lifecycle: ManagedWorktreeLifecycle::Requested,
            primary_root,
            repository_common_dir,
            worktree_root,
            branch_ref: managed_branch_ref(goal_id),
            base_commit,
            source_ref,
            created_goal_revision,
            created_plan_revision,
            lock_reason: managed_lock_reason(goal_id),
            last_reconciled_head: None,
            last_reconciled_at: None,
        };
        record.validate()?;
        Ok(record)
    }

    pub(crate) fn worktree_id(&self) -> &WorktreeId {
        &self.worktree_id
    }

    pub(crate) fn goal_id(&self) -> &GoalId {
        &self.goal_id
    }

    pub(crate) fn lifecycle(&self) -> ManagedWorktreeLifecycle {
        self.lifecycle
    }

    pub(crate) fn primary_root(&self) -> &Path {
        &self.primary_root
    }

    pub(crate) fn repository_common_dir(&self) -> &Path {
        &self.repository_common_dir
    }

    pub(crate) fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }

    pub(crate) fn branch_ref(&self) -> &str {
        &self.branch_ref
    }

    pub(crate) fn base_commit(&self) -> &str {
        &self.base_commit
    }

    pub(crate) fn source_ref(&self) -> Option<&str> {
        self.source_ref.as_deref()
    }

    pub(crate) fn created_goal_revision(&self) -> u64 {
        self.created_goal_revision
    }

    pub(crate) fn created_plan_revision(&self) -> u32 {
        self.created_plan_revision
    }

    pub(crate) fn lock_reason(&self) -> &str {
        &self.lock_reason
    }

    pub(crate) fn last_reconciled_head(&self) -> Option<&str> {
        self.last_reconciled_head.as_deref()
    }

    pub(crate) fn last_reconciled_at(&self) -> Option<&str> {
        self.last_reconciled_at.as_deref()
    }

    /// Whether a creation intent is still outstanding for this record.
    ///
    /// Design section 11 persists the intent before Git and reconciles before
    /// committing `ACTIVE`, so an outstanding intent implies a lifecycle that
    /// has not been reconciled yet.
    pub(crate) fn requires_creation_intent(&self) -> bool {
        matches!(
            self.lifecycle,
            ManagedWorktreeLifecycle::Requested | ManagedWorktreeLifecycle::Prepared
        )
    }

    /// Whether this lifecycle may still carry a `PREPARED` creation intent.
    ///
    /// A blocked creation keeps its intent so explicit host recovery can
    /// reconcile the exact intended target. A worktree that is already active,
    /// cleanup-eligible, or removed has been reconciled and must not.
    pub(crate) fn permits_creation_intent(&self) -> bool {
        self.requires_creation_intent() || self.lifecycle == ManagedWorktreeLifecycle::Blocked
    }

    /// Pure validation of the record itself. No filesystem or Git access.
    pub(crate) fn validate(&self) -> Result<(), OrchestratorError> {
        self.worktree_id.validate()?;
        self.goal_id.validate()?;
        validate_canonical_absolute(&self.primary_root, "primary_root")?;
        validate_canonical_absolute(&self.repository_common_dir, "repository_common_dir")?;
        validate_canonical_absolute(&self.worktree_root, "worktree_root")?;
        if self.worktree_root == self.primary_root {
            return Err(OrchestratorError::CorruptGoal(
                "managed worktree root must differ from the primary root".to_owned(),
            ));
        }
        // A managed execution root that is nested inside the primary workspace,
        // or that contains it, would relocate managed execution into the primary
        // workspace and its Git administrative internals. `Goal::execution_root`
        // hands this field through verbatim once the worktree is `Active`, so the
        // hazard is rejected purely from durable data, without touching the
        // filesystem (design sections 2.7, 2.8, and 13).
        if self.worktree_root.starts_with(&self.primary_root)
            || self.primary_root.starts_with(&self.worktree_root)
        {
            return Err(OrchestratorError::CorruptGoal(
                "managed worktree root must not overlap the primary workspace".to_owned(),
            ));
        }
        validate_expected_branch_ref(&self.goal_id, &self.branch_ref)?;
        validate_expected_lock_reason(&self.goal_id, &self.lock_reason)?;
        validate_object_id(&self.base_commit, "base_commit")?;
        if let Some(source_ref) = &self.source_ref {
            validate_informational_ref(source_ref)?;
        }
        if self.created_goal_revision == 0 {
            return Err(OrchestratorError::CorruptGoal(
                "managed-worktree created_goal_revision must start at 1".to_owned(),
            ));
        }
        // Design section 21: the worktree is established before Planner path
        // materialization, and workspace lifecycle mutations never advance the
        // plan revision, so the creation-time plan revision is 0.
        if self.created_plan_revision != 0 {
            return Err(OrchestratorError::CorruptGoal(
                "managed worktree must be created before initial plan materialization".to_owned(),
            ));
        }
        let (head, at) = (&self.last_reconciled_head, &self.last_reconciled_at);
        if head.is_some() != at.is_some() {
            return Err(OrchestratorError::CorruptGoal(
                "managed-worktree reconciliation observation must record both head and time"
                    .to_owned(),
            ));
        }
        if self.lifecycle.forbids_reconciled_observation() && head.is_some() {
            return Err(OrchestratorError::CorruptGoal(
                "uninvoked managed-worktree lifecycle must not carry a reconciliation observation"
                    .to_owned(),
            ));
        }
        if self.lifecycle.requires_reconciled_observation() && head.is_none() {
            return Err(OrchestratorError::CorruptGoal(
                "active managed worktree requires a mechanical reconciliation observation"
                    .to_owned(),
            ));
        }
        if let Some(head) = head {
            validate_object_id(head, "last_reconciled_head")?;
        }
        if let Some(at) = at {
            validate_bounded_text(at, "last_reconciled_at")?;
        }
        Ok(())
    }
}

fn validate_canonical_absolute(path: &Path, field: &str) -> Result<(), OrchestratorError> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(OrchestratorError::CorruptGoal(format!(
            "managed-worktree {field} must be a canonical absolute path"
        )));
    }
    // A canonical path has no `.` or `..` components. `Path::components()`
    // silently drops `.` (and collapses repeated and trailing separators), so
    // checking component kinds alone is not enough: rebuild the path from its
    // components and require the raw bytes to be identical.
    //
    // The comparison must be on `as_os_str()`: `Path`'s `PartialEq` compares
    // *components*, under which `/managed/./x` and `/managed/x` are equal, so
    // comparing paths directly would accept a non-canonical spelling.
    //
    // These are pure structural checks over durable data. They deliberately do
    // not touch the filesystem, so validation never depends on a path existing
    // and never trusts an external observation.
    let components: Vec<_> = path.components().collect();
    let structural = components.iter().all(|component| {
        matches!(
            component,
            Component::RootDir | Component::Prefix(_) | Component::Normal(_)
        )
    });
    let rebuilt: PathBuf = components.iter().collect();
    if !structural || rebuilt.as_os_str() != path.as_os_str() {
        return Err(OrchestratorError::CorruptGoal(format!(
            "managed-worktree {field} must be a canonical absolute path"
        )));
    }
    Ok(())
}

fn validate_bounded_text(value: &str, field: &str) -> Result<(), OrchestratorError> {
    if value.trim().is_empty() || value.chars().count() > MAX_REASON_CHARS {
        return Err(OrchestratorError::CorruptGoal(format!(
            "managed-worktree {field} must be non-empty and bounded"
        )));
    }
    Ok(())
}

/// Accept only a full Git object ID in lowercase hex (SHA-1 or SHA-256).
///
/// Design section 10 forbids an unvalidated commit-ish; freezing full-length
/// lowercase hex removes abbreviated and ref-ambiguous base commits.
fn validate_object_id(value: &str, field: &str) -> Result<(), OrchestratorError> {
    let plausible = matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    if !plausible {
        return Err(OrchestratorError::CorruptGoal(format!(
            "managed-worktree {field} must be a full lowercase hex Git object ID"
        )));
    }
    Ok(())
}

fn validate_expected_branch_ref(
    goal_id: &GoalId,
    branch_ref: &str,
) -> Result<(), OrchestratorError> {
    if branch_ref != managed_branch_ref(goal_id) {
        return Err(OrchestratorError::CorruptGoal(
            "managed-worktree branch_ref must be the host-derived Goal branch ref".to_owned(),
        ));
    }
    Ok(())
}

fn validate_expected_lock_reason(
    goal_id: &GoalId,
    lock_reason: &str,
) -> Result<(), OrchestratorError> {
    if lock_reason != managed_lock_reason(goal_id) {
        return Err(OrchestratorError::CorruptGoal(
            "managed-worktree lock_reason must be the host-derived Goal lock reason".to_owned(),
        ));
    }
    Ok(())
}

/// Validate an informational ref without granting it any authority.
///
/// Design section 8 marks `source_ref` informational; it may be absent for a
/// detached source. It is never used to select a base, a path, or a branch.
fn validate_informational_ref(value: &str) -> Result<(), OrchestratorError> {
    let unsafe_chars = value.chars().any(|character| {
        character.is_whitespace() || character.is_control() || "~^:?*[\\".contains(character)
    });
    let unsafe_shape = value.len() > MAX_REF_CHARS
        || !value.starts_with("refs/")
        || value.ends_with('/')
        || value.contains("//")
        || value.contains("..")
        // `@{` opens a reflog selector and is rejected by Git ref rules.
        || value.contains("@{")
        || value.ends_with(".lock")
        || value.split('/').any(|segment| {
            // Git rejects an empty component, a leading `.`, a trailing `.`, and
            // a `.lock` suffix.
            segment.is_empty() || segment.starts_with('.') || segment.ends_with('.')
        });
    if unsafe_chars || unsafe_shape {
        return Err(OrchestratorError::CorruptGoal(
            "managed-worktree source_ref is not a well-formed informational ref".to_owned(),
        ));
    }
    Ok(())
}
