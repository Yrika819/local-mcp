//! E5-0 — multi-task DAG readiness propagation (AT-E5-001..012).
//!
//! Every scenario drives the real external loop (`goal_next` / `goal_submit`)
//! against an isolated temp store, with deep plans built from the host-issued
//! Planner request. No test spawns a process, touches a repository, or relaxes a
//! host gate.

use std::path::PathBuf;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::config;
use crate::external_goal_api;
use crate::external_model_store::external_state;
use crate::goal::{Goal, GoalId, GoalStatus};
use crate::goal_api;
use crate::model_accounting;
use crate::readiness;
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
    let root = std::env::temp_dir().join(format!("local-mcp-e5-dag-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(workspace.join("tracked.txt"), b"e5 dag\n").unwrap();
    let session = config::Session {
        id: format!("e5-dag-{label}-{}", Uuid::new_v4()),
        cwd: workspace.clone(),
        permitted_directories: vec![workspace],
    };
    Fixture {
        root,
        session,
        store: TaskStore::with_state_root(state),
    }
}

fn start_external(session: &config::Session, store: &TaskStore) -> GoalId {
    let view = goal_api::goal_start(
        &json!({
            "session_id": session.id,
            "objective": "Walk a multi-Task read-only DAG",
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

fn goal_next(session: &config::Session, store: &TaskStore, goal_id: &GoalId) -> Value {
    serde_json::to_value(
        goal_next_call(
            &json!({"session_id": session.id, "goal_id": goal_id.as_str()}),
            session,
            store,
        )
        .unwrap(),
    )
    .unwrap()
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

fn submit(
    session: &config::Session,
    store: &TaskStore,
    args: &Value,
) -> Result<Value, goal_api::GoalApiError> {
    external_goal_api::goal_submit(args, session, store)
        .map(|view| serde_json::to_value(view).unwrap())
}

/// One read-only Task definition with a distinct evidence requirement.
fn task(proposal_id: &str, dependencies: &[&str], evidence: &str, mandatory: bool) -> Value {
    json!({
        "proposal_id": proposal_id,
        "title": format!("Inspect {proposal_id}"),
        "objective": format!("Collect structured evidence for {proposal_id}"),
        "mandatory": mandatory,
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

fn planner_proposal(work: &Value, tasks: Vec<Value>, bind_all_to: &str) -> Value {
    let criterion_id = work["request"]["completion_criteria"][0]["criterion_id"]
        .as_str()
        .expect("criterion id");
    let refs = tasks
        .iter()
        .map(|task| task["proposal_id"].clone())
        .collect::<Vec<_>>();
    let mandatory_refs: Vec<Value> = tasks
        .iter()
        .filter(|task| task["mandatory"].as_bool().unwrap_or(false))
        .map(|task| task["proposal_id"].clone())
        .collect();
    json!({
        "goal_id": work["goal_id"],
        "goal_revision": work["goal_revision"],
        "summary": bind_all_to,
        "tasks": tasks,
        "criterion_bindings": [{
            "criterion_id": criterion_id,
            "task_refs": if mandatory_refs.is_empty() { refs } else { mandatory_refs }
        }]
    })
}

fn readonly_answer(work: &Value, evidence: &str) -> Value {
    let request = &work["request"];
    json!({
        "goal_id": request["goal_id"],
        "task_id": request["task_id"],
        "attempt_id": request["attempt_id"],
        "goal_revision": request["goal_revision"],
        "plan_revision": request["plan_revision"],
        "status": "candidate_complete",
        "summary": "read-only inspection",
        "evidence": [{"kind": evidence, "value": "tracked.txt was inspected"}]
    })
}

/// Issue the plan and walk one readonly WorkItem to a committed answer.
/// Returns the readonly WorkItem's task id.
fn plan_and_answer(f: &Fixture, goal_id: &GoalId, tasks: Vec<Value>, evidence: &str) -> TaskId {
    let planner_work = goal_next(&f.session, &f.store, goal_id)["model_work"].clone();
    let planned = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            goal_id,
            &planner_work,
            planner_proposal(&planner_work, tasks, "dag"),
        ),
    )
    .unwrap();
    assert_eq!(planned["outcome"], "COMMITTED", "{planned}");

    let next = goal_next(&f.session, &f.store, goal_id);
    assert_eq!(next["outcome"], "MODEL_REQUIRED", "{next}");
    let work = next["model_work"].clone();
    assert_eq!(work["role"], "READONLY");
    let answered = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, goal_id, &work, readonly_answer(&work, evidence)),
    )
    .unwrap();
    assert_eq!(answered["outcome"], "COMMITTED", "{answered}");
    TaskId::parse(work["task_id"].as_str().unwrap()).unwrap()
}

fn goal_of(f: &Fixture, goal_id: &GoalId) -> Goal {
    f.store.load_goal(&f.session.id, goal_id).unwrap()
}

fn task_status(goal: &Goal, task_id: &TaskId) -> TaskStatus {
    goal.tasks()[task_id].status()
}

fn task_by_title(goal: &Goal, needle: &str) -> TaskId {
    goal.tasks()
        .iter()
        .find(|(_, task)| task.title().contains(needle))
        .map(|(id, _)| id.clone())
        .expect("task title")
}

// ---------------------------------------------------------------------------
// AT-E5-001 / 002 — linear and diamond chains.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_001_a_dependent_task_stays_pending_until_its_predecessor_is_verified() {
    let f = fixture("linear");
    let goal_id = start_external(&f.session, &f.store);
    let first = plan_and_answer(
        &f,
        &goal_id,
        vec![
            task("a", &[], "ev-a", true),
            task("b", &["a"], "ev-b", true),
        ],
        "linear",
    );

    let goal = goal_of(&f, &goal_id);
    let b = task_by_title(&goal, "Inspect b");
    assert_eq!(task_status(&goal, &b), TaskStatus::Pending);
    assert_eq!(task_status(&goal, &first), TaskStatus::Verifying);

    // The host arms nothing while the predecessor is only VERIFYING.
    assert!(!readiness::propagation_pending(&goal).unwrap());
}

#[test]
fn at_e5_002_a_diamond_arms_the_join_only_after_both_branches_verify() {
    let f = fixture("diamond");
    let goal_id = start_external(&f.session, &f.store);
    plan_and_answer(
        &f,
        &goal_id,
        vec![task("a", &[], "ev-a", true)],
        "root only",
    );

    // Replan-free diamond: build the plan in one shot instead.
    let f = fixture("diamond-full");
    let goal_id = start_external(&f.session, &f.store);
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let planned = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            planner_proposal(
                &planner_work,
                vec![
                    task("a", &[], "ev-a", true),
                    task("b", &["a"], "ev-b", true),
                    task("c", &["a"], "ev-c", true),
                    task("d", &["b", "c"], "ev-d", true),
                ],
                "diamond",
            ),
        ),
    )
    .unwrap();
    assert_eq!(planned["outcome"], "COMMITTED", "{planned}");
    let goal = goal_of(&f, &goal_id);
    let b = task_by_title(&goal, "Inspect b");
    let _c = task_by_title(&goal, "Inspect c");
    let d = task_by_title(&goal, "Inspect d");
    assert_eq!(task_status(&goal, &b), TaskStatus::Pending);
    assert_eq!(task_status(&goal, &d), TaskStatus::Pending);
    assert!(!readiness::propagation_pending(&goal).unwrap());
    drop(f);
}

// ---------------------------------------------------------------------------
// AT-E5-003 / 004 — deterministic multi-ready ordering.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_003_two_independent_tasks_are_armed_in_one_deterministic_step() {
    let f = fixture("two-ready");
    let goal_id = start_external(&f.session, &f.store);
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            planner_proposal(
                &planner_work,
                vec![
                    task("a", &[], "ev-a", true),
                    task("b", &[], "ev-b", true),
                    task("c", &["a", "b"], "ev-c", true),
                ],
                "two ready",
            ),
        ),
    )
    .unwrap();

    // The initial materialization already arms both roots in the same commit.
    let goal = goal_of(&f, &goal_id);
    let a = task_by_title(&goal, "Inspect a");
    let b = task_by_title(&goal, "Inspect b");
    assert_eq!(task_status(&goal, &a), TaskStatus::Ready);
    assert_eq!(task_status(&goal, &b), TaskStatus::Ready);
    // Deterministic order: mandatory-first, plan revision, TaskId.
    let mut expected = [a.clone(), b.clone()];
    expected.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let armed = readiness::eligible_task_ids(&goal).unwrap();
    assert!(armed.is_empty(), "roots are already armed");

    // Two concurrent readiness steps cannot double-arm.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let args = json!({"session_id": f.session.id, "goal_id": goal_id.as_str()});
    let view = runtime
        .block_on(external_goal_api::goal_next(&args, &f.session, &f.store))
        .unwrap();
    let view = serde_json::to_value(view).unwrap();
    assert_eq!(view["outcome"], "MODEL_REQUIRED", "{view}");
    assert_eq!(view["model_work"]["role"], "READONLY");
    // The issued WorkItem targets the deterministic first READY Task.
    assert_eq!(
        view["model_work"]["task_id"].as_str().unwrap(),
        expected[0].as_str()
    );
}

