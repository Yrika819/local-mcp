//! External Agent Backend V1 — E3-A durable foundation tests.
//!
//! These tests cover the *storage and policy* layer only: backend opt-in,
//! durable WorkItem issuance and reload, the single-active-WorkItem limit,
//! optimistic revision enforcement, cancellation invalidation, crash/partial
//! write fail-closed behavior, and GoalStore downgrade semantics. The
//! `goal_next` / `goal_submit` contract tests live in `external_goal_api_tests`
//! and the failure-injection security matrix in `external_goal_recovery_tests`.
//!
//! Everything runs in an isolated temp root. No test reads or writes the host
//! state directory, spawns a process, or touches a repository.

use std::path::PathBuf;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::config;
use crate::external_model_store::{
    cancel_active_work_in_place, commit_issued_work_item, commit_submission, external_state,
    external_state_mut, mutate_external,
};
use crate::external_model_work::{
    ExternalModelState, MAX_ACTIVE_WORK_ITEMS_PER_GOAL, MAX_MODEL_WORK_ITEMS_PER_GOAL,
    MAX_REQUEST_PAYLOAD_BYTES, ModelWorkItem, OUTPUT_CONTRACT_PLANNER_V1, SubmissionDecision,
    SubmissionId, SubmissionReceipt, WorkId, WorkItemState, WorkRole, canonical_json,
    payload_digest, sha256_hex,
};
use crate::goal::{Goal, GoalId, GoalStatus};
use crate::goal_api;
use crate::model_backend::ModelBackend;
use crate::orchestrator_error::OrchestratorError;
use crate::task_store::{FaultPoint, TaskStore, utc_now_rfc3339};

