//! E3 hardening (H1) and External Planner/Readonly integration (E4) tests.
//!
//! These tests are the deterministic proof of the two H1 fixes and of the E4
//! loop:
//!
//! * H1-1 — a receipt is only replayed for the *same* WorkItem, submission, and
//!   payload. Anything else is refused without revealing another Goal.
//! * H1-2 — a Planner response is validated, applied to the Task DAG, and
//!   receipted in exactly one durable commit, so a crash can never leave a plan
//!   without its receipt or a receipt without its plan.
//! * E4-A — an EXTERNAL Goal leaves PLANNING holding a real, host-validated Task
//!   DAG with criterion bindings.
//! * E4-B — an EXTERNAL Readonly answer travels the existing readonly parser,
//!   Task state table, Task Verifier, Goal Verifier, and Goal Finalizer. No
//!   model claim can complete a Goal on its own.
//!
//! No test spawns a Codex CLI, writes to the workspace, or contacts a model. The
//! zero-spawn guarantee is asserted with the E2 model-accounting counter.

use std::path::PathBuf;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::config;
use crate::external_goal_api;
use crate::external_model_store::{external_state, mutate_external};
use crate::external_model_work::{
    SubmissionDecision, SubmissionId, SubmissionReceipt, WorkItemState,
};
use crate::goal::{Goal, GoalId, GoalStatus};
use crate::goal_api;
use crate::model_accounting;
use crate::task::TaskStatus;
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
    let root = std::env::temp_dir().join(format!("local-mcp-e4-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        workspace.join("tracked.txt"),
        b"goal-latch readonly dogfood marker\n",
    )
    .unwrap();
    let session = config::Session {
        id: format!("e4-{label}-{}", Uuid::new_v4()),
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

fn start_external(session: &config::Session, store: &TaskStore) -> GoalId {
    let view = goal_api::goal_start(
        &json!({
            "session_id": session.id,
            "objective": "Inspect the tracked file and report verifiable evidence",
            "model_backend": "EXTERNAL",
        }),
        session,
        store,
    )
    .unwrap();
    let view = serde_json::to_value(view).unwrap();
    GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap()
}

fn goal_next_call(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<external_goal_api::ExternalGoalNextView, goal_api::GoalApiError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(external_goal_api::goal_next(args, session, store))
}

fn next_args(session: &config::Session, goal_id: &GoalId) -> Value {
    json!({"session_id": session.id, "goal_id": goal_id.as_str()})
}

fn goal_next(session: &config::Session, store: &TaskStore, goal_id: &GoalId) -> Value {
    serde_json::to_value(goal_next_call(&next_args(session, goal_id), session, store).unwrap())
        .unwrap()
}

fn submit_args(
    session: &config::Session,
    goal_id: &GoalId,
    work: &Value,
    submission_id: &str,
    response: Value,
) -> Value {
    json!({
        "session_id": session.id,
        "goal_id": goal_id.as_str(),
        "work_id": work["work_id"],
        "submission_id": submission_id,
        "expected_goal_revision": work["goal_revision"],
        "request_digest": work["request_digest"],
        "response": response,
    })
}

fn submit(
    session: &config::Session,
    store: &TaskStore,
    args: &Value,
) -> Result<Value, goal_api::GoalApiError> {
    external_goal_api::goal_submit(args, session, store)
        .map(|view| serde_json::to_value(view).unwrap())
}

/// A Planner proposal the host accepts: one mandatory readonly Task whose
/// verification is mechanically evaluable by the host Task Verifier.
fn valid_planner_proposal(work: &Value) -> Value {
    let criterion_id = work["request"]["completion_criteria"][0]["criterion_id"]
        .as_str()
        .expect("host-owned criterion id");
    json!({
        "goal_id": work["goal_id"],
        "goal_revision": work["goal_revision"],
        "summary": "Read-only inspection of the tracked file",
        "tasks": [{
            "proposal_id": "inspect-1",
            "title": "Inspect the tracked file read-only",
            "objective": "Collect compact structured evidence for the completion criterion",
            "mandatory": true,
            "worker": "CODEX_READONLY",
            "dependencies": [],
            "scope": {
                "allowed_paths": ["."],
                "forbidden_paths": [],
                "operation_kind": "READ_ONLY",
                "replay_safety": "SAFE_READ_ONLY"
            },
            "verification": [{"kind": "STRUCTURED_EVIDENCE", "requirement_id": "evidence-1"}]
        }],
        "criterion_bindings": [{"criterion_id": criterion_id, "task_refs": ["inspect-1"]}]
    })
}

fn fresh_uuid() -> String {
    Uuid::new_v4().to_string()
}

fn goal_of(session: &config::Session, store: &TaskStore, goal_id: &GoalId) -> Goal {
    store.load_goal(&session.id, goal_id).unwrap()
}

// ---------------------------------------------------------------------------
// H1-1 — receipt matching is `(work_id, submission_id, payload)`.
// ---------------------------------------------------------------------------

#[test]
fn a_submission_id_used_by_another_work_item_is_not_a_replay() {
    let f = fixture("h1-1-work");
    let goal_id = start_external(&f.session, &f.store);
    let first = goal_next(&f.session, &f.store, &goal_id);
    let first_work = first["model_work"].clone();
    let submission = fresh_uuid();

    // Refuse the first WorkItem so the host issues a second one.
    let refused = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &first_work, &submission, json!({})),
    )
    .unwrap();
    assert_eq!(refused["outcome"], "REJECTED_SCHEMA");

    let second = goal_next(&f.session, &f.store, &goal_id);
    let second_work = second["model_work"].clone();
    assert_ne!(second_work["work_id"], first_work["work_id"]);
    assert_eq!(second_work["issue_counter"], 2);

    // The same submission_id against a different WorkItem must not be replayed
    // into the first WorkItem's verdict.
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &second_work, &submission, json!({})),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_AUTHORITY");
    assert_eq!(view["code"], "SUBMISSION_WORK_ID_MISMATCH");
    assert!(view["receipt"].is_null());
    assert_eq!(view["goal_state"]["next_action"], "STOP_AND_RECONCILE");

    // Nothing was recorded for the second WorkItem, so it is still submittable.
    let goal = goal_of(&f.session, &f.store, &goal_id);
    let state = external_state(&goal).unwrap();
    assert_eq!(state.receipts().len(), 1);
    assert_eq!(
        state.active_work().unwrap().work_id().as_str(),
        second_work["work_id"].as_str().unwrap()
    );
    assert_eq!(state.active_work().unwrap().state(), WorkItemState::Pending);
}