#[test]
fn at_e5_004_mandatory_first_order_is_preserved_across_a_mixed_plan() {
    let f = fixture("mandatory-first");
    let goal_id = start_external(&f.session, &f.store);
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            planner_proposal(
                &planner_work,
                vec![
                    task("optional", &[], "ev-opt", false),
                    task("required", &[], "ev-req", true),
                ],
                "mandatory first",
            ),
        ),
    )
    .unwrap();
    let goal = goal_of(&f, &goal_id);
    let required = task_by_title(&goal, "Inspect required");
    let optional = task_by_title(&goal, "Inspect optional");
    let next = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(
        next["model_work"]["task_id"].as_str().unwrap(),
        required.as_str()
    );
    assert_ne!(
        next["model_work"]["task_id"].as_str().unwrap(),
        optional.as_str()
    );
}

// ---------------------------------------------------------------------------
// AT-E5-005 / 006 / 007 — blocked, failed, and superseded predecessors.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_005_a_blocked_predecessor_never_arms_its_descendant() {
    let f = fixture("blocked-pred");
    let goal_id = start_external(&f.session, &f.store);
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            planner_proposal(
                &planner_work,
                vec![
                    task("a", &[], "ev-a", true),
                    task("b", &["a"], "ev-b", true),
                ],
                "blocked",
            ),
        ),
    )
    .unwrap();
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = next["model_work"].clone();
    let mut answer = readonly_answer(&work, "ev-a");
    answer["status"] = json!("blocked");
    let submitted = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, answer),
    )
    .unwrap();
    assert_eq!(submitted["outcome"], "COMMITTED", "{submitted}");

    let goal = goal_of(&f, &goal_id);
    let a = task_by_title(&goal, "Inspect a");
    let b = task_by_title(&goal, "Inspect b");
    assert_eq!(task_status(&goal, &a), TaskStatus::Blocked);
    assert_eq!(task_status(&goal, &b), TaskStatus::Pending);
    assert!(!readiness::propagation_pending(&goal).unwrap());
    let view = goal_next(&f.session, &f.store, &goal_id);
    assert_ne!(view["outcome"], "MODEL_REQUIRED", "{view}");
    assert_eq!(view["status"], "RUNNING");
    assert_ne!(view["result_kind"], "COMPLETED");
}

