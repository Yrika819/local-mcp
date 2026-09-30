//! Managed Worktrees V1 — Phase 1 durable state model tests.
//!
//! Covers `docs/MANAGED_WORKTREES_V1_DESIGN.md` section 25 groups A (opt-in /
//! regression) and C (authority) for the schema and pure state model only.
//! Git mutation authority belongs to Phase 3 and is deliberately not exercised
//! or asserted here.

use std::path::PathBuf;

use serde_json::Value;

use crate::goal::{GOAL_SCHEMA_VERSION, GOAL_STORE_FORMAT, Goal};
use crate::managed_worktree::{
    MANAGED_BRANCH_REF_PREFIX, MANAGED_LOCK_REASON_PREFIX, ManagedWorktreeCreationIntent,
    ManagedWorktreeIntentState, ManagedWorktreeLifecycle, ManagedWorktreeRecord, WorkspaceMode,
    WorktreeId, WorktreeOperationId, managed_branch_ref, managed_lock_reason,
};
use crate::orchestrator_error::OrchestratorError;

const NOW: &str = "2026-01-01T00:00:00Z";
#[cfg(not(windows))]
const PRIMARY_ROOT: &str = "/repo/primary";
#[cfg(windows)]
const PRIMARY_ROOT: &str = r"C:\repo\primary";
#[cfg(not(windows))]
const COMMON_DIR: &str = "/repo/primary/.git";
#[cfg(windows)]
const COMMON_DIR: &str = r"C:\repo\primary\.git";
#[cfg(not(windows))]
const WORKTREE_ROOT: &str = "/managed/session-1/goal-1";
#[cfg(windows)]
const WORKTREE_ROOT: &str = r"C:\managed\session-1\goal-1";
#[cfg(not(windows))]
const FILESYSTEM_ROOT: &str = "/";
#[cfg(windows)]
const FILESYSTEM_ROOT: &str = r"C:\";
const BASE_COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn managed_goal() -> Goal {
    Goal::new(
        "session-1",
        PathBuf::from(PRIMARY_ROOT),
        "managed objective",
        Some("managed".into()),
        vec![],
        vec!["all checks pass".into()],
        NOW,
    )
    .unwrap()
}

fn requested_record(goal: &Goal) -> ManagedWorktreeRecord {
    ManagedWorktreeRecord::requested(
        goal.id(),
        WorktreeId::new(),
        PathBuf::from(PRIMARY_ROOT),
        PathBuf::from(COMMON_DIR),
        PathBuf::from(WORKTREE_ROOT),
        BASE_COMMIT.to_owned(),
        Some("refs/heads/main".to_owned()),
        goal.revision(),
        0,
    )
    .unwrap()
}

fn prepared_intent(goal: &Goal, record: &ManagedWorktreeRecord) -> ManagedWorktreeCreationIntent {
    ManagedWorktreeCreationIntent::prepared(
        goal.id(),
        record.worktree_id().clone(),
        WorktreeOperationId::new(),
        PathBuf::from(COMMON_DIR),
        PathBuf::from(WORKTREE_ROOT),
        BASE_COMMIT.to_owned(),
    )
    .unwrap()
}

// --- Section 3: public opt-in and PRIMARY default ---------------------------

#[test]
fn workspace_mode_defaults_to_primary_and_serializes_away() {
    assert_eq!(WorkspaceMode::default(), WorkspaceMode::Primary);
    assert!(serde_json::to_string(&WorkspaceMode::default()).unwrap() == "\"PRIMARY\"");
    assert!(
        serde_json::to_string(&WorkspaceMode::ManagedWorktree).unwrap() == "\"MANAGED_WORKTREE\""
    );
    assert!(
        serde_json::from_str::<WorkspaceMode>("\"MANAGED_WORKTREE\"").unwrap()
            == WorkspaceMode::ManagedWorktree
    );
}

#[test]
fn new_goal_is_primary_with_no_managed_state() {
    let goal = managed_goal();
    assert_eq!(goal.workspace_mode(), WorkspaceMode::Primary);
    assert!(goal.managed_worktree().is_none());
    assert!(goal.managed_worktree_creation_intent().is_none());
    // PRIMARY execution root is exactly the current behavior: Goal.cwd.
    assert_eq!(
        goal.execution_root(),
        Some(PathBuf::from(PRIMARY_ROOT).as_path())
    );
}