struct Fixture {
    root: PathBuf,
    session: config::Session,
    store: TaskStore,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fixture(label: &str) -> Fixture {
    let root = std::env::temp_dir().join(format!("local-mcp-e3a-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let session = config::Session {
        id: format!("e3a-{label}-{}", Uuid::new_v4()),
        cwd: workspace.clone(),
        permitted_directories: vec![workspace],
    };
    let store = TaskStore::with_state_root(state);
    Fixture {
        root,
        session,
        store,
    }
}

fn start(session: &config::Session, store: &TaskStore, backend: Option<&str>) -> Value {
    let mut args = json!({
        "session_id": session.id,
        "objective": "Produce a verified external model plan",
    });
    if let Some(backend) = backend {
        args["model_backend"] = Value::String(backend.to_owned());
    }
    serde_json::to_value(goal_api::goal_start(&args, session, store).unwrap()).unwrap()
}

fn started_goal_id(view: &Value) -> GoalId {
    GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap()
}

fn external_goal(session: &config::Session, now: &str) -> Goal {
    Goal::new(
        session.id.clone(),
        session.cwd.clone(),
        "Produce a verified external model plan",
        None,
        Vec::new(),
        Vec::new(),
        now,
    )
    .unwrap()
    .with_creation_backend(ModelBackend::External)
}

fn planner_work_item(goal: &Goal, now: &str) -> ModelWorkItem {
    planner_work_item_with_counter(goal, now, 1)
}

fn planner_work_item_with_counter(goal: &Goal, now: &str, issue_counter: u32) -> ModelWorkItem {
    let payload = canonical_json(&json!({ "objective": goal.objective() }));
    ModelWorkItem::pending(
        WorkId::new(),
        goal.session_id().to_owned(),
        goal.id().as_str().to_owned(),
        WorkRole::Planner,
        None,
        None,
        goal.revision(),
        goal.plan_revision(),
        sha256_hex(payload.as_bytes()),
        payload,
        OUTPUT_CONTRACT_PLANNER_V1,
        issue_counter,
        now,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Backend policy: CODEX default, EXTERNAL opt-in, HYBRID refused.
// ---------------------------------------------------------------------------

#[test]
fn goal_start_defaults_to_codex_and_omits_the_new_durable_fields() {
    let f = fixture("codex-default");
    let view = start(&f.session, &f.store, None);
    assert_eq!(view["model_backend"], "CODEX");
    let goal = f
        .store
        .load_goal(&f.session.id, &started_goal_id(&view))
        .unwrap();
    assert_eq!(goal.model_backend(), ModelBackend::Codex);
    assert!(goal.external_model().is_none());
    assert_eq!(external_state(&goal).unwrap_err().to_string(), {
        "goal state is corrupt: goal is not an EXTERNAL backend goal".to_owned()
    });
    // The serialized document must not mention either new field at all, so a
    // pre-E3 binary can read and rewrite it.
    let document = serde_json::to_value(&goal).unwrap();
    assert!(document.get("model_backend").is_none());
    assert!(document.get("external_model").is_none());
}

#[test]
fn goal_start_opts_into_external_and_records_durable_state() {
    let f = fixture("external-opt-in");
    let view = start(&f.session, &f.store, Some("EXTERNAL"));
    assert_eq!(view["model_backend"], "EXTERNAL");
    let goal = f
        .store
        .load_goal(&f.session.id, &started_goal_id(&view))
        .unwrap();
    assert!(goal.model_backend().is_external());
    let state = external_state(&goal).unwrap();
    assert_eq!(state.issue_counter(), 0);
    assert!(state.active_work().is_none());
    assert_eq!(state.receipts().len(), 0);
    let document = serde_json::to_value(&goal).unwrap();
    assert_eq!(document["model_backend"], "EXTERNAL");
    assert_eq!(document["external_model"]["schema_version"], 1);
}

#[test]
fn hybrid_and_unknown_backends_are_refused_and_create_nothing() {
    let f = fixture("hybrid");
    let args = json!({
        "session_id": f.session.id,
        "objective": "Never silently downgraded",
        "model_backend": "HYBRID",
    });
    let error = goal_api::goal_start(&args, &f.session, &f.store).unwrap_err();
    assert_eq!(error.code(), "UNSUPPORTED_MODEL_BACKEND");
    let args = json!({
        "session_id": f.session.id,
        "objective": "Never silently downgraded",
        "model_backend": "codex",
    });
    let error = goal_api::goal_start(&args, &f.session, &f.store).unwrap_err();
    assert_eq!(error.code(), "UNKNOWN_MODEL_BACKEND");
    assert!(
        f.store
            .list_goals_for_session(&f.session.id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn idempotency_replay_is_refused_across_backend_modes() {
    let f = fixture("idempotency-backend");
    let key = "e3a-backend-key";
    let codex = json!({
        "session_id": f.session.id,
        "objective": "Same key, different backend",
        "idempotency_key": key,
    });
    let view =
        serde_json::to_value(goal_api::goal_start(&codex, &f.session, &f.store).unwrap()).unwrap();
    assert_eq!(view["idempotent_replay"], false);

    let external = json!({
        "session_id": f.session.id,
        "objective": "Same key, different backend",
        "idempotency_key": key,
        "model_backend": "EXTERNAL",
    });
    let error = goal_api::goal_start(&external, &f.session, &f.store).unwrap_err();
    assert_eq!(error.code(), "IDEMPOTENCY_CONFLICT");

    // The original Codex goal is unchanged and replaying it stays a replay.
    let replay =
        serde_json::to_value(goal_api::goal_start(&codex, &f.session, &f.store).unwrap()).unwrap();
    assert_eq!(replay["idempotent_replay"], true);
    assert_eq!(replay["model_backend"], "CODEX");
}

// ---------------------------------------------------------------------------
// Durable WorkItem issuance, reload, and limits.
// ---------------------------------------------------------------------------

#[test]
fn issued_work_item_is_durable_and_reloaded_by_a_fresh_store() {
    let f = fixture("durable-item");
    let now = utc_now_rfc3339();
    let goal = external_goal(&f.session, &now);
    f.store.create_goal(&goal).unwrap();
    let item = planner_work_item(&goal, &now);
    let work_id = item.work_id().clone();
    let digest = item.request_digest().to_owned();
    let committed =
        commit_issued_work_item(&f.store, &f.session.id, goal.id(), goal.revision(), item).unwrap();
    assert_eq!(committed.revision(), goal.revision() + 1);

    // A completely fresh store on the same state root is the restart case: the
    // WorkItem must be recoverable from durable state alone.
    let restarted = TaskStore::with_state_root(f.root.join("state"));
    let reloaded = restarted.load_goal(&f.session.id, goal.id()).unwrap();
    let state = external_state(&reloaded).unwrap();
    let active = state.active_work().expect("work item survives restart");
    assert_eq!(active.work_id(), &work_id);
    assert_eq!(active.request_digest(), digest);
    assert_eq!(active.state(), WorkItemState::Pending);
    assert_eq!(active.role(), WorkRole::Planner);
    assert_eq!(state.issue_counter(), 1);
    assert_eq!(state.role_attempts(WorkRole::Planner), 1);
    // The same WorkItem is the only one: re-reading is idempotent by identity.
    assert_eq!(state.role_attempts(WorkRole::Planner), 1);
}

#[test]
fn a_second_active_work_item_is_refused_and_the_budget_is_not_consumed() {
    let f = fixture("one-active");
    let now = utc_now_rfc3339();
    let goal = external_goal(&f.session, &now);
    f.store.create_goal(&goal).unwrap();
    let first = planner_work_item(&goal, &now);
    let first_id = first.work_id().clone();
    let after_first =
        commit_issued_work_item(&f.store, &f.session.id, goal.id(), goal.revision(), first)
            .unwrap();

    let second = planner_work_item_with_counter(&after_first, &now, 2);
    let error = mutate_external(
        &f.store,
        &f.session.id,
        goal.id(),
        after_first.revision(),
        move |state, _now| state.record_issued(second),
    )
    .unwrap_err();
    assert!(matches!(error, OrchestratorError::CorruptGoal(_)));

    let reloaded = f.store.load_goal(&f.session.id, goal.id()).unwrap();
    let state = external_state(&reloaded).unwrap();
    assert_eq!(state.issue_counter(), 1);
    assert_eq!(state.active_work().unwrap().work_id(), &first_id);
    assert_eq!(MAX_ACTIVE_WORK_ITEMS_PER_GOAL, 1);
}

#[test]
fn stale_expected_revision_is_refused_without_a_partial_write() {
    let f = fixture("revision-cas");
    let now = utc_now_rfc3339();
    let goal = external_goal(&f.session, &now);
    f.store.create_goal(&goal).unwrap();
    let item = planner_work_item(&goal, &now);
    let error = commit_issued_work_item(
        &f.store,
        &f.session.id,
        goal.id(),
        goal.revision() + 7,
        item,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        OrchestratorError::RevisionConflict {
            expected: 8,
            actual: 1
        }
    ));
    let reloaded = f.store.load_goal(&f.session.id, goal.id()).unwrap();
    assert_eq!(reloaded.revision(), 1);
    assert!(external_state(&reloaded).unwrap().active_work().is_none());
    assert_eq!(external_state(&reloaded).unwrap().issue_counter(), 0);
}

#[test]
fn work_and_submission_ids_must_be_canonical_lowercase_uuids() {
    assert!(WorkId::parse("not-a-uuid").is_err());
    assert!(SubmissionId::parse("not-a-uuid").is_err());
    let canonical = Uuid::new_v4().to_string();
    assert!(WorkId::parse(&canonical).is_ok());
    assert!(SubmissionId::parse(&canonical).is_ok());
    assert!(WorkId::parse(&canonical.to_uppercase()).is_err());
    assert!(SubmissionId::parse(&canonical.to_uppercase()).is_err());
    // A UUID with braces or a different byte order is not canonical either.
    assert!(WorkId::parse(&format!("{{{canonical}}}")).is_err());
}

#[test]
fn work_item_construction_enforces_the_payload_and_digest_bounds() {
    let f = fixture("payload-bound");
    let now = utc_now_rfc3339();
    let goal = external_goal(&f.session, &now);
    let oversized = "x".repeat(MAX_REQUEST_PAYLOAD_BYTES + 1);
    let error = ModelWorkItem::pending(
        WorkId::new(),
        goal.session_id().to_owned(),
        goal.id().as_str().to_owned(),
        WorkRole::Planner,
        None,
        None,
        goal.revision(),
        0,
        sha256_hex(b"{}"),
        oversized,
        OUTPUT_CONTRACT_PLANNER_V1,
        1,
        &now,
    )
    .unwrap_err();
    assert!(matches!(error, OrchestratorError::CorruptGoal(_)));

    let bad_digest = ModelWorkItem::pending(
        WorkId::new(),
        goal.session_id().to_owned(),
        goal.id().as_str().to_owned(),
        WorkRole::Planner,
        None,
        None,
        goal.revision(),
        0,
        "deadbeef".to_owned(),
        "{}".to_owned(),
        OUTPUT_CONTRACT_PLANNER_V1,
        1,
        &now,
    );
    assert!(bad_digest.is_err());

    // The request digest is host-authored: identical canonical payloads produce
    // identical digests regardless of caller key order.
    let left = canonical_json(&json!({ "b": 1, "a": { "d": 2, "c": 3 } }));
    let right = canonical_json(&json!({ "a": { "c": 3, "d": 2 }, "b": 1 }));
    assert_eq!(left, right);
    assert_eq!(sha256_hex(left.as_bytes()), sha256_hex(right.as_bytes()));
}

#[test]
fn a_work_item_bound_to_another_goal_is_refused_by_goal_validation() {
    let f = fixture("cross-goal-item");
    let now = utc_now_rfc3339();
    let goal = external_goal(&f.session, &now);
    f.store.create_goal(&goal).unwrap();
    let foreign = ModelWorkItem::pending(
        WorkId::new(),
        goal.session_id().to_owned(),
        "99999999-9999-4999-8999-999999999999".to_owned(),
        WorkRole::Planner,
        None,
        None,
        goal.revision(),
        0,
        sha256_hex(b"{}"),
        "{}".to_owned(),
        OUTPUT_CONTRACT_PLANNER_V1,
        1,
        &now,
    )
    .unwrap();
    let error =
        commit_issued_work_item(&f.store, &f.session.id, goal.id(), goal.revision(), foreign)
            .unwrap_err();
    assert!(matches!(error, OrchestratorError::CorruptGoal(_)));
    let reloaded = f.store.load_goal(&f.session.id, goal.id()).unwrap();
    assert!(external_state(&reloaded).unwrap().active_work().is_none());
}

#[test]
fn codex_goals_cannot_carry_external_state() {
    let f = fixture("codex-cannot-gain");
    let now = utc_now_rfc3339();
    let goal = Goal::new(
        f.session.id.clone(),
        f.session.cwd.clone(),
        "Codex only",
        None,
        Vec::new(),
        Vec::new(),
        &now,
    )
    .unwrap();
    let mut goal = goal;
    assert!(external_state_mut(&mut goal).is_err());
    // A hand-built CODEX Goal that smuggles external state must fail
    // validation rather than decode as a weaker-but-valid record.
    let mut smuggled = serde_json::to_value(&goal).unwrap();
    smuggled["external_model"] = serde_json::to_value(ExternalModelState::new()).unwrap();
    let decoded: Goal = serde_json::from_value(smuggled).unwrap();
    assert!(
        decoded.validate().is_err(),
        "a CODEX Goal must never decode as valid while carrying external state"
    );
}

// ---------------------------------------------------------------------------
// Cancellation and crash/partial-write fail-closed behavior.
// ---------------------------------------------------------------------------

#[test]
fn cancellation_invalidates_the_active_work_item_in_the_same_commit() {
    let f = fixture("cancel-invalidates");
    let now = utc_now_rfc3339();
    let goal = external_goal(&f.session, &now);
    f.store.create_goal(&goal).unwrap();
    let item = planner_work_item(&goal, &now);
    let work_id = item.work_id().clone();
    let after_issue =
        commit_issued_work_item(&f.store, &f.session.id, goal.id(), goal.revision(), item).unwrap();
    let cancelled = f
        .store
        .mutate_goal_snapshot(
            &f.session.id,
            goal.id(),
            after_issue.revision(),
            |goal, _now| {
                cancel_active_work_in_place(goal, "GOAL_CANCELLED");
                Ok(())
            },
        )
        .unwrap();
    let state = external_state(&cancelled).unwrap();
    assert!(state.active_work().is_none());
    assert_eq!(state.work_history().len(), 1);
    assert_eq!(state.work_history()[0].work_id(), &work_id);
    assert_eq!(state.work_history()[0].state(), WorkItemState::Cancelled);
    // Cancellation never refunds budget, so a cancelled WorkItem cannot be
    // replayed into a fresh issuance loop.
    assert_eq!(state.issue_counter(), 1);
}

#[test]
fn a_failed_commit_leaves_no_issued_work_item() {
    let f = fixture("crash-before-replace");
    let now = utc_now_rfc3339();
    let goal = external_goal(&f.session, &now);
    f.store.create_goal(&goal).unwrap();

    let failing = TaskStore::with_fault(f.root.join("state"), FaultPoint::BeforeReplace);
    let item = planner_work_item(&goal, &now);
    let result = commit_issued_work_item(&failing, &f.session.id, goal.id(), goal.revision(), item);
    assert!(
        result.is_err(),
        "a commit that cannot replace the Goal document must fail"
    );

    // The durable document is exactly as it was: no issued counter, no WorkItem.
    let clean = TaskStore::with_state_root(f.root.join("state"));
    let reloaded = clean.load_goal(&f.session.id, goal.id()).unwrap();
    let state = external_state(&reloaded).unwrap();
    assert!(state.active_work().is_none());
    assert_eq!(state.issue_counter(), 0);
    assert_eq!(state.role_attempts(WorkRole::Planner), 0);
    assert_eq!(reloaded.revision(), 1);
}

#[test]
fn a_crash_before_the_temp_write_leaves_the_previous_revision_intact() {
    let f = fixture("crash-before-temp");
    let now = utc_now_rfc3339();
    let goal = external_goal(&f.session, &now);
    f.store.create_goal(&goal).unwrap();
    let failing = TaskStore::with_fault(f.root.join("state"), FaultPoint::BeforeTempWrite);
    let item = planner_work_item(&goal, &now);
    assert!(
        commit_issued_work_item(&failing, &f.session.id, goal.id(), goal.revision(), item).is_err()
    );
    let clean = TaskStore::with_state_root(f.root.join("state"));
    let reloaded = clean.load_goal(&f.session.id, goal.id()).unwrap();
    assert_eq!(reloaded.revision(), 1);
    assert!(external_state(&reloaded).unwrap().active_work().is_none());
}

#[test]
fn submission_receipts_are_durable_and_replayable_from_a_fresh_store() {
    let f = fixture("receipt-durable");
    let now = utc_now_rfc3339();
    let goal = external_goal(&f.session, &now);
    f.store.create_goal(&goal).unwrap();
    let item = planner_work_item(&goal, &now);
    let work_id = item.work_id().clone();
    let after_issue =
        commit_issued_work_item(&f.store, &f.session.id, goal.id(), goal.revision(), item).unwrap();
    let submission = SubmissionId::parse(&Uuid::new_v4().to_string()).unwrap();
    let payload = payload_digest(&json!({ "summary": "accepted shape" }));
    let receipt = SubmissionReceipt::new(
        work_id,
        submission.clone(),
        payload.clone(),
        SubmissionDecision::AcceptedForHostProcessing,
        None,
        "OK",
        None,
        &now,
    );
    let committed = commit_submission(
        &f.store,
        &f.session.id,
        goal.id(),
        after_issue.revision(),
        receipt,
    )
    .unwrap();
    assert_eq!(committed.revision(), after_issue.revision() + 1);

    let restarted = TaskStore::with_state_root(f.root.join("state"));
    let reloaded = restarted.load_goal(&f.session.id, goal.id()).unwrap();
    let state = external_state(&reloaded).unwrap();
    let stored = state
        .find_receipt(&submission)
        .expect("receipt survives restart");
    assert_eq!(stored.payload_sha256(), payload);
    assert_eq!(
        stored.decision(),
        SubmissionDecision::AcceptedForHostProcessing
    );
    assert_eq!(stored.committed_revision(), None);
    assert_eq!(
        state.active_work().unwrap().state(),
        WorkItemState::Submitted
    );
}

// ---------------------------------------------------------------------------
// GoalStore compatibility: additive read, fail-closed downgrade.
// ---------------------------------------------------------------------------

#[test]
fn a_pre_e3_reader_can_read_codex_goals_and_refuses_external_goals() {
    let f = fixture("downgrade");
    let now = utc_now_rfc3339();
    let codex = Goal::new(
        f.session.id.clone(),
        f.session.cwd.clone(),
        "Codex document",
        None,
        Vec::new(),
        Vec::new(),
        &now,
    )
    .unwrap();
    let codex_document = serde_json::to_value(&codex).unwrap();
    let external = codex.with_creation_backend(ModelBackend::External);
    let external_document = serde_json::to_value(&external).unwrap();

    // 1. A CODEX document carries no key a pre-E3 reader would not know, so an
    //    older binary can still read and rewrite it unchanged.
    let codex_object = codex_document.as_object().unwrap();
    let external_object = external_document.as_object().unwrap();
    let mut added: Vec<&str> = external_object
        .keys()
        .filter(|key| !codex_object.contains_key(*key))
        .map(String::as_str)
        .collect();
    added.sort_unstable();
    assert_eq!(added, vec!["external_model", "model_backend"]);
    let reparsed: Goal = serde_json::from_value(codex_document.clone()).unwrap();
    assert_eq!(reparsed.model_backend(), ModelBackend::Codex);

    // 2. The durable shape is strict (`deny_unknown_fields`). That is exactly how
    //    a reader that does not declare the two additive fields treats them, so
    //    a downgrade refuses an EXTERNAL document instead of silently dropping
    //    its durable state. The probe key stands in for any field such a reader
    //    does not know.
    let mut unknown = codex_document.clone();
    unknown
        .as_object_mut()
        .unwrap()
        .insert("pre_e3_unknown_probe".to_owned(), Value::Bool(true));
    let strict_error = serde_json::from_value::<Goal>(unknown).unwrap_err();
    assert!(
        strict_error.to_string().contains("unknown field"),
        "the durable shape must refuse an unmodelled authoritative field: {strict_error}"
    );

    // 3. A lossy downgrade that drops only the mode key must not yield a valid
    //    CODEX Goal, because the external record would then be orphaned.
    let mut stripped = external_document;
    stripped.as_object_mut().unwrap().remove("model_backend");
    let stripped_goal: Goal = serde_json::from_value(stripped).unwrap();
    assert!(stripped_goal.validate().is_err());
}

#[test]
fn an_external_goal_round_trips_through_the_current_store_unchanged() {
    let f = fixture("round-trip");
    let now = utc_now_rfc3339();
    let goal = external_goal(&f.session, &now);
    f.store.create_goal(&goal).unwrap();
    let loaded = f.store.load_goal(&f.session.id, goal.id()).unwrap();
    assert_eq!(
        serde_json::to_value(&loaded).unwrap(),
        serde_json::to_value(&goal).unwrap()
    );
    // A re-read must not resurrect external state on a Goal that has none.
    let codex = Goal::new(
        f.session.id.clone(),
        f.session.cwd.clone(),
        "unplanned",
        None,
        Vec::new(),
        Vec::new(),
        &now,
    )
    .unwrap();
    assert_eq!(codex.model_backend(), ModelBackend::Codex);
    assert_eq!(codex.status(), GoalStatus::Planning);
    assert_eq!(codex.external_model(), None);
}

#[test]
fn the_work_history_bound_and_counter_are_enforced_on_load() {
    let mut state = ExternalModelState::new();
    // Synthetic over-budget counter: the loader must refuse it, not truncate.
    let mut document = serde_json::to_value(&state).unwrap();
    document["issue_counter"] = Value::from(MAX_MODEL_WORK_ITEMS_PER_GOAL + 1);
    let decoded: ExternalModelState = serde_json::from_value(document).unwrap();
    state = decoded;
    assert!(
        state
            .validate_for_goal("11111111-1111-4111-8111-111111111111", "session")
            .is_err()
    );
}
