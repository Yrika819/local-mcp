use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::agent::{AgentError, ModelInvocation, ModelInvocationOutput, ModelRole, ModelTransport};
use crate::config;
use crate::fallback::{SideEffectClass, SideEffectState};
use crate::goal::{Goal, GoalStatus};
use crate::goal_backends::ProductionGoalBackends;
use crate::mutation::{
    FileObservation, MutationIntent, MutationIntentState, MutationOperationIntent, MutationPreimage,
};
use crate::mutation_recovery;
use crate::planner::{PlannerBackend, planner_request_for_goal};
use crate::readonly_worker::{
    ReadonlyBackend, ReadonlyError, readonly_request_for_model_backend_test,
};
use crate::scheduler::{SchedulerDecision, select_next_action};
use crate::task::{
    ReplaySafety, Task, TaskOperationKind, TaskScope, TaskStatus, TaskTransitionContext,
    VerificationSpec, WorkerKind,
};
use crate::task_store::TaskStore;
use crate::writer::{WriterBackend, WriterError, writer_request_for_model_backend_test};

const NOW: &str = "2026-09-16T00:00:00Z";

struct ScriptedTransport {
    calls: Mutex<Vec<ModelInvocation>>,
    script: Mutex<VecDeque<Result<ModelInvocationOutput, AgentError>>>,
}

impl ScriptedTransport {
    fn new(script: impl IntoIterator<Item = Result<ModelInvocationOutput, AgentError>>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            script: Mutex::new(script.into_iter().collect()),
        }
    }
}

impl ModelTransport for ScriptedTransport {
    fn invoke(&self, request: &ModelInvocation) -> Result<ModelInvocationOutput, AgentError> {
        self.calls.lock().unwrap().push(request.clone());
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .expect("synthetic transport script must cover every invocation")
    }
}

fn output(value: &str) -> Result<ModelInvocationOutput, AgentError> {
    Ok(ModelInvocationOutput::new(
        value.as_bytes().to_vec(),
        String::new(),
        0,
    ))
}

fn session(id: &str, cwd: PathBuf) -> config::Session {
    config::Session {
        id: id.to_owned(),
        cwd: cwd.clone(),
        permitted_directories: vec![cwd],
    }
}

fn scope(repo: &std::path::Path, worker: WorkerKind) -> TaskScope {
    let (operation, replay) = if worker == WorkerKind::CodexReadonly {
        (TaskOperationKind::ReadOnly, ReplaySafety::SafeReadOnly)
    } else {
        (
            TaskOperationKind::LocalMutation,
            ReplaySafety::VerifyBeforeRetry,
        )
    };
    TaskScope::new(
        vec![repo.to_path_buf()],
        vec![repo.join(".git")],
        operation,
        replay,
    )
}

fn synthetic_goal(
    root: &std::path::Path,
    worker: WorkerKind,
) -> (config::Session, TaskStore, Goal, crate::task::TaskId) {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("sentinel.txt"), b"unchanged\n").unwrap();
    let session = session(&format!("hardening-{}", uuid::Uuid::new_v4()), repo.clone());
    let store = TaskStore::with_state_root(root.join("state"));
    let mut goal = Goal::new(
        session.id.clone(),
        repo.clone(),
        "synthetic hardening objective",
        Some("synthetic hardening fixture".to_owned()),
        vec!["preserve authority".to_owned()],
        vec!["host verification remains required".to_owned()],
        NOW,
    )
    .unwrap();
    let task = Task::new(
        "synthetic task",
        "exercise one isolated state transition",
        true,
        worker,
        scope(&repo, worker),
        vec![VerificationSpec::FileExists {
            path: repo.join("sentinel.txt"),
            must_be_file: true,
        }],
        2,
        1,
        NOW,
    )
    .unwrap();
    let task_id = task.id().clone();
    goal.materialize_initial_plan(vec![task], NOW).unwrap();
    (session, store, goal, task_id)
}

