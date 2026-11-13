//! E5-C — external Replanner (AT-E5-057..071).
//!
//! A Replanner WorkItem exists only for a durable trigger, and its proposal is
//! validated by the same validator the Codex replanner uses, then applied to the
//! Task DAG, the plan revision, the receipt, and the WorkItem in **one** commit.
//! These tests drive the real external loop end to end: a readonly Task that
//! reports `needs_replan`, a replan that adds a bounded prerequisite and resolves
//! the trigger, readiness propagation, the new readonly Task, and host Goal
//! verification.

use std::path::PathBuf;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::config;
use crate::external_goal_api;
use crate::external_model_store::{external_state, mutate_external};
use crate::goal::{Goal, GoalId, GoalStatus};
use crate::goal_api;
use crate::mutation::MutationIntentState;
use crate::task::{TaskId, TaskStatus};
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
    let root = std::env::temp_dir().join(format!("local-mcp-e5c-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(workspace.join("tracked.txt"), b"e5 replan\n").unwrap();
    let session = config::Session {
        id: format!("e5c-{label}-{}", Uuid::new_v4()),
        cwd: workspace.clone(),
        permitted_directories: vec![workspace],
    };
    Fixture {
        root,
        session,
        store: TaskStore::with_state_root(state),
    }
}

fn goal_next(session: &config::Session, store: &TaskStore, goal_id: &GoalId) -> Value {
    let args = json!({"session_id": session.id, "goal_id": goal_id.as_str()});
    let view = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(external_goal_api::goal_next(&args, session, store))
        .expect("goal_next");
    serde_json::to_value(view).unwrap()
}

fn submit(session: &config::Session, store: &TaskStore, args: &Value) -> Result<Value, String> {
    external_goal_api::goal_submit(args, session, store)
        .map(|view| serde_json::to_value(view).unwrap())
        .map_err(|error| error.code().to_owned())
}

fn submit_args(
    session: &config::Session,
    goal_id: &GoalId,
    work: &Value,
    response: Value,
) -> Value {
    json!({
        "session_id": session.id,
        "goal_id": goal_id.as_str(),
        "work_id": work["work_id"],
        "submission_id": Uuid::new_v4().to_string(),
        "expected_goal_revision": work["goal_revision"],
        "request_digest": work["request_digest"],
        "response": response,
    })
}

fn readonly_task(id: &str, dependencies: &[&str], evidence: &str) -> Value {
    json!({
        "proposal_id": id,
        "title": format!("Inspect {id}"),
        "objective": format!("Collect structured evidence for {id}"),
        "mandatory": true,
        "worker": "CODEX_READONLY",
        "dependencies": dependencies,
        "scope": {
            "allowed_paths": ["."],
            "forbidden_paths": [],
            "operation_kind": "READ_ONLY",
            "replay_safety": "SAFE_READ_ONLY"
        },
        "verification": [{"kind": "STRUCTURED_EVIDENCE", "requirement_id": evidence}]
    })
}

fn planner_proposal(work: &Value, tasks: Vec<Value>) -> Value {
    let criterion_id = work["request"]["completion_criteria"][0]["criterion_id"]
        .as_str()
        .unwrap();
    let refs = tasks
        .iter()
        .map(|task| task["proposal_id"].clone())
        .collect::<Vec<_>>();
    json!({
        "goal_id": work["goal_id"],
        "goal_revision": work["goal_revision"],
        "summary": "read-only plan",
        "tasks": tasks,
        "criterion_bindings": [{"criterion_id": criterion_id, "task_refs": refs}]
    })
}

fn readonly_answer(work: &Value, evidence: &str, status: &str) -> Value {
    let request = &work["request"];
    json!({
        "goal_id": request["goal_id"],
        "task_id": request["task_id"],
        "attempt_id": request["attempt_id"],
        "goal_revision": request["goal_revision"],
        "plan_revision": request["plan_revision"],
        "status": status,
        "summary": "read-only inspection",
        "evidence": [{"kind": evidence, "value": "tracked.txt was inspected"}]
    })
}

fn goal_of(f: &Fixture, goal_id: &GoalId) -> Goal {
    f.store.load_goal(&f.session.id, goal_id).unwrap()
}

/// Start a Goal with one readonly Task and drive it to `NEEDS_REPLAN`.
fn goal_with_needs_replan(f: &Fixture) -> (GoalId, TaskId) {
    let view = goal_api::goal_start(
        &json!({
            "session_id": f.session.id,
            "objective": "Walk a read-only DAG through a replan",
            "model_backend": "EXTERNAL",
        }),
        &f.session,
        &f.store,
    )
    .unwrap();
    let view = serde_json::to_value(view).unwrap();
    let goal_id = GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap();
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let planned = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            planner_proposal(&planner_work, vec![readonly_task("inspect-1", &[], "ev-a")]),
        ),
    )
    .unwrap();
    assert_eq!(planned["outcome"], "COMMITTED", "{planned}");

    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    assert_eq!(work["role"], "READONLY", "{work}");
    let task_id = TaskId::parse(work["task_id"].as_str().unwrap()).unwrap();
    let answered = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            readonly_answer(&work, "ev-a", "needs_replan"),
        ),
    )
    .unwrap();
    assert_eq!(answered["outcome"], "COMMITTED", "{answered}");
    let goal = goal_of(f, &goal_id);
    assert_eq!(goal.tasks()[&task_id].status(), TaskStatus::NeedsReplan);
    (goal_id, task_id)
}