#[test]
fn at_e5_006_a_failed_predecessor_never_arms_its_descendant() {
    let f = fixture("failed-pred");
    let goal_id = start_external(&f.session, &f.store);
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            planner_proposal(
                &planner_work,
                vec![
                    task("a", &[], "ev-a", true),
                    task("b", &["a"], "ev-b", true),
                ],
                "failed",
            ),
        ),
    )
    .unwrap();
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = next["model_work"].clone();
    let mut answer = readonly_answer(&work, "ev-a");
    answer["status"] = json!("failed");
    submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, answer),
    )
    .unwrap();

    let goal = goal_of(&f, &goal_id);
    let a = task_by_title(&goal, "Inspect a");
    let b = task_by_title(&goal, "Inspect b");
    assert_eq!(task_status(&goal, &a), TaskStatus::Failed);
    assert_eq!(task_status(&goal, &b), TaskStatus::Pending);
    assert!(!readiness::propagation_pending(&goal).unwrap());
}

#[test]
fn at_e5_007_a_superseded_predecessor_is_not_completion_proof() {
    // The readiness rule reads active plan authority only. A superseded Task is
    // excluded from `dependencies_satisfied`, so a dependent can never be armed
    // by stale completion evidence.
    let f = fixture("superseded");
    let goal_id = start_external(&f.session, &f.store);
    let first = plan_and_answer(
        &f,
        &goal_id,
        vec![
            task("a", &[], "ev-a", true),
            task("b", &["a"], "ev-b", true),
        ],
        "superseded",
    );
    let goal = goal_of(&f, &goal_id);
    assert!(goal.tasks()[&first].is_active_plan_authority());
    let b = task_by_title(&goal, "Inspect b");
    assert_eq!(task_status(&goal, &b), TaskStatus::Pending);
    // A finished Task with a passing verification result still does not arm the
    // dependent until the host verifier records COMPLETED.
    assert_eq!(task_status(&goal, &first), TaskStatus::Verifying);
}

