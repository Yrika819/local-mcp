use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use uuid::Uuid;

use crate::config;
use crate::fallback::{SideEffectClass, SideEffectState};
use crate::goal::{
    CompletionCriterionId, GOAL_SCHEMA_VERSION, Goal, GoalBlocker, GoalCriterionBinding,
    GoalFinalVerificationSpec, GoalStatus, GoalVerificationRequirement, ReplanMutation,
};
use crate::goal_api;
use crate::goal_finalizer::{self, GoalFinalizationOutcome};
use crate::goal_verifier::{
    GoalVerifierError, commit_goal_verification, evaluate_goal_verification,
    prepare_goal_verification, verify_goal,
};
use crate::orchestrator_error::OrchestratorError;
use crate::planner::{self, PlannerBackend, PlannerError, PlannerRequest};
use crate::readonly_worker::{ReadonlyBackend, ReadonlyError, ReadonlyRequest};
use crate::replanner::{ReplannerBackend, ReplannerError, ReplannerRequest};
use crate::scheduler::{
    SchedulerAction, SchedulerDecision, SchedulerStepOutcome, scheduler_step, select_next_action,
};
use crate::task::{
    ReplaySafety, Task, TaskId, TaskOperationKind, TaskScope, TaskStatus, TaskTransitionContext,
    VerificationCheckResult, VerificationOutcome, VerificationResult, VerificationSpec, WorkerKind,
};
use crate::task_store::TaskStore;
use crate::writer::{ReviewerBackend, ReviewerRequest, WriterBackend, WriterError, WriterRequest};

const NOW: &str = "2026-09-14T08:00:00Z";

struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    session: config::Session,
    store: TaskStore,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn fixture(label: &str) -> Fixture {
    let root = std::env::temp_dir().join(format!("local-mcp-phase9b-{label}-{}", Uuid::new_v4()));
    let repo = root.join("repo");
    let state = root.join("state");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("sentinel.txt"), b"ok\n").unwrap();
    let session = config::Session {
        id: format!("p9b-{}", Uuid::new_v4()),
        cwd: repo.clone(),
        permitted_directories: vec![repo.clone()],
    };
    Fixture {
        root,
        repo,
        session,
        store: TaskStore::with_state_root(state),
    }
}

fn scope(repo: &Path) -> TaskScope {
    TaskScope::new(
        vec![repo.to_path_buf()],
        vec![],
        TaskOperationKind::ReadOnly,
        ReplaySafety::SafeReadOnly,
    )
}

fn structured_spec(name: &str) -> Vec<VerificationSpec> {
    vec![VerificationSpec::StructuredEvidence {
        requirement_id: name.to_owned(),
    }]
}

fn task(f: &Fixture, title: &str, mandatory: bool, plan_revision: u32) -> Task {
    Task::new(
        title,
        title,
        mandatory,
        WorkerKind::CodexReadonly,
        scope(&f.repo),
        structured_spec(title),
        2,
        plan_revision,
        NOW,
    )
    .unwrap()
}

fn goal_with_contract(
    f: &Fixture,
    task_count: usize,
    optional_count: usize,
) -> (Goal, Vec<TaskId>) {
    let mut goal = Goal::new(
        f.session.id.clone(),
        f.repo.clone(),
        "prove structured Goal completion",
        Some("Phase 9B".into()),
        vec!["structured authority only".into()],
        vec!["required proof is mechanically established".into()],
        NOW,
    )
    .unwrap();
    let mut tasks = Vec::new();
    let mut ids = Vec::new();
    for i in 0..task_count {
        let t = task(f, &format!("required-{i}"), true, 1);
        ids.push(t.id().clone());
        tasks.push(t);
    }
    for i in 0..optional_count {
        let t = task(f, &format!("optional-{i}"), false, 1);
        ids.push(t.id().clone());
        tasks.push(t);
    }
    let criterion = goal.completion_criteria()[0].id().clone();
    let binding = GoalCriterionBinding::new(
        criterion,
        vec![GoalVerificationRequirement::TaskVerified {
            task_id: ids[0].clone(),
        }],
    );
    goal.materialize_initial_plan_with_contract(
        tasks,
        GoalFinalVerificationSpec::new(1, vec![binding]),
        NOW,
    )
    .unwrap();
    (goal, ids)
}

fn pass_task(goal: &mut Goal, task_id: &TaskId) {
    let ctx = TaskTransitionContext {
        active_worker_stopped: true,
        side_effect_reconciled: true,
    };
    goal.transition_task(task_id, TaskStatus::Running, ctx, NOW)
        .unwrap();
    goal.transition_task(task_id, TaskStatus::Verifying, ctx, NOW)
        .unwrap();
    goal.task_record_verification_result(
        task_id,
        VerificationResult::new(
            VerificationOutcome::Passed,
            vec![VerificationCheckResult::new(0, true, Some("ok".into()))],
            NOW,
            NOW,
        ),
    )
    .unwrap();
    goal.transition_task(task_id, TaskStatus::Completed, ctx, NOW)
        .unwrap();
}