#[test]
fn primary_goal_serializes_without_managed_fields_for_byte_compatibility() {
    let value = serde_json::to_value(managed_goal()).unwrap();
    let object = value.as_object().unwrap();
    assert!(!object.contains_key("workspace_mode"));
    assert!(!object.contains_key("managed_worktree"));
    assert!(!object.contains_key("managed_worktree_creation_intent"));
}

#[test]
fn legacy_goal_json_without_workspace_fields_deserializes_as_primary() {
    // A schema-3 durable payload read directly through serde must not acquire
    // managed ownership merely because the field is absent.
    let goal = managed_goal();
    let mut value = serde_json::to_value(&goal).unwrap();
    value["schema_version"] = Value::from(3_u64);
    let decoded: Goal = serde_json::from_value(value).unwrap();
    assert_eq!(decoded.workspace_mode(), WorkspaceMode::Primary);
    assert!(decoded.managed_worktree().is_none());
}

#[test]
fn primary_goal_rejects_managed_state() {
    // Use one Goal so the record's own identity is consistent with the Goal and
    // the only reason for rejection is the PRIMARY-with-managed-state rule.
    let goal = managed_goal();
    let mut value = serde_json::to_value(&goal).unwrap();
    let mut record = serde_json::to_value(requested_record(&goal)).unwrap();
    record["goal_id"] = Value::from(goal.id().as_str());
    value
        .as_object_mut()
        .unwrap()
        .insert("managed_worktree".to_owned(), record);
    let decoded: Goal = serde_json::from_value(value).unwrap();
    let error = decoded.validate().unwrap_err();
    assert!(
        matches!(error, OrchestratorError::CorruptGoal(_)),
        "PRIMARY Goal with managed state was not rejected: {error}"
    );
    assert!(
        error
            .to_string()
            .contains("PRIMARY workspace must not carry managed-worktree state"),
        "rejection did not come from the PRIMARY rule: {error}"
    );
}

#[test]
fn primary_goal_rejects_managed_creation_intent() {
    let goal = managed_goal();
    let record = requested_record(&goal);
    let mut value = serde_json::to_value(&goal).unwrap();
    value.as_object_mut().unwrap().insert(
        "managed_worktree_creation_intent".to_owned(),
        serde_json::to_value(prepared_intent(&goal, &record)).unwrap(),
    );
    let decoded: Goal = serde_json::from_value(value).unwrap();
    assert!(matches!(
        decoded.validate(),
        Err(OrchestratorError::CorruptGoal(_))
    ));
}

// --- Section 8: host-derived identity ---------------------------------------

#[test]
fn branch_ref_and_lock_reason_are_host_derived_from_goal_id() {
    let goal = managed_goal();
    let expected_ref = format!("{MANAGED_BRANCH_REF_PREFIX}{}", goal.id().as_str());
    let expected_reason = format!("{MANAGED_LOCK_REASON_PREFIX}{}", goal.id().as_str());
    assert_eq!(managed_branch_ref(goal.id()), expected_ref);
    assert_eq!(managed_lock_reason(goal.id()), expected_reason);

    let record = requested_record(&goal);
    assert_eq!(record.branch_ref(), expected_ref);
    assert_eq!(record.lock_reason(), expected_reason);
    // The full UUID is authoritative; a shortened form is not.
    assert!(record.branch_ref().ends_with(goal.id().as_str()));
    assert!(!record.branch_ref().ends_with(&goal.id().as_str()[..8]));
}

#[test]
fn caller_supplied_branch_ref_and_lock_reason_are_rejected() {
    let goal = managed_goal();
    let record = requested_record(&goal);
    for (field, hostile) in [
        ("branch_ref", "refs/heads/attacker-controlled"),
        ("branch_ref", "refs/heads/local-mcp/goal/shortened-uuid"),
        ("lock_reason", "some other reason"),
        (
            "lock_reason",
            "local-mcp goal 00000000-0000-0000-0000-000000000000",
        ),
    ] {
        let mut value = serde_json::to_value(&record).unwrap();
        value[field] = Value::from(hostile);
        let decoded: ManagedWorktreeRecord = serde_json::from_value(value).unwrap();
        let error = decoded.validate().unwrap_err();
        assert!(
            matches!(error, OrchestratorError::CorruptGoal(_)),
            "field {field} accepted hostile value {hostile}"
        );
    }
}

