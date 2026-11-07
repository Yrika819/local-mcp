//! E3-B — `goal_next` / `goal_submit` contract tests.
//!
//! These tests exercise the API surface directly with an isolated temp store:
//! issuance, re-issue identity, the bounded host-step contract, strict
//! validation through the *existing* Planner validator, receipt idempotency and
//! conflict handling, and every refusal the acceptance list names.
//!
//! No test spawns a process, starts a Codex CLI, applies a plan, or touches a
//! repository. The Codex zero-spawn guarantee is asserted with the E2 model
//! accounting counter, and reinforced by a source-level guard that the external
//! modules never name the Codex transport types.

use std::path::PathBuf;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::config;
use crate::external_goal_api;
use crate::external_model_store::external_state;
use crate::external_model_work::{
    MAX_HOST_STEPS_PER_CALL, MAX_MODEL_RESPONSE_BYTES, MAX_ROLE_ATTEMPTS,
};
use crate::goal::GoalId;
use crate::goal_api;
use crate::model_accounting;
use crate::task_store::TaskStore;

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
    let root = std::env::temp_dir().join(format!("local-mcp-e3b-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let session = config::Session {
        id: format!("e3b-{label}-{}", Uuid::new_v4()),
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

fn start_goal(session: &config::Session, store: &TaskStore, backend: Option<&str>) -> Value {
    let mut args = json!({
        "session_id": session.id,
        "objective": "Produce a verified external model plan",
    });
    if let Some(backend) = backend {
        args["model_backend"] = Value::String(backend.to_owned());
    }
    serde_json::to_value(goal_api::goal_start(&args, session, store).unwrap()).unwrap()
}

fn goal_id_of(view: &Value) -> GoalId {
    GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap()
}

fn next_args(session: &config::Session, goal_id: &GoalId) -> Value {
    json!({"session_id": session.id, "goal_id": goal_id.as_str()})
}

/// `goal_next` is async because its host steps await host observations
/// (Task Verifier, Goal Verifier, Goal Finalizer).
fn goal_next_call(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<external_goal_api::ExternalGoalNextView, goal_api::GoalApiError> {
    external_goal_api::goal_next(args, session, store)
}

fn goal_next(session: &config::Session, store: &TaskStore, goal_id: &GoalId) -> Value {
    let args = next_args(session, goal_id);
    serde_json::to_value(goal_next_call(&args, session, store).unwrap()).unwrap()
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

fn goal_submit(
    session: &config::Session,
    store: &TaskStore,
    args: &Value,
) -> Result<Value, String> {
    match external_goal_api::goal_submit(args, session, store) {
        Ok(view) => Ok(serde_json::to_value(view).unwrap()),
        Err(error) => Err(error.code().to_owned()),
    }
}

/// Build a Planner proposal that the host validator accepts, derived from the
/// exact request the host issued.
fn valid_planner_proposal(model_work: &Value) -> Value {
    let request = &model_work["request"];
    let criterion_id = request["completion_criteria"][0]["criterion_id"]
        .as_str()
        .expect("the host request carries a host-owned criterion id");
    json!({
        "goal_id": model_work["goal_id"],
        "goal_revision": model_work["goal_revision"],
        "summary": "Read-only inspection of the requested evidence",
        "tasks": [{
            "proposal_id": "inspect-1",
            "title": "Inspect the workspace read-only",
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

// ---------------------------------------------------------------------------
// goal_next.
// ---------------------------------------------------------------------------

#[test]
fn goal_next_issues_one_bounded_planner_work_item_without_codex() {
    let f = fixture("issue");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    model_accounting::reset_for_test();

    let view = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(view["outcome"], "MODEL_REQUIRED");
    assert_eq!(view["next_action"], "RESPOND_WITH_GOAL_SUBMIT");
    assert_eq!(view["execution_wait"], "WAITING_FOR_EXTERNAL_MODEL");
    assert_eq!(view["host_steps_executed"], 1);
    assert_eq!(view["max_host_steps"], MAX_HOST_STEPS_PER_CALL);
    let work = &view["model_work"];
    assert_eq!(work["role"], "PLANNER");
    assert_eq!(work["output_contract"], "planner-proposal-v1");
    assert_eq!(work["state"], "PENDING");
    assert_eq!(work["issue_counter"], 1);
    assert_eq!(work["goal_id"], goal_id.as_str());
    assert_eq!(
        work["response_required_fields"],
        json!([
            "goal_id",
            "goal_revision",
            "summary",
            "tasks",
            "criterion_bindings"
        ])
    );
    // The host request is delivered, including the host-owned criterion IDs.
    assert!(work["request"]["completion_criteria"].is_array());
    assert!(work["request"]["criterion_id"].is_null());
    assert_eq!(
        work["request"]["completion_criteria"][0]["criterion_id"]
            .as_str()
            .unwrap()
            .len(),
        36
    );
    assert_eq!(work["request_digest"].as_str().unwrap().len(), 64);
    // The WorkItem was committed before it was reported.
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(goal.revision(), 2);
    assert_eq!(
        external_state(&goal)
            .unwrap()
            .active_work()
            .unwrap()
            .work_id()
            .as_str(),
        work["work_id"].as_str().unwrap()
    );
    // Zero Codex spawns: the external path never reaches a transport.
    assert_eq!(model_accounting::snapshot_for_test().codex_spawn_count, 0);
}

#[test]
fn goal_next_repeats_the_same_work_item_and_digest_without_advancing() {
    let f = fixture("repeat");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let first = goal_next(&f.session, &f.store, &goal_id);
    let second = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(second["outcome"], "MODEL_REQUIRED");
    assert_eq!(
        second["model_work"]["work_id"],
        first["model_work"]["work_id"]
    );
    assert_eq!(
        second["model_work"]["request_digest"],
        first["model_work"]["request_digest"]
    );
    assert_eq!(
        second["model_work"]["goal_revision"],
        first["model_work"]["goal_revision"]
    );
    assert_eq!(second["host_steps_executed"], 0);
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(
        goal.revision(),
        2,
        "re-issuing never renumbers or re-commits"
    );
    assert_eq!(external_state(&goal).unwrap().issue_counter(), 1);
}

#[test]
fn goal_next_bounds_the_host_step_budget() {
    let f = fixture("steps");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let mut args = next_args(&f.session, &goal_id);
    args["max_host_steps"] = Value::from(0);
    let error = goal_next_call(&args, &f.session, &f.store).unwrap_err();
    assert_eq!(error.code(), "INVALID_ARGUMENT");

    let mut args = next_args(&f.session, &goal_id);
    args["max_host_steps"] = Value::from(MAX_HOST_STEPS_PER_CALL + 1);
    let error = goal_next_call(&args, &f.session, &f.store).unwrap_err();
    assert_eq!(error.code(), "HOST_STEP_BUDGET_EXCEEDED");

    // An unknown field is refused rather than ignored.
    let mut args = next_args(&f.session, &goal_id);
    args["role"] = Value::from("PLANNER");
    let error = goal_next_call(&args, &f.session, &f.store).unwrap_err();
    assert_eq!(error.code(), "INVALID_ARGUMENT");
}

#[test]
fn goal_next_on_a_codex_goal_is_a_typed_no_action() {
    let f = fixture("codex-next");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, None));
    let view = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(view["outcome"], "NO_ACTION_WITH_REASON");
    assert_eq!(view["no_action_reason"], "MODEL_BACKEND_CODEX");
    assert_eq!(view["execution_wait"], "NONE");
    assert!(view["model_work"].is_null());
    assert!(view["budget"].is_null());
}

#[test]
fn goal_next_on_a_cancelled_goal_reports_terminal_without_a_model_request() {
    let f = fixture("cancelled-next");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let _ = goal_next(&f.session, &f.store, &goal_id);
    goal_api::goal_cancel(
        &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
        &f.session,
        &f.store,
    )
    .unwrap();
    let view = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(view["outcome"], "TERMINAL");
    assert_eq!(view["result_kind"], "CANCELLED");
    assert_eq!(view["next_action"], "OBSERVE_TERMINAL_STATE");
    // Cancellation invalidated the outstanding WorkItem in the same commit.
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    assert!(external_state(&goal).unwrap().active_work().is_none());
}

// ---------------------------------------------------------------------------
// goal_submit: acceptance, validation, idempotency, refusals.
// ---------------------------------------------------------------------------

#[test]
fn goal_submit_accepts_a_host_valid_planner_proposal_and_commits_it() {
    let f = fixture("accept");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = &next["model_work"];
    let proposal = valid_planner_proposal(work);
    let submission = fresh_uuid();

    let view = goal_submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, work, &submission, proposal),
    )
    .unwrap();
    // H1-2 / E4-A: validation, Task DAG application, and the receipt are one
    // durable commit, so the verdict is COMMITTED rather than merely accepted.
    assert_eq!(view["outcome"], "COMMITTED");
    assert_eq!(view["code"], "PLAN_APPLIED");
    assert_eq!(view["receipt"]["decision"], "COMMITTED");
    assert!(view["receipt"]["committed_revision"].is_number());
    assert_eq!(view["goal_state"]["result_kind"], "NONTERMINAL");
    assert_eq!(view["goal_state"]["execution_wait"], "NONE");

    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    let state = external_state(&goal).unwrap();
    // Committed is applied, and still not completed.
    assert_eq!(goal.plan_revision(), 1);
    assert_eq!(goal.tasks().len(), 1);
    assert!(!goal.status().is_terminal());
    assert_eq!(state.receipts().len(), 1);
    assert!(state.active_work().is_none());
    assert_eq!(
        state.work_history()[0].state().as_str(),
        "COMMITTED",
        "the WorkItem is committed, not left active"
    );
    assert_eq!(
        view["receipt"]["committed_revision"].as_u64(),
        Some(goal.revision())
    );
}

#[test]
fn goal_submit_rejects_an_invented_criterion_id_through_the_host_validator() {
    let f = fixture("invented-criterion");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = &next["model_work"];
    let mut proposal = valid_planner_proposal(work);
    const MARKER: &str = "MODEL_PROSE_MUST_NOT_PERSIST";
    proposal["criterion_bindings"][0]["criterion_id"] = Value::String(fresh_uuid());
    proposal["summary"] = Value::String(format!("invented criterion {MARKER}"));

    let view = goal_submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, work, &fresh_uuid(), proposal),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA");
    assert_eq!(view["code"], "PROPOSAL_SCHEMA_VIOLATION");
    assert_eq!(view["receipt"]["decision"], "REJECTED_SCHEMA");
    // The refusal never carries model prose, and durable state gained only the
    // receipt plus the WorkItem rejection.
    let rendered = serde_json::to_string(&view).unwrap();
    assert!(
        !rendered.contains(MARKER),
        "receipt detail leaked model text"
    );
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(goal.plan_revision(), 0);
    assert!(goal.tasks().is_empty());
    assert!(external_state(&goal).unwrap().active_work().is_none());
}

#[test]
fn goal_submit_bounds_the_response_size() {
    let f = fixture("oversize");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = &next["model_work"];
    let oversized =
        json!({"goal_id": work["goal_id"], "blob": "x".repeat(MAX_MODEL_RESPONSE_BYTES + 1)});
    let view = goal_submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, work, &fresh_uuid(), oversized),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA");
    assert_eq!(view["code"], "RESPONSE_TOO_LARGE");
}

#[test]
fn goal_submit_rejects_a_non_object_response() {
    let f = fixture("non-object");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = &next["model_work"];
    let view = goal_submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, work, &fresh_uuid(), json!([1, 2, 3])),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA");
    assert_eq!(view["code"], "RESPONSE_NOT_AN_OBJECT");
}

#[test]
fn goal_submit_rejects_a_deeply_nested_response() {
    let f = fixture("deep");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = &next["model_work"];
    let mut nested = json!(0);
    for _ in 0..40 {
        nested = json!([nested]);
    }
    let view = goal_submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            work,
            &fresh_uuid(),
            json!({"deep": nested}),
        ),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA");
    assert_eq!(view["code"], "RESPONSE_TOO_DEEP");
}

#[test]
fn goal_submit_replay_returns_the_same_receipt_without_new_side_effects() {
    let f = fixture("replay");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = &next["model_work"];
    let proposal = valid_planner_proposal(work);
    let submission = fresh_uuid();
    let args = submit_args(&f.session, &goal_id, work, &submission, proposal.clone());

    let first = goal_submit(&f.session, &f.store, &args).unwrap();
    let revision_after_first = f
        .store
        .load_goal(&f.session.id, &goal_id)
        .unwrap()
        .revision();
    let second = goal_submit(&f.session, &f.store, &args).unwrap();

    assert_eq!(second["outcome"], "REPLAY_OF_ACCEPTED_SUBMISSION");
    assert_eq!(second["receipt"], first["receipt"]);
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(
        goal.revision(),
        revision_after_first,
        "a replay never commits"
    );
    assert_eq!(external_state(&goal).unwrap().receipts().len(), 1);
    assert_eq!(external_state(&goal).unwrap().issue_counter(), 1);
}

#[test]
fn goal_submit_conflicting_duplicate_never_replaces_the_first_verdict() {
    let f = fixture("conflict");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = &next["model_work"];
    let submission = fresh_uuid();
    let first = goal_submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            work,
            &submission,
            valid_planner_proposal(work),
        ),
    )
    .unwrap();
    assert_eq!(first["outcome"], "COMMITTED");

    let second = goal_submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            work,
            &submission,
            json!({"other": true}),
        ),
    )
    .unwrap();
    assert_eq!(second["outcome"], "CONFLICTING_DUPLICATE");
    assert_eq!(
        second["receipt"]["payload_sha256"],
        first["receipt"]["payload_sha256"]
    );
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(external_state(&goal).unwrap().receipts().len(), 1);
}