fn fail_task(goal: &mut Goal, task_id: &TaskId) {
    let ctx = TaskTransitionContext {
        active_worker_stopped: true,
        side_effect_reconciled: true,
    };
    goal.transition_task(task_id, TaskStatus::Running, ctx, NOW)
        .unwrap();
    goal.transition_task(task_id, TaskStatus::Failed, ctx, NOW)
        .unwrap();
}

fn persist(f: &Fixture, goal: &Goal) {
    f.store.create_goal(goal).unwrap();
}

fn rewrite_goal_json(f: &Fixture, goal: &Goal, edit: impl FnOnce(&mut Value)) {
    let path = f
        .store
        .goal_path_for_test(&f.session.id, goal.id())
        .unwrap();
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    edit(&mut value);
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

fn planner_task_value(id: &str, mandatory: bool) -> Value {
    json!({
        "proposal_id": id,
        "title": id,
        "objective": id,
        "mandatory": mandatory,
        "worker": "CODEX_READONLY",
        "dependencies": [],
        "scope": {
            "allowed_paths": ["."],
            "forbidden_paths": [],
            "operation_kind": "READ_ONLY",
            "replay_safety": "SAFE_READ_ONLY"
        },
        "verification": [{"kind":"STRUCTURED_EVIDENCE","requirement_id":format!("proof.{id}")}]
    })
}

fn planning_goal(f: &Fixture) -> Goal {
    Goal::new(
        f.session.id.clone(),
        f.repo.clone(),
        "planner coverage",
        None,
        vec![],
        vec!["criterion-a".into(), "criterion-b".into()],
        NOW,
    )
    .unwrap()
}

fn planner_proposal(goal: &Goal, tasks: Vec<Value>, bindings: Vec<Value>) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "goal_id": goal.id().as_str(),
        "goal_revision": goal.revision(),
        "summary": "phase9b planner proof",
        "tasks": tasks,
        "criterion_bindings": bindings,
    }))
    .unwrap()
}

#[test]
fn schema_v2_model_and_ids_roundtrip() {
    let f = fixture("schema-roundtrip");
    let (goal, _) = goal_with_contract(&f, 1, 0);
    let bytes = serde_json::to_vec_pretty(&goal).unwrap();
    let decoded: Goal = serde_json::from_slice(&bytes).unwrap();
    decoded.validate().unwrap();
    assert_eq!(decoded.schema_version(), GOAL_SCHEMA_VERSION);
    let id = decoded.completion_criteria()[0].id();
    assert_eq!(CompletionCriterionId::parse(id.as_str()).unwrap(), *id);
    assert_eq!(
        decoded.final_verification_spec(),
        goal.final_verification_spec()
    );
}

#[test]
fn task_verified_model_roundtrip() {
    let req = GoalVerificationRequirement::TaskVerified {
        task_id: TaskId::new(),
    };
    let encoded = serde_json::to_vec(&req).unwrap();
    let decoded: GoalVerificationRequirement = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, req);
}

#[test]
fn contract_digest_is_deterministic_order_independent_and_structural() {
    let c1 = CompletionCriterionId::new();
    let c2 = CompletionCriterionId::new();
    let t1 = TaskId::new();
    let t2 = TaskId::new();
    let a = GoalFinalVerificationSpec::new(
        7,
        vec![
            GoalCriterionBinding::new(
                c1.clone(),
                vec![
                    GoalVerificationRequirement::TaskVerified {
                        task_id: t1.clone(),
                    },
                    GoalVerificationRequirement::TaskVerified {
                        task_id: t2.clone(),
                    },
                ],
            ),
            GoalCriterionBinding::new(
                c2.clone(),
                vec![GoalVerificationRequirement::TaskVerified {
                    task_id: t2.clone(),
                }],
            ),
        ],
    );
    let b = GoalFinalVerificationSpec::new(
        7,
        vec![
            GoalCriterionBinding::new(
                c2,
                vec![GoalVerificationRequirement::TaskVerified {
                    task_id: t2.clone(),
                }],
            ),
            GoalCriterionBinding::new(
                c1,
                vec![
                    GoalVerificationRequirement::TaskVerified {
                        task_id: t2.clone(),
                    },
                    GoalVerificationRequirement::TaskVerified {
                        task_id: t1.clone(),
                    },
                ],
            ),
        ],
    );
    assert_eq!(a.canonical_digest(), a.canonical_digest());
    assert_eq!(a.canonical_digest(), b.canonical_digest());
    let changed = GoalFinalVerificationSpec::new(
        7,
        vec![GoalCriterionBinding::new(
            CompletionCriterionId::new(),
            vec![GoalVerificationRequirement::TaskVerified { task_id: t1 }],
        )],
    );
    assert_ne!(a.canonical_digest(), changed.canonical_digest());
}

