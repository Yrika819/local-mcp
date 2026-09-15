use std::sync::{Arc, Mutex};

use crate::agent::{AgentError, ModelInvocation, ModelInvocationOutput, ModelRole, ModelTransport};
use crate::config;
use crate::goal_backends::ProductionGoalBackends;
use crate::planner::{PlannerBackend, PlannerError, planner_request_for_goal};
use crate::readonly_worker::{
    ReadonlyBackend, ReadonlyError, readonly_request_for_model_backend_test,
};
use crate::replanner::{
    ReplannerBackend, ReplannerError, replanner_request_for_model_backend_test,
};
use crate::writer::{
    ReviewerBackend, WriterBackend, WriterError, reviewer_request_for_model_backend_test,
    writer_request_for_model_backend_test,
};

struct FakeModel {
    calls: Mutex<Vec<ModelInvocation>>,
    result: Result<ModelInvocationOutput, AgentError>,
}

impl FakeModel {
    fn success(response: impl Into<Vec<u8>>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            result: Ok(ModelInvocationOutput::new(
                response.into(),
                String::new(),
                0,
            )),
        }
    }

    fn failure(error: AgentError) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            result: Err(error),
        }
    }
}

impl ModelTransport for FakeModel {
    fn invoke(&self, request: &ModelInvocation) -> Result<ModelInvocationOutput, AgentError> {
        self.calls.lock().unwrap().push(request.clone());
        self.result.clone()
    }
}

fn session() -> config::Session {
    let cwd = std::env::current_dir().unwrap();
    config::Session {
        id: "backend-test".into(),
        cwd: cwd.clone(),
        permitted_directories: vec![cwd],
    }
}

fn planner_request(session: &config::Session) -> crate::planner::PlannerRequest {
    let goal = crate::goal::Goal::new(
        session.id.clone(),
        session.cwd.clone(),
        "objective",
        None,
        vec![],
        vec![],
        "2026-01-01T00:00:00Z",
    )
    .unwrap();
    planner_request_for_goal(&goal, session).unwrap()
}

#[test]
fn all_production_adapters_use_fixed_roles_and_return_exact_raw_model_bytes() {
    let session = session();
    let raw = b"raw bytes\0\n".to_vec();
    let fake = Arc::new(FakeModel::success(raw.clone()));
    let backends = ProductionGoalBackends::with_transport(&session, fake.clone());

    assert_eq!(
        backends
            .planner()
            .propose_initial_plan(&planner_request(&session))
            .unwrap(),
        raw
    );
    assert_eq!(
        backends
            .readonly()
            .investigate(&readonly_request_for_model_backend_test(
                session.cwd.clone()
            ))
            .unwrap(),
        raw
    );
    assert_eq!(
        backends
            .writer()
            .propose(&writer_request_for_model_backend_test(session.cwd.clone()))
            .unwrap(),
        raw
    );
    assert_eq!(
        backends
            .reviewer()
            .review(&reviewer_request_for_model_backend_test())
            .unwrap(),
        raw
    );
    assert_eq!(
        backends
            .replanner()
            .propose_replan(&replanner_request_for_model_backend_test(
                session.cwd.clone()
            ))
            .unwrap(),
        raw
    );

    let calls = fake.calls.lock().unwrap();
    assert_eq!(calls.len(), 5);
    assert_eq!(
        calls.iter().map(ModelInvocation::role).collect::<Vec<_>>(),
        vec![
            ModelRole::Planner,
            ModelRole::Readonly,
            ModelRole::Writer,
            ModelRole::Reviewer,
            ModelRole::Replanner,
        ]
    );
    let canonical_session_cwd = std::fs::canonicalize(&session.cwd).unwrap();
    assert!(
        calls
            .iter()
            .all(|call| { std::fs::canonicalize(call.cwd()).unwrap() == canonical_session_cwd })
    );
    for call in calls.iter() {
        assert!(call.prompt().contains("untrusted request data"));
        assert!(call.prompt().contains("Return JSON only"));
        assert!(call.prompt().contains("no tools are available"));
    }
    assert!(calls[0].prompt().contains("SAFE_READ_ONLY"));
    assert!(calls[0].prompt().contains("VERIFY_BEFORE_RETRY"));
    assert!(calls[0].prompt().contains("NEVER_AUTOMATIC"));
    assert!(calls[0].prompt().contains("command:[\"argv0\",\"arg1\"]"));
    assert!(
        calls[0]
            .prompt()
            .contains("requires 1..=64 non-empty paths")
    );
    assert!(
        calls[0]
            .prompt()
            .contains("Every Task requires at least one verification entry")
    );
    assert!(calls[1].prompt().contains("read-only investigator"));
    assert!(
        calls[1]
            .prompt()
            .contains("read-only shell/inspection capability")
    );
    assert!(
        calls[1]
            .prompt()
            .contains("future host-owned Verifier checks")
    );
    assert!(calls[1].prompt().contains("needs_replan"));
    assert!(calls[1].prompt().contains("no writes"));
    assert!(calls[2].prompt().contains("You are read-only"));
    assert!(
        calls[3]
            .prompt()
            .contains("blocking_findings MUST be one non-negative JSON integer")
    );
    assert!(calls[3].prompt().contains("never an array"));
    assert!(calls[3].prompt().contains("evidence MUST be a JSON array"));
}

#[test]
fn all_adapter_model_failures_map_once_without_retry_or_repair() {
    let session = session();
    let fake = Arc::new(FakeModel::failure(AgentError::Timeout));
    let backends = ProductionGoalBackends::with_transport(&session, fake.clone());

    assert!(matches!(
        backends
            .planner()
            .propose_initial_plan(&planner_request(&session))
            .unwrap_err(),
        PlannerError::Model(AgentError::Timeout)
    ));
    assert!(matches!(
        backends
            .readonly()
            .investigate(&readonly_request_for_model_backend_test(
                session.cwd.clone()
            ))
            .unwrap_err(),
        ReadonlyError::Model(AgentError::Timeout)
    ));
    assert!(matches!(
        backends
            .writer()
            .propose(&writer_request_for_model_backend_test(session.cwd.clone()))
            .unwrap_err(),
        WriterError::Model(AgentError::Timeout)
    ));
    assert!(matches!(
        backends
            .reviewer()
            .review(&reviewer_request_for_model_backend_test())
            .unwrap_err(),
        WriterError::Model(AgentError::Timeout)
    ));
    assert!(matches!(
        backends
            .replanner()
            .propose_replan(&replanner_request_for_model_backend_test(
                session.cwd.clone()
            ))
            .unwrap_err(),
        ReplannerError::Model(AgentError::Timeout)
    ));
    assert_eq!(fake.calls.lock().unwrap().len(), 5);
}

#[test]
fn production_composition_is_inert_and_backends_are_proposal_only() {
    let session = session();
    let fake = Arc::new(FakeModel::success(b"{}".to_vec()));
    let _backends = ProductionGoalBackends::with_transport(&session, fake.clone());
    assert!(fake.calls.lock().unwrap().is_empty());

    let source = include_str!("goal_backends.rs");
    for forbidden in [
        "write_file(",
        "std::fs::write",
        "execution::",
        "scheduler::",
        "goal_runner::",
        "transition_task",
        "complete_goal",
    ] {
        assert!(
            !source.contains(forbidden),
            "production backend contains forbidden authority token {forbidden}"
        );
    }
}