// --- Section 8: record shape and malformed identifiers -----------------------

#[test]
fn record_rejects_non_absolute_or_non_canonical_paths() {
    let goal = managed_goal();
    for (field, hostile) in [
        ("primary_root", "relative/primary"),
        ("repository_common_dir", "relative/.git"),
        ("worktree_root", "relative/managed"),
    ] {
        let mut value = serde_json::to_value(requested_record(&goal)).unwrap();
        value[field] = Value::from(hostile);
        let decoded: ManagedWorktreeRecord = serde_json::from_value(value).unwrap();
        assert!(
            matches!(decoded.validate(), Err(OrchestratorError::CorruptGoal(_))),
            "field {field} accepted hostile path {hostile}"
        );
    }

    let root = PathBuf::from(WORKTREE_ROOT);
    let parent = root.parent().unwrap();
    for hostile in [
        parent
            .join("..")
            .join(parent.file_name().unwrap())
            .join("escape"),
        parent.join(".").join("canonical-but-not-normalized"),
    ] {
        let mut value = serde_json::to_value(requested_record(&goal)).unwrap();
        value["worktree_root"] = serde_json::to_value(&hostile).unwrap();
        let decoded: ManagedWorktreeRecord = serde_json::from_value(value).unwrap();
        assert!(
            matches!(decoded.validate(), Err(OrchestratorError::CorruptGoal(_))),
            "accepted non-canonical path {}",
            hostile.display()
        );
    }
}

#[test]
fn record_rejects_worktree_root_overlapping_the_primary_workspace() {
    // A managed execution root nested inside the primary workspace (including
    // its Git administrative internals), or one that contains the primary
    // workspace, would relocate managed execution into the primary. Reject it
    // purely from durable data (design sections 2.7, 2.8, 13).
    let goal = managed_goal();
    let primary_root = PathBuf::from(PRIMARY_ROOT);
    let hostile_roots = [
        primary_root.join(".git"),
        primary_root.join(".git/worktrees"),
        primary_root.join("src"),
        primary_root.join("nested/deep"),
        primary_root.parent().unwrap().to_path_buf(),
        PathBuf::from(FILESYSTEM_ROOT),
    ];
    for hostile in hostile_roots {
        let mut value = serde_json::to_value(requested_record(&goal)).unwrap();
        value["worktree_root"] = serde_json::to_value(&hostile).unwrap();
        let decoded: ManagedWorktreeRecord = serde_json::from_value(value).unwrap();
        let error = decoded.validate().unwrap_err();
        assert!(
            matches!(error, OrchestratorError::CorruptGoal(_)),
            "worktree_root {} was accepted",
            hostile.display()
        );
        assert!(
            error
                .to_string()
                .contains("must not overlap the primary workspace"),
            "worktree_root {} was rejected for the wrong reason: {error}",
            hostile.display()
        );
    }
}

#[test]
fn record_rejects_sibling_worktree_roots_outside_the_primary_workspace() {
    // A legitimately placed managed root is accepted.
    let goal = managed_goal();
    requested_record(&goal).validate().unwrap();
}

#[test]
fn record_rejects_worktree_root_equal_to_primary_root() {
    let goal = managed_goal();
    let record = ManagedWorktreeRecord::requested(
        goal.id(),
        WorktreeId::new(),
        PathBuf::from(PRIMARY_ROOT),
        PathBuf::from(COMMON_DIR),
        PathBuf::from(PRIMARY_ROOT),
        BASE_COMMIT.to_owned(),
        None,
        goal.revision(),
        0,
    );
    assert!(matches!(record, Err(OrchestratorError::CorruptGoal(_))));
}

#[test]
fn record_rejects_abbreviated_or_malformed_base_commit() {
    let goal = managed_goal();
    for hostile in [
        "0123456",
        "0123456789abcdef0123456789abcdef0123456",
        "0123456789ABCDEF0123456789abcdef01234567",
        "0123456789abcdef0123456789abcdef0123456g",
        "not-a-commit",
        "",
    ] {
        let mut value = serde_json::to_value(requested_record(&goal)).unwrap();
        value["base_commit"] = Value::from(hostile);
        let decoded: ManagedWorktreeRecord = serde_json::from_value(value).unwrap();
        assert!(
            matches!(decoded.validate(), Err(OrchestratorError::CorruptGoal(_))),
            "base_commit accepted {hostile}"
        );
    }
}