// ---------------------------------------------------------------------------
// AT-E5-008 / 009 / 010 — budget, concurrency, restart.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_008_the_host_step_budget_bounds_a_ready_chain() {
    let f = fixture("budget");
    let goal_id = start_external(&f.session, &f.store);
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            planner_proposal(
                &planner_work,
                vec![
                    task("a", &[], "ev-a", true),
                    task("b", &["a"], "ev-b", true),
                    task("c", &["b"], "ev-c", true),
                ],
                "chain",
            ),
        ),
    )
    .unwrap();

    // Answer A, then one goal_next with max_host_steps=1 may only verify or arm.
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = next["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, readonly_answer(&work, "ev-a")),
    )
    .unwrap();
    let one_step = serde_json::to_value(
        goal_next_call(
            &json!({"session_id": f.session.id, "goal_id": goal_id.as_str(), "max_host_steps": 1}),
            &f.session,
            &f.store,
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        one_step["host_steps_executed"].as_u64().unwrap() <= 1,
        "{one_step}"
    );
}

#[test]
fn at_e5_009_concurrent_goal_next_calls_do_not_double_arm() {
    let f = fixture("concurrent");
    let goal_id = start_external(&f.session, &f.store);
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            planner_proposal(
                &planner_work,
                vec![
                    task("a", &[], "ev-a", true),
                    task("b", &["a"], "ev-b", true),
                ],
                "concurrent",
            ),
        ),
    )
    .unwrap();
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = next["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, readonly_answer(&work, "ev-a")),
    )
    .unwrap();

    let mut handles = Vec::new();
    for _ in 0..2 {
        let session = f.session.clone();
        let store = f.store.clone();
        let args = json!({"session_id": f.session.id, "goal_id": goal_id.as_str()});
        handles.push(std::thread::spawn(move || {
            goal_next_call(&args, &session, &store).map(|view| {
                let view = serde_json::to_value(view).unwrap();
                (
                    view["outcome"].as_str().unwrap_or_default().to_owned(),
                    view["model_work"]["work_id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                )
            })
        }));
    }
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let mut issued = Vec::new();
    for outcome in &outcomes {
        match outcome {
            Ok((code, work_id)) => {
                assert!(
                    !code.starts_with("GOAL_PERSISTENCE") && code != "GOAL_STATE_CORRUPT",
                    "{outcomes:?}"
                );
                if !work_id.is_empty() {
                    issued.push(work_id.clone());
                }
            }
            Err(error) => assert!(
                error.code() == "REVISION_CONFLICT"
                    || error.code() == "GOAL_STATE_CORRUPT"
                    || error.code() == "GOAL_PERSISTENCE_ERROR",
                "unexpected transport error {}",
                error.code()
            ),
        }
    }
    // Both callers, when they got a WorkItem at all, got the same one: the CAS
    // and the single-active-WorkItem rule make a duplicate issuance impossible.
    assert!(!issued.is_empty(), "{outcomes:?}");
    for work_id in &issued {
        assert_eq!(work_id, &issued[0], "two different WorkItems were issued");
    }
    let goal = goal_of(&f, &goal_id);
    let state = external_state(&goal).unwrap();
    // Planner + readonly A were committed during setup; exactly one more
    // WorkItem exists for B, and it is the single active slot.
    assert_eq!(state.work_history().len(), 2);
    let active = state.active_work().expect("B is pending");
    assert_eq!(
        active.role(),
        crate::external_model_work::WorkRole::Readonly
    );
    assert_eq!(active.issue_counter(), 3);
}

#[test]
fn at_e5_010_a_ready_transition_survives_a_restart_exactly_once() {
    let f = fixture("restart");
    let goal_id = start_external(&f.session, &f.store);
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            planner_proposal(
                &planner_work,
                vec![
                    task("a", &[], "ev-a", true),
                    task("b", &["a"], "ev-b", true),
                ],
                "restart",
            ),
        ),
    )
    .unwrap();
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = next["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, readonly_answer(&work, "ev-a")),
    )
    .unwrap();

    let restarted = TaskStore::with_state_root(f.root.join("state"));
    let goal = restarted.load_goal(&f.session.id, &goal_id).unwrap();
    let b = task_by_title(&goal, "Inspect b");
    assert_eq!(task_status(&goal, &b), TaskStatus::Pending);

    // One call arms it (after verifying A); a restart never replays the arm.
    let view = goal_next(&f.session, &restarted, &goal_id);
    assert_eq!(view["outcome"], "MODEL_REQUIRED", "{view}");
    assert_eq!(view["model_work"]["role"], "READONLY");
    let armed_goal = restarted.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(task_status(&armed_goal, &b), TaskStatus::Running);
    let revision = armed_goal.revision();
    let restarted_again = TaskStore::with_state_root(f.root.join("state"));
    let reloaded = restarted_again.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(reloaded.revision(), revision);
    assert_eq!(task_status(&reloaded, &b), TaskStatus::Running);
}

