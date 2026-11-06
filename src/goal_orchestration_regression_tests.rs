//! G0-B Goal Orchestration Regression — reproduction tests for the Dogfooding
//! V1 orchestration failures.
//!
//! Two observed failures are pinned here as regression tests:
//!
//! 1. `CriterionId` rejection: the planner returned a `criterion_bindings`
//!    entry whose `criterion_id` was not a host-owned `CompletionCriterionId`,
//!    and the host failed the proposal with
//!    "planner proposal violates the Phase 4 schema: criterion binding
//!    references an unknown host CompletionCriterionId". These tests fix the
//!    classification: the validator is fail-closed and correct (no unknown ID is
//!    ever auto-corrected or accepted), and the failure is a model output
//!    schema violation with a contributing prompt-contract gap (see
//!    `docs/GOAL_ORCHESTRATION_G0B_REGRESSION_NOTES.md`).
//!
//! 2. `READONLY_BLOCKED` stall: a Readonly Worker returning `blocked` turns the
//!    mandatory Task `Blocked` with a mandatory `READONLY_BLOCKED` blocker, the
//!    scheduler reports `NoAction { BlockedTasks }`, and the Goal stays
//!    `RUNNING` (`goal_run` reports NO_ACTION). These tests pin that BLOCKED is
//!    never silently converted into NEEDS_REPLAN and that the stall is durable.
//!
//! They also cover the additive `result_kind` discriminator on `goal_result`,
//! which distinguishes COMPLETED / FAILED / CANCELLED / BLOCKED / NONTERMINAL
//! without touching the frozen `result_state` contract.

use std::path::PathBuf;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::agent::AgentError;
use crate::config;
use crate::goal::{CheckpointReason, Goal, GoalStatus};
use crate::goal_api;
use crate::planner;
use crate::readonly_worker::{
    ReadonlyBackend, ReadonlyError, ReadonlyRequest, run_readonly_attempt,
};
use crate::scheduler::{SchedulerDecision, SchedulerNoActionReason, select_next_action};
use crate::task::{
    ReplaySafety, TaskOperationKind, TaskScope, TaskStatus, TaskTransitionContext,
    VerificationOutcome, VerificationResult, WorkerKind,
};
use crate::task_store::TaskStore;

const NOW: &str = "2026-11-06T00:00:00Z";

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
    let root = std::env::temp_dir().join(format!("local-mcp-g0b-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let session = config::Session {
        id: format!("g0b-{label}-{}", Uuid::new_v4()),
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

fn read_only_scope() -> TaskScope {
    TaskScope::new(
        vec![PathBuf::from("src")],
        vec![],
        TaskOperationKind::ReadOnly,
        ReplaySafety::SafeReadOnly,
    )
}

/// Build (but do not persist) a goal with the given completion-criterion
/// descriptions. Persist it explicitly with `store.create_goal` once its state
/// is final, so a fixture holds at most one active Goal.
fn build_goal(fixture: &Fixture, label: &str, criteria: Vec<String>) -> Goal {
    Goal::new(
        fixture.session.id.clone(),
        fixture.session.cwd.clone(),
        format!("{label} objective"),
        None,
        vec![],
        criteria,
        NOW,
    )
    .unwrap()
}

fn task_value(id: &str, worker: &str) -> Value {
    json!({
        "proposal_id": id,
        "title": format!("Task {id}"),
        "objective": format!("Complete {id}"),
        "mandatory": true,
        "worker": worker,
        "dependencies": [],
        "scope": {
            "allowed_paths": ["."],
            "forbidden_paths": [],
            "operation_kind": "READ_ONLY",
            "replay_safety": "SAFE_READ_ONLY"
        },
        "verification": [{
            "kind": "STRUCTURED_EVIDENCE",
            "requirement_id": format!("evidence.{id}")
        }]
    })
}

fn proposal(goal: &Goal, tasks: Vec<Value>, bindings: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "goal_id": goal.id().as_str(),
        "goal_revision": goal.revision(),
        "summary": "bounded initial plan",
        "tasks": tasks,
        "criterion_bindings": bindings
    }))
    .unwrap()
}