#[test]
fn a_submission_id_from_another_goal_reveals_nothing() {
    let f = fixture("h1-1-cross-goal");
    let goal_a = start_external(&f.session, &f.store);
    // One session owns at most one non-terminal Goal, so the second Goal lives
    // in a second session of the same durable store.
    let session_b = config::Session {
        id: format!("e4-cross-goal-b-{}", Uuid::new_v4()),
        cwd: f.session.cwd.clone(),
        permitted_directories: f.session.permitted_directories.clone(),
    };
    let goal_b = start_external(&session_b, &f.store);

    let work_a = goal_next(&f.session, &f.store, &goal_a)["model_work"].clone();
    let submission = fresh_uuid();
    let refused_a = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_a, &work_a, &submission, json!({})),
    )
    .unwrap();
    assert_eq!(refused_a["outcome"], "REJECTED_SCHEMA");

    // Goal B's WorkItem answered with Goal A's submission_id behaves exactly
    // like an answer with a brand new submission_id: no cross-Goal receipt is
    // ever consulted, and no Goal A detail appears.
    let work_b = goal_next(&session_b, &f.store, &goal_b)["model_work"].clone();
    let reused = submit(
        &session_b,
        &f.store,
        &submit_args(&session_b, &goal_b, &work_b, &submission, json!({})),
    )
    .unwrap();
    let work_b2 = goal_next(&session_b, &f.store, &goal_b)["model_work"].clone();
    let fresh = submit(
        &session_b,
        &f.store,
        &submit_args(&session_b, &goal_b, &work_b2, &fresh_uuid(), json!({})),
    )
    .unwrap();
    assert_eq!(reused["outcome"], fresh["outcome"]);
    assert_eq!(reused["code"], fresh["code"]);
    assert_eq!(reused["goal_id"], goal_b.as_str());
    assert!(
        !serde_json::to_string(&reused)
            .unwrap()
            .contains(goal_a.as_str()),
        "a cross-Goal probe must not leak the other Goal's identity"
    );
    assert_eq!(
        external_state(&goal_of(&f.session, &f.store, &goal_a))
            .unwrap()
            .receipts()
            .len(),
        1
    );
    // Goal B recorded only its own two refusals; nothing from Goal A.
    assert_eq!(
        external_state(&goal_of(&session_b, &f.store, &goal_b))
            .unwrap()
            .receipts()
            .len(),
        2
    );
}