#[test]
fn goal_submit_refuses_a_digest_mismatch_without_touching_durable_state() {
    let f = fixture("digest");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = &next["model_work"];
    let revision = f
        .store
        .load_goal(&f.session.id, &goal_id)
        .unwrap()
        .revision();
    let mut args = submit_args(
        &f.session,
        &goal_id,
        work,
        &fresh_uuid(),
        valid_planner_proposal(work),
    );
    args["request_digest"] = Value::String("0".repeat(64));

    let view = goal_submit(&f.session, &f.store, &args).unwrap();
    assert_eq!(view["outcome"], "REJECTED_AUTHORITY");
    assert_eq!(view["code"], "REQUEST_DIGEST_MISMATCH");
    assert!(view["receipt"].is_null());
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(goal.revision(), revision);
    assert_eq!(external_state(&goal).unwrap().receipts().len(), 0);
    assert!(external_state(&goal).unwrap().active_work().is_some());
}

#[test]
fn goal_submit_refuses_a_stale_revision_without_touching_durable_state() {
    let f = fixture("stale");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = &next["model_work"];
    let revision = f
        .store
        .load_goal(&f.session.id, &goal_id)
        .unwrap()
        .revision();
    let mut args = submit_args(
        &f.session,
        &goal_id,
        work,
        &fresh_uuid(),
        valid_planner_proposal(work),
    );
    args["expected_goal_revision"] = Value::from(1);

    let view = goal_submit(&f.session, &f.store, &args).unwrap();
    assert_eq!(view["outcome"], "REJECTED_STALE_REVISION");
    assert_eq!(view["code"], "GOAL_REVISION_STALE");
    assert_eq!(view["goal_state"]["next_action"], "CALL_GOAL_NEXT");
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(goal.revision(), revision);
    assert_eq!(external_state(&goal).unwrap().receipts().len(), 0);
}