#[test]
fn record_rejects_unsafe_source_refs() {
    let goal = managed_goal();
    for hostile in [
        "refs/heads/../../../etc/passwd",
        "refs/heads/feature branch",
        "refs/heads/feature..branch",
        "refs/heads/feature\\branch",
        "refs//heads/doubled",
        "refs/heads/trailing/",
        "refs/heads/.hidden",
        "refs/heads/branch.lock",
        "main",
        "refs/heads/tilde~1",
    ] {
        let mut value = serde_json::to_value(requested_record(&goal)).unwrap();
        value["source_ref"] = Value::from(hostile);
        let decoded: ManagedWorktreeRecord = serde_json::from_value(value).unwrap();
        assert!(
            matches!(decoded.validate(), Err(OrchestratorError::CorruptGoal(_))),
            "source_ref accepted {hostile}"
        );
    }
}

#[test]
fn absent_source_ref_is_allowed_for_a_detached_source() {
    let goal = managed_goal();
    let record = ManagedWorktreeRecord::requested(
        goal.id(),
        WorktreeId::new(),
        PathBuf::from(PRIMARY_ROOT),
        PathBuf::from(COMMON_DIR),
        PathBuf::from(WORKTREE_ROOT),
        BASE_COMMIT.to_owned(),
        None,
        goal.revision(),
        0,
    )
    .unwrap();
    assert!(record.source_ref().is_none());
    record.validate().unwrap();
}

#[test]
fn malformed_worktree_and_operation_identifiers_are_rejected() {
    for hostile in ["", "not-a-uuid", "../../escape", "WORKTREE-ID", "a/b", ".."] {
        assert!(
            WorktreeId::parse(hostile).is_err(),
            "WorktreeId accepted {hostile}"
        );
        assert!(
            WorktreeOperationId::parse(hostile).is_err(),
            "WorktreeOperationId accepted {hostile}"
        );
    }
    // A non-canonical UUID spelling parses but is canonicalized. Durable
    // validation must still refuse the non-canonical stored form so a Goal and
    // its worktree identity cannot be spelled two different ways.
    let upper = WorktreeId::new().as_str().to_uppercase();
    assert!(WorktreeId::parse(&upper).is_ok());
    let mut value = serde_json::to_value(requested_record(&managed_goal())).unwrap();
    value["worktree_id"] = Value::from(upper.clone());
    let decoded: ManagedWorktreeRecord = serde_json::from_value(value).unwrap();
    assert!(matches!(
        decoded.validate(),
        Err(OrchestratorError::UnsafeIdentifier(_))
    ));
}

// --- Section 21: impossible revision bindings -------------------------------

#[test]
fn record_rejects_impossible_revision_bindings() {
    let goal = managed_goal();
    // Plan revision must be 0: the worktree precedes initial plan materialization.
    let record = ManagedWorktreeRecord::requested(
        goal.id(),
        WorktreeId::new(),
        PathBuf::from(PRIMARY_ROOT),
        PathBuf::from(COMMON_DIR),
        PathBuf::from(WORKTREE_ROOT),
        BASE_COMMIT.to_owned(),
        None,
        goal.revision(),
        1,
    );
    assert!(matches!(record, Err(OrchestratorError::CorruptGoal(_))));

    // Goal revision must start at 1.
    let record = ManagedWorktreeRecord::requested(
        goal.id(),
        WorktreeId::new(),
        PathBuf::from(PRIMARY_ROOT),
        PathBuf::from(COMMON_DIR),
        PathBuf::from(WORKTREE_ROOT),
        BASE_COMMIT.to_owned(),
        None,
        0,
        0,
    );
    assert!(matches!(record, Err(OrchestratorError::CorruptGoal(_))));
}