fn binding(criterion_id: &str, task_refs: &[&str]) -> Value {
    json!({
        "criterion_id": criterion_id,
        "task_refs": task_refs
    })
}

fn materialize(
    fixture: &Fixture,
    goal: &Goal,
    proposal: &[u8],
) -> Result<Goal, planner::PlannerError> {
    planner::materialize_initial_plan_output(
        &fixture.store,
        &fixture.session,
        goal.id(),
        goal.revision(),
        proposal,
    )
}

fn schema_violation(result: Result<Goal, planner::PlannerError>) -> String {
    match result {
        Ok(_) => panic!("proposal must be rejected"),
        Err(planner::PlannerError::PlannerSchemaViolation(reason)) => reason,
        Err(other) => panic!("expected a schema violation, got: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Target 1 — CompletionCriterionId binding
// ---------------------------------------------------------------------------

/// Host-side generation and handoff: a Goal without explicit criteria gets one
/// host-synthesized criterion whose id is a host-owned UUID, and exactly that
/// id is handed to the Planner. The model never mints these ids.
#[test]
fn criteria_omitted_synthesizes_one_host_owned_criterion_handed_to_planner() {
    let fixture = fixture("criteria-default");
    let goal = build_goal(&fixture, "default", vec![]);
    let criteria = goal.completion_criteria();
    assert_eq!(criteria.len(), 1);
    assert_eq!(criteria[0].description(), "default objective");
    assert!(
        crate::goal::CompletionCriterionId::parse(criteria[0].id().as_str()).is_ok(),
        "criterion ids are host-owned UUIDs"
    );

    let request = planner::planner_request_for_goal(&goal, &fixture.session).unwrap();
    let request_json = serde_json::to_value(&request).unwrap();
    assert_eq!(
        request_json["completion_criteria"][0]["criterion_id"],
        json!(criteria[0].id().as_str()),
        "the Planner request must carry the exact host criterion id"
    );
    assert_eq!(
        request_json["completion_criteria"][0]["description"],
        json!("default objective")
    );
}

/// The success arm of the observed failure: a proposal that echoes the host
/// criterion id is accepted for one and for several explicit criteria.
#[test]
fn host_criterion_id_echo_is_accepted_for_one_and_many_criteria() {
    for descriptions in [
        vec!["criterion one".to_owned()],
        vec!["criterion one".to_owned(), "criterion two".to_owned()],
    ] {
        let fixture = fixture("criteria-echo");
        let goal = build_goal(&fixture, "echo", descriptions);
        fixture.store.create_goal(&goal).unwrap();
        let tasks = vec![task_value("t1", "CODEX_READONLY")];
        let bindings = goal
            .completion_criteria()
            .iter()
            .map(|criterion| binding(criterion.id().as_str(), &["t1"]))
            .collect();
        let planned = materialize(&fixture, &goal, &proposal(&goal, tasks, bindings)).unwrap();
        assert_eq!(planned.status(), GoalStatus::Running);
        let spec = planned.final_verification_spec().unwrap();
        assert_eq!(
            spec.criterion_bindings().len(),
            planned.completion_criteria().len()
        );
    }
}

/// The observed failure, reproduced: an invented (but well-formed) criterion id
/// is refused with exactly the Dogfooding error message. This is a model output
/// schema violation; the validator must never repair or accept it.
#[test]
fn invented_criterion_id_is_rejected_with_the_observed_error() {
    let fixture = fixture("criteria-invented");
    let goal = build_goal(&fixture, "invented", vec!["criterion one".to_owned()]);
    fixture.store.create_goal(&goal).unwrap();
    let invented = Uuid::new_v4().to_string();
    let proposal = proposal(
        &goal,
        vec![task_value("t1", "CODEX_READONLY")],
        vec![binding(&invented, &["t1"])],
    );
    let reason = schema_violation(materialize(&fixture, &goal, &proposal));
    assert_eq!(
        reason, "criterion binding references an unknown host CompletionCriterionId",
        "the Dogfooding failure message must stay stable"
    );
}

/// A Planner that returns the criterion *description* instead of its id is the
/// most likely real-world shape of the observed failure (the description is
/// what the model reads in the request). It is refused the same way.
#[test]
fn description_used_as_criterion_id_is_rejected() {
    let fixture = fixture("criteria-description");
    let goal = build_goal(
        &fixture,
        "description",
        vec!["host validation passes".to_owned()],
    );
    fixture.store.create_goal(&goal).unwrap();
    let proposal = proposal(
        &goal,
        vec![task_value("t1", "CODEX_READONLY")],
        vec![binding("host validation passes", &["t1"])],
    );
    let reason = schema_violation(materialize(&fixture, &goal, &proposal));
    assert_eq!(
        reason,
        "criterion binding references an unknown host CompletionCriterionId"
    );
}

/// No auto-correction of unknown ids: an alternate spelling of the correct UUID
/// (uppercase) is refused rather than normalized into acceptance.
#[test]
fn alternate_uuid_spelling_is_not_auto_corrected() {
    let fixture = fixture("criteria-spelling");
    let goal = build_goal(&fixture, "spelling", vec!["criterion one".to_owned()]);
    fixture.store.create_goal(&goal).unwrap();
    let canonical = goal.completion_criteria()[0].id().as_str().to_uppercase();
    let proposal = proposal(
        &goal,
        vec![task_value("t1", "CODEX_READONLY")],
        vec![binding(&canonical, &["t1"])],
    );
    let reason = schema_violation(materialize(&fixture, &goal, &proposal));
    assert_eq!(
        reason, "criterion binding references an unknown host CompletionCriterionId",
        "alternate UUID spellings must not be silently canonicalized"
    );
}

#[test]
fn duplicate_criterion_bindings_are_rejected() {
    let fixture = fixture("criteria-duplicate");
    let goal = build_goal(&fixture, "duplicate", vec!["criterion one".to_owned()]);
    fixture.store.create_goal(&goal).unwrap();
    let id = goal.completion_criteria()[0].id().as_str().to_owned();
    let proposal = proposal(
        &goal,
        vec![task_value("t1", "CODEX_READONLY")],
        vec![binding(&id, &["t1"]), binding(&id, &["t1"])],
    );
    let reason = schema_violation(materialize(&fixture, &goal, &proposal));
    assert_eq!(reason, "duplicate criterion binding");
}

#[test]
fn missing_required_criterion_binding_is_rejected() {
    let fixture = fixture("criteria-missing");
    let goal = build_goal(
        &fixture,
        "missing",
        vec!["criterion one".to_owned(), "criterion two".to_owned()],
    );
    fixture.store.create_goal(&goal).unwrap();
    let first = goal.completion_criteria()[0].id().as_str().to_owned();
    let proposal = proposal(
        &goal,
        vec![task_value("t1", "CODEX_READONLY")],
        vec![binding(&first, &["t1"])],
    );
    let reason = schema_violation(materialize(&fixture, &goal, &proposal));
    assert_eq!(
        reason,
        "required completion criterion is missing structured TaskVerified coverage"
    );
}

#[test]
fn criterion_binding_to_unknown_task_is_rejected() {
    let fixture = fixture("criteria-taskref");
    let goal = build_goal(&fixture, "taskref", vec!["criterion one".to_owned()]);
    fixture.store.create_goal(&goal).unwrap();
    let id = goal.completion_criteria()[0].id().as_str().to_owned();
    let proposal = proposal(
        &goal,
        vec![task_value("t1", "CODEX_READONLY")],
        vec![binding(&id, &["ghost"])],
    );
    let reason = schema_violation(materialize(&fixture, &goal, &proposal));
    assert_eq!(
        reason,
        "criterion binding references an unknown proposal-local Task"
    );
}

// ---------------------------------------------------------------------------
// Target 2 — READONLY_BLOCKED stall
// ---------------------------------------------------------------------------

struct ScriptedReadonly {
    status: &'static str,
    summary: String,
    error: Option<AgentError>,
}

impl ReadonlyBackend for ScriptedReadonly {
    fn investigate(&self, request: &ReadonlyRequest) -> Result<Vec<u8>, ReadonlyError> {
        if let Some(error) = self.error {
            return Err(ReadonlyError::Model(error));
        }
        Ok(serde_json::to_vec(&json!({
            "goal_id": request.goal_id(),
            "task_id": request.task_id(),
            "attempt_id": request.attempt_id(),
            "goal_revision": request.goal_revision(),
            "plan_revision": request.plan_revision(),
            "status": self.status,
            "summary": self.summary,
            "evidence": [{"kind": "note", "value": "scripted evidence"}],
        }))
        .unwrap())
    }
}

/// A RUNNING Goal whose only mandatory Task is a READY CODEX_READONLY Task.
fn readonly_ready_goal(fixture: &Fixture) -> (Goal, crate::task::TaskId) {
    let mut goal = build_goal(
        fixture,
        "readonly blocked",
        vec!["investigation provided".to_owned()],
    );
    let task_id = goal
        .add_task(
            "inspect",
            "inspect the tree and report",
            true,
            WorkerKind::CodexReadonly,
            read_only_scope(),
            vec![],
            3,
            NOW,
        )
        .unwrap();
    goal.transition_to(GoalStatus::Running, NOW).unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Ready,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    (goal, task_id)
}

/// Dogfooding case 1 (model proposes `blocked`) reproduced end to end through
/// the real readonly persistence path: the mandatory Task becomes BLOCKED with
/// a mandatory READONLY_BLOCKED blocker, the scheduler reports
/// NoAction{BlockedTasks} instead of a Replan, and the Goal stays RUNNING so
/// `goal_run` reports NO_ACTION.
#[test]
fn readonly_blocked_proposal_blocks_task_without_replan_and_stays_running() {
    let fixture = fixture("readonly-blocked");
    let (goal, task_id) = readonly_ready_goal(&fixture);
    let goal_id = goal.id().clone();
    fixture.store.create_goal(&goal).unwrap();

    let backend = ScriptedReadonly {
        status: "blocked",
        summary: "external prerequisite missing".to_owned(),
        error: None,
    };
    let updated = run_readonly_attempt(
        &fixture.store,
        &fixture.session,
        &goal_id,
        &task_id,
        goal.revision(),
        &backend,
    )
    .unwrap();

    let task = &updated.tasks()[&task_id];
    assert_eq!(task.status(), TaskStatus::Blocked);
    let blocker = task
        .blockers()
        .iter()
        .find(|blocker| blocker.code() == "READONLY_BLOCKED")
        .expect("the READONLY_BLOCKED blocker must be recorded");
    assert!(blocker.mandatory(), "the recorded blocker is mandatory");

    // The Goal stays RUNNING: BLOCKED is never silently terminalized and never
    // converted into NEEDS_REPLAN (the monotonic Replanner contract stays).
    assert_eq!(updated.status(), GoalStatus::Running);
    assert!(!updated.is_terminal());

    // `goal_run` reports NO_ACTION because the scheduler has nothing runnable.
    let decision = select_next_action(&updated).unwrap();
    assert!(
        matches!(
            decision,
            SchedulerDecision::NoAction {
                reason: SchedulerNoActionReason::BlockedTasks
            }
        ),
        "expected NoAction{{BlockedTasks}}, got: {decision:?}"
    );

    // The stall is durable: a fresh store view sees the same blocked state.
    let reloaded = fixture
        .store
        .load_goal(&fixture.session.id, &goal_id)
        .unwrap();
    assert_eq!(reloaded.tasks()[&task_id].status(), TaskStatus::Blocked);
    assert!(
        reloaded.tasks()[&task_id]
            .blockers()
            .iter()
            .any(|blocker| blocker.code() == "READONLY_BLOCKED" && blocker.mandatory())
    );
}

/// A transport/backend failure is a *different* cause than a model `blocked`
/// proposal: a backend error surfaces as an error and is persisted on the
/// retryable failure path, never as a `READONLY_BLOCKED` model verdict. Cause
/// separation is what lets the External Backend tell policy/env/approval/model
/// causes apart later.
#[test]
fn readonly_backend_error_is_not_recorded_as_readonly_blocked() {
    let fixture = fixture("readonly-backend-error");
    let (goal, task_id) = readonly_ready_goal(&fixture);
    let goal_id = goal.id().clone();
    fixture.store.create_goal(&goal).unwrap();

    let backend = ScriptedReadonly {
        status: "blocked",
        summary: String::new(),
        error: Some(AgentError::TransportFailure),
    };
    let result = run_readonly_attempt(
        &fixture.store,
        &fixture.session,
        &goal_id,
        &task_id,
        goal.revision(),
        &backend,
    );
    assert!(result.is_err(), "a backend error surfaces as an error");

    let reloaded = fixture
        .store
        .load_goal(&fixture.session.id, &goal_id)
        .unwrap();
    let task = &reloaded.tasks()[&task_id];
    assert!(
        !task
            .blockers()
            .iter()
            .any(|blocker| blocker.code() == "READONLY_BLOCKED"),
        "a backend failure must not masquerade as a model `blocked` proposal"
    );
}

/// A goal-level BLOCKED state (e.g. cancel reconciliation) also reports
/// NO_ACTION from the scheduler with its own typed reason, so `goal_run`
/// consumers can tell goal-level BLOCKED from task-level stalls.
#[test]
fn goal_level_blocked_reports_goal_blocked_no_action() {
    let fixture = fixture("goal-blocked");
    let mut goal = build_goal(&fixture, "goal blocked", vec![]);
    goal.transition_to(GoalStatus::Blocked, NOW).unwrap();
    fixture.store.create_goal(&goal).unwrap();
    let decision = select_next_action(&goal).unwrap();
    assert!(matches!(
        decision,
        SchedulerDecision::NoAction {
            reason: SchedulerNoActionReason::GoalBlocked
        }
    ));
}

// ---------------------------------------------------------------------------
// Target 3 — additive result discrimination
// ---------------------------------------------------------------------------

/// Mirror of the goal_api test fixture's completed-goal construction, used to
/// reach the Finalizer-reserved COMPLETED state through documented authorities.
fn completed_goal(fixture: &Fixture) -> Goal {
    let mut goal = build_goal(fixture, "completed", vec![]);
    let task_id = goal
        .add_task(
            "complete task",
            "complete task objective",
            true,
            WorkerKind::Verifier,
            read_only_scope(),
            vec![],
            1,
            NOW,
        )
        .unwrap();
    goal.transition_to(GoalStatus::Running, NOW).unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Ready,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Running,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Verifying,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.task_record_verification_result(
        &task_id,
        VerificationResult::new(VerificationOutcome::Passed, vec![], NOW, NOW),
    )
    .unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Completed,
        TaskTransitionContext {
            active_worker_stopped: true,
            side_effect_reconciled: true,
        },
        NOW,
    )
    .unwrap();
    goal.enter_verifying_for_test(NOW).unwrap();
    goal.record_final_verification(VerificationResult::new(
        VerificationOutcome::Passed,
        vec![],
        NOW,
        NOW,
    ))
    .unwrap();
    goal.add_checkpoint(CheckpointReason::FinalVerification, NOW)
        .unwrap();
    crate::goal_finalizer::complete_goal_for_test(&mut goal, NOW).unwrap();
    goal
}

/// The frozen `result_state` contract is preserved (`TERMINAL` /
/// `NOT_TERMINAL`) while the additive `result_kind` discriminates the five
/// outcomes an External Backend needs.
#[test]
fn result_kind_discriminates_five_outcomes_without_touching_result_state() {
    // NONTERMINAL: an active goal.
    let first = fixture("result-nonterminal");
    let running = build_goal(&first, "nonterminal", vec![]);
    let running_id = running.id().clone();
    first.store.create_goal(&running).unwrap();
    assert_result(&first, &running_id, "NONTERMINAL", "NOT_TERMINAL");

    // CANCELLED: terminalized through the public cancel flow.
    goal_api::goal_cancel(
        &json!({"session_id": first.session.id}),
        &first.session,
        &first.store,
    )
    .unwrap();
    assert_result(&first, &running_id, "CANCELLED", "TERMINAL");

    // BLOCKED: a goal reconciled into the goal-level blocked state.
    let second = fixture("result-blocked");
    let mut blocked = build_goal(&second, "blocked", vec![]);
    blocked.transition_to(GoalStatus::Blocked, NOW).unwrap();
    let blocked_id = blocked.id().clone();
    second.store.create_goal(&blocked).unwrap();
    assert_result(&second, &blocked_id, "BLOCKED", "NOT_TERMINAL");

    // FAILED: host-declared failure.
    let third = fixture("result-failed");
    let mut failed = build_goal(&third, "failed", vec![]);
    failed.transition_to(GoalStatus::Failed, NOW).unwrap();
    let failed_id = failed.id().clone();
    third.store.create_goal(&failed).unwrap();
    assert_result(&third, &failed_id, "FAILED", "TERMINAL");

    // COMPLETED: through the Finalizer authority path.
    let fourth = fixture("result-completed");
    let completed = completed_goal(&fourth);
    let completed_id = completed.id().clone();
    fourth.store.create_goal(&completed).unwrap();
    assert_result(&fourth, &completed_id, "COMPLETED", "TERMINAL");
}

fn assert_result(fixture: &Fixture, goal_id: &crate::goal::GoalId, kind: &str, state: &str) {
    let view = goal_api::goal_result(
        &json!({"session_id": fixture.session.id, "goal_id": goal_id.as_str()}),
        &fixture.session,
        &fixture.store,
    )
    .unwrap();
    let value = serde_json::to_value(&view).unwrap();
    assert_eq!(
        value["result_kind"],
        json!(kind),
        "goal {}",
        goal_id.as_str()
    );
    assert_eq!(
        value["result_state"],
        json!(state),
        "goal {}",
        goal_id.as_str()
    );
}

/// The status→kind mapping itself, pinned exhaustively.
#[test]
fn result_kind_mapping_is_pinned_for_every_goal_status() {
    let cases = [
        (GoalStatus::Planning, "NONTERMINAL"),
        (GoalStatus::Running, "NONTERMINAL"),
        (GoalStatus::Replanning, "NONTERMINAL"),
        (GoalStatus::Pausing, "NONTERMINAL"),
        (GoalStatus::Paused, "NONTERMINAL"),
        (GoalStatus::Verifying, "NONTERMINAL"),
        (GoalStatus::Cancelling, "NONTERMINAL"),
        (GoalStatus::Blocked, "BLOCKED"),
        (GoalStatus::Completed, "COMPLETED"),
        (GoalStatus::Failed, "FAILED"),
        (GoalStatus::Cancelled, "CANCELLED"),
    ];
    for (status, expected) in cases {
        assert_eq!(goal_api::result_kind_for_status(status), expected);
    }
}