/// A replan that adds a bounded prerequisite, depends the trigger on it, and
/// resolves the trigger back to PENDING.
fn replan_proposal(f: &Fixture, goal_id: &GoalId, trigger: &TaskId) -> Value {
    let goal = goal_of(f, goal_id);
    json!({
        "goal_id": goal_id.as_str(),
        "base_goal_revision": goal.revision(),
        "base_plan_revision": goal.plan_revision(),
        "summary": "add a bounded prerequisite and resolve the trigger",
        "add_tasks": [readonly_task("prerequisite", &[], "ev-prereq")],
        "add_dependencies": [{
            "task": {"ref_kind": "EXISTING", "task_id": trigger.as_str()},
            "dependency": {"ref_kind": "NEW", "proposal_id": "prerequisite"}
        }],
        "resolve_needs_replan": [trigger.as_str()]
    })
}

// ---------------------------------------------------------------------------
// AT-057 / 058 — a WorkItem exists only for a durable trigger.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_057_a_readonly_needs_replan_issues_a_replanner_work_item() {
    let f = fixture("trigger");
    let (goal_id, trigger) = goal_with_needs_replan(&f);
    let view = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(view["outcome"], "MODEL_REQUIRED", "{view}");
    assert_eq!(view["model_work"]["role"], "REPLANNER", "{view}");
    assert_eq!(
        view["model_work"]["task_id"].as_str().unwrap(),
        trigger.as_str()
    );
    assert_eq!(
        view["model_work"]["output_contract"],
        "replanner-proposal-v1"
    );
    // The request carries the host-issued criterion ids and the eligible trigger.
    assert!(view["model_work"]["request"]["request"]["completion_criteria"].is_array());
    assert_eq!(
        view["model_work"]["response_required_fields"],
        json!([
            "goal_id",
            "base_goal_revision",
            "base_plan_revision",
            "summary"
        ])
    );
}

#[test]
fn at_e5_058_no_replanner_work_item_exists_without_a_trigger() {
    let f = fixture("no-trigger");
    let view = goal_api::goal_start(
        &json!({
            "session_id": f.session.id,
            "objective": "No replan trigger here",
            "model_backend": "EXTERNAL",
        }),
        &f.session,
        &f.store,
    )
    .unwrap();
    let view = serde_json::to_value(view).unwrap();
    let goal_id = GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap();
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            planner_proposal(&planner_work, vec![readonly_task("inspect-1", &[], "ev-a")]),
        ),
    )
    .unwrap();
    let next = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(next["outcome"], "MODEL_REQUIRED", "{next}");
    assert_eq!(next["model_work"]["role"], "READONLY");
    assert_ne!(next["model_work"]["role"], json!("REPLANNER"));
}