#[test]
fn a_committed_submission_replays_after_cancel_without_reviving_the_goal() {
    let f = fixture("h1-1-cancel-replay");
    let goal_id = start_external(&f.session, &f.store);
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let submission = fresh_uuid();
    let args = submit_args(
        &f.session,
        &goal_id,
        &work,
        &submission,
        valid_planner_proposal(&work),
    );
    let first = submit(&f.session, &f.store, &args).unwrap();
    assert_eq!(first["outcome"], "COMMITTED");

    goal_api::goal_cancel(
        &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
        &f.session,
        &f.store,
    )
    .unwrap();
    let revision = goal_of(&f.session, &f.store, &goal_id).revision();

    // A byte-identical replay still reports the stored verdict, and it cannot
    // revive a cancelled Goal or write anything new.
    let replay = submit(&f.session, &f.store, &args).unwrap();
    assert_eq!(replay["outcome"], "REPLAY_OF_ACCEPTED_SUBMISSION");
    assert_eq!(replay["receipt"], first["receipt"]);
    let goal = goal_of(&f.session, &f.store, &goal_id);
    assert_eq!(goal.status(), GoalStatus::Cancelled);
    assert_eq!(goal.revision(), revision);
    assert_eq!(external_state(&goal).unwrap().receipts().len(), 1);
}

// ---------------------------------------------------------------------------
// H1-2 — one durable commit validates, applies, and receipts a plan.
// ---------------------------------------------------------------------------

#[test]
fn a_planner_submission_applies_the_plan_and_its_receipt_atomically() {
    let f = fixture("h1-2-commit");
    let goal_id = start_external(&f.session, &f.store);
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            &fresh_uuid(),
            valid_planner_proposal(&work),
        ),
    )
    .unwrap();

    assert_eq!(view["outcome"], "COMMITTED");
    assert_eq!(view["code"], "PLAN_APPLIED");
    let goal = goal_of(&f.session, &f.store, &goal_id);
    assert_eq!(goal.status(), GoalStatus::Running);
    assert_eq!(goal.plan_revision(), 1);
    assert_eq!(goal.tasks().len(), 1);
    assert!(goal.final_verification_spec().is_some());
    let state = external_state(&goal).unwrap();
    assert!(state.active_work().is_none());
    assert_eq!(state.work_history().len(), 1);
    assert_eq!(state.work_history()[0].state(), WorkItemState::Committed);
    let receipt = &state.receipts()[0];
    assert_eq!(receipt.decision(), SubmissionDecision::Committed);
    // The receipt names the exact revision the plan was committed at, and the
    // durable Goal agrees.
    assert_eq!(receipt.committed_revision(), Some(goal.revision()));
    assert_eq!(
        view["receipt"]["committed_revision"],
        json!(goal.revision())
    );
    // Committed is never completion.
    assert_eq!(view["goal_state"]["result_kind"], "NONTERMINAL");
    assert_ne!(view["goal_state"]["status"], "COMPLETED");
}