#[test]
fn goal_rejects_record_created_above_current_goal_revision() {
    let goal = managed_goal();
    let record = ManagedWorktreeRecord::requested(
        goal.id(),
        WorktreeId::new(),
        PathBuf::from(PRIMARY_ROOT),
        PathBuf::from(COMMON_DIR),
        PathBuf::from(WORKTREE_ROOT),
        BASE_COMMIT.to_owned(),
        None,
        goal.revision() + 5,
        0,
    )
    .unwrap();
    let mut value = serde_json::to_value(&goal).unwrap();
    value["workspace_mode"] = Value::from("MANAGED_WORKTREE");
    value["managed_worktree"] = serde_json::to_value(&record).unwrap();
    value["managed_worktree_creation_intent"] =
        serde_json::to_value(prepared_intent(&goal, &record)).unwrap();
    let decoded: Goal = serde_json::from_value(value).unwrap();
    assert!(matches!(
        decoded.validate(),
        Err(OrchestratorError::CorruptGoal(_))
    ));
}

#[test]
fn goal_rejects_record_whose_primary_root_moves_goal_cwd() {
    let goal = managed_goal();
    let record = ManagedWorktreeRecord::requested(
        goal.id(),
        WorktreeId::new(),
        PathBuf::from(PRIMARY_ROOT).with_file_name("somewhere-else"),
        PathBuf::from(COMMON_DIR),
        PathBuf::from(WORKTREE_ROOT),
        BASE_COMMIT.to_owned(),
        None,
        goal.revision(),
        0,
    )
    .unwrap();
    let mut value = serde_json::to_value(&goal).unwrap();
    value["workspace_mode"] = Value::from("MANAGED_WORKTREE");
    value["managed_worktree"] = serde_json::to_value(&record).unwrap();
    value["managed_worktree_creation_intent"] =
        serde_json::to_value(prepared_intent(&goal, &record)).unwrap();
    let decoded: Goal = serde_json::from_value(value).unwrap();
    assert!(matches!(
        decoded.validate(),
        Err(OrchestratorError::CorruptGoal(_))
    ));
}

// --- Sections 4, 11: record/intent ownership consistency --------------------

#[test]
fn record_owned_by_a_different_goal_is_rejected() {
    let goal = managed_goal();
    let other = managed_goal();
    let foreign = ManagedWorktreeRecord::requested(
        other.id(),
        WorktreeId::new(),
        PathBuf::from(PRIMARY_ROOT),
        PathBuf::from(COMMON_DIR),
        PathBuf::from(WORKTREE_ROOT),
        BASE_COMMIT.to_owned(),
        None,
        goal.revision(),
        0,
    )
    .unwrap();
    let mut value = serde_json::to_value(&goal).unwrap();
    value["workspace_mode"] = Value::from("MANAGED_WORKTREE");
    value["managed_worktree"] = serde_json::to_value(&foreign).unwrap();
    let decoded: Goal = serde_json::from_value(value).unwrap();
    assert!(matches!(
        decoded.validate(),
        Err(OrchestratorError::CorruptGoal(_))
    ));
}

#[test]
fn intent_must_describe_the_same_target_as_the_record() {
    let goal = managed_goal();
    let record = requested_record(&goal);
    let intent = prepared_intent(&goal, &record);
    intent.validate_against_record(&record).unwrap();

    let other_worktree_root = PathBuf::from(WORKTREE_ROOT).with_file_name("goal-2");
    let other_common_dir = PathBuf::from(COMMON_DIR).with_file_name(".git-other");
    for (field, hostile) in [
        (
            "worktree_root",
            serde_json::to_value(other_worktree_root).unwrap(),
        ),
        ("branch_ref", Value::from("refs/heads/local-mcp/goal/other")),
        (
            "base_commit",
            Value::from("fedcba9876543210fedcba9876543210fedcba98"),
        ),
        (
            "repository_common_dir",
            serde_json::to_value(other_common_dir).unwrap(),
        ),
    ] {
        let mut value = serde_json::to_value(&intent).unwrap();
        value[field] = hostile;
        let decoded: ManagedWorktreeCreationIntent = serde_json::from_value(value).unwrap();
        assert!(
            matches!(
                decoded.validate_against_record(&record),
                Err(OrchestratorError::CorruptGoal(_))
            ),
            "intent field {field} was allowed to diverge from the record"
        );
    }
}