#[test]
fn goal_submit_refuses_an_unknown_work_id_without_touching_durable_state() {
    let f = fixture("unknown-work");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = &next["model_work"];
    let revision = f
        .store
        .load_goal(&f.session.id, &goal_id)
        .unwrap()
        .revision();
    let mut args = submit_args(
        &f.session,
        &goal_id,
        work,
        &fresh_uuid(),
        valid_planner_proposal(work),
    );
    args["work_id"] = Value::String(fresh_uuid());

    let view = goal_submit(&f.session, &f.store, &args).unwrap();
    assert_eq!(view["outcome"], "REJECTED_AUTHORITY");
    assert_eq!(view["code"], "UNKNOWN_WORK_ID");
    assert_eq!(view["goal_state"]["next_action"], "STOP_AND_RECONCILE");
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(goal.revision(), revision);
    assert_eq!(external_state(&goal).unwrap().receipts().len(), 0);
}

#[test]
fn goal_submit_cannot_probe_another_sessions_goal() {
    let a = fixture("probe-a");
    let goal_id = goal_id_of(&start_goal(&a.session, &a.store, Some("EXTERNAL")));
    let next = goal_next(&a.session, &a.store, &goal_id);
    let work = &next["model_work"];
    let args = submit_args(
        &a.session,
        &goal_id,
        work,
        &fresh_uuid(),
        valid_planner_proposal(work),
    );

    // A different session in the same store cannot even name the Goal.
    let other = config::Session {
        id: format!("e3b-probe-b-{}", Uuid::new_v4()),
        cwd: a.session.cwd.clone(),
        permitted_directories: a.session.permitted_directories.clone(),
    };
    let error = external_goal_api::goal_submit(&args, &other, &a.store).unwrap_err();
    assert_eq!(error.code(), "INVALID_ARGUMENT");

    // Presenting its own session id, the other session still cannot reach the
    // Goal: the lookup is indistinguishable from a missing Goal.
    let mut foreign = args.clone();
    foreign["session_id"] = Value::String(other.id.clone());
    let error = external_goal_api::goal_submit(&foreign, &other, &a.store).unwrap_err();
    assert_eq!(error.code(), "GOAL_NOT_FOUND");

    // The owner's durable state is untouched by either probe.
    let goal = a.store.load_goal(&a.session.id, &goal_id).unwrap();
    assert_eq!(external_state(&goal).unwrap().receipts().len(), 0);
    assert!(external_state(&goal).unwrap().active_work().is_some());
}