// ---------------------------------------------------------------------------
// AT-E5-011 / 012 — host goal verification and CODEX regression.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_011_a_two_task_chain_reaches_host_goal_verification() {
    let f = fixture("e2e-chain");
    model_accounting::reset_for_test();
    let goal_id = start_external(&f.session, &f.store);
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            planner_proposal(
                &planner_work,
                vec![
                    task("a", &[], "ev-a", true),
                    task("b", &["a"], "ev-b", true),
                ],
                "chain e2e",
            ),
        ),
    )
    .unwrap();

    for evidence in ["ev-a", "ev-b"] {
        let next = goal_next(&f.session, &f.store, &goal_id);
        assert_eq!(next["outcome"], "MODEL_REQUIRED", "{next}");
        let work = next["model_work"].clone();
        let submitted = submit(
            &f.session,
            &f.store,
            &submit_args(
                &f.session,
                &goal_id,
                &work,
                readonly_answer(&work, evidence),
            ),
        )
        .unwrap();
        assert_eq!(submitted["outcome"], "COMMITTED", "{submitted}");
    }

    let finished = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(finished["status"], "COMPLETED", "{finished}");
    assert_eq!(finished["result_kind"], "COMPLETED");
    let goal = goal_of(&f, &goal_id);
    assert_eq!(goal.status(), GoalStatus::Completed);
    assert!(goal.tasks().values().all(|task| {
        task.status() == TaskStatus::Completed
            && task
                .verification_results()
                .last()
                .map(|result| result.outcome())
                == Some(crate::task::VerificationOutcome::Passed)
    }));
    assert_eq!(model_accounting::snapshot_for_test().codex_spawn_count, 0);
}

#[test]
fn at_e5_012_codex_goals_keep_their_existing_readiness_behaviour() {
    // E5-0 is wired into the EXTERNAL driver only; goal_next on a CODEX Goal
    // still returns the typed no-action and never propagates readiness.
    let f = fixture("codex-regression");
    let view = goal_api::goal_start(
        &json!({"session_id": f.session.id, "objective": "codex goal"}),
        &f.session,
        &f.store,
    )
    .unwrap();
    let view = serde_json::to_value(view).unwrap();
    let goal_id = GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap();
    let next = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(next["outcome"], "NO_ACTION_WITH_REASON");
    assert_eq!(next["no_action_reason"], "MODEL_BACKEND_CODEX");
    assert!(next["model_work"].is_null());
    let goal = goal_of(&f, &goal_id);
    assert_eq!(goal.status(), GoalStatus::Planning);
    assert_eq!(goal.revision(), 1);
}