#[test]
fn intent_for_a_different_worktree_identity_is_rejected() {
    let goal = managed_goal();
    let record = requested_record(&goal);
    let mut value = serde_json::to_value(prepared_intent(&goal, &record)).unwrap();
    value["worktree_id"] = serde_json::to_value(WorktreeId::new()).unwrap();
    let decoded: ManagedWorktreeCreationIntent = serde_json::from_value(value).unwrap();
    assert!(matches!(
        decoded.validate_against_record(&record),
        Err(OrchestratorError::CorruptGoal(_))
    ));
}

#[test]
fn outstanding_creation_requires_a_prepared_intent() {
    let goal = managed_goal();
    let record = requested_record(&goal);
    let mut value = serde_json::to_value(&goal).unwrap();
    value["workspace_mode"] = Value::from("MANAGED_WORKTREE");
    value["managed_worktree"] = serde_json::to_value(&record).unwrap();
    let decoded: Goal = serde_json::from_value(value).unwrap();
    assert!(matches!(
        decoded.validate(),
        Err(OrchestratorError::CorruptGoal(_))
    ));
}

#[test]
fn managed_mode_requires_a_record() {
    let mut value = serde_json::to_value(managed_goal()).unwrap();
    value["workspace_mode"] = Value::from("MANAGED_WORKTREE");
    let decoded: Goal = serde_json::from_value(value).unwrap();
    assert!(matches!(
        decoded.validate(),
        Err(OrchestratorError::CorruptGoal(_))
    ));
}

#[test]
fn intent_state_is_closed_to_prepared() {
    assert_eq!(
        serde_json::to_value(ManagedWorktreeIntentState::Prepared).unwrap(),
        Value::from("PREPARED")
    );
    for hostile in ["ACTIVE", "REQUESTED", "prepared", "REMOVED", ""] {
        assert!(
            serde_json::from_str::<ManagedWorktreeIntentState>(&format!("\"{hostile}\"")).is_err(),
            "intent state accepted {hostile}"
        );
    }
}

// --- Section 5/11/19: lifecycle and execution root ---------------------------

#[test]
fn lifecycle_edges_follow_the_frozen_creation_order() {
    use ManagedWorktreeLifecycle::{
        Active, Blocked, CleanupEligible, Prepared, Removed, Requested,
    };
    let all = [
        Requested,
        Prepared,
        Active,
        CleanupEligible,
        Blocked,
        Removed,
    ];
    for from in all {
        for to in all {
            let expected = matches!(
                (from, to),
                (Requested, Prepared | Blocked)
                    | (Prepared, Active | Blocked)
                    | (Active, CleanupEligible | Removed | Blocked)
                    | (CleanupEligible, Removed | Blocked)
                    | (Blocked, Active | CleanupEligible | Removed | Blocked)
            );
            assert_eq!(
                from.can_transition_to(to),
                expected,
                "{from:?} -> {to:?} expected {expected}"
            );
        }
    }
    // Requested must not reach ACTIVE without passing through PREPARED.
    assert!(!Requested.can_transition_to(Active));
    // Only REMOVED is terminal; a retained review candidate is not.
    assert!(Removed.is_terminal());
    assert!(!Active.is_terminal());
    assert!(!CleanupEligible.is_terminal());
    assert!(!Blocked.is_terminal());
}

#[test]
fn uninvoked_lifecycle_rejects_a_reconciliation_observation() {
    let goal = managed_goal();
    for lifecycle in [
        ManagedWorktreeLifecycle::Requested,
        ManagedWorktreeLifecycle::Prepared,
    ] {
        let mut value = serde_json::to_value(requested_record(&goal)).unwrap();
        value["lifecycle"] = serde_json::to_value(lifecycle).unwrap();
        value["last_reconciled_head"] = Value::from(BASE_COMMIT);
        value["last_reconciled_at"] = Value::from(NOW);
        let decoded: ManagedWorktreeRecord = serde_json::from_value(value).unwrap();
        assert!(
            matches!(decoded.validate(), Err(OrchestratorError::CorruptGoal(_))),
            "{lifecycle:?} accepted a pre-invocation reconciliation observation"
        );
    }
}