#[test]
fn synthetic_fault_script_is_ordered_and_preserves_role_contracts() {
    let cwd = std::env::current_dir().unwrap();
    let session = session("hardening-transport", cwd.clone());
    let transport = Arc::new(ScriptedTransport::new([
        output("planner"),
        Err(AgentError::Timeout),
        output("writer"),
    ]));
    let backends = ProductionGoalBackends::with_transport(&session, transport.clone());

    assert_eq!(
        backends
            .planner()
            .propose_initial_plan(
                &planner_request_for_goal(
                    &Goal::new(
                        session.id.clone(),
                        cwd.clone(),
                        "objective",
                        None,
                        vec![],
                        vec![],
                        NOW
                    )
                    .unwrap(),
                    &session,
                )
                .unwrap()
            )
            .unwrap(),
        b"planner"
    );
    let readonly_error = backends
        .readonly()
        .investigate(&readonly_request_for_model_backend_test(cwd.clone()))
        .unwrap_err();
    assert!(matches!(
        readonly_error,
        ReadonlyError::Model(AgentError::Timeout)
    ));
    assert_eq!(
        backends
            .writer()
            .propose(&writer_request_for_model_backend_test(cwd))
            .unwrap(),
        b"writer"
    );

    let calls = transport.calls.lock().unwrap();
    assert_eq!(
        calls.iter().map(ModelInvocation::role).collect::<Vec<_>>(),
        vec![ModelRole::Planner, ModelRole::Readonly, ModelRole::Writer,]
    );
    assert!(
        calls
            .iter()
            .all(|call| call.prompt().contains("Return JSON only"))
    );
}

#[test]
fn synthetic_writer_fault_is_typed_without_host_mutation() {
    let cwd = std::env::current_dir().unwrap();
    let session = session("hardening-writer-fault", cwd.clone());
    let transport = Arc::new(ScriptedTransport::new([Err(AgentError::NonZeroExit)]));
    let backends = ProductionGoalBackends::with_transport(&session, transport);
    let error = backends
        .writer()
        .propose(&writer_request_for_model_backend_test(cwd))
        .unwrap_err();
    assert!(matches!(error, WriterError::Model(AgentError::NonZeroExit)));
}