#[test]
fn description_is_not_proof_authority() {
    let f = fixture("description");
    let (goal, _) = goal_with_contract(&f, 1, 0);
    let digest = goal.final_verification_spec().unwrap().canonical_digest();
    let mut value = serde_json::to_value(&goal).unwrap();
    value["completion_criteria"][0]["description"] = json!("different human prose");
    let changed: Goal = serde_json::from_value(value).unwrap();
    changed.validate().unwrap();
    assert_eq!(
        digest,
        changed
            .final_verification_spec()
            .unwrap()
            .canonical_digest()
    );
}

#[test]
fn final_verification_history_roundtrip_and_idempotency() {
    let f = fixture("history");
    let (mut goal, ids) = goal_with_contract(&f, 1, 0);
    pass_task(&mut goal, &ids[0]);
    persist(&f, &goal);
    let first = verify_goal(&f.store, &f.session.id, goal.id(), goal.revision()).unwrap();
    assert!(first.applied());
    assert_eq!(first.goal().final_verifications().len(), 1);
    let current = f.store.load_goal(&f.session.id, goal.id()).unwrap();
    let before_bytes = serde_json::to_vec_pretty(&current).unwrap();
    let decoded: Goal = serde_json::from_slice(&before_bytes).unwrap();
    assert_eq!(decoded.final_verifications(), current.final_verifications());
    let second = verify_goal(&f.store, &f.session.id, goal.id(), current.revision()).unwrap();
    assert!(!second.applied());
    assert_eq!(second.goal().revision(), current.revision());
    assert_eq!(
        second.goal().final_verifications(),
        current.final_verifications()
    );
}

fn write_schema1_fixture(f: &Fixture, status: &str) -> (crate::goal::GoalId, Vec<u8>) {
    let mut goal = Goal::new(
        f.session.id.clone(),
        f.repo.clone(),
        "legacy objective",
        None,
        vec![],
        vec!["legacy criterion".into()],
        NOW,
    )
    .unwrap();
    if status == "FAILED" {
        goal.transition_to(GoalStatus::Failed, NOW).unwrap();
    }
    persist(f, &goal);
    let path = f
        .store
        .goal_path_for_test(&f.session.id, goal.id())
        .unwrap();
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["schema_version"] = json!(1);
    value["status"] = json!(status);
    value["completion_criteria"] = json!(["legacy criterion"]);
    value
        .as_object_mut()
        .unwrap()
        .remove("final_verification_spec");
    value.as_object_mut().unwrap().remove("final_verifications");
    value["final_verification"] = Value::Null;
    let bytes = serde_json::to_vec_pretty(&value).unwrap();
    fs::write(&path, &bytes).unwrap();
    (goal.id().clone(), bytes)
}