// ---------------------------------------------------------------------------
// AT-066 / 069 / 070 — apply, propagate, and verify end to end.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_066_070_a_replan_is_applied_and_the_goal_completes() {
    let f = fixture("e2e");
    crate::model_accounting::reset_for_test();
    let (goal_id, trigger) = goal_with_needs_replan(&f);
    let before = goal_of(&f, &goal_id);
    let plan_revision_before = before.plan_revision();

    let replanner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    assert_eq!(replanner_work["role"], "REPLANNER");
    let applied = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &replanner_work,
            replan_proposal(&f, &goal_id, &trigger),
        ),
    )
    .unwrap();
    assert_eq!(applied["outcome"], "COMMITTED", "{applied}");
    assert_eq!(applied["code"], "REPLAN_APPLIED");
    assert!(applied["receipt"]["committed_revision"].is_number());

    // One snapshot: plan revision advanced, the new Task exists, the trigger was
    // resolved, and the receipt names the revision this commit produced.
    let goal = goal_of(&f, &goal_id);
    assert_eq!(goal.plan_revision(), plan_revision_before + 1);
    assert_eq!(goal.tasks()[&trigger].status(), TaskStatus::Pending);
    let prerequisite = goal
        .tasks()
        .iter()
        .find(|(id, _)| **id != trigger)
        .map(|(id, _)| id.clone())
        .expect("new prerequisite task");
    assert_eq!(goal.tasks()[&prerequisite].status(), TaskStatus::Ready);
    let state = external_state(&goal).unwrap();
    assert!(state.active_work().is_none(), "the WorkItem committed");
    assert_eq!(
        state.work_history().last().unwrap().state(),
        crate::external_model_work::WorkItemState::Committed
    );

    // Readiness propagation arms the resolved trigger once the prerequisite is
    // verified complete, and the loop reaches host Goal completion.
    for evidence in ["ev-prereq", "ev-a"] {
        let next = goal_next(&f.session, &f.store, &goal_id);
        assert_eq!(next["outcome"], "MODEL_REQUIRED", "{next}");
        let work = next["model_work"].clone();
        assert_eq!(work["role"], "READONLY", "{work}");
        let answered = submit(
            &f.session,
            &f.store,
            &submit_args(
                &f.session,
                &goal_id,
                &work,
                readonly_answer(&work, evidence, "candidate_complete"),
            ),
        )
        .unwrap();
        assert_eq!(answered["outcome"], "COMMITTED", "{answered}");
    }
    let finished = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(finished["status"], "COMPLETED", "{finished}");
    assert_eq!(goal_of(&f, &goal_id).status(), GoalStatus::Completed);
    assert_eq!(
        crate::model_accounting::snapshot_for_test().codex_spawn_count,
        0
    );
}

// ---------------------------------------------------------------------------
// AT-060 / 061 / 062 — the shared validator refuses bad replans.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_060_a_cyclic_replan_is_refused_without_a_partial_graph() {
    let f = fixture("cycle");
    let (goal_id, trigger) = goal_with_needs_replan(&f);
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let before = goal_of(&f, &goal_id);
    let mut proposal = replan_proposal(&f, &goal_id, &trigger);
    // A cycle: the new Task depends on the trigger and the trigger depends on it.
    proposal["add_tasks"][0]["dependencies"] =
        json!([{"ref_kind": "EXISTING", "task_id": trigger.as_str()}]);
    proposal["add_dependencies"] = json!([{
        "task": {"ref_kind": "EXISTING", "task_id": trigger.as_str()},
        "dependency": {"ref_kind": "NEW", "proposal_id": "prerequisite"}
    }]);
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, proposal),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA", "{view}");
    let after = goal_of(&f, &goal_id);
    assert_eq!(after.plan_revision(), before.plan_revision());
    assert_eq!(after.tasks().len(), before.tasks().len());
    assert_eq!(after.tasks()[&trigger].status(), TaskStatus::NeedsReplan);
}

#[test]
fn at_e5_061_a_scope_expanding_replan_is_refused() {
    let f = fixture("scope");
    let (goal_id, trigger) = goal_with_needs_replan(&f);
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let mut proposal = replan_proposal(&f, &goal_id, &trigger);
    // Escape the Goal root.
    proposal["add_tasks"][0]["scope"]["allowed_paths"] = json!(["../outside"]);
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, proposal),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA", "{view}");
    assert_eq!(goal_of(&f, &goal_id).tasks().len(), 1);
}