#[test]
fn goal_submit_on_a_codex_goal_is_refused() {
    let f = fixture("codex-submit");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, None));
    let args = json!({
        "session_id": f.session.id,
        "goal_id": goal_id.as_str(),
        "work_id": fresh_uuid(),
        "submission_id": fresh_uuid(),
        "expected_goal_revision": 1,
        "request_digest": "0".repeat(64),
        "response": {},
    });
    match external_goal_api::goal_submit(&args, &f.session, &f.store) {
        Ok(view) => panic!(
            "expected a refusal, got {}",
            serde_json::to_string(&view).unwrap()
        ),
        Err(error) => assert_eq!(error.code(), "MODEL_BACKEND_CODEX"),
    }
}

#[test]
fn goal_next_after_a_committed_plan_reports_the_unwired_readonly_role() {
    let f = fixture("await-host");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = &next["model_work"];
    let committed = goal_submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            work,
            &fresh_uuid(),
            valid_planner_proposal(work),
        ),
    )
    .unwrap();
    assert_eq!(committed["outcome"], "COMMITTED");

    // E4-B is not wired on this branch: the Scheduler selects the READY
    // readonly Task, and the host reports the role as unwired instead of
    // faking progress or completion.
    let after = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(after["outcome"], "NO_ACTION_WITH_REASON");
    assert_eq!(after["no_action_reason"], "EXTERNAL_ROLE_NOT_WIRED");
    assert!(after["model_work"].is_null());
    assert_ne!(after["result_kind"], "COMPLETED");
}