#[test]
fn active_lifecycle_requires_a_paired_reconciliation_observation() {
    let goal = managed_goal();
    let record = requested_record(&goal);

    // Missing entirely.
    let mut value = serde_json::to_value(&record).unwrap();
    value["lifecycle"] = Value::from("ACTIVE");
    let decoded: ManagedWorktreeRecord = serde_json::from_value(value).unwrap();
    assert!(matches!(
        decoded.validate(),
        Err(OrchestratorError::CorruptGoal(_))
    ));

    // Half the pair.
    let mut value = serde_json::to_value(&record).unwrap();
    value["lifecycle"] = Value::from("ACTIVE");
    value["last_reconciled_head"] = Value::from(BASE_COMMIT);
    let decoded: ManagedWorktreeRecord = serde_json::from_value(value).unwrap();
    assert!(matches!(
        decoded.validate(),
        Err(OrchestratorError::CorruptGoal(_))
    ));

    // Complete pair.
    let mut value = serde_json::to_value(&record).unwrap();
    value["lifecycle"] = Value::from("ACTIVE");
    value["last_reconciled_head"] = Value::from(BASE_COMMIT);
    value["last_reconciled_at"] = Value::from(NOW);
    let decoded: ManagedWorktreeRecord = serde_json::from_value(value).unwrap();
    decoded.validate().unwrap();
    assert_eq!(decoded.last_reconciled_head(), Some(BASE_COMMIT));
    assert_eq!(decoded.last_reconciled_at(), Some(NOW));
}

#[test]
fn reconciled_lifecycle_rejects_a_malformed_reconciled_head() {
    let goal = managed_goal();
    let mut value = serde_json::to_value(requested_record(&goal)).unwrap();
    value["lifecycle"] = Value::from("ACTIVE");
    value["last_reconciled_head"] = Value::from("not-a-sha");
    value["last_reconciled_at"] = Value::from(NOW);
    let decoded: ManagedWorktreeRecord = serde_json::from_value(value).unwrap();
    assert!(matches!(
        decoded.validate(),
        Err(OrchestratorError::CorruptGoal(_))
    ));
}

#[test]
fn execution_root_is_primary_until_a_managed_worktree_is_active() {
    let goal = managed_goal();
    let record = requested_record(&goal);
    let intent = prepared_intent(&goal, &record);

    // Requested: managed execution root is not yet usable.
    let mut value = serde_json::to_value(&goal).unwrap();
    value["workspace_mode"] = Value::from("MANAGED_WORKTREE");
    value["managed_worktree"] = serde_json::to_value(&record).unwrap();
    value["managed_worktree_creation_intent"] = serde_json::to_value(&intent).unwrap();
    let requested: Goal = serde_json::from_value(value.clone()).unwrap();
    requested.validate().unwrap();
    assert_eq!(requested.execution_root(), None);

    // Blocked stays unusable, and a blocked creation keeps its intent so an
    // explicit host recovery can still reconcile the exact intended target.
    let mut blocked = value.clone();
    blocked["managed_worktree"]["lifecycle"] = Value::from("BLOCKED");
    let blocked: Goal = serde_json::from_value(blocked).unwrap();
    blocked.validate().unwrap();
    assert_eq!(blocked.execution_root(), None);

    // Active resolves to the managed worktree root, never the primary root.
    // A reconciled worktree has consumed its intent, so none may remain.
    let mut active = value.clone();
    active
        .as_object_mut()
        .unwrap()
        .remove("managed_worktree_creation_intent");
    active["managed_worktree"]["lifecycle"] = Value::from("ACTIVE");
    active["managed_worktree"]["last_reconciled_head"] = Value::from(BASE_COMMIT);
    active["managed_worktree"]["last_reconciled_at"] = Value::from(NOW);
    let active: Goal = serde_json::from_value(active).unwrap();
    active.validate().unwrap();
    assert_eq!(
        active.execution_root(),
        Some(PathBuf::from(WORKTREE_ROOT).as_path())
    );
    // The durable identity root is unchanged.
    assert_eq!(active.cwd(), PathBuf::from(PRIMARY_ROOT).as_path());
}