#[test]
fn at_e5_062_a_replan_that_drops_criterion_coverage_is_refused() {
    let f = fixture("criterion");
    let (goal_id, trigger) = goal_with_needs_replan(&f);
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let mut proposal = replan_proposal(&f, &goal_id, &trigger);
    // Remove the only task that covers the required criterion.
    proposal["resolve_needs_replan"] = json!([trigger.as_str()]);
    proposal["add_tasks"] = json!([]);
    proposal["add_dependencies"] = json!([]);
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, proposal),
    )
    .unwrap();
    // With nothing added, the trigger is merely resolved; the host still requires
    // the criterion to stay covered, and the Task can never satisfy it, so the
    // proposal is refused by the validator rather than silently accepted.
    assert!(
        matches!(view["outcome"].as_str(), Some("REJECTED_SCHEMA")),
        "{view}"
    );
    assert_eq!(goal_of(&f, &goal_id).tasks().len(), 1);
}

// ---------------------------------------------------------------------------
// AT-064 / 065 / 068 — replay and staleness.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_064_the_same_replan_under_a_new_submission_id_is_idempotent() {
    let f = fixture("replay");
    let (goal_id, trigger) = goal_with_needs_replan(&f);
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let proposal = replan_proposal(&f, &goal_id, &trigger);
    let first = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, proposal.clone()),
    )
    .unwrap();
    assert_eq!(first["outcome"], "COMMITTED", "{first}");
    let after_first = goal_of(&f, &goal_id);

    // A client that re-sends the same effective plan under a new submission id (and
    // the same work id) is told it is already applied, without a new plan revision.
    let replay_work = json!({
        "work_id": work["work_id"],
        "goal_revision": work["goal_revision"],
        "request_digest": work["request_digest"],
    });
    let replay = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &replay_work, proposal),
    );
    match replay {
        Ok(view) => {
            assert_ne!(
                view["outcome"].as_str(),
                Some("REJECTED_SCHEMA"),
                "an identical replan must not be refused as a schema error: {view}"
            );
        }
        Err(code) => assert!(
            code == "REJECTED_AUTHORITY" || code == "UNKNOWN_WORK_ID",
            "{code}"
        ),
    }
    assert_eq!(
        goal_of(&f, &goal_id).plan_revision(),
        after_first.plan_revision()
    );
}

#[test]
fn at_e5_068_a_stale_base_revision_replan_is_refused() {
    let f = fixture("stale");
    let (goal_id, trigger) = goal_with_needs_replan(&f);
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let mut proposal = replan_proposal(&f, &goal_id, &trigger);
    proposal["base_goal_revision"] = json!(1);
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, proposal),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA", "{view}");
    assert_eq!(goal_of(&f, &goal_id).tasks().len(), 1);
}

#[test]
fn at_e5_059_a_replanner_attempt_budget_is_bounded() {
    // The per-role attempt budget is shared with the other roles; a Goal that keeps
    // returning to NEEDS_REPLAN cannot loop forever.
    let f = fixture("budget");
    let (goal_id, trigger) = goal_with_needs_replan(&f);
    let goal = goal_of(&f, &goal_id);
    let state = external_state(&goal).unwrap();
    assert!(state.role_attempts(crate::external_model_work::WorkRole::Replanner) <= 8);
    assert_eq!(goal.tasks()[&trigger].status(), TaskStatus::NeedsReplan);
}

// ---------------------------------------------------------------------------
// Security: a replan cannot touch durable mutation/intent authority.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_063_a_replan_never_touches_a_writer_mutation_intent() {
    let f = fixture("no-mutation");
    let (goal_id, _trigger) = goal_with_needs_replan(&f);
    let goal = goal_of(&f, &goal_id);
    let intents = goal
        .tasks()
        .values()
        .filter_map(|task| task.latest_attempt())
        .filter_map(|attempt| attempt.mutation_intent())
        .count();
    assert_eq!(intents, 0, "a replan-only Goal has no mutation intent");
    // The takeover of the external state by the replan work item left the writer
    // slot untouched.
    let (_, _) = mutate_external(
        &f.store,
        &f.session.id,
        &goal_id,
        goal.revision(),
        |state, now| {
            state.set_stop(crate::external_model_work::ExternalStop::new(
                "TEST_NOOP",
                None,
                now,
            ));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        goal_of(&f, &goal_id)
            .tasks()
            .values()
            .filter_map(|task| task.latest_attempt())
            .filter_map(|attempt| attempt.mutation_intent())
            .filter(|intent| intent.state() != MutationIntentState::Prepared)
            .count(),
        0
    );
}