#[test]
fn schema1_terminal_is_read_only_and_historical_apis_work() {
    let f = fixture("schema1-terminal");
    let (id, bytes) = write_schema1_fixture(&f, "FAILED");
    let loaded = f.store.load_goal(&f.session.id, &id).unwrap();
    assert_eq!(loaded.schema_version(), 1);
    assert_eq!(loaded.status(), GoalStatus::Failed);
    let path = f.store.goal_path_for_test(&f.session.id, &id).unwrap();
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert!(
        goal_api::goal_status(
            &json!({"session_id":f.session.id,"goal_id":id.as_str()}),
            &f.session,
            &f.store
        )
        .is_ok()
    );
    assert!(
        goal_api::goal_result(
            &json!({"session_id":f.session.id,"goal_id":id.as_str()}),
            &f.session,
            &f.store
        )
        .is_ok()
    );
    let err = f
        .store
        .mutate_goal_snapshot(&f.session.id, &id, loaded.revision(), |_g, _| Ok(()))
        .unwrap_err();
    assert!(matches!(err, OrchestratorError::SchemaUpgradeRequired(1)));
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn schema1_nonterminal_states_require_explicit_upgrade_without_rewrite() {
    for status in [
        "PLANNING",
        "RUNNING",
        "REPLANNING",
        "PAUSED",
        "BLOCKED",
        "VERIFYING",
    ] {
        let f = fixture(status);
        let (id, bytes) = write_schema1_fixture(&f, status);
        let path = f.store.goal_path_for_test(&f.session.id, &id).unwrap();
        let err = f.store.load_goal(&f.session.id, &id).unwrap_err();
        assert!(
            matches!(err, OrchestratorError::SchemaUpgradeRequired(1)),
            "{status}: {err}"
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn goal_start_preserves_input_and_host_owns_criterion_ids() {
    let f = fixture("goal-start");
    goal_api::goal_start(
        &json!({
            "session_id": f.session.id,
            "objective":"ship objective",
            "completion_criteria":["criterion one","criterion two"]
        }),
        &f.session,
        &f.store,
    )
    .unwrap();
    let goal = f.store.load_active_goal(&f.session.id).unwrap().unwrap();
    assert_eq!(goal.schema_version(), GOAL_SCHEMA_VERSION);
    assert_eq!(
        goal.completion_criterion_descriptions(),
        vec!["criterion one", "criterion two"]
    );
    for criterion in goal.completion_criteria() {
        assert!(Uuid::parse_str(criterion.id().as_str()).is_ok());
        assert!(criterion.required());
    }
}

#[test]
fn goal_start_empty_criteria_uses_objective() {
    let f = fixture("goal-start-empty");
    goal_api::goal_start(
        &json!({"session_id":f.session.id,"objective":"objective fallback"}),
        &f.session,
        &f.store,
    )
    .unwrap();
    let goal = f.store.load_active_goal(&f.session.id).unwrap().unwrap();
    assert_eq!(
        goal.completion_criterion_descriptions(),
        vec!["objective fallback"]
    );
}

#[test]
fn planner_requires_complete_valid_criterion_coverage_and_materializes_host_task_ids() {
    let f = fixture("planner-ok");
    let goal = planning_goal(&f);
    let id = goal.id().clone();
    persist(&f, &goal);
    let c = goal.completion_criteria();
    let proposal = planner_proposal(
        &goal,
        vec![
            planner_task_value("local-a", true),
            planner_task_value("local-b", true),
        ],
        vec![
            json!({"criterion_id":c[0].id().as_str(),"task_refs":["local-a"]}),
            json!({"criterion_id":c[1].id().as_str(),"task_refs":["local-b"]}),
        ],
    );
    let durable =
        planner::materialize_initial_plan_output(&f.store, &f.session, &id, 1, &proposal).unwrap();
    assert_eq!(durable.status(), GoalStatus::Running);
    let spec = durable.final_verification_spec().unwrap();
    assert_eq!(spec.criterion_bindings().len(), 2);
    for binding in spec.criterion_bindings() {
        for req in binding.requirements() {
            assert!(Uuid::parse_str(req.task_id().as_str()).is_ok());
            assert_ne!(req.task_id().as_str(), "local-a");
            assert_ne!(req.task_id().as_str(), "local-b");
            assert!(durable.tasks().contains_key(req.task_id()));
        }
    }
}

#[test]
fn planner_rejects_missing_unknown_duplicate_and_local_ref_errors() {
    enum Case {
        Missing,
        UnknownCriterion,
        UnknownTask,
        DuplicateBinding,
        DuplicateTaskRef,
        FabricatedPass,
    }
    for case in [
        Case::Missing,
        Case::UnknownCriterion,
        Case::UnknownTask,
        Case::DuplicateBinding,
        Case::DuplicateTaskRef,
        Case::FabricatedPass,
    ] {
        let f = fixture("planner-bad");
        let goal = planning_goal(&f);
        let id = goal.id().clone();
        persist(&f, &goal);
        let c = goal.completion_criteria();
        let bindings = match case {
            Case::Missing => vec![json!({"criterion_id":c[0].id().as_str(),"task_refs":["a"]})],
            Case::UnknownCriterion => vec![
                json!({"criterion_id":Uuid::new_v4().to_string(),"task_refs":["a"]}),
                json!({"criterion_id":c[1].id().as_str(),"task_refs":["b"]}),
            ],
            Case::UnknownTask => vec![
                json!({"criterion_id":c[0].id().as_str(),"task_refs":["missing"]}),
                json!({"criterion_id":c[1].id().as_str(),"task_refs":["b"]}),
            ],
            Case::DuplicateBinding => vec![
                json!({"criterion_id":c[0].id().as_str(),"task_refs":["a"]}),
                json!({"criterion_id":c[0].id().as_str(),"task_refs":["b"]}),
            ],
            Case::DuplicateTaskRef => vec![
                json!({"criterion_id":c[0].id().as_str(),"task_refs":["a","a"]}),
                json!({"criterion_id":c[1].id().as_str(),"task_refs":["b"]}),
            ],
            Case::FabricatedPass => vec![
                json!({"criterion_id":c[0].id().as_str(),"task_refs":["a"],"outcome":"PASSED"}),
                json!({"criterion_id":c[1].id().as_str(),"task_refs":["b"]}),
            ],
        };
        let proposal = planner_proposal(
            &goal,
            vec![planner_task_value("a", true), planner_task_value("b", true)],
            bindings,
        );
        let err = planner::materialize_initial_plan_output(&f.store, &f.session, &id, 1, &proposal)
            .unwrap_err();
        assert!(matches!(err, PlannerError::PlannerSchemaViolation(_)));
    }
}

#[test]
fn replan_is_additive_preserves_old_pass_and_invalidates_applicability() {
    let f = fixture("replan-invalidate");
    let (mut goal, ids) = goal_with_contract(&f, 1, 0);
    pass_task(&mut goal, &ids[0]);
    persist(&f, &goal);
    let verified = verify_goal(&f.store, &f.session.id, goal.id(), 1)
        .unwrap()
        .goal()
        .clone();
    assert_eq!(verified.status(), GoalStatus::Verifying);
    assert_eq!(verified.final_verifications().len(), 1);
    let old = verified.final_verifications()[0].clone();
    let current = f.store.load_goal(&f.session.id, goal.id()).unwrap();
    let criterion = current.completion_criteria()[0].id().clone();
    let new_task = task(&f, "strengthening", true, 2);
    let new_id = new_task.id().clone();
    let changed = f
        .store
        .mutate_goal_snapshot(
            &f.session.id,
            goal.id(),
            current.revision(),
            move |g, now| {
                g.transition_to(GoalStatus::Replanning, now)?;
                g.apply_replan_mutation(
                    ReplanMutation {
                        new_tasks: vec![new_task],
                        add_dependencies: vec![],
                        add_verification: vec![],
                        strengthen_mandatory: vec![],
                        resolve_needs_replan: vec![],
                        add_criterion_requirements: vec![(
                            criterion,
                            vec![GoalVerificationRequirement::TaskVerified { task_id: new_id }],
                        )],
                    },
                    now,
                )?;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(changed.status(), GoalStatus::Running);
    assert_eq!(changed.final_verifications().len(), 1);
    assert_eq!(changed.final_verifications()[0], old);
    assert!(changed.latest_applicable_final_verification().is_none());
}

#[test]
fn replan_cannot_duplicate_existing_proof_or_rewrite_history() {
    let f = fixture("replan-monotonic");
    let (goal, ids) = goal_with_contract(&f, 1, 0);
    let criterion = goal.completion_criteria()[0].id().clone();
    let mut candidate = goal.clone();
    let err = candidate
        .apply_replan_mutation(
            ReplanMutation {
                new_tasks: vec![],
                add_dependencies: vec![],
                add_verification: vec![],
                strengthen_mandatory: vec![],
                resolve_needs_replan: vec![],
                add_criterion_requirements: vec![(
                    criterion,
                    vec![GoalVerificationRequirement::TaskVerified {
                        task_id: ids[0].clone(),
                    }],
                )],
            },
            NOW,
        )
        .unwrap_err();
    assert!(matches!(err, OrchestratorError::InvalidDag(_)));
    assert_eq!(candidate, goal);
}

#[test]
fn goal_verifier_passed_enters_verifying_once_without_task_mutation() {
    let f = fixture("passed");
    let (mut goal, ids) = goal_with_contract(&f, 1, 0);
    pass_task(&mut goal, &ids[0]);
    let tasks_before = goal.tasks().clone();
    persist(&f, &goal);
    let result = verify_goal(&f.store, &f.session.id, goal.id(), 1).unwrap();
    assert!(result.applied());
    assert_eq!(result.decision().outcome(), VerificationOutcome::Passed);
    assert_eq!(result.goal().status(), GoalStatus::Verifying);
    assert_eq!(result.goal().revision(), 2);
    assert_eq!(result.goal().plan_revision(), 1);
    assert_eq!(result.goal().tasks(), &tasks_before);
    assert_eq!(result.goal().completed_at(), None);
    assert_eq!(result.goal().final_verifications().len(), 1);
}

#[test]
fn goal_verifier_failed_commits_failed_without_task_mutation() {
    let f = fixture("failed");
    let (mut goal, ids) = goal_with_contract(&f, 1, 0);
    fail_task(&mut goal, &ids[0]);
    let tasks_before = goal.tasks().clone();
    persist(&f, &goal);
    let result = verify_goal(&f.store, &f.session.id, goal.id(), 1).unwrap();
    assert_eq!(result.decision().outcome(), VerificationOutcome::Failed);
    assert_eq!(result.goal().status(), GoalStatus::Failed);
    assert_eq!(result.goal().tasks(), &tasks_before);
    assert_eq!(result.goal().final_verifications().len(), 1);
    let second = verify_goal(&f.store, &f.session.id, goal.id(), 2).unwrap();
    assert!(!second.applied());
    assert_eq!(second.goal().revision(), 2);
    assert_eq!(second.goal().final_verifications().len(), 1);
}

#[test]
fn goal_verifier_indeterminate_blocks_without_task_mutation() {
    let f = fixture("indeterminate");
    let (mut goal, ids) = goal_with_contract(&f, 1, 0);
    pass_task(&mut goal, &ids[0]);
    goal.add_blocker(GoalBlocker::new(
        "UNRESOLVED",
        "cannot establish truth",
        true,
    ))
    .unwrap();
    let tasks_before = goal.tasks().clone();
    persist(&f, &goal);
    let result = verify_goal(&f.store, &f.session.id, goal.id(), 1).unwrap();
    assert_eq!(
        result.decision().outcome(),
        VerificationOutcome::Indeterminate
    );
    assert_eq!(result.goal().status(), GoalStatus::Blocked);
    assert_eq!(result.goal().tasks(), &tasks_before);
    assert_eq!(result.goal().final_verifications().len(), 1);
    let second = verify_goal(&f.store, &f.session.id, goal.id(), 2).unwrap();
    assert!(!second.applied());
    assert_eq!(second.goal().revision(), 2);
}

#[test]
fn unknown_side_effect_prevents_goal_pass() {
    let f = fixture("unknown-side-effect");
    let (mut goal, ids) = goal_with_contract(&f, 1, 1);
    pass_task(&mut goal, &ids[0]);
    let optional = &ids[1];
    goal.transition_task(
        optional,
        TaskStatus::Running,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.task_bind_latest_attempt_execution(
        optional,
        Some("op-unknown".into()),
        Some("scope".into()),
        Some("request".into()),
        Some(SideEffectClass::None),
        Some(SideEffectState::Unknown),
        Some(1),
        Some(0),
    )
    .unwrap();
    goal.transition_task(
        optional,
        TaskStatus::Blocked,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    persist(&f, &goal);
    let result = verify_goal(&f.store, &f.session.id, goal.id(), 1).unwrap();
    assert_eq!(
        result.decision().outcome(),
        VerificationOutcome::Indeterminate
    );
    assert_eq!(result.goal().status(), GoalStatus::Blocked);
}

#[test]
fn unmapped_required_criterion_is_never_passed() {
    let f = fixture("unmapped");
    let (mut goal, ids) = goal_with_contract(&f, 1, 0);
    pass_task(&mut goal, &ids[0]);
    let mut value = serde_json::to_value(&goal).unwrap();
    value["final_verification_spec"]["criterion_bindings"] = json!([]);
    let corrupt: Goal = serde_json::from_value(value).unwrap();
    assert!(corrupt.validate().is_err());
}

#[test]
fn stale_goal_revision_is_rejected_without_append() {
    let f = fixture("stale-goal-rev");
    let (mut goal, ids) = goal_with_contract(&f, 1, 0);
    pass_task(&mut goal, &ids[0]);
    persist(&f, &goal);
    let snapshot = prepare_goal_verification(&f.store, &f.session.id, goal.id()).unwrap();
    let decision = evaluate_goal_verification(&snapshot).unwrap();
    f.store
        .mutate_goal_snapshot(&f.session.id, goal.id(), 1, |g, _| {
            g.add_blocker(GoalBlocker::new("RACE", "newer", true))
        })
        .unwrap();
    let err = commit_goal_verification(&f.store, &snapshot, &decision).unwrap_err();
    assert!(matches!(
        err,
        GoalVerifierError::Store(OrchestratorError::RevisionConflict {
            expected: 1,
            actual: 2
        })
    ));
    let current = f.store.load_goal(&f.session.id, goal.id()).unwrap();
    assert_eq!(current.revision(), 2);
    assert!(current.final_verifications().is_empty());
}

#[test]
fn stale_plan_revision_is_rejected_even_when_goal_revision_matches() {
    let f = fixture("stale-plan-rev");
    let (mut goal, ids) = goal_with_contract(&f, 1, 0);
    pass_task(&mut goal, &ids[0]);
    persist(&f, &goal);
    let snapshot = prepare_goal_verification(&f.store, &f.session.id, goal.id()).unwrap();
    let decision = evaluate_goal_verification(&snapshot).unwrap();
    rewrite_goal_json(&f, &goal, |v| {
        v["plan_revision"] = json!(2);
        v["final_verification_spec"]["plan_revision"] = json!(2);
    });
    f.store.load_goal(&f.session.id, goal.id()).unwrap();
    let err = commit_goal_verification(&f.store, &snapshot, &decision).unwrap_err();
    assert!(matches!(err, GoalVerifierError::StaleEvaluation(_)));
    assert!(
        f.store
            .load_goal(&f.session.id, goal.id())
            .unwrap()
            .final_verifications()
            .is_empty()
    );
}

#[test]
fn stale_contract_digest_is_rejected_without_append() {
    let f = fixture("stale-digest");
    let (mut goal, ids) = goal_with_contract(&f, 2, 0);
    pass_task(&mut goal, &ids[0]);
    pass_task(&mut goal, &ids[1]);
    persist(&f, &goal);
    let snapshot = prepare_goal_verification(&f.store, &f.session.id, goal.id()).unwrap();
    let decision = evaluate_goal_verification(&snapshot).unwrap();
    let second = ids[1].as_str().to_owned();
    rewrite_goal_json(&f, &goal, move |v| {
        v["final_verification_spec"]["criterion_bindings"][0]["requirements"]
            .as_array_mut()
            .unwrap()
            .push(json!({"kind":"TASK_VERIFIED","task_id":second}));
    });
    f.store.load_goal(&f.session.id, goal.id()).unwrap();
    let err = commit_goal_verification(&f.store, &snapshot, &decision).unwrap_err();
    assert!(matches!(err, GoalVerifierError::StaleEvaluation(_)));
    assert!(
        f.store
            .load_goal(&f.session.id, goal.id())
            .unwrap()
            .final_verifications()
            .is_empty()
    );
}

#[test]
fn stale_task_verification_identity_is_rejected_without_silent_rebase() {
    let f = fixture("stale-task-verification");
    let (mut goal, ids) = goal_with_contract(&f, 1, 0);
    pass_task(&mut goal, &ids[0]);
    persist(&f, &goal);
    let snapshot = prepare_goal_verification(&f.store, &f.session.id, goal.id()).unwrap();
    let decision = evaluate_goal_verification(&snapshot).unwrap();
    let task_id = ids[0].as_str().to_owned();
    rewrite_goal_json(&f, &goal, move |v| {
        let task = v["tasks"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|t| t["id"] == task_id)
            .unwrap();
        let mut newer = task["verification_results"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        newer["id"] = json!(Uuid::new_v4().to_string());
        task["verification_results"]
            .as_array_mut()
            .unwrap()
            .push(newer);
    });
    f.store.load_goal(&f.session.id, goal.id()).unwrap();
    let err = commit_goal_verification(&f.store, &snapshot, &decision).unwrap_err();
    assert!(matches!(err, GoalVerifierError::StaleEvaluation(_)));
    assert!(
        f.store
            .load_goal(&f.session.id, goal.id())
            .unwrap()
            .final_verifications()
            .is_empty()
    );
}

#[test]
fn generic_goal_paths_cannot_enter_verifying_or_completed() {
    let f = fixture("authority-seal");
    let (mut goal, _) = goal_with_contract(&f, 1, 0);
    assert!(goal.transition_to(GoalStatus::Verifying, NOW).is_err());
    assert!(goal.transition_to(GoalStatus::Completed, NOW).is_err());
}

#[test]
fn finalizer_requires_latest_applicable_passed_record() {
    let f = fixture("finalizer-applicability");
    let (mut goal, ids) = goal_with_contract(&f, 1, 0);
    pass_task(&mut goal, &ids[0]);
    persist(&f, &goal);
    verify_goal(&f.store, &f.session.id, goal.id(), 1).unwrap();
    let snap =
        goal_finalizer::prepare_goal_finalization(&f.store, &f.session.id, goal.id()).unwrap();
    assert_eq!(
        goal_finalizer::evaluate_goal_finalization(&snap)
            .unwrap()
            .outcome(),
        GoalFinalizationOutcome::ReadyToComplete
    );

    let current = f.store.load_goal(&f.session.id, goal.id()).unwrap();
    let criterion = current.completion_criteria()[0].id().clone();
    let new_task = task(&f, "new-proof", true, 2);
    let new_id = new_task.id().clone();
    f.store
        .mutate_goal_snapshot(
            &f.session.id,
            goal.id(),
            current.revision(),
            move |g, now| {
                g.transition_to(GoalStatus::Replanning, now)?;
                g.apply_replan_mutation(
                    ReplanMutation {
                        new_tasks: vec![new_task],
                        add_dependencies: vec![],
                        add_verification: vec![],
                        strengthen_mandatory: vec![],
                        resolve_needs_replan: vec![],
                        add_criterion_requirements: vec![(
                            criterion,
                            vec![GoalVerificationRequirement::TaskVerified { task_id: new_id }],
                        )],
                    },
                    now,
                )?;
                Ok(())
            },
        )
        .unwrap();
    let changed = f.store.load_goal(&f.session.id, goal.id()).unwrap();
    assert_eq!(changed.status(), GoalStatus::Running);
    assert!(changed.latest_applicable_final_verification().is_none());
    let snap =
        goal_finalizer::prepare_goal_finalization(&f.store, &f.session.id, goal.id()).unwrap();
    assert_ne!(
        goal_finalizer::evaluate_goal_finalization(&snap)
            .unwrap()
            .outcome(),
        GoalFinalizationOutcome::ReadyToComplete
    );
}

#[derive(Default)]
struct NeverPlanner {
    calls: Cell<usize>,
}
impl PlannerBackend for NeverPlanner {
    fn propose_initial_plan(&self, _request: &PlannerRequest) -> Result<Vec<u8>, PlannerError> {
        self.calls.set(self.calls.get() + 1);
        Err(PlannerError::PlannerUnavailable)
    }
}
#[derive(Default)]
struct NeverReadonly {
    calls: Cell<usize>,
}
impl ReadonlyBackend for NeverReadonly {
    fn investigate(&self, _request: &ReadonlyRequest) -> Result<Vec<u8>, ReadonlyError> {
        self.calls.set(self.calls.get() + 1);
        Err(ReadonlyError::Backend(
            "unexpected readonly call".to_owned(),
        ))
    }
}

#[derive(Default)]
struct NeverWriter {
    calls: Cell<usize>,
}
impl WriterBackend for NeverWriter {
    fn propose(&self, _request: &WriterRequest) -> Result<Vec<u8>, WriterError> {
        self.calls.set(self.calls.get() + 1);
        Err(WriterError::Backend("unexpected".into()))
    }
}
#[derive(Default)]
struct NeverReviewer {
    calls: Cell<usize>,
}
impl ReviewerBackend for NeverReviewer {
    fn review(&self, _request: &ReviewerRequest) -> Result<Vec<u8>, WriterError> {
        self.calls.set(self.calls.get() + 1);
        Err(WriterError::Backend("unexpected".into()))
    }
}
#[derive(Default)]
struct NeverReplanner {
    calls: Cell<usize>,
}
impl ReplannerBackend for NeverReplanner {
    fn propose_replan(&self, _request: &ReplannerRequest) -> Result<Vec<u8>, ReplannerError> {
        self.calls.set(self.calls.get() + 1);
        Err(ReplannerError::ReplannerUnavailable)
    }
}

#[tokio::test]
async fn scheduler_step_n_then_n_plus_1_keeps_task_goal_finalizer_stages_separate() {
    let f = fixture("scheduler-separation");
    let mut goal = Goal::new(
        f.session.id.clone(),
        f.repo.clone(),
        "three authorities",
        None,
        vec![],
        vec!["file exists".into()],
        NOW,
    )
    .unwrap();
    let t = Task::new(
        "verify-file",
        "verify-file",
        true,
        WorkerKind::CodexReadonly,
        scope(&f.repo),
        vec![VerificationSpec::FileExists {
            path: f.repo.join("sentinel.txt"),
            must_be_file: true,
        }],
        2,
        1,
        NOW,
    )
    .unwrap();
    let tid = t.id().clone();
    let cid = goal.completion_criteria()[0].id().clone();
    goal.materialize_initial_plan_with_contract(
        vec![t],
        GoalFinalVerificationSpec::new(
            1,
            vec![GoalCriterionBinding::new(
                cid,
                vec![GoalVerificationRequirement::TaskVerified {
                    task_id: tid.clone(),
                }],
            )],
        ),
        NOW,
    )
    .unwrap();
    goal.transition_task(
        &tid,
        TaskStatus::Running,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.transition_task(
        &tid,
        TaskStatus::Verifying,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    persist(&f, &goal);

    let p = NeverPlanner::default();
    let w = NeverWriter::default();
    let r = NeverReviewer::default();
    let rp = NeverReplanner::default();
    let s1 = scheduler_step(
        &f.store,
        &f.session,
        goal.id(),
        1,
        &p,
        &NeverReadonly::default(),
        &w,
        &r,
        &rp,
    )
    .await
    .unwrap();
    assert_eq!(s1.action, SchedulerAction::VerifyTask);
    assert_eq!(s1.outcome, SchedulerStepOutcome::Applied);
    let after1 = f.store.load_goal(&f.session.id, goal.id()).unwrap();
    assert_eq!(after1.status(), GoalStatus::Running);
    assert_eq!(after1.tasks()[&tid].status(), TaskStatus::Completed);
    assert!(after1.final_verifications().is_empty());

    let s2 = scheduler_step(
        &f.store,
        &f.session,
        goal.id(),
        after1.revision(),
        &p,
        &NeverReadonly::default(),
        &w,
        &r,
        &rp,
    )
    .await
    .unwrap();
    assert_eq!(s2.action, SchedulerAction::VerifyGoal);
    assert_eq!(s2.outcome, SchedulerStepOutcome::Applied);
    let after2 = f.store.load_goal(&f.session.id, goal.id()).unwrap();
    assert_eq!(after2.status(), GoalStatus::Verifying);
    assert_eq!(after2.final_verifications().len(), 1);
    assert_eq!(
        after2.final_verifications()[0].outcome(),
        VerificationOutcome::Passed
    );
    assert_eq!(after2.completed_at(), None);

    let final_result = goal_finalizer::finalize_goal(&f.store, &f.session.id, goal.id()).unwrap();
    assert_eq!(final_result.goal().status(), GoalStatus::Completed);
    assert_eq!(p.calls.get(), 0);
    assert_eq!(w.calls.get(), 0);
    assert_eq!(r.calls.get(), 0);
    assert_eq!(rp.calls.get(), 0);
}

#[test]
fn scheduler_verify_goal_outranks_optional_ready_but_not_required_ready() {
    let f = fixture("scheduler-priority");
    let (mut goal, ids) = goal_with_contract(&f, 1, 1);
    pass_task(&mut goal, &ids[0]);
    assert_eq!(
        select_next_action(&goal).unwrap(),
        SchedulerDecision::VerifyGoal
    );

    let (goal2, ids2) = goal_with_contract(&f, 2, 0);
    match select_next_action(&goal2).unwrap() {
        SchedulerDecision::RunReadonly { task_id } => assert!(ids2.contains(&task_id)),
        other => panic!("required READY work must precede VerifyGoal, got {other:?}"),
    }
}

#[test]
fn goal_verifier_provenance_revision_contract_and_task_identity_are_bound() {
    let f = fixture("record-binding");
    let (mut goal, ids) = goal_with_contract(&f, 1, 0);
    pass_task(&mut goal, &ids[0]);
    let task_verification_id = goal.tasks()[&ids[0]]
        .verification_results()
        .last()
        .unwrap()
        .id()
        .clone();
    let digest = goal.final_verification_spec().unwrap().canonical_digest();
    persist(&f, &goal);
    let result = verify_goal(&f.store, &f.session.id, goal.id(), 1).unwrap();
    let record = &result.goal().final_verifications()[0];
    assert_eq!(record.evaluated_goal_revision(), 1);
    assert_eq!(record.committed_goal_revision(), 2);
    assert_eq!(record.plan_revision(), 1);
    assert_eq!(record.contract_digest(), digest);
    let observations = record.criterion_results()[0].observations();
    let (task_id, verification_id) = observations[0].task_verification_identity();
    assert_eq!(task_id, &ids[0]);
    assert_eq!(verification_id, Some(&task_verification_id));
}
