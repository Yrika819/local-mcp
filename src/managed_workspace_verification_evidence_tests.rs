//! Slice 3A tests: the pure durable workspace-verification evidence record.
//!
//! Every test is **pure** — no Git, no filesystem, no `TaskStore`, no Goal state
//! transition beyond the pure constructors. The Goal-level claims are proven by
//! injecting one already-serialized evidence value into a real serialized
//! managed Goal, so the durable decode path, `#[serde(deny_unknown_fields)]`,
//! and `Goal::validate` are exercised exactly as a reload would exercise them.
//!
//! The critical properties are fail-closed ones: an absent field decodes as "no
//! evidence", a partially written record must not decode as a weaker-but-valid
//! record, and every durable binding mismatch must be refused.

use std::path::PathBuf;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::goal::{
    GOAL_SCHEMA_VERSION, Goal, GoalCriterionBinding, GoalFinalVerificationRecord,
    GoalFinalVerificationSpec, GoalId, GoalVerificationRequirement,
    MANAGED_WORKSPACE_VERIFICATION_EVIDENCE_SCHEMA_VERSION, MAX_TASK_VERIFICATION_IDENTITIES,
    ManagedWorkspaceBaseAdvanced, ManagedWorkspaceVerificationEvidenceV1,
};
use crate::managed_worktree::{ManagedWorktreeRecord, WorktreeId};
use crate::orchestrator_error::OrchestratorError;
use crate::task::{
    ReplaySafety, Task, TaskId, TaskOperationKind, TaskScope, VerificationId, VerificationOutcome,
    WorkerKind,
};
use crate::task_store::TaskStore;

const BASE_OID: &str = "0123456789abcdef0123456789abcdef01234567";
const OBSERVED_OID: &str = "1123456789abcdef0123456789abcdef01234567";
const PRIMARY_HEAD_OID: &str = "2123456789abcdef0123456789abcdef01234567";
const SNAPSHOT_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NOW: &str = "2026-10-11T00:00:00Z";

/// `ManagedWorktreeRecord::validate` requires canonical absolute paths, which on
/// Windows means a drive prefix, so the shapes are platform-specific exactly as
/// the managed-worktree tests do.
#[cfg(not(windows))]
const PRIMARY_ROOT: &str = "/managed/primary";
#[cfg(windows)]
const PRIMARY_ROOT: &str = r"C:\managed\primary";
#[cfg(not(windows))]
const COMMON_DIR: &str = "/managed/repo-common";
#[cfg(windows)]
const COMMON_DIR: &str = r"C:\managed\repo-common";
#[cfg(not(windows))]
const WORKTREE_ROOT: &str = "/managed/worktree";
#[cfg(windows)]
const WORKTREE_ROOT: &str = r"C:\managed\worktree";

fn goal_id() -> GoalId {
    GoalId::parse("00000000-0000-4000-8000-000000000002").unwrap()
}

fn worktree_id() -> WorktreeId {
    WorktreeId::parse("00000000-0000-4000-8000-000000000001").unwrap()
}

fn other_worktree_id() -> WorktreeId {
    WorktreeId::parse("00000000-0000-4000-8000-000000000009").unwrap()
}

fn task_id() -> TaskId {
    TaskId::parse("00000000-0000-4000-8000-000000000003").unwrap()
}

fn verification_id() -> VerificationId {
    VerificationId::parse("00000000-0000-4000-8000-000000000004").unwrap()
}

/// An `ACTIVE` durable record reached through the real pure lifecycle path.
fn active_record() -> ManagedWorktreeRecord {
    ManagedWorktreeRecord::requested(
        &goal_id(),
        worktree_id(),
        PathBuf::from(PRIMARY_ROOT),
        PathBuf::from(COMMON_DIR),
        PathBuf::from(WORKTREE_ROOT),
        BASE_OID.to_owned(),
        None,
        1,
        0,
    )
    .unwrap()
    .to_prepared()
    .unwrap()
    .to_active(OBSERVED_OID, NOW)
    .unwrap()
}

fn evidence() -> ManagedWorkspaceVerificationEvidenceV1 {
    ManagedWorkspaceVerificationEvidenceV1::new(
        goal_id(),
        7,
        1,
        worktree_id(),
        BASE_OID.to_owned(),
        OBSERVED_OID.to_owned(),
        SNAPSHOT_DIGEST.to_owned(),
        verification_id(),
        vec![(task_id(), verification_id())],
        None,
        NOW.to_owned(),
    )
    .unwrap()
}

/// Reload a Goal with one evidence value injected and validate it, exercising
/// the durable decode path plus [`Goal::validate`] exactly as a real
/// [`crate::task_store::TaskStore`] load would.
fn reload_with_evidence(goal: &Goal, evidence: Value) -> Result<Goal, OrchestratorError> {
    let mut value = serde_json::to_value(goal).unwrap();
    value["managed_workspace_verification_evidence"] = evidence;
    let decoded: Goal = serde_json::from_value(value)
        .map_err(|error| OrchestratorError::CorruptGoal(format!("evidence decode: {error}")))?;
    decoded.validate()?;
    Ok(decoded)
}