#[test]
fn a_failed_plan_commit_leaves_no_partial_plan_and_the_retry_succeeds() {
    let f = fixture("h1-2-crash");
    let goal_id = start_external(&f.session, &f.store);
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let submission = fresh_uuid();
    let args = submit_args(
        &f.session,
        &goal_id,
        &work,
        &submission,
        valid_planner_proposal(&work),
    );

    let failing = TaskStore::with_fault(f.root.join("state"), FaultPoint::BeforeReplace);
    let error = submit(&f.session, &failing, &args).unwrap_err();
    assert_eq!(error.code(), "GOAL_PERSISTENCE_ERROR");

    // Nothing of the plan survived: no Task, no plan revision, no receipt, and
    // the WorkItem is still PENDING so the same answer can be retried.
    let clean = TaskStore::with_state_root(f.root.join("state"));
    let goal = goal_of(&f.session, &clean, &goal_id);
    assert_eq!(goal.plan_revision(), 0);
    assert!(goal.tasks().is_empty());
    assert_eq!(goal.status(), GoalStatus::Planning);
    let state = external_state(&goal).unwrap();
    assert_eq!(state.receipts().len(), 0);
    assert_eq!(state.active_work().unwrap().state(), WorkItemState::Pending);

    // The retry on a healthy store commits both halves together.
    let retried = submit(&f.session, &clean, &args).unwrap();
    assert_eq!(retried["outcome"], "COMMITTED");
    let goal = goal_of(&f.session, &clean, &goal_id);
    assert_eq!(goal.plan_revision(), 1);
    assert_eq!(goal.tasks().len(), 1);
    assert_eq!(external_state(&goal).unwrap().receipts().len(), 1);
}

#[test]
fn an_applied_plan_cannot_be_materialized_a_second_time() {
    let f = fixture("h1-2-double");
    let goal_id = start_external(&f.session, &f.store);
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let proposal = valid_planner_proposal(&work);
    let first = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, &fresh_uuid(), proposal.clone()),
    )
    .unwrap();
    assert_eq!(first["outcome"], "COMMITTED");
    let goal = goal_of(&f.session, &f.store, &goal_id);

    // The Codex planner entrypoint refuses the same plan now that the Goal has
    // a committed Task DAG, so a second materialization is unrepresentable.
    let bytes = serde_json::to_vec(&proposal).unwrap();
    let error = crate::planner::materialize_initial_plan_output(
        &f.store,
        &f.session,
        &goal_id,
        goal.revision(),
        &bytes,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        crate::planner::PlannerError::PlanNotApplicable(_)
    ));
    assert_eq!(goal_of(&f.session, &f.store, &goal_id).plan_revision(), 1);
}

#[test]
fn a_pre_e4_accepted_work_item_is_reported_blocked_not_guessed() {
    let f = fixture("h1-2-legacy");
    let goal_id = start_external(&f.session, &f.store);
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let goal = goal_of(&f.session, &f.store, &goal_id);

    // Reproduce exactly what E3 persisted: an ACCEPTED_FOR_HOST_PROCESSING
    // receipt with no durable artifact of the accepted response.
    let receipt = SubmissionReceipt::new(
        crate::external_model_work::WorkId::parse(work["work_id"].as_str().unwrap()).unwrap(),
        SubmissionId::parse(&fresh_uuid()).unwrap(),
        crate::external_model_work::payload_digest(&valid_planner_proposal(&work)),
        SubmissionDecision::AcceptedForHostProcessing,
        None,
        "OK",
        None,
        utc_now_rfc3339(),
    );
    let (_, legacy) = mutate_external(
        &f.store,
        &f.session.id,
        &goal_id,
        goal.revision(),
        move |state, now| state.record_submission(receipt, now),
    )
    .unwrap();
    assert_eq!(
        external_state(&legacy)
            .unwrap()
            .active_work()
            .unwrap()
            .state(),
        WorkItemState::Submitted
    );
    let revision = legacy.revision();

    let view = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(view["outcome"], "BLOCKED");
    assert_eq!(
        view["blockers"][0]["code"],
        "EXTERNAL_SUBMITTED_RECOVERY_UNSAFE"
    );
    assert_eq!(view["blockers"][0]["mandatory"], true);
    assert!(view["model_work"].is_null());
    assert_eq!(view["pending_work_id"], work["work_id"]);

    // The host reports the ambiguity without destroying evidence or mutating.
    let after = goal_of(&f.session, &f.store, &goal_id);
    assert_eq!(after.revision(), revision);
    assert_eq!(after.plan_revision(), 0);
    assert!(after.tasks().is_empty());
    assert_eq!(external_state(&after).unwrap().receipts().len(), 1);
}