#[test]
fn role_attempt_budget_stops_new_work_items() {
    let f = fixture("attempt-budget");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    for _ in 0..MAX_ROLE_ATTEMPTS {
        let next = goal_next(&f.session, &f.store, &goal_id);
        assert_eq!(next["outcome"], "MODEL_REQUIRED");
        let work = &next["model_work"];
        let view = goal_submit(
            &f.session,
            &f.store,
            &submit_args(&f.session, &goal_id, work, &fresh_uuid(), json!({})),
        )
        .unwrap();
        assert_eq!(view["outcome"], "REJECTED_SCHEMA");
    }
    let exhausted = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(exhausted["outcome"], "BLOCKED");
    assert_eq!(
        exhausted["blockers"][0]["code"],
        "ROLE_ATTEMPT_BUDGET_EXHAUSTED"
    );
    assert_eq!(exhausted["budget"]["role_attempts"], MAX_ROLE_ATTEMPTS);
    assert!(exhausted["model_work"].is_null());
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    assert!(!goal.status().is_terminal());
    assert_eq!(
        external_state(&goal).unwrap().issue_counter(),
        MAX_ROLE_ATTEMPTS
    );
}

// ---------------------------------------------------------------------------
// Structural guarantees.
// ---------------------------------------------------------------------------