#[test]
fn a_reconciled_worktree_must_not_retain_a_prepared_intent() {
    let goal = managed_goal();
    let mut value = serde_json::to_value(&goal).unwrap();
    value["workspace_mode"] = Value::from("MANAGED_WORKTREE");
    value["managed_worktree"] = serde_json::to_value(requested_record(&goal)).unwrap();
    value["managed_worktree"]["lifecycle"] = Value::from("ACTIVE");
    value["managed_worktree"]["last_reconciled_head"] = Value::from(BASE_COMMIT);
    value["managed_worktree"]["last_reconciled_at"] = Value::from(NOW);
    value["managed_worktree_creation_intent"] =
        serde_json::to_value(prepared_intent(&goal, &requested_record(&goal))).unwrap();
    let decoded: Goal = serde_json::from_value(value).unwrap();
    assert!(matches!(
        decoded.validate(),
        Err(OrchestratorError::CorruptGoal(_))
    ));
}

// --- Section 9: schema, round trip, and corruption detection ----------------

#[test]
fn managed_goal_round_trips_through_durable_json() {
    let goal = managed_goal();
    let record = requested_record(&goal);
    let intent = prepared_intent(&goal, &record);
    let mut value = serde_json::to_value(&goal).unwrap();
    value["workspace_mode"] = Value::from("MANAGED_WORKTREE");
    value["managed_worktree"] = serde_json::to_value(&record).unwrap();
    value["managed_worktree_creation_intent"] = serde_json::to_value(&intent).unwrap();
    let managed: Goal = serde_json::from_value(value).unwrap();
    managed.validate().unwrap();

    let encoded = serde_json::to_vec_pretty(&managed).unwrap();
    let decoded: Goal = serde_json::from_slice(&encoded).unwrap();
    decoded.validate().unwrap();
    assert_eq!(decoded, managed);
    assert_eq!(decoded.schema_version(), GOAL_SCHEMA_VERSION);
    let stored: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(stored["store_format"], Value::from(GOAL_STORE_FORMAT));
    assert_eq!(stored["workspace_mode"], Value::from("MANAGED_WORKTREE"));
    assert_eq!(stored["schema_version"], Value::from(GOAL_SCHEMA_VERSION));
}

#[test]
fn deny_unknown_fields_still_rejects_unexpected_managed_fields() {
    let goal = managed_goal();
    let record = requested_record(&goal);
    let mut record_value = serde_json::to_value(&record).unwrap();
    record_value["unexpected_authority_field"] = Value::from("worktree_root=/etc");
    assert!(serde_json::from_value::<ManagedWorktreeRecord>(record_value).is_err());

    let mut intent_value = serde_json::to_value(prepared_intent(&goal, &record)).unwrap();
    intent_value["unexpected_authority_field"] = Value::from("worktree_root=/etc");
    assert!(serde_json::from_value::<ManagedWorktreeCreationIntent>(intent_value).is_err());

    let mut value = serde_json::to_value(&goal).unwrap();
    value["unexpected_goal_authority_field"] = Value::from(true);
    assert!(serde_json::from_value::<Goal>(value).is_err());
}

// --- Purity: validation must not require or trust external state -------------

#[test]
fn validation_is_pure_and_does_not_require_paths_to_exist() {
    // None of these paths exist on the test host; validation must still pass
    // because it is a pure function of durable data only.
    let goal = managed_goal();
    let record = requested_record(&goal);
    let intent = prepared_intent(&goal, &record);
    assert!(!PathBuf::from(PRIMARY_ROOT).exists());
    assert!(!PathBuf::from(WORKTREE_ROOT).exists());
    assert!(!PathBuf::from(COMMON_DIR).exists());
    record.validate().unwrap();
    intent.validate().unwrap();

    let mut value = serde_json::to_value(&goal).unwrap();
    value["workspace_mode"] = Value::from("MANAGED_WORKTREE");
    value["managed_worktree"] = serde_json::to_value(&record).unwrap();
    value["managed_worktree_creation_intent"] = serde_json::to_value(&intent).unwrap();
    let managed: Goal = serde_json::from_value(value).unwrap();
    managed.validate().unwrap();
}

#[test]
fn validation_does_not_mutate_the_record_it_checks() {
    // `validate` must be a pure predicate over durable data. Asserting that
    // re-validating an unchanged record keeps every field byte-identical is
    // falsifiable: it fails if validation ever starts normalizing, defaulting,
    // or otherwise rewriting the state it inspects.
    let goal = managed_goal();
    let record = requested_record(&goal);
    let before = serde_json::to_value(&record).unwrap();
    assert!(record.validate().is_ok());
    let after = serde_json::to_value(&record).unwrap();
    assert_eq!(before, after);
}