fn managed_goal() -> Goal {
    Goal::new_managed_with_id(
        goal_id(),
        "slice-3a".to_owned(),
        PathBuf::from(PRIMARY_ROOT),
        "managed objective".to_owned(),
        None,
        Vec::new(),
        Vec::new(),
        NOW,
        active_record(),
    )
    .unwrap()
}

fn primary_goal() -> Goal {
    Goal::new_with_id(
        goal_id(),
        "slice-3a-primary",
        PathBuf::from(PRIMARY_ROOT),
        "primary objective".to_owned(),
        None,
        Vec::new(),
        Vec::new(),
        NOW,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Durable schema boundary (schema 5 -> 6)
// ---------------------------------------------------------------------------

/// A durable document as it would be stored at `version`: a current Goal minus
/// the schema-6 evidence field, with the older `schema_version` stamped.
fn document_at_version(goal: &Goal, version: u64) -> Value {
    let mut value = serde_json::to_value(goal).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("managed_workspace_verification_evidence");
    object.insert("schema_version".to_owned(), json!(version));
    value
}

/// A schema-4 document additionally predates the durable attempt budget.
fn schema_four_document(goal: &Goal) -> Value {
    let mut value = document_at_version(goal, 4);
    if let Some(record) = goal.managed_worktree() {
        let mut record_value = serde_json::to_value(record).unwrap();
        record_value
            .as_object_mut()
            .unwrap()
            .remove("creation_attempts_consumed");
        value["managed_worktree"] = record_value;
    }
    value
}

/// Write `document` where the store would have persisted `goal`, then load it
/// through the real durable path. Returns the loaded Goal plus the durable bytes
/// as they remain after the load, so a test can prove loading did not rewrite
/// the stored document.
fn load_document(goal: &Goal, document: &Value) -> (Goal, Vec<u8>) {
    let root = std::env::temp_dir().join(format!("local-mcp-slice-3a-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let store = TaskStore::with_state_root(root.clone());
    let path = store
        .goal_path_for_test(goal.session_id(), goal.id())
        .expect("goal path");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = serde_json::to_vec_pretty(document).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let loaded = store.load_goal(goal.session_id(), goal.id()).unwrap();
    let persisted = std::fs::read(&path).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    (loaded, persisted)
}

// ---------------------------------------------------------------------------
// Pure record construction
// ---------------------------------------------------------------------------

#[test]
fn new_accepts_a_well_formed_record_and_exposes_its_bindings() {
    let record = evidence();
    assert_eq!(
        record.schema_version(),
        MANAGED_WORKSPACE_VERIFICATION_EVIDENCE_SCHEMA_VERSION
    );
    assert_eq!(record.goal_id(), &goal_id());
    assert_eq!(record.goal_revision(), 7);
    assert_eq!(record.plan_revision(), 1);
    assert_eq!(record.worktree_id(), &worktree_id());
    assert_eq!(record.base_commit(), BASE_OID);
    assert_eq!(record.observed_head(), OBSERVED_OID);
    assert_eq!(record.workspace_snapshot_digest(), SNAPSHOT_DIGEST);
    assert_eq!(record.final_verification_id(), &verification_id());
    assert_eq!(record.task_verification_identities().len(), 1);
    assert_eq!(record.verified_at(), NOW);
    assert!(record.base_advanced().is_none());
}

#[test]
fn new_stamps_the_frozen_schema_version() {
    assert_eq!(
        evidence().schema_version(),
        MANAGED_WORKSPACE_VERIFICATION_EVIDENCE_SCHEMA_VERSION
    );
    assert_eq!(MANAGED_WORKSPACE_VERIFICATION_EVIDENCE_SCHEMA_VERSION, 1);
}

#[test]
fn base_advanced_reports_a_real_advance_only() {
    let advanced =
        ManagedWorkspaceBaseAdvanced::new(BASE_OID.to_owned(), PRIMARY_HEAD_OID.to_owned())
            .unwrap();
    assert_eq!(advanced.old_base(), BASE_OID);
    assert_eq!(advanced.current_primary_head(), PRIMARY_HEAD_OID);

    // An "advance" to the same commit is not an advance, and a malformed head is
    // not an object ID at all: both fail closed.
    assert!(ManagedWorkspaceBaseAdvanced::new(BASE_OID.to_owned(), BASE_OID.to_owned()).is_err());
    assert!(
        ManagedWorkspaceBaseAdvanced::new(BASE_OID.to_owned(), "not-an-object-id".to_owned())
            .is_err()
    );
}

#[test]
fn new_refuses_non_durable_object_identities() {
    for (base, observed) in [
        ("not-an-object-id", OBSERVED_OID),
        (BASE_OID, "not-an-object-id"),
        (&BASE_OID.to_uppercase(), OBSERVED_OID),
    ] {
        let result = ManagedWorkspaceVerificationEvidenceV1::new(
            goal_id(),
            7,
            1,
            worktree_id(),
            base.to_owned(),
            observed.to_owned(),
            SNAPSHOT_DIGEST.to_owned(),
            verification_id(),
            Vec::new(),
            None,
            NOW.to_owned(),
        );
        assert!(result.is_err(), "accepted base={base} observed={observed}");
    }
}

#[test]
fn new_refuses_a_malformed_snapshot_digest() {
    for digest in [
        "a".repeat(63),
        format!("{}a", "A".repeat(63)),
        "z".repeat(64),
        String::new(),
    ] {
        let result = ManagedWorkspaceVerificationEvidenceV1::new(
            goal_id(),
            7,
            1,
            worktree_id(),
            BASE_OID.to_owned(),
            OBSERVED_OID.to_owned(),
            digest.clone(),
            verification_id(),
            Vec::new(),
            None,
            NOW.to_owned(),
        );
        assert!(result.is_err(), "accepted digest {digest}");
    }
}

#[test]
fn new_refuses_zero_revisions_and_a_non_rfc3339_timestamp() {
    for (revision, plan) in [(0, 1), (7, 0), (0, 0)] {
        let result = ManagedWorkspaceVerificationEvidenceV1::new(
            goal_id(),
            revision,
            plan,
            worktree_id(),
            BASE_OID.to_owned(),
            OBSERVED_OID.to_owned(),
            SNAPSHOT_DIGEST.to_owned(),
            verification_id(),
            Vec::new(),
            None,
            NOW.to_owned(),
        );
        assert!(result.is_err(), "accepted revision={revision} plan={plan}");
    }
    for timestamp in [
        "2026-10-11T00:00:00",
        "2026-10-11T00:00:00+00:00",
        "2026-10-11 00:00:00Z",
        "not-a-timestamp",
    ] {
        let result = ManagedWorkspaceVerificationEvidenceV1::new(
            goal_id(),
            7,
            1,
            worktree_id(),
            BASE_OID.to_owned(),
            OBSERVED_OID.to_owned(),
            SNAPSHOT_DIGEST.to_owned(),
            verification_id(),
            Vec::new(),
            None,
            timestamp.to_owned(),
        );
        assert!(result.is_err(), "accepted timestamp {timestamp}");
    }
}

#[test]
fn new_refuses_a_repeated_task_verification_identity() {
    let result = ManagedWorkspaceVerificationEvidenceV1::new(
        goal_id(),
        7,
        1,
        worktree_id(),
        BASE_OID.to_owned(),
        OBSERVED_OID.to_owned(),
        SNAPSHOT_DIGEST.to_owned(),
        verification_id(),
        vec![
            (task_id(), verification_id()),
            (task_id(), verification_id()),
        ],
        None,
        NOW.to_owned(),
    );
    assert!(result.is_err());
}

#[test]
fn a_well_formed_record_round_trips_through_durable_json() {
    let record = evidence();
    let value = serde_json::to_value(&record).unwrap();
    assert_eq!(value["schema_version"], json!(1));
    assert!(
        value.get("base_advanced").is_some(),
        "an absent optional field must still be written so a reload is explicit"
    );
    let decoded: ManagedWorkspaceVerificationEvidenceV1 = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn an_unknown_evidence_schema_version_is_refused_by_the_durable_goal() {
    let mut record = serde_json::to_value(evidence()).unwrap();
    record["schema_version"] = json!(MANAGED_WORKSPACE_VERIFICATION_EVIDENCE_SCHEMA_VERSION + 1);
    let error = reload_with_evidence(&managed_goal(), record).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("evidence schema version is unsupported"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_record_with_a_missing_authority_field_fails_closed() {
    let mut value = serde_json::to_value(evidence()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("workspace_snapshot_digest")
        .unwrap();
    let decoded: Result<ManagedWorkspaceVerificationEvidenceV1, _> = serde_json::from_value(value);
    assert!(
        decoded.is_err(),
        "a record missing its digest must not decode as a weaker-but-valid record"
    );
}

// ---------------------------------------------------------------------------
// Durable Goal binding
// ---------------------------------------------------------------------------

#[test]
fn a_schema_six_goal_states_the_absence_of_evidence_explicitly() {
    // The field is authority-bearing state, so a schema-6 document always
    // carries it: `null` is the explicit statement that no evidence exists,
    // never a hole that a serde default fills in.
    for goal in [managed_goal(), primary_goal()] {
        let value = serde_json::to_value(&goal).unwrap();
        assert_eq!(
            value["schema_version"],
            json!(crate::goal::GOAL_SCHEMA_VERSION)
        );
        assert_eq!(
            value["managed_workspace_verification_evidence"],
            Value::Null,
            "a schema-6 document must state the absence of evidence explicitly"
        );
        let decoded: Goal = serde_json::from_value(value).unwrap();
        assert!(decoded.managed_workspace_verification_evidence().is_none());
        decoded.validate().unwrap();
    }
}

#[test]
fn a_current_schema_document_that_omits_the_field_fails_closed() {
    // No serde default may stand in for the schema boundary: a document that
    // does not state the evidence field is not a current-schema document, so the
    // durable decode path must refuse it and only the migration chain may
    // continue.
    let goal = managed_goal();
    let mut document = serde_json::to_value(&goal).unwrap();
    document["schema_version"] = json!(GOAL_SCHEMA_VERSION);
    document
        .as_object_mut()
        .unwrap()
        .remove("managed_workspace_verification_evidence")
        .unwrap();
    let root = std::env::temp_dir().join(format!("local-mcp-slice-3a-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let store = TaskStore::with_state_root(root.clone());
    let path = store
        .goal_path_for_test(goal.session_id(), goal.id())
        .expect("goal path");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let error = store.load_goal(goal.session_id(), goal.id()).unwrap_err();
    assert!(
        matches!(error, OrchestratorError::CorruptGoal(ref detail) if detail.contains("must state managed-workspace verification evidence")),
        "unexpected error: {error}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_primary_goal_refuses_workspace_evidence() {
    let error = reload_with_evidence(&primary_goal(), serde_json::to_value(evidence()).unwrap())
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("PRIMARY Goal must not carry managed-workspace verification evidence"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_managed_goal_refuses_a_foreign_worktree_identity() {
    let mut record = serde_json::to_value(evidence()).unwrap();
    record["worktree_id"] = json!(other_worktree_id());
    let error = reload_with_evidence(&managed_goal(), record).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("worktree_id does not match the durable record"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_managed_goal_refuses_a_substituted_base_commit() {
    let mut record = serde_json::to_value(evidence()).unwrap();
    record["base_commit"] = json!(PRIMARY_HEAD_OID);
    let error = reload_with_evidence(&managed_goal(), record).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("base_commit does not match the durable record"),
        "unexpected error: {error}"
    );
}

#[test]
fn base_advanced_must_report_the_durable_base_commit() {
    let advanced =
        ManagedWorkspaceBaseAdvanced::new(BASE_OID.to_owned(), PRIMARY_HEAD_OID.to_owned())
            .unwrap();
    let record = ManagedWorkspaceVerificationEvidenceV1::new(
        goal_id(),
        1,
        1,
        worktree_id(),
        BASE_OID.to_owned(),
        OBSERVED_OID.to_owned(),
        SNAPSHOT_DIGEST.to_owned(),
        verification_id(),
        Vec::new(),
        Some(advanced),
        NOW.to_owned(),
    )
    .unwrap();
    let mut value = serde_json::to_value(record).unwrap();
    value["base_advanced"]["old_base"] = json!(PRIMARY_HEAD_OID);
    let error = reload_with_evidence(&managed_goal(), value).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("BASE_ADVANCED old_base does not match the durable managed base commit"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_managed_goal_refuses_evidence_bound_to_an_unreachable_revision() {
    let mut record = serde_json::to_value(evidence()).unwrap();
    record["goal_revision"] = json!(99);
    let error = reload_with_evidence(&managed_goal(), record).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("revisions are inconsistent with the Goal"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_managed_goal_refuses_evidence_bound_to_another_goal() {
    let mut record = serde_json::to_value(evidence()).unwrap();
    record["goal_id"] = json!(GoalId::parse("00000000-0000-4000-8000-0000000000ff").unwrap());
    let error = reload_with_evidence(&managed_goal(), record).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("goal_id does not match the durable Goal"),
        "unexpected error: {error}"
    );
}

#[test]
fn schema_five_primary_goal_migrates_to_six_without_evidence() {
    let goal = primary_goal();
    let document = document_at_version(&goal, 5);
    let (loaded, persisted) = load_document(&goal, &document);
    assert_eq!(loaded.schema_version(), GOAL_SCHEMA_VERSION);
    assert_eq!(
        loaded.workspace_mode(),
        crate::managed_worktree::WorkspaceMode::Primary
    );
    assert!(loaded.managed_worktree().is_none());
    // The migration decides "no evidence"; it never invents one.
    assert!(loaded.managed_workspace_verification_evidence().is_none());
    assert_eq!(loaded.execution_root(), Some(loaded.cwd()));
    // Loading must not rewrite the durable document.
    assert_eq!(persisted, serde_json::to_vec_pretty(&document).unwrap());
}

#[test]
fn schema_five_managed_goal_migrates_to_six_without_evidence() {
    let goal = managed_goal();
    let document = document_at_version(&goal, 5);
    let (loaded, persisted) = load_document(&goal, &document);
    assert_eq!(loaded.schema_version(), GOAL_SCHEMA_VERSION);
    assert_eq!(
        loaded.workspace_mode(),
        crate::managed_worktree::WorkspaceMode::ManagedWorktree
    );
    let record = loaded.managed_worktree().expect("managed record survives");
    assert_eq!(record.worktree_id(), &worktree_id());
    assert_eq!(record.base_commit(), BASE_OID);
    // A managed Goal with a live candidate still has no evidence until the
    // finalization path assembles one.
    assert!(loaded.managed_workspace_verification_evidence().is_none());
    assert_eq!(persisted, serde_json::to_vec_pretty(&document).unwrap());
}

#[test]
fn schema_four_goal_migrates_through_five_to_six() {
    let goal = managed_goal();
    let document = schema_four_document(&goal);
    let (loaded, _) = load_document(&goal, &document);
    assert_eq!(loaded.schema_version(), GOAL_SCHEMA_VERSION);
    let record = loaded.managed_worktree().expect("managed record survives");
    // The schema-5 step still decides the durable attempt budget on the way: an
    // ACTIVE record is migrated to a spent budget rather than a fresh one.
    assert_eq!(
        record.creation_attempts_consumed(),
        crate::managed_worktree::MAX_LIFETIME_CREATION_ATTEMPTS
    );
    assert!(!record.permits_creation_attempt());
    // And the schema-6 step states the absence of evidence, inventing nothing.
    assert!(loaded.managed_workspace_verification_evidence().is_none());
}

#[test]
fn schema_three_goal_migrates_through_the_whole_chain_to_six() {
    let goal = primary_goal();
    let document = document_at_version(&goal, 3);
    let (loaded, _) = load_document(&goal, &document);
    assert_eq!(loaded.schema_version(), GOAL_SCHEMA_VERSION);
    assert_eq!(
        loaded.workspace_mode(),
        crate::managed_worktree::WorkspaceMode::Primary
    );
    assert!(loaded.managed_worktree().is_none());
    assert!(loaded.managed_workspace_verification_evidence().is_none());
}

#[test]
fn schema_two_goal_migrates_through_the_whole_chain_to_six() {
    let goal = primary_goal();
    let mut document = document_at_version(&goal, 2);
    document
        .as_object_mut()
        .unwrap()
        .remove("pristine_plan_supersessions");
    let (loaded, _) = load_document(&goal, &document);
    assert_eq!(loaded.schema_version(), GOAL_SCHEMA_VERSION);
    assert_eq!(
        loaded.workspace_mode(),
        crate::managed_worktree::WorkspaceMode::Primary
    );
    assert!(loaded.managed_worktree().is_none());
    assert!(loaded.managed_workspace_verification_evidence().is_none());
}

/// A terminal legacy (schema 1) durable document, built exactly as the store
/// would have stored it before structured criteria existed.
fn schema_one_document(goal: &Goal) -> Value {
    let mut value = serde_json::to_value(goal).unwrap();
    let object = value.as_object_mut().unwrap();
    // The store's legacy path performs the criteria rename itself, so the
    // document keeps `completion_criteria` and carries none of the later
    // authority fields.
    object.remove("legacy_completion_criteria");
    object.remove("legacy_final_verification");
    object.remove("final_verifications");
    object.remove("final_verification_spec");
    object.remove("managed_worktree");
    object.remove("managed_worktree_creation_intent");
    object.remove("managed_workspace_verification_evidence");
    object.remove("workspace_mode");
    object.remove("pre_execution_plan_rejections");
    object.remove("pristine_plan_supersessions");
    object.remove("failed_task_replan_requests");
    object.remove("failed_task_replacements");
    object.insert("schema_version".to_owned(), json!(1));
    // Schema 1 stored criteria as plain strings; the store renames them.
    object.insert(
        "completion_criteria".to_owned(),
        json!(["primary objective"]),
    );
    object.insert("status".to_owned(), json!("COMPLETED"));
    object.insert("completed_at".to_owned(), json!(NOW));
    value
}

#[test]
fn schema_one_terminal_goal_is_unchanged_by_the_schema_boundary() {
    let goal = primary_goal();
    let document = schema_one_document(&goal);
    let (loaded, persisted) = load_document(&goal, &document);
    // A legacy terminal Goal keeps its own durable version and stays readable.
    assert_eq!(loaded.schema_version(), 1);
    assert!(loaded.status().is_terminal());
    assert_eq!(loaded.execution_root(), Some(loaded.cwd()));
    assert!(loaded.managed_worktree().is_none());
    assert!(loaded.managed_workspace_verification_evidence().is_none());
    assert_eq!(
        loaded.completion_criterion_descriptions(),
        ["primary objective"]
    );
    assert_eq!(persisted, serde_json::to_vec_pretty(&document).unwrap());
}

#[test]
fn a_schema_one_goal_cannot_smuggle_workspace_evidence() {
    let goal = primary_goal();
    let mut document = schema_one_document(&goal);
    document["managed_workspace_verification_evidence"] = serde_json::to_value(evidence()).unwrap();
    let root = std::env::temp_dir().join(format!("local-mcp-slice-3a-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let store = TaskStore::with_state_root(root.clone());
    let path = store
        .goal_path_for_test(goal.session_id(), goal.id())
        .expect("goal path");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let error = store.load_goal(goal.session_id(), goal.id()).unwrap_err();
    assert!(
        matches!(error, OrchestratorError::CorruptGoal(_)),
        "a legacy Goal smuggled workspace evidence: {error}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn an_unsupported_future_schema_is_still_rejected() {
    let goal = managed_goal();
    let mut document = serde_json::to_value(&goal).unwrap();
    document["schema_version"] = json!(GOAL_SCHEMA_VERSION + 1);
    let root = std::env::temp_dir().join(format!("local-mcp-slice-3a-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let store = TaskStore::with_state_root(root.clone());
    let path = store
        .goal_path_for_test(goal.session_id(), goal.id())
        .expect("goal path");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let error = store.load_goal(goal.session_id(), goal.id()).unwrap_err();
    assert!(
        matches!(error, OrchestratorError::UnsupportedSchema(version) if version == GOAL_SCHEMA_VERSION as u64 + 1),
        "unexpected error: {error}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_schema_five_document_may_not_carry_the_evidence_field() {
    // Seizing a field from a later schema would let a document hand itself
    // post-verification evidence authority, so the migration refuses it.
    let goal = managed_goal();
    let mut document = document_at_version(&goal, 5);
    document["managed_workspace_verification_evidence"] = serde_json::to_value(evidence()).unwrap();
    let root = std::env::temp_dir().join(format!("local-mcp-slice-3a-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let store = TaskStore::with_state_root(root.clone());
    let path = store
        .goal_path_for_test(goal.session_id(), goal.id())
        .expect("goal path");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let error = store.load_goal(goal.session_id(), goal.id()).unwrap_err();
    assert!(
        matches!(error, OrchestratorError::CorruptGoal(_)),
        "a schema-5 document carried the evidence field: {error}"
    );
    assert!(
        error
            .to_string()
            .contains("must not carry managed-workspace verification evidence"),
        "unexpected error: {error}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

// ---------------------------------------------------------------------------
// Fully bound Goal fixture: deserialization/validation-boundary proof
// ---------------------------------------------------------------------------

/// A managed Goal whose durable bindings fully satisfy a Slice 3A evidence
/// record: plan 1 with one mandatory task, a matching final-verification
/// contract, and one passing host-verifier record.
///
/// The plan goes through the real pure materialization path; only the
/// `final_verifications` history is injected as JSON, because no pure seam
/// mints a verifier record outside the verifier itself. The record JSON is
/// produced by the real [`GoalFinalVerificationRecord`] constructor with only
/// its random identity overwritten, so the shape stays
/// deny-unknown-fields faithful.
///
/// With this fixture every binding check in
/// [`ManagedWorkspaceVerificationEvidenceV1::validate_against_goal`] passes,
/// which isolates the structural checks: a mutated record that still
/// validates proves the structural invariant never ran on the durable path.
fn bound_goal() -> Goal {
    let mut goal = managed_goal();
    let task = Task::new(
        "slice-3a task",
        "slice-3a task objective",
        true,
        WorkerKind::CodexReadonly,
        TaskScope::new(
            vec![PathBuf::from(PRIMARY_ROOT).join("src")],
            Vec::new(),
            TaskOperationKind::ReadOnly,
            ReplaySafety::SafeReadOnly,
        ),
        Vec::new(),
        1,
        1,
        NOW,
    )
    .unwrap();
    let criterion = goal.completion_criteria()[0].id().clone();
    let binding = GoalCriterionBinding::new(
        criterion,
        vec![GoalVerificationRequirement::TaskVerified {
            task_id: task.id().clone(),
        }],
    );
    goal.materialize_initial_plan_with_contract(
        vec![task],
        GoalFinalVerificationSpec::new(1, vec![binding]),
        NOW,
    )
    .unwrap();
    let mut value = serde_json::to_value(&goal).unwrap();
    let revision = value["revision"].as_u64().unwrap();
    let mut record = serde_json::to_value(GoalFinalVerificationRecord::new(
        VerificationOutcome::Passed,
        goal_id(),
        revision,
        revision,
        1,
        "b".repeat(64),
        Vec::new(),
        NOW,
        NOW,
    ))
    .unwrap();
    record["id"] = json!(verification_id());
    value
        .as_object_mut()
        .unwrap()
        .insert("final_verifications".to_owned(), json!([record]));
    let bound: Goal = serde_json::from_value(value).unwrap();
    bound.validate().unwrap();
    bound
}

/// Evidence whose bindings match [`bound_goal`]: the same Goal and worktree
/// identities, the Goal's own revision, plan 1, and the passing
/// final-verification record the fixture carries.
fn bound_evidence_value(goal: &Goal) -> Value {
    let mut record = serde_json::to_value(evidence()).unwrap();
    record["goal_revision"] = json!(goal.revision());
    record["plan_revision"] = json!(1);
    record
}

/// Decode one evidence value into a [`bound_goal`] and run [`Goal::validate`],
/// exactly as a schema-6 [`TaskStore`] load would after its field-presence
/// check. A rebuilt fixture carries identical checked bindings (Goal and
/// worktree identities, revisions, verification identity), so only the
/// mutated structural field can decide the outcome.
fn reload_bound_goal_with_evidence(evidence: Value) -> Result<Goal, OrchestratorError> {
    let goal = bound_goal();
    let mut value = serde_json::to_value(&goal).unwrap();
    value["managed_workspace_verification_evidence"] = evidence;
    let decoded: Goal = serde_json::from_value(value)
        .map_err(|error| OrchestratorError::CorruptGoal(format!("evidence decode: {error}")))?;
    decoded.validate()?;
    Ok(decoded)
}

fn evidence_at(
    verified_at: &str,
) -> Result<ManagedWorkspaceVerificationEvidenceV1, OrchestratorError> {
    ManagedWorkspaceVerificationEvidenceV1::new(
        goal_id(),
        7,
        1,
        worktree_id(),
        BASE_OID.to_owned(),
        OBSERVED_OID.to_owned(),
        SNAPSHOT_DIGEST.to_owned(),
        verification_id(),
        vec![(task_id(), verification_id())],
        None,
        verified_at.to_owned(),
    )
}

#[test]
fn a_fully_bound_record_validates_against_its_goal() {
    // Positive control for the boundary battery below: every binding the
    // durable path checks is satisfied, so this record must validate. If this
    // ever fails, the fixture — not the evidence contract — is broken.
    let goal = bound_goal();
    let reloaded = reload_bound_goal_with_evidence(bound_evidence_value(&goal)).unwrap();
    let stored = reloaded
        .managed_workspace_verification_evidence()
        .expect("evidence survives the durable round trip");
    assert_eq!(stored.goal_revision(), goal.revision());
    assert_eq!(stored.plan_revision(), 1);
    assert_eq!(stored.final_verification_id(), &verification_id());
}

#[test]
fn a_bound_goal_rejects_a_malformed_snapshot_digest() {
    // The constructor refuses these shapes; the durable path must refuse them
    // too, because deserialization never runs the constructor.
    for digest in ["a".repeat(63), "z".repeat(64), "A".repeat(64)] {
        let mut record = bound_evidence_value(&bound_goal());
        record["workspace_snapshot_digest"] = json!(digest.clone());
        let error = reload_bound_goal_with_evidence(record).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("workspace snapshot digest must be 64 lowercase hex"),
            "unexpected error for digest {digest}: {error}"
        );
    }
}

#[test]
fn a_bound_goal_rejects_a_malformed_observed_head() {
    // `observed_head` has no durable counterpart to bind against (it is fresh
    // observation data), so its object-ID shape is the only durable check —
    // and it must run on deserialized records, not just the constructor.
    let mut record = bound_evidence_value(&bound_goal());
    record["observed_head"] = json!("not-an-object-id");
    let error = reload_bound_goal_with_evidence(record).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("evidence observed_head must be a full lowercase hex"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_bound_goal_rejects_task_identity_list_violations() {
    let task = task_id().as_str().to_owned();
    let verification = verification_id().as_str().to_owned();
    let oversized: Vec<Value> = (0..=MAX_TASK_VERIFICATION_IDENTITIES as u32)
        .map(|index| json!([format!("00000000-0000-4000-8000-{index:012}"), verification,]))
        .collect();
    assert_eq!(oversized.len(), MAX_TASK_VERIFICATION_IDENTITIES + 1);
    let cases: Vec<(Value, &str)> = vec![
        (
            json!([
                [task.clone(), verification.clone()],
                [task.clone(), verification.clone()],
            ]),
            "repeats a task verification identity",
        ),
        (Value::Array(oversized), "task identity list is too large"),
        (
            json!([["not-a-task-id", verification.clone()]]),
            "unsafe TaskId identifier",
        ),
        (
            json!([[task.clone(), "not-a-verification-id"]]),
            "unsafe VerificationId identifier",
        ),
    ];
    for (identities, expected) in cases {
        let mut record = bound_evidence_value(&bound_goal());
        record["task_verification_identities"] = identities;
        let error = reload_bound_goal_with_evidence(record).unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "unexpected error: {error}"
        );
    }
}

#[test]
fn a_bound_goal_rejects_a_malformed_timestamp() {
    let mut record = bound_evidence_value(&bound_goal());
    record["verified_at"] = json!("not-a-timestamp");
    let error = reload_bound_goal_with_evidence(record).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("verified_at must be RFC3339 UTC"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_bound_goal_rejects_a_calendrically_impossible_timestamp() {
    // The frozen contract is RFC 3339 UTC (orchestrator design section 5.1,
    // freeze note J7): month 13, day 40, hour 25, minute 61, a leap second,
    // and February 29 on a common year are not timestamps the minter
    // (`utc_now_rfc3339`) can ever produce.
    for timestamp in [
        "2026-13-11T00:00:00Z",
        "2026-10-40T00:00:00Z",
        "2026-10-11T25:00:00Z",
        "2026-10-11T00:61:00Z",
        "2026-10-11T00:00:60Z",
        "2023-02-29T00:00:00Z",
    ] {
        let mut record = bound_evidence_value(&bound_goal());
        record["verified_at"] = json!(timestamp);
        let error = reload_bound_goal_with_evidence(record).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("verified_at must be RFC3339 UTC"),
            "unexpected error for timestamp {timestamp}: {error}"
        );
    }
}

#[test]
fn new_refuses_a_calendrically_impossible_timestamp() {
    // The constructor and the durable path share one validator, so the
    // constructor refuses impossible dates too — while a real leap day still
    // builds.
    for timestamp in ["2026-13-11T00:00:00Z", "2023-02-29T00:00:00Z"] {
        assert!(
            evidence_at(timestamp).is_err(),
            "accepted timestamp {timestamp}"
        );
    }
    assert!(evidence_at("2024-02-29T00:00:00Z").is_ok());
}

#[test]
fn a_bound_goal_rejects_malformed_base_advanced_evidence() {
    let cases: Vec<(Value, &str)> = vec![
        (
            json!({
                "old_base": BASE_OID,
                "current_primary_head": "not-an-object-id",
            }),
            "base_advanced current_primary_head must be a full lowercase hex",
        ),
        (
            json!({
                "old_base": BASE_OID,
                "current_primary_head": BASE_OID,
            }),
            "BASE_ADVANCED requires the primary HEAD to differ",
        ),
        (
            json!({
                "old_base": "not-an-object-id",
                "current_primary_head": PRIMARY_HEAD_OID,
            }),
            // A substituted old base is refused by the binding check before
            // the structural re-check runs; the binding transitively proves
            // the shape, so the mismatch — not the spelling — is reported.
            "BASE_ADVANCED old_base does not match the durable managed base commit",
        ),
    ];
    for (advanced, expected) in cases {
        let mut record = bound_evidence_value(&bound_goal());
        record["base_advanced"] = advanced;
        let error = reload_bound_goal_with_evidence(record).unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "unexpected error: {error}"
        );
    }
}

#[test]
fn an_unknown_evidence_field_is_rejected_during_deserialization() {
    // `deny_unknown_fields` fires inside serde, before any Goal validation
    // runs: the error must come from the decode layer, not the validator.
    let mut record = bound_evidence_value(&bound_goal());
    record["unknown_future_field"] = json!(1);
    let error = reload_bound_goal_with_evidence(record).unwrap_err();
    assert!(
        error.to_string().contains("evidence decode:"),
        "unexpected error layer: {error}"
    );
}

#[test]
fn task_store_rejects_a_bound_goal_carrying_a_malformed_digest() {
    // The same malformed record through the real durable load path: the store
    // must refuse to hand back a Goal whose evidence never passed structural
    // validation.
    let goal = bound_goal();
    let mut document = serde_json::to_value(&goal).unwrap();
    let mut evidence = bound_evidence_value(&goal);
    evidence["workspace_snapshot_digest"] = json!("z".repeat(64));
    document["managed_workspace_verification_evidence"] = evidence;
    let root = std::env::temp_dir().join(format!("local-mcp-slice-3a-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let store = TaskStore::with_state_root(root.clone());
    let path = store
        .goal_path_for_test(goal.session_id(), goal.id())
        .expect("goal path");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    let error = store.load_goal(goal.session_id(), goal.id()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("workspace snapshot digest must be 64 lowercase hex"),
        "unexpected error: {error}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn task_store_loads_a_bound_goal_carrying_valid_evidence() {
    // Store-layer happy path: a schema-6 document whose evidence is fully
    // bound loads unchanged, proving the hardened validation does not refuse
    // a record the finalization path (3C/3D) will legitimately mint. Loading
    // must not rewrite the durable document.
    let goal = bound_goal();
    let mut document = serde_json::to_value(&goal).unwrap();
    document["managed_workspace_verification_evidence"] = bound_evidence_value(&goal);
    let (loaded, persisted) = load_document(&goal, &document);
    let stored = loaded
        .managed_workspace_verification_evidence()
        .expect("evidence survives the store round trip");
    assert_eq!(stored.goal_revision(), goal.revision());
    assert_eq!(persisted, serde_json::to_vec_pretty(&document).unwrap());
}