#[test]
fn external_modules_never_reference_the_codex_transport() {
    // Mechanical interception of the ADR-08 guarantee at the source level: if
    // someone ever wires a Codex transport into the external modules, this test
    // fails immediately instead of relying on a runtime counter alone.
    // Transport *types* only: the two ADR-08 gate helpers (`codex_fallback_decision`
    // and `goal_run_backend_decision`) deliberately live in `external_goal_api`
    // and are asserted separately below.
    let forbidden = [
        "ProductionModelTransport",
        "GoalModelAgent",
        "ProductionGoalBackends",
        "ModelTransport",
    ];
    for path in [
        "src/external_goal_api.rs",
        "src/external_goal_driver.rs",
        "src/external_model_work.rs",
        "src/external_model_store.rs",
        "src/model_backend.rs",
    ] {
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("cannot read {path}: {error}"));
        for needle in forbidden {
            assert!(
                !source.contains(needle),
                "{path} must never reference {needle}: EXTERNAL must not reach the Codex path"
            );
        }
    }

    // The gate helpers exist and are the only Codex-facing surface here.
    let api = std::fs::read_to_string("src/external_goal_api.rs").unwrap();
    assert!(api.contains("fn codex_fallback_decision"));
    assert!(api.contains("fn goal_run_backend_decision"));
}

#[test]
fn goal_run_decision_and_outcome_keep_external_goals_off_codex() {
    // One Goal per session, so each backend is observed in its own fixture.
    let codex_fixture = fixture("run-decision-codex");
    let codex_id = goal_id_of(&start_goal(
        &codex_fixture.session,
        &codex_fixture.store,
        None,
    ));
    let codex = codex_fixture
        .store
        .load_goal(&codex_fixture.session.id, &codex_id)
        .unwrap();
    assert_eq!(
        external_goal_api::goal_run_backend_decision(&codex),
        external_goal_api::GoalRunBackendDecision::Codex
    );

    let f = fixture("run-decision-external");
    let external_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let external = f.store.load_goal(&f.session.id, &external_id).unwrap();
    assert_eq!(
        external_goal_api::goal_run_backend_decision(&external),
        external_goal_api::GoalRunBackendDecision::ExternalModelRequired
    );

    let outcome: Value =
        serde_json::from_str(&external_goal_api::external_goal_run_outcome(&external)).unwrap();
    assert_eq!(outcome["outcome"], "EXTERNAL_MODEL_REQUIRED");
    assert_eq!(outcome["codex_invoked"], false);
    assert_eq!(outcome["next_action"], "CALL_GOAL_NEXT");
    assert_eq!(outcome["model_backend"], "EXTERNAL");
}

#[test]
fn goal_status_and_result_expose_the_additive_external_projection() {
    let f = fixture("projection");
    let goal_id = goal_id_of(&start_goal(&f.session, &f.store, Some("EXTERNAL")));
    let status = serde_json::to_value(
        goal_api::goal_status(
            &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
            &f.session,
            &f.store,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(status["model_backend"], "EXTERNAL");
    assert_eq!(status["execution_wait"], "NONE");
    assert_eq!(status["external_budget"]["max_work_items"], 64);

    let next = goal_next(&f.session, &f.store, &goal_id);
    let status = serde_json::to_value(
        goal_api::goal_status(
            &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
            &f.session,
            &f.store,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(status["execution_wait"], "WAITING_FOR_EXTERNAL_MODEL");
    assert_eq!(status["pending_work_id"], next["model_work"]["work_id"]);
    // The frozen status vocabulary is untouched.
    assert_eq!(status["status"], "PLANNING");
}
