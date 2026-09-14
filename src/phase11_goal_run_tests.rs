use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use serde_json::json;
use uuid::Uuid;

use crate::agent::{AgentError, ModelInvocation, ModelInvocationOutput, ModelTransport};
use crate::config;
use crate::goal::{GoalId, GoalStatus};
use crate::goal_backends::ProductionGoalBackends;
use crate::goal_finalizer::GoalFinalizationOutcome;
use crate::goal_runner::{
    GoalRunLimits, GoalRunResult, GoalRunStopReason, GoalRunTraceAction, GoalRunTraceEntry,
    GoalRunTraceOutcome, GoalRunnerAuthority,
};
use crate::scheduler::SchedulerNoActionReason;
use crate::task::WorkerKind;
use crate::task_store::TaskStore;

#[test]
fn phase11_catalog_has_exactly_one_additive_goal_run_tool() {
    let catalog = crate::mcp::tools();
    let names = catalog
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names.len(), 18);
    assert_eq!(names.iter().filter(|name| **name == "goal_run").count(), 1);
    let goal_run = catalog
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "goal_run")
        .unwrap();
    assert_eq!(goal_run["inputSchema"]["additionalProperties"], false);
    assert_eq!(
        goal_run["inputSchema"]["required"],
        json!(["session_id", "goal_id", "max_steps"])
    );
    assert_eq!(
        goal_run["inputSchema"]["properties"]["max_steps"]["minimum"],
        1
    );
    assert_eq!(
        goal_run["inputSchema"]["properties"]["max_steps"]["maximum"],
        256
    );
}

#[test]
fn goal_run_arguments_are_strict_and_require_a_bounded_integer() {
    let base_goal = "00000000-0000-4000-8000-000000000001";
    for args in [
        json!({"session_id":"s","goal_id":base_goal}),
        json!({"session_id":"s","goal_id":base_goal,"max_steps":0}),
        json!({"session_id":"s","goal_id":base_goal,"max_steps":257}),
        json!({"session_id":"s","goal_id":base_goal,"max_steps":-1}),
        json!({"session_id":"s","goal_id":base_goal,"max_steps":1.5}),
        json!({"session_id":"s","goal_id":base_goal,"max_steps":"1"}),
        json!({"session_id":"s","goal_id":base_goal,"max_steps":null}),
        json!({"session_id":"s","goal_id":base_goal,"max_steps":1,"unknown":true}),
    ] {
        assert!(crate::mcp::parse_goal_run_args(&args).is_err());
    }
    for forbidden in [
        "cwd",
        "root",
        "scope",
        "worker",
        "provider",
        "model",
        "prompt",
        "backend",
        "without_sandbox",
        "approval",
        "task_state",
        "goal_state",
        "verification_result",
    ] {
        let mut args = json!({"session_id":"s","goal_id":base_goal,"max_steps":1});
        args.as_object_mut()
            .unwrap()
            .insert(forbidden.to_owned(), json!("forbidden"));
        assert!(crate::mcp::parse_goal_run_args(&args).is_err());
    }
    for max_steps in [1, 256] {
        let args = json!({"session_id":"s","goal_id":base_goal,"max_steps":max_steps});
        assert_eq!(
            crate::mcp::parse_goal_run_args(&args).unwrap().max_steps,
            max_steps
        );
    }
}

#[test]
fn goal_run_serialization_is_safe_and_trace_is_bounded_by_the_runner_limit() {
    let goal_id = GoalId::parse("00000000-0000-4000-8000-000000000001").unwrap();
    let trace = (1..=256)
        .map(|step_index| GoalRunTraceEntry {
            step_index,
            action: GoalRunTraceAction::Scheduler(None),
            task_id: None,
            revision_before: step_index as u64,
            revision_after: step_index as u64,
            outcome: GoalRunTraceOutcome::NoAction(SchedulerNoActionReason::NoEligibleAction),
        })
        .collect();
    let value: serde_json::Value =
        serde_json::from_str(&crate::mcp::serialize_goal_run_result(&GoalRunResult {
            goal_id,
            revision_before: Some(1),
            revision_after: Some(1),
            steps_attempted: 256,
            steps_applied: 0,
            terminal_status: Some(GoalStatus::Paused),
            stop_reason: GoalRunStopReason::Paused,
            trace,
        }))
        .unwrap();
    assert_eq!(value["terminal_status"], "PAUSED");
    assert_eq!(value["stop_reason"], "PAUSED");
    assert_eq!(value["trace"].as_array().unwrap().len(), 256);
    for field in [
        "goal_id",
        "revision_before",
        "revision_after",
        "steps_attempted",
        "steps_applied",
        "terminal_status",
        "stop_reason",
        "trace",
    ] {
        assert!(value.get(field).is_some(), "missing safe result field {field}");
    }
    let serialized = serde_json::to_string(&value).unwrap();
    for forbidden in ["prompt", "stderr", "provider", "model", "without_sandbox"] {
        assert!(!serialized.contains(forbidden));
    }
}