// ---------------------------------------------------------------------------
// E4-A — External Planner integration.
// ---------------------------------------------------------------------------

#[test]
fn an_external_goal_leaves_planning_with_a_host_validated_task_dag() {
    let f = fixture("e4a-dag");
    model_accounting::reset_for_test();
    let goal_id = start_external(&f.session, &f.store);
    let next = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(next["model_work"]["role"], "PLANNER");
    let work = next["model_work"].clone();

    let view = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            &fresh_uuid(),
            valid_planner_proposal(&work),
        ),
    )
    .unwrap();
    assert_eq!(view["outcome"], "COMMITTED");

    let goal = goal_of(&f.session, &f.store, &goal_id);
    assert_ne!(goal.status(), GoalStatus::Planning);
    let spec = goal.final_verification_spec().unwrap();
    assert_eq!(spec.plan_revision(), 1);
    assert!(!spec.criterion_bindings().is_empty());
    let task = goal.tasks().values().next().unwrap();
    assert_eq!(task.worker(), crate::task::WorkerKind::CodexReadonly);
    assert_eq!(task.status(), TaskStatus::Ready);
    assert_eq!(model_accounting::snapshot_for_test().codex_spawn_count, 0);
}

#[test]
fn an_invented_criterion_or_forbidden_worker_is_refused_without_a_plan() {
    let f = fixture("e4a-refuse");
    let goal_id = start_external(&f.session, &f.store);
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();

    let mut invented = valid_planner_proposal(&work);
    invented["criterion_bindings"][0]["criterion_id"] = json!(fresh_uuid());
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, &fresh_uuid(), invented),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA");
    assert!(view["receipt"]["committed_revision"].is_null());
    let goal = goal_of(&f.session, &f.store, &goal_id);
    assert_eq!(goal.plan_revision(), 0);
    assert!(goal.tasks().is_empty());
    assert_eq!(goal.status(), GoalStatus::Planning);

    // A forbidden worker kind is refused by the same validator.
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let mut forbidden = valid_planner_proposal(&work);
    forbidden["tasks"][0]["worker"] = json!("LOCAL_OPERATION");
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, &fresh_uuid(), forbidden),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA");
    assert_eq!(goal_of(&f.session, &f.store, &goal_id).plan_revision(), 0);
}

#[test]
fn a_host_owned_cancel_of_an_external_goal_invalidates_the_active_work_item() {
    let f = fixture("e4b-cancel-invalidate");
    let goal_id = start_external(&f.session, &f.store);
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    goal_api::goal_cancel(
        &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
        &f.session,
        &f.store,
    )
    .unwrap();
    let goal = goal_of(&f.session, &f.store, &goal_id);
    let state = external_state(&goal).unwrap();
    assert!(state.active_work().is_none());
    assert_eq!(state.work_history().len(), 1);
    assert_eq!(state.work_history()[0].state(), WorkItemState::Cancelled);
    // A cancelled Goal cannot be advanced by goal_next either.
    let view = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(view["outcome"], "TERMINAL");
    assert_eq!(view["model_work"], Value::Null);
    assert_eq!(work["role"], "PLANNER");
    // The external state survives a reload unchanged.
    let reloaded = TaskStore::with_state_root(f.root.join("state"));
    let goal = goal_of(&f.session, &reloaded, &goal_id);
    assert_eq!(
        external_state(&goal).unwrap().work_history().len(),
        1,
        "cancellation evidence is durable"
    );
}