#[test]
fn scheduler_decision_survives_durable_reload_for_readonly_and_writer_routes() {
    for worker in [WorkerKind::CodexReadonly, WorkerKind::CodexWriter] {
        let root =
            std::env::temp_dir().join(format!("local-mcp-hardening-{}", uuid::Uuid::new_v4()));
        let (session, store, goal, task_id) = synthetic_goal(&root, worker);
        let expected = match worker {
            WorkerKind::CodexReadonly => SchedulerDecision::RunReadonly {
                task_id: task_id.clone(),
            },
            WorkerKind::CodexWriter => SchedulerDecision::RunWriter {
                task_id: task_id.clone(),
            },
            _ => unreachable!(),
        };
        assert_eq!(select_next_action(&goal).unwrap(), expected);
        store.create_goal(&goal).unwrap();
        let reloaded = store.load_goal(&session.id, goal.id()).unwrap();
        assert_eq!(select_next_action(&reloaded).unwrap(), expected);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn crash_reload_with_unknown_writer_side_effect_is_blocked_and_not_replayed() {
    let root = std::env::temp_dir().join(format!("local-mcp-hardening-{}", uuid::Uuid::new_v4()));
    let (session, store, goal, task_id) = synthetic_goal(&root, WorkerKind::CodexWriter);
    store.create_goal(&goal).unwrap();
    let running = store
        .mutate_goal_snapshot(&session.id, goal.id(), goal.revision(), |goal, now| {
            goal.transition_task(
                &task_id,
                TaskStatus::Running,
                TaskTransitionContext::default(),
                now,
            )?;
            goal.task_bind_latest_attempt_execution(
                &task_id,
                Some("writer-op".to_owned()),
                Some("writer-scope".to_owned()),
                Some("writer-request".to_owned()),
                Some(SideEffectClass::LocalMutation),
                Some(SideEffectState::Unknown),
                Some(1),
                Some(1),
            )
        })
        .unwrap();
    let recovered = store
        .recover_goal(&session.id, goal.id(), running.revision())
        .unwrap();
    assert_eq!(recovered.status(), GoalStatus::Blocked);
    let task = recovered.tasks().get(&task_id).unwrap();
    assert_eq!(task.status(), TaskStatus::Blocked);
    assert_eq!(
        task.blockers().last().unwrap().code(),
        "RECOVERY_RECONCILIATION_REQUIRED"
    );
    assert_eq!(task.attempts().len(), 1);
    assert_eq!(
        task.latest_attempt().unwrap().side_effect_state(),
        Some(SideEffectState::Unknown)
    );
    assert!(task.latest_attempt().unwrap().operation_id().is_some());
    let reloaded = store.load_goal(&session.id, goal.id()).unwrap();
    assert_eq!(reloaded, recovered);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn durable_writer_reconciliation_records_confirmed_not_performed_after_reload() {
    let root = std::env::temp_dir().join(format!("local-mcp-reconcile-{}", uuid::Uuid::new_v4()));
    let (session, store, goal, task_id) = synthetic_goal(&root, WorkerKind::CodexWriter);
    store.create_goal(&goal).unwrap();
    let prepared = store
        .mutate_goal_snapshot(&session.id, goal.id(), goal.revision(), |goal, now| {
            goal.transition_task(
                &task_id,
                TaskStatus::Running,
                TaskTransitionContext::default(),
                now,
            )?;
            let target = std::fs::canonicalize(&session.cwd)
                .unwrap()
                .join("reconcile.txt");
            let intent = MutationIntent::new(
                "operation-reload".to_owned(),
                "scope-reload".to_owned(),
                vec![MutationOperationIntent::new(
                    0,
                    target,
                    MutationPreimage::Absent,
                    FileObservation::absent(),
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                    "request-reload".to_owned(),
                )],
            )
            .unwrap();
            goal.task_prepare_latest_mutation_intent(&task_id, intent)
        })
        .unwrap();
    let mut reloaded = store.load_goal(&session.id, prepared.id()).unwrap();
    assert!(mutation_recovery::reconcile_goal_mutations(&mut reloaded, NOW).unwrap());
    let task = reloaded.tasks().get(&task_id).unwrap();
    assert_eq!(
        task.latest_attempt()
            .unwrap()
            .mutation_intent()
            .unwrap()
            .state(),
        MutationIntentState::ReconciledNotPerformed
    );
    assert!(task.evidence().iter().any(|evidence| matches!(
        evidence,
        crate::task::TaskEvidence::MutationReconciliation { .. }
    )));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn synthetic_campaign_manifest_has_thirty_unique_bounded_cases() {
    const CASES: [&str; 30] = [
        "readonly-only-success",
        "writer-only-success",
        "readonly-timeout",
        "readonly-transport-failure",
        "readonly-malformed-output",
        "readonly-blocked",
        "readonly-needs-replan",
        "readonly-failed",
        "readonly-exhausted-budget",
        "writer-safe-pre-mutation-retry",
        "writer-review-block",
        "writer-stale-preimage",
        "writer-forbidden-path",
        "writer-symlink-escape",
        "writer-postimage-mismatch",
        "writer-confirmed-performed-recovery",
        "writer-unknown-side-effect-recovery",
        "writer-exhausted-budget",
        "writer-lease-held",
        "reviewer-schema-rejection",
        "planner-schema-rejection",
        "replanner-monotonic-addition",
        "replanner-trigger-preservation",
        "dependency-wait",
        "readiness-propagation",
        "paused-no-action",
        "terminal-no-action",
        "goal-blocked-no-action",
        "goal-finalizer-gate",
        "long-chain-reload",
    ];
    let unique = CASES.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(unique.len(), CASES.len());
    assert!(
        CASES
            .iter()
            .all(|case| !case.is_empty() && case.len() <= 64)
    );
}