fn serialized_stop_reason(reason: GoalRunStopReason) -> String {
    let result = GoalRunResult {
        goal_id: GoalId::parse("00000000-0000-4000-8000-000000000001").unwrap(),
        revision_before: Some(1),
        revision_after: Some(1),
        steps_attempted: 0,
        steps_applied: 0,
        terminal_status: None,
        stop_reason: reason,
        trace: Vec::new(),
    };
    serde_json::from_str::<serde_json::Value>(&crate::mcp::serialize_goal_run_result(&result))
        .unwrap()["stop_reason"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn goal_run_preserves_all_phase10_stop_reason_categories() {
    let cases = [
        (GoalRunStopReason::Completed, "COMPLETED"),
        (GoalRunStopReason::Failed, "FAILED"),
        (GoalRunStopReason::Cancelled, "CANCELLED"),
        (GoalRunStopReason::Paused, "PAUSED"),
        (
            GoalRunStopReason::ControlState(GoalStatus::Pausing),
            "CONTROL_STATE_PAUSING",
        ),
        (GoalRunStopReason::Blocked, "BLOCKED"),
        (
            GoalRunStopReason::NoAction(SchedulerNoActionReason::NoEligibleAction),
            "NO_ACTION",
        ),
        (
            GoalRunStopReason::UnsupportedWorker(WorkerKind::CodexReadonly),
            "UNSUPPORTED_WORKER",
        ),
        (
            GoalRunStopReason::RevisionConflict {
                expected: 1,
                actual: 2,
            },
            "REVISION_CONFLICT",
        ),
        (
            GoalRunStopReason::LowerAuthorityError {
                authority: GoalRunnerAuthority::Writer,
                detail: "safe internal detail".to_owned(),
            },
            "LOWER_AUTHORITY_ERROR",
        ),
        (GoalRunStopReason::StepBudgetExhausted, "STEP_BUDGET_EXHAUSTED"),
        (
            GoalRunStopReason::NoProgress {
                action: GoalRunTraceAction::Scheduler(None),
            },
            "NO_PROGRESS",
        ),
        (
            GoalRunStopReason::FinalizationNotReady(GoalFinalizationOutcome::NotReady),
            "FINALIZATION_NOT_READY",
        ),
    ];
    for (reason, expected) in cases {
        assert_eq!(serialized_stop_reason(reason), expected);
    }
}

#[derive(Default)]
struct CountingModel {
    calls: AtomicUsize,
    response: Vec<u8>,
}

impl CountingModel {
    fn with_response(response: impl Into<Vec<u8>>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            response: response.into(),
        }
    }
}

impl ModelTransport for CountingModel {
    fn invoke(&self, _request: &ModelInvocation) -> Result<ModelInvocationOutput, AgentError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ModelInvocationOutput::new(
            self.response.clone(),
            String::new(),
            0,
        ))
    }
}

fn phase11_fixture() -> (std::path::PathBuf, config::Session, TaskStore, GoalId) {
    let cwd = std::env::current_dir().unwrap();
    let root = cwd
        .join("target")
        .join(format!("phase11-goal-run-{}", Uuid::new_v4()));
    let session = config::Session {
        id: format!("phase11-{}", Uuid::new_v4()),
        cwd: cwd.clone(),
        permitted_directories: vec![cwd],
    };
    let store = TaskStore::with_state_root(root.clone());
    let started = crate::goal_api::goal_start(
        &json!({"session_id":session.id,"objective":"phase11 fixture"}),
        &session,
        &store,
    )
    .unwrap();
    let started = serde_json::to_value(started).unwrap();
    let goal_id = GoalId::parse(started["goal_id"].as_str().unwrap()).unwrap();
    (root, session, store, goal_id)
}

#[tokio::test]
async fn terminal_goal_uses_real_production_composition_without_model_invocation() {
    let (root, session, store, goal_id) = phase11_fixture();
    crate::goal_api::goal_cancel(
        &json!({"session_id":session.id,"goal_id":goal_id.as_str()}),
        &session,
        &store,
    )
    .unwrap();

    let model = Arc::new(CountingModel::with_response(b"must not be called".to_vec()));
    let backends = ProductionGoalBackends::with_transport(&session, model.clone());
    let result = crate::goal_runner::run_goal_foreground(
        &store,
        &session,
        &goal_id,
        GoalRunLimits::new(1).unwrap(),
        backends.planner(),
        backends.writer(),
        backends.reviewer(),
        backends.replanner(),
    )
    .await;
    assert_eq!(result.stop_reason, GoalRunStopReason::Cancelled);
    assert_eq!(result.steps_attempted, 0);
    assert_eq!(result.steps_applied, 0);
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn max_steps_one_dispatches_at_most_one_production_model_action() {
    let (root, session, store, goal_id) = phase11_fixture();
    let model = Arc::new(CountingModel::with_response(b"{}".to_vec()));
    let backends = ProductionGoalBackends::with_transport(&session, model.clone());
    let result = crate::goal_runner::run_goal_foreground(
        &store,
        &session,
        &goal_id,
        GoalRunLimits::new(1).unwrap(),
        backends.planner(),
        backends.writer(),
        backends.reviewer(),
        backends.replanner(),
    )
    .await;
    assert_eq!(result.steps_attempted, 1);
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        result.stop_reason,
        GoalRunStopReason::LowerAuthorityError {
            authority: GoalRunnerAuthority::Planner,
            ..
        }
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn goal_run_is_the_only_public_runner_call_site_and_has_no_background_path() {
    let source = include_str!("mcp.rs");
    assert_eq!(source.matches("goal_runner::run_goal_foreground").count(), 1);
    let body = source
        .split("async fn goal_run")
        .nth(1)
        .unwrap()
        .split("pub(crate) fn parse_goal_run_args")
        .next()
        .unwrap();
    for forbidden in [
        "Job",
        "tokio::spawn",
        "thread::spawn",
        "without_sandbox",
        "start_command",
        "poll_job",
    ] {
        assert!(!body.contains(forbidden), "goal_run contains {forbidden}");
    }
}
