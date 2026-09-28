//! Focused regressions for generic failed-task replacement and budget-aware
//! decomposition.
//!
//! These cover, in order: the structured-failure policy split, the durable
//! replacement transaction (supersession + rewiring + criterion rebinding +
//! preserved retry accounting), atomicity, idempotent replay, host validation
//! of unsafe graphs, budget-aware decomposition, the negative transient case,
//! and the pre-existing safety invariants that must not regress.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use uuid::Uuid;

use crate::failure_class::FailureClass;
use crate::fallback::{SideEffectClass, SideEffectState};
use crate::goal::{
    CheckpointReason, Goal, GoalCriterionBinding, GoalFinalVerificationSpec, GoalId, GoalStatus,
    GoalVerificationRequirement,
};
use crate::goal_api::goal_resume;
use crate::planner::{self, MAX_SINGLE_READONLY_TASK_RECORDS};
use crate::replanner::{ReplannerError, TaskSizingProfile, materialize_replan_output};
use crate::task::{
    ReplaySafety, Task, TaskDependency, TaskId, TaskOperationKind, TaskScope, TaskStatus,
    TaskTransitionContext, WorkerKind, WorkerReport,
};
use crate::task_store::{FaultPoint, TaskStore};

const NOW: &str = "2026-09-28T00:00:00Z";
const REQUEST_ID: &str = "replace-oversized-research-1";
const CRITERION_DESCRIPTION: &str = "canonical authority is durably recovered";

struct Fixture {
    root: PathBuf,
    state: PathBuf,
    repo: PathBuf,
    session: crate::config::Session,
    store: TaskStore,
    goal_id: GoalId,
    /// The oversized, structurally invalid read-only Task.
    a: TaskId,
    /// A broad pre-execution workaround Task that must also be superseded.
    workaround: TaskId,
    b: TaskId,
    c: TaskId,
    criterion_id: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn readonly_scope(repo: &Path) -> TaskScope {
    TaskScope::new(
        vec![repo.to_path_buf()],
        Vec::new(),
        TaskOperationKind::ReadOnly,
        ReplaySafety::SafeReadOnly,
    )
}

/// The narrow read-only scope the oversized Task actually holds. Replacement
/// closures must stay inside it, so the scope-widening rule is testable.
fn narrow_readonly_scope(repo: &Path) -> TaskScope {
    TaskScope::new(
        vec![repo.join("src")],
        vec![repo.join("src").join("generated")],
        TaskOperationKind::ReadOnly,
        ReplaySafety::SafeReadOnly,
    )
}

fn writer_scope(repo: &Path) -> TaskScope {
    TaskScope::new(
        vec![repo.join("src")],
        Vec::new(),
        TaskOperationKind::LocalMutation,
        ReplaySafety::VerifyBeforeRetry,
    )
}

fn verification(id: &str) -> Vec<crate::task::VerificationSpec> {
    vec![crate::task::VerificationSpec::StructuredEvidence {
        requirement_id: id.to_owned(),
    }]
}

fn add_dependency(task: &mut Task, dependency: &TaskId) {
    task.strengthen_dependencies(
        vec![TaskDependency::completed(dependency.clone())],
        &BTreeSet::new(),
    )
    .unwrap();
}

/// Build the production-shaped durable fixture: an oversized read-only Task A that
/// failed with a deterministic host output limit, a broad reassessment
/// workaround W that was prepended downstream of nothing, a downstream Task B,
/// a Writer C downstream of B, and a completion criterion requiring A.
fn fixture(failure_class: FailureClass) -> Fixture {
    fixture_under_root(
        std::env::temp_dir().join(format!("local-mcp-replace-{}", Uuid::new_v4())),
        failure_class,
    )
}

/// Build the same fixture underneath a root that is only reachable through a
/// symlink.
///
/// macOS reaches its temporary directory through a symlink, so a fixture built
/// from the raw `std::env::temp_dir()` prefix stores one spelling of the
/// workspace while the Replanner normalizes the replacement's scope against
/// the canonical Goal cwd and holds another. Creating the alias explicitly
/// reproduces that shape on every host, so the guarantee that a genuine
/// narrowing is accepted is checked rather than assumed.
#[cfg(unix)]
fn symlinked_fixture(failure_class: FailureClass) -> Fixture {
    let real = std::env::temp_dir().join(format!("local-mcp-replace-{}", Uuid::new_v4()));
    fs::create_dir_all(&real).unwrap();
    let alias = std::env::temp_dir().join(format!("local-mcp-replace-alias-{}", Uuid::new_v4()));
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    fixture_under_root(alias, failure_class)
}

fn fixture_under_root(root: PathBuf, failure_class: FailureClass) -> Fixture {
    fs::create_dir_all(root.join("repo").join("src")).unwrap();
    // Resolve the root before any of it reaches the Goal. The Planner
    // canonicalizes a Goal cwd and normalizes every Task scope path against
    // that canonical root, so a stored Goal only ever holds canonical absolute
    // paths. A raw temporary-directory prefix is not one on a host where the
    // temporary directory is itself reached through a symlink (`/tmp` links to
    // `/private/tmp` and `/var` to `/private/var` on macOS), and comparing an
    // aliased superseded scope against a canonical replacement scope would
    // refuse a replacement that really is a narrowing.
    let root = fs::canonicalize(&root).unwrap();
    let repo = root.join("repo");
    let state = root.join("state");
    fs::write(repo.join("sentinel.txt"), b"unchanged\n").unwrap();
    let session = crate::config::Session {
        id: format!("replace-{}", Uuid::new_v4()),
        cwd: repo.clone(),
        permitted_directories: vec![repo.clone()],
    };
    let store = TaskStore::with_state_root(state.clone());
    let mut goal = Goal::new(
        session.id.clone(),
        repo.clone(),
        "recover the canonical authority with bounded read-only work",
        Some("replacement fixture".to_owned()),
        vec!["stay inside repository authority".to_owned()],
        vec![CRITERION_DESCRIPTION.to_owned()],
        NOW,
    )
    .unwrap();

    let a = Task::new(
        "recover canonical authority",
        "recover the full authority for every entity and every evidence dimension",
        true,
        WorkerKind::CodexReadonly,
        narrow_readonly_scope(&repo),
        verification("authority.recovered"),
        2,
        1,
        NOW,
    )
    .unwrap();
    let a_id = a.id().clone();

    // The broad reassessment workaround the old Replanner prepended, which A
    // then became blocked behind.
    let workaround = Task::new(
        "reassess and recover authority",
        "reassess the whole authority question broadly before retrying",
        true,
        WorkerKind::CodexReadonly,
        readonly_scope(&repo),
        verification("authority.reassessed"),
        2,
        1,
        NOW,
    )
    .unwrap();
    let workaround_id = workaround.id().clone();

    let mut b = Task::new(
        "define supported scope",
        "record the supported scope as durable evidence",
        true,
        WorkerKind::CodexReadonly,
        readonly_scope(&repo),
        verification("scope.recorded"),
        2,
        1,
        NOW,
    )
    .unwrap();
    let b_id = b.id().clone();
    add_dependency(&mut b, &a_id);

    let mut c = Task::new(
        "apply the bounded change",
        "apply the scoped change downstream of the recovered authority",
        true,
        WorkerKind::CodexWriter,
        writer_scope(&repo),
        verification("change.applied"),
        1,
        1,
        NOW,
    )
    .unwrap();
    let c_id = c.id().clone();
    add_dependency(&mut c, &b_id);

    // Both replaceable read-only Tasks are bound to the completion criterion,
    // so rebinding is observable without ambiguity and a shared-criterion
    // transaction is exercised.
    let criterion_id = goal.completion_criteria()[0].id().clone();
    let contract = GoalFinalVerificationSpec::new(
        1,
        vec![GoalCriterionBinding::new(
            criterion_id.clone(),
            vec![
                GoalVerificationRequirement::TaskVerified {
                    task_id: a_id.clone(),
                },
                GoalVerificationRequirement::TaskVerified {
                    task_id: workaround_id.clone(),
                },
            ],
        )],
    );
    goal.materialize_initial_plan_with_contract(vec![a, workaround, b, c], contract, NOW)
        .unwrap();
    assert_eq!(goal.tasks()[&a_id].status(), TaskStatus::Ready);
    assert_eq!(goal.tasks()[&workaround_id].status(), TaskStatus::Ready);
    assert_eq!(goal.tasks()[&b_id].status(), TaskStatus::Pending);
    assert_eq!(goal.tasks()[&c_id].status(), TaskStatus::Pending);

    // Attempt 1 on A: RETRYABLE, one attempt consumed of two, no side effects.
    goal.transition_task(
        &a_id,
        TaskStatus::Running,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.task_record_latest_worker_report(
        &a_id,
        WorkerReport::new("READONLY_BACKEND_ERROR: shape failure", Vec::new()),
    )
    .unwrap();
    goal.task_record_latest_attempt_failure_class(&a_id, failure_class)
        .unwrap();
    goal.task_bind_latest_attempt_execution(
        &a_id,
        None,
        None,
        None,
        Some(SideEffectClass::None),
        Some(SideEffectState::ConfirmedNotPerformed),
        Some(1),
        Some(0),
    )
    .unwrap();
    goal.transition_task(
        &a_id,
        TaskStatus::Retryable,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();

    let criterion_id = criterion_id.as_str().to_owned();
    let goal_id = goal.id().clone();
    store.create_goal(&goal).unwrap();
    Fixture {
        root,
        state,
        repo,
        session,
        store,
        goal_id,
        a: a_id,
        workaround: workaround_id,
        b: b_id,
        c: c_id,
        criterion_id,
    }
}

fn load(fixture: &Fixture) -> Goal {
    fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .unwrap()
}

fn bytes(fixture: &Fixture) -> Vec<u8> {
    fs::read(
        fixture
            .state
            .join("goals")
            .join(&fixture.session.id)
            .join(format!("{}.json", fixture.goal_id.as_str())),
    )
    .unwrap()
}

fn repo_bytes(fixture: &Fixture) -> Vec<u8> {
    fs::read(fixture.repo.join("sentinel.txt")).unwrap()
}

fn replan_args(fixture: &Fixture, request_id: &str, trigger: &TaskId, policy: &str) -> Value {
    json!({
        "session_id": fixture.session.id,
        "goal_id": fixture.goal_id.as_str(),
        "failed_task_replan_requests": [{
            "request_id": request_id,
            "expected_goal_revision": load(fixture).revision(),
            "expected_plan_revision": load(fixture).plan_revision(),
            "trigger_task_id": trigger.as_str(),
            "reason": "the Task shape is deterministically too large for one bounded worker",
            "policy": policy
        }]
    })
}

fn request_replan(fixture: &Fixture, request_id: &str, trigger: &TaskId, policy: &str) -> Goal {
    goal_resume(
        &replan_args(fixture, request_id, trigger, policy),
        &fixture.session,
        &fixture.store,
    )
    .unwrap();
    load(fixture)
}

/// Request replacement for a pristine, pre-execution trigger using an existing
/// pre-execution plan rejection as its authority.
fn request_pre_execution_replan(
    fixture: &Fixture,
    request_id: &str,
    trigger: &TaskId,
    policy: &str,
    authority_request_id: &str,
) -> Goal {
    let current = load(fixture);
    goal_resume(
        &json!({
            "session_id": fixture.session.id,
            "goal_id": fixture.goal_id.as_str(),
            "failed_task_replan_requests": [{
                "request_id": request_id,
                "expected_goal_revision": current.revision(),
                "expected_plan_revision": current.plan_revision(),
                "trigger_task_id": trigger.as_str(),
                "reason": "the broad reassessment exists only because of the old replacement limitation",
                "policy": policy,
                "trigger_kind": "PRE_EXECUTION_REJECTION",
                "authority_request_id": authority_request_id
            }]
        }),
        &fixture.session,
        &fixture.store,
    )
    .unwrap();
    load(fixture)
}

fn read_only_task(proposal_id: &str, dependencies: Vec<Value>) -> Value {
    json!({
        "proposal_id": proposal_id,
        "title": format!("Task {proposal_id}"),
        "objective": format!("Recover only the {proposal_id} evidence dimension as compact records"),
        "mandatory": true,
        "worker": "CODEX_READONLY",
        "dependencies": dependencies,
        "scope": {
            "allowed_paths": ["src"],
            "forbidden_paths": ["src/generated"],
            "operation_kind": "READ_ONLY",
            "replay_safety": "SAFE_READ_ONLY"
        },
        "verification": [{
            "kind": "STRUCTURED_EVIDENCE",
            "requirement_id": format!("evidence.{proposal_id}")
        }]
    })
}

fn existing_ref(id: &TaskId) -> Value {
    json!({"ref_kind": "EXISTING", "task_id": id.as_str()})
}

fn new_ref(proposal_id: &str) -> Value {
    json!({"ref_kind": "NEW", "proposal_id": proposal_id})
}

/// A materially decomposed replacement: three bounded read-only research Tasks
/// plus a compact join/synthesis Task, exactly as a budget-aware Replanner must
/// produce. The domain split is arbitrary; nothing here is project-specific.
fn decomposed_proposal(fixture: &Fixture, request_id: &str) -> Value {
    let goal = load(fixture);
    json!({
        "goal_id": fixture.goal_id.as_str(),
        "base_goal_revision": goal.revision(),
        "base_plan_revision": goal.plan_revision(),
        "summary": "replace the oversized Task with a bounded decomposition",
        "add_tasks": [
            read_only_task("dimension-provenance", vec![]),
            read_only_task("dimension-membership", vec![]),
            read_only_task("dimension-semantics", vec![]),
            read_only_task("join-authority", vec![
                new_ref("dimension-provenance"),
                new_ref("dimension-membership"),
                new_ref("dimension-semantics")
            ])
        ],
        "replace_tasks": [{
            "replan_request_id": request_id,
            "old_task_id": fixture.a.as_str(),
            "completion_closure_task_refs": [new_ref("join-authority")],
            "criterion_rebindings": [{
                "criterion_id": fixture.criterion_id,
                "replacement_task_refs": [new_ref("join-authority")]
            }]
        }]
    })
}

/// One single broad read-only Task: the shape the host must refuse to accept as
/// a repair for a size/budget failure.
fn broad_proposal(fixture: &Fixture, request_id: &str) -> Value {
    let goal = load(fixture);
    json!({
        "goal_id": fixture.goal_id.as_str(),
        "base_goal_revision": goal.revision(),
        "base_plan_revision": goal.plan_revision(),
        "summary": "prepend one broad reassessment and retry the same shape",
        "add_tasks": [
            read_only_task("broad-reassessment", vec![]),
            read_only_task("rejoin", vec![new_ref("broad-reassessment")])
        ],
        "replace_tasks": [{
            "replan_request_id": request_id,
            "old_task_id": fixture.a.as_str(),
            "completion_closure_task_refs": [new_ref("rejoin")],
            "criterion_rebindings": [{
                "criterion_id": fixture.criterion_id,
                "replacement_task_refs": [new_ref("rejoin")]
            }]
        }]
    })
}

fn apply(fixture: &Fixture, proposal: &Value) -> Result<Goal, ReplannerError> {
    let current = load(fixture);
    materialize_replan_output(
        &fixture.store,
        &fixture.session,
        &fixture.goal_id,
        current.revision(),
        current.plan_revision(),
        &serde_json::to_vec(proposal).unwrap(),
    )
}

fn has_dependency(goal: &Goal, task: &TaskId, dependency: &TaskId) -> bool {
    goal.tasks()[task]
        .dependencies()
        .iter()
        .any(|edge| edge.task_id() == dependency)
}

fn tasks_by_title_fragment<'a>(goal: &'a Goal, fragment: &str) -> Vec<&'a Task> {
    goal.tasks()
        .values()
        .filter(|task| task.title().contains(fragment))
        .collect()
}

fn assert_no_partial_replacement(goal: &Goal, replaced: &TaskId) {
    // Requirement D: no committed snapshot may show a superseded Task whose
    // downstream still depends on it, or an active Task bound to superseded work.
    if goal.tasks()[replaced].status() == TaskStatus::Superseded {
        for task in goal.tasks().values() {
            assert!(
                !task.is_active_plan_authority()
                    || !task
                        .dependencies()
                        .iter()
                        .any(|edge| edge.task_id() == replaced),
                "an active Task still depends on a superseded Task"
            );
        }
        let spec = goal.final_verification_spec().unwrap();
        for binding in spec.criterion_bindings() {
            assert!(
                !binding
                    .requirements()
                    .iter()
                    .any(|requirement| requirement.task_id() == replaced),
                "criterion authority remains bound to a superseded Task"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// TDD PHASE 1 — structured failure classification drives retry policy
// ---------------------------------------------------------------------------

#[test]
fn host_output_limit_attempt_cannot_return_to_ready_and_requires_replacement() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    let goal = load(&fixture);
    let task = &goal.tasks()[&fixture.a];
    assert_eq!(task.status(), TaskStatus::Retryable);
    assert_eq!(
        task.latest_attempt().unwrap().failure_class(),
        Some(FailureClass::HostOutputLimit)
    );
    assert_eq!(
        task.latest_attempt().unwrap().side_effect_state(),
        Some(SideEffectState::ConfirmedNotPerformed)
    );
    assert!(!task.unchanged_retry_permitted());

    // The durable state machine refuses to make it runnable again.
    let current = load(&fixture);
    let denied = fixture.store.mutate_goal_snapshot(
        &fixture.session.id,
        &fixture.goal_id,
        current.revision(),
        |goal, now| {
            goal.transition_task(
                &fixture.a,
                TaskStatus::Ready,
                TaskTransitionContext::default(),
                now,
            )
        },
    );
    assert!(denied.is_err());
    assert_eq!(
        load(&fixture).tasks()[&fixture.a].status(),
        TaskStatus::Retryable
    );
}

#[test]
fn transient_model_failure_keeps_its_bounded_unchanged_retry() {
    let fixture = fixture(FailureClass::TransientModelFailure);
    let goal = load(&fixture);
    let task = &goal.tasks()[&fixture.a];
    assert!(task.unchanged_retry_permitted());
    assert_eq!(task.semantic_attempts_remaining(), 1);

    let current = load(&fixture);
    let ready = fixture
        .store
        .mutate_goal_snapshot(
            &fixture.session.id,
            &fixture.goal_id,
            current.revision(),
            |goal, now| {
                goal.transition_task(
                    &fixture.a,
                    TaskStatus::Ready,
                    TaskTransitionContext::default(),
                    now,
                )
            },
        )
        .unwrap();
    assert_eq!(ready.tasks()[&fixture.a].status(), TaskStatus::Ready);
    assert_eq!(ready.tasks()[&fixture.a].semantic_attempts_remaining(), 1);
}

#[test]
fn unknown_side_effect_blocks_replan_request_and_replacement_entirely() {
    let root = std::env::temp_dir().join(format!("local-mcp-unknown-{}", Uuid::new_v4()));
    let repo = root.join("repo");
    let state = root.join("state");
    fs::create_dir_all(&repo).unwrap();
    let session = crate::config::Session {
        id: format!("unknown-{}", Uuid::new_v4()),
        cwd: repo.clone(),
        permitted_directories: vec![repo.clone()],
    };
    let store = TaskStore::with_state_root(state);
    let mut goal = Goal::new(
        session.id.clone(),
        repo.clone(),
        "unknown side effects must never be replaced",
        None,
        vec![],
        vec![],
        NOW,
    )
    .unwrap();
    let task = Task::new(
        "t",
        "o",
        true,
        WorkerKind::CodexReadonly,
        readonly_scope(&repo),
        verification("t"),
        2,
        1,
        NOW,
    )
    .unwrap();
    let task_id = task.id().clone();
    goal.materialize_initial_plan(vec![task], NOW).unwrap();
    goal.transition_task(
        &task_id,
        TaskStatus::Running,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.task_record_latest_attempt_failure_class(&task_id, FailureClass::HostOutputLimit)
        .unwrap();
    // Side effects are UNKNOWN: the class binds, but no replacement authority
    // may be derived from it until reconciliation proves safety.
    assert!(
        goal.task_bind_latest_attempt_execution(
            &task_id,
            Some("op".to_owned()),
            Some("scope".to_owned()),
            None,
            Some(SideEffectClass::None),
            Some(SideEffectState::Unknown),
            Some(1),
            Some(0),
        )
        .is_ok()
    );
    goal.transition_task(
        &task_id,
        TaskStatus::Retryable,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    let goal_id = goal.id().clone();
    store.create_goal(&goal).unwrap();

    let args = json!({
        "session_id": session.id,
        "goal_id": goal_id.as_str(),
        "failed_task_replan_requests": [{
            "request_id": "unknown-side-effects",
            "expected_goal_revision": 1,
            "expected_plan_revision": 1,
            "trigger_task_id": task_id.as_str(),
            "reason": "replace it anyway",
            "policy": "REQUIRE_REPLACEMENT"
        }]
    });
    let error = goal_resume(&args, &session, &store).unwrap_err();
    assert_eq!(error.code(), "INVALID_GOAL_STATE");
    let durable = store.load_goal(&session.id, &goal_id).unwrap();
    assert!(durable.failed_task_replan_requests().is_empty());
    assert_eq!(durable.tasks()[&task_id].status(), TaskStatus::Retryable);
    let _ = fs::remove_dir_all(&root);
}

// ---------------------------------------------------------------------------
// TDD PHASE 2 — core replacement: supersede, decompose, rewire, rebind
// ---------------------------------------------------------------------------

#[test]
fn replacement_supersedes_old_task_decomposes_work_rewires_downstream_and_rebinds_criteria() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    let before = load(&fixture);
    let before_plan_revision = before.plan_revision();
    let before_attempts = before.tasks()[&fixture.a].attempts().len();
    let before_attempt_id = before.tasks()[&fixture.a].attempts()[0].id().clone();

    let requested = request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    assert_eq!(requested.status(), GoalStatus::Replanning);
    assert_eq!(
        requested.tasks()[&fixture.a].status(),
        TaskStatus::NeedsReplan
    );
    // The request itself must not consume the remaining retry.
    assert_eq!(
        requested.tasks()[&fixture.a].semantic_attempts_remaining(),
        1
    );
    assert_eq!(
        requested.tasks()[&fixture.a].attempts().len(),
        before_attempts
    );

    let after = apply(&fixture, &decomposed_proposal(&fixture, REQUEST_ID)).unwrap();

    // Old Task permanently non-runnable, with its history intact.
    assert_eq!(after.tasks()[&fixture.a].status(), TaskStatus::Superseded);
    assert_eq!(after.tasks()[&fixture.a].attempts().len(), before_attempts);
    assert_eq!(
        after.tasks()[&fixture.a].attempts()[0].id(),
        &before_attempt_id
    );
    assert_eq!(after.tasks()[&fixture.a].max_attempts(), 2);
    assert_eq!(after.tasks()[&fixture.a].semantic_attempts_consumed(), 1);
    assert_eq!(after.tasks()[&fixture.a].semantic_attempts_remaining(), 1);

    // Materially decomposed bounded read-only work plus a compact join.
    let research = tasks_by_title_fragment(&after, "dimension-");
    assert_eq!(research.len(), 3);
    for task in &research {
        assert_eq!(task.worker(), WorkerKind::CodexReadonly);
        assert_eq!(task.scope().operation_kind(), TaskOperationKind::ReadOnly);
        assert!(task.status() == TaskStatus::Ready || task.status() == TaskStatus::Pending);
    }
    let join = tasks_by_title_fragment(&after, "join-authority");
    assert_eq!(join.len(), 1);
    assert_eq!(join[0].dependencies().len(), 3);

    // Downstream B depends on the join, never on the superseded Task.
    assert!(has_dependency(&after, &fixture.b, &join[0].id().clone()));
    assert!(!has_dependency(&after, &fixture.b, &fixture.a));
    // Writer C stays downstream of B only and is never runnable.
    assert!(has_dependency(&after, &fixture.c, &fixture.b));
    assert!(!has_dependency(&after, &fixture.c, &fixture.a));
    assert_ne!(after.tasks()[&fixture.c].status(), TaskStatus::Ready);
    assert_ne!(after.tasks()[&fixture.c].status(), TaskStatus::Running);

    // Criterion X rebound from A to the join; the unrelated workaround proof is
    // preserved rather than dropped.
    let spec = after.final_verification_spec().unwrap();
    let binding = spec
        .criterion_bindings()
        .iter()
        .find(|binding| binding.criterion_id().as_str() == fixture.criterion_id)
        .unwrap();
    let bound = binding
        .requirements()
        .iter()
        .map(|requirement| requirement.task_id().clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(bound.len(), 2);
    assert!(bound.contains(join[0].id()));
    assert!(bound.contains(&fixture.workaround));
    assert!(!bound.contains(&fixture.a));
    assert!(!matches!(
        &binding.requirements()[0],
        GoalVerificationRequirement::TaskVerified { task_id } if task_id == &fixture.a
    ));

    // Exactly one plan revision, and the durable record proves it.
    assert_eq!(after.plan_revision(), before_plan_revision + 1);
    assert_eq!(after.failed_task_replacements().len(), 1);
    let record = &after.failed_task_replacements()[0];
    assert_eq!(record.replan_request_id(), REQUEST_ID);
    assert_eq!(record.replaced_task_id(), &fixture.a);
    assert_eq!(record.preserved_max_attempts(), 2);
    assert_eq!(record.preserved_consumed_attempts(), 1);
    assert_eq!(record.completion_closure_task_ids(), [join[0].id().clone()]);
    assert_eq!(record.rebound_criterion_ids().len(), 1);
    assert_eq!(
        record.replaced_attempt_ids(),
        [before.tasks()[&fixture.a].attempts()[0].id().clone()]
    );

    assert_no_partial_replacement(&after, &fixture.a);
    assert_eq!(repo_bytes(&fixture), b"unchanged\n");
    // Reload proves the whole thing is durable, not just in-memory.
    assert_eq!(load(&fixture), after);
    assert_eq!(after.status(), GoalStatus::Running);
}

#[test]
fn superseding_the_broad_workaround_in_the_same_transaction_removes_redundant_recovery_work() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    // The broad reassessment workaround exists only because the old Replanner
    // could not replace A. It has not run, so its authority is a pre-execution
    // plan rejection, exactly as the transition matrix prescribes. That path
    // must be taken while the Goal is still RUNNING.
    let before_rejection = load(&fixture);
    goal_resume(
        &json!({
            "session_id": fixture.session.id,
            "goal_id": fixture.goal_id.as_str(),
            "pre_execution_plan_rejection": {
                "request_id": "reject-broad-workaround-1",
                "expected_goal_revision": before_rejection.revision(),
                "expected_plan_revision": before_rejection.plan_revision(),
                "trigger_task_id": fixture.workaround.as_str(),
                "reason": "the broad reassessment must never run"
            }
        }),
        &fixture.session,
        &fixture.store,
    )
    .unwrap();
    let after_rejection = load(&fixture);
    assert_eq!(
        after_rejection.tasks()[&fixture.workaround].status(),
        TaskStatus::NeedsReplan
    );
    assert_eq!(after_rejection.status(), GoalStatus::Replanning);
    // A pre-execution rejection alone is not a replacement: A is untouched.
    assert_eq!(
        after_rejection.tasks()[&fixture.a].status(),
        TaskStatus::Retryable
    );

    // The dedicated failed-task replan requests name their authorities and
    // route both Tasks into replacement. A's authority is its structured
    // post-attempt failure; the workaround's is the pre-execution rejection.
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let after_replan = request_pre_execution_replan(
        &fixture,
        "replace-broad-workaround-1",
        &fixture.workaround,
        "REQUIRE_DECOMPOSITION",
        "reject-broad-workaround-1",
    );
    assert_eq!(
        after_replan.tasks()[&fixture.workaround].status(),
        TaskStatus::NeedsReplan
    );
    assert_eq!(after_replan.failed_task_replan_requests().len(), 2);
    // A pre-execution trigger carries no attempt, so its retry budget is intact
    // and was never consumed.
    assert_eq!(
        after_replan.tasks()[&fixture.workaround].attempts().len(),
        0
    );
    assert_eq!(
        after_replan.tasks()[&fixture.workaround].semantic_attempts_consumed(),
        0
    );

    let goal = load(&fixture);
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["base_goal_revision"] = json!(goal.revision());
    proposal["base_plan_revision"] = json!(goal.plan_revision());
    // The oversized Task is superseded by the same bounded closure in this
    // transaction. The criterion is rebound once, covering both replaced proofs.
    proposal["replace_tasks"][0]["criterion_rebindings"] = json!([{
        "criterion_id": fixture.criterion_id,
        "replacement_task_refs": [new_ref("join-authority")]
    }]);
    proposal["replace_tasks"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "replan_request_id": "replace-broad-workaround-1",
            "old_task_id": fixture.workaround.as_str(),
            "completion_closure_task_refs": [new_ref("join-authority")],
            "criterion_rebindings": [{
                "criterion_id": fixture.criterion_id,
                "replacement_task_refs": [new_ref("join-authority")]
            }]
        }));

    let after = apply(&fixture, &proposal).unwrap();
    assert_eq!(after.tasks()[&fixture.a].status(), TaskStatus::Superseded);
    assert_eq!(
        after.tasks()[&fixture.workaround].status(),
        TaskStatus::Superseded
    );
    assert_eq!(after.failed_task_replacements().len(), 2);
    assert!(!has_dependency(&after, &fixture.b, &fixture.a));
    // History itself is preserved: both old Tasks remain durably present.
    assert!(after.tasks().contains_key(&fixture.a));
    assert!(after.tasks().contains_key(&fixture.workaround));
    let join = tasks_by_title_fragment(&after, "join-authority");
    assert!(has_dependency(&after, &fixture.b, join[0].id()));
    assert_no_partial_replacement(&after, &fixture.a);
    assert_no_partial_replacement(&after, &fixture.workaround);
}

#[test]
fn next_runnable_task_after_replacement_is_a_bounded_read_only_task() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let after = apply(&fixture, &decomposed_proposal(&fixture, REQUEST_ID)).unwrap();
    let runnable = after
        .tasks()
        .values()
        .filter(|task| task.status() == TaskStatus::Ready)
        .collect::<Vec<_>>();
    assert!(!runnable.is_empty());
    for task in runnable {
        assert_ne!(task.id(), &fixture.c);
        assert_eq!(task.worker(), WorkerKind::CodexReadonly);
        assert_eq!(task.scope().operation_kind(), TaskOperationKind::ReadOnly);
        assert_eq!(task.scope().replay_safety(), ReplaySafety::SafeReadOnly);
    }
    assert_eq!(
        after.tasks()[&fixture.c].status(),
        TaskStatus::Pending,
        "the Writer must never become runnable while its prerequisite is unmet"
    );
}

// ---------------------------------------------------------------------------
// TDD PHASE 3 — atomicity
// ---------------------------------------------------------------------------

#[test]
fn persistence_failure_during_replacement_commits_nothing() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let before = load(&fixture);
    let before_bytes = bytes(&fixture);
    let before_repo = repo_bytes(&fixture);

    // Host is already past the replan request, so only the replacement commit
    // is faulted.
    let faulty = TaskStore::with_fault(fixture.state.clone(), FaultPoint::BeforeReplace);
    let current = load(&fixture);
    let result = materialize_replan_output(
        &faulty,
        &fixture.session,
        &fixture.goal_id,
        current.revision(),
        current.plan_revision(),
        &serde_json::to_vec(&decomposed_proposal(&fixture, REQUEST_ID)).unwrap(),
    );
    assert!(result.is_err());

    assert_eq!(
        bytes(&fixture),
        before_bytes,
        "no partial replacement committed"
    );
    let after = load(&fixture);
    assert_eq!(after, before);
    assert_eq!(after.plan_revision(), current.plan_revision());
    assert_eq!(after.tasks()[&fixture.a].status(), TaskStatus::NeedsReplan);
    assert!(after.failed_task_replacements().is_empty());
    assert_eq!(repo_bytes(&fixture), before_repo);
    assert_no_partial_replacement(&after, &fixture.a);
}

#[test]
fn a_never_materialized_replacement_leaves_the_original_plan_exactly_unchanged() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let before = load(&fixture);
    let before_bytes = bytes(&fixture);

    // A proposal that fails host validation after the model proposed it.
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["replace_tasks"][0]["criterion_rebindings"] = json!([]);
    assert!(apply(&fixture, &proposal).is_err());

    assert_eq!(bytes(&fixture), before_bytes);
    assert_eq!(load(&fixture), before);
    assert!(load(&fixture).failed_task_replacements().is_empty());
    assert_eq!(load(&fixture).plan_revision(), before.plan_revision());
    assert_eq!(
        load(&fixture).tasks()[&fixture.a].status(),
        TaskStatus::NeedsReplan
    );
}

// ---------------------------------------------------------------------------
// TDD PHASE 4 — idempotency
// ---------------------------------------------------------------------------

#[test]
fn replaying_the_same_replacement_commits_no_duplicate_state() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let accepted = serde_json::to_vec(&decomposed_proposal(&fixture, REQUEST_ID)).unwrap();
    let first = apply(
        &fixture,
        &serde_json::from_slice::<Value>(&accepted).unwrap(),
    )
    .unwrap();
    let first_bytes = bytes(&fixture);
    let task_count = first.tasks().len();
    let plan_revision = first.plan_revision();

    // The identical accepted proposal bytes are replayed verbatim.
    let current = load(&fixture);
    let replayed = materialize_replan_output(
        &fixture.store,
        &fixture.session,
        &fixture.goal_id,
        current.revision(),
        current.plan_revision(),
        &accepted,
    )
    .unwrap();

    assert_eq!(
        replayed, first,
        "replay returns the already-materialized result"
    );
    assert_eq!(replayed.tasks().len(), task_count);
    assert_eq!(replayed.plan_revision(), plan_revision);
    assert_eq!(replayed.failed_task_replacements().len(), 1);
    assert_eq!(replayed.tasks()[&fixture.a].semantic_attempts_consumed(), 1);
    assert_eq!(
        bytes(&fixture),
        first_bytes,
        "replay must not rewrite durable bytes"
    );
}

#[test]
fn reusing_a_replacement_identity_with_a_different_effective_proposal_is_rejected() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    apply(&fixture, &decomposed_proposal(&fixture, REQUEST_ID)).unwrap();
    let before = load(&fixture);
    let before_bytes = bytes(&fixture);

    // Same replan_request_id, different effective proposal.
    let current = load(&fixture);
    let mut tampered = decomposed_proposal(&fixture, REQUEST_ID);
    tampered["base_goal_revision"] = json!(current.revision());
    tampered["add_tasks"] = json!([read_only_task("dimension-other", vec![])]);
    tampered["replace_tasks"][0]["completion_closure_task_refs"] =
        json!([new_ref("dimension-other")]);
    let error = materialize_replan_output(
        &fixture.store,
        &fixture.session,
        &fixture.goal_id,
        current.revision(),
        current.plan_revision(),
        &serde_json::to_vec(&tampered).unwrap(),
    )
    .unwrap_err();
    assert!(matches!(error, ReplannerError::ReplanAuthorityViolation(_)));

    assert_eq!(bytes(&fixture), before_bytes);
    assert_eq!(load(&fixture), before);
}

#[test]
fn a_replacement_request_is_idempotent_and_conflicts_are_non_mutating() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    let first = request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let first_bytes = bytes(&fixture);

    // Exact replay is a no-op.
    let replay_args = json!({
        "session_id": fixture.session.id,
        "goal_id": fixture.goal_id.as_str(),
        "failed_task_replan_requests": [{
            "request_id": REQUEST_ID,
            "expected_goal_revision": first.revision() - 1,
            "expected_plan_revision": first.plan_revision(),
            "trigger_task_id": fixture.a.as_str(),
            "reason": "the Task shape is deterministically too large for one bounded worker",
            "policy": "REQUIRE_DECOMPOSITION"
        }]
    });
    goal_resume(&replay_args, &fixture.session, &fixture.store).unwrap();
    assert_eq!(bytes(&fixture), first_bytes);

    // Same identity, different payload.
    let conflicting = json!({
        "session_id": fixture.session.id,
        "goal_id": fixture.goal_id.as_str(),
        "failed_task_replan_requests": [{
            "request_id": REQUEST_ID,
            "expected_goal_revision": first.revision(),
            "expected_plan_revision": first.plan_revision(),
            "trigger_task_id": fixture.a.as_str(),
            "reason": "a different reason entirely",
            "policy": "REQUIRE_DECOMPOSITION"
        }]
    });
    let error = goal_resume(&conflicting, &fixture.session, &fixture.store).unwrap_err();
    assert_eq!(error.code(), "IDEMPOTENCY_CONFLICT");
    assert_eq!(bytes(&fixture), first_bytes);
}

// ---------------------------------------------------------------------------
// TDD PHASE 5 — graph and authority safety
// ---------------------------------------------------------------------------

fn expect_rejected(fixture: &Fixture, proposal: &Value) {
    let before = load(fixture);
    let before_bytes = bytes(fixture);
    let result = apply(fixture, proposal);
    assert!(
        result.is_err(),
        "unsafe replacement proposal was accepted: {proposal}"
    );
    assert_eq!(bytes(fixture), before_bytes, "rejection must not mutate");
    assert_eq!(load(fixture), before);
}

// ---------------------------------------------------------------------------
// Atomic multi-replacement rewiring.
//
// A transaction may replace several Tasks whose completion closures overlap. A
// surviving dependent that sits behind two replaced Tasks must converge on the
// union of their closures, which is a SET: one shared closure Task is one edge,
// not two. The fixtures below reproduce that shape directly.
// ---------------------------------------------------------------------------

const OVERSIZED_REQUEST: &str = "replace-oversized-authority-1";
const WORKAROUND_REQUEST: &str = "replace-broad-workaround-1";
const WORKAROUND_REJECTION: &str = "reject-broad-workaround-1";
/// Enough filler Tasks to reach the host dependency-edge ceiling while keeping
/// every Task's fan-in inside the per-Task bound.
const FILLER_TASKS: usize = 46;

struct ClosureFixture {
    root: PathBuf,
    state: PathBuf,
    session: crate::config::Session,
    store: TaskStore,
    goal_id: GoalId,
    /// The oversized post-attempt trigger.
    oversized: TaskId,
    /// The broad pre-execution workaround the oversized Task sits behind.
    workaround: TaskId,
    /// A surviving dependent of BOTH replaced Tasks.
    downstream: TaskId,
    /// The Writer downstream of `downstream`.
    writer: TaskId,
    /// Unrelated authorities `downstream` also depends on.
    unrelated: Vec<TaskId>,
    criterion_id: String,
}

impl Drop for ClosureFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Build the live-shaped fixture: an oversized read-only Task A that failed with
/// a host output limit, a broad workaround W that A sits behind, a downstream
/// Task B behind both, a Writer C behind B, and optionally `unrelated` filler
/// authorities B also depends on plus `filler_edges` unrelated edges.
///
/// A depends on nothing so it is READY and can consume a real attempt; the
/// workaround is an independent pristine pre-execution trigger, and B sits
/// behind both of them.
fn closure_fixture(with_unrelated: bool, filler_edges: usize) -> ClosureFixture {
    let root = std::env::temp_dir().join(format!("local-mcp-closure-{}", Uuid::new_v4()));
    fs::create_dir_all(root.join("repo").join("src")).unwrap();
    // Canonicalize for the same reason `fixture_under_root` does: every stored
    // scope path is compared against a path the Replanner normalized from the
    // canonical Goal cwd, so the fixture has to hold the real path too.
    let root = fs::canonicalize(&root).unwrap();
    let repo = root.join("repo");
    let state = root.join("state");
    fs::write(repo.join("sentinel.txt"), b"unchanged\n").unwrap();
    let session = crate::config::Session {
        id: format!("closure-{}", Uuid::new_v4()),
        cwd: repo.clone(),
        permitted_directories: vec![repo.clone()],
    };
    let store = TaskStore::with_state_root(state.clone());
    let mut goal = Goal::new(
        session.id.clone(),
        repo.clone(),
        "recover the canonical authority with bounded read-only work",
        Some("overlapping closure fixture".to_owned()),
        vec!["stay inside repository authority".to_owned()],
        vec![CRITERION_DESCRIPTION.to_owned()],
        NOW,
    )
    .unwrap();

    // Unrelated authority held only to prove the transaction never disturbs an
    // edge it does not own. Edges point forward, so the filler stays acyclic.
    let mut filler = (0..FILLER_TASKS)
        .map(|index| {
            Task::new(
                format!("filler {index:02}"),
                format!("hold unrelated filler authority {index:02}"),
                true,
                WorkerKind::CodexReadonly,
                readonly_scope(&repo),
                verification(&format!("filler.{index:02}")),
                2,
                1,
                NOW,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    let filler_ids = filler
        .iter()
        .map(|task| task.id().clone())
        .collect::<Vec<_>>();
    let mut placed = 0usize;
    let mut exhausted = false;
    for (index, task) in filler.iter_mut().enumerate() {
        let mut edges = Vec::new();
        for later in filler_ids.iter().skip(index + 1) {
            if placed == filler_edges {
                exhausted = true;
                break;
            }
            edges.push(TaskDependency::completed(later.clone()));
            placed += 1;
        }
        if !edges.is_empty() {
            task.strengthen_dependencies(edges, &BTreeSet::new())
                .unwrap();
        }
        if exhausted {
            break;
        }
    }
    assert_eq!(placed, filler_edges, "filler edge budget was not reachable");

    let oversized = Task::new(
        "recover canonical authority",
        "recover the full authority for every entity and every evidence dimension",
        true,
        WorkerKind::CodexReadonly,
        narrow_readonly_scope(&repo),
        verification("authority.recovered"),
        2,
        1,
        NOW,
    )
    .unwrap();
    let oversized_id = oversized.id().clone();

    let workaround = Task::new(
        "reassess and recover authority",
        "reassess the whole authority question broadly before retrying",
        true,
        WorkerKind::CodexReadonly,
        readonly_scope(&repo),
        verification("authority.reassessed"),
        2,
        1,
        NOW,
    )
    .unwrap();
    let workaround_id = workaround.id().clone();

    let mut unrelated = Vec::new();
    let mut unrelated_tasks = Vec::new();
    if with_unrelated {
        for name in ["hold unrelated x", "hold unrelated y"] {
            let task = Task::new(
                name,
                format!("hold unrelated authority {name}"),
                true,
                WorkerKind::CodexReadonly,
                readonly_scope(&repo),
                verification("unrelated.held"),
                2,
                1,
                NOW,
            )
            .unwrap();
            unrelated.push(task.id().clone());
            unrelated_tasks.push(task);
        }
    }

    // The live-shaped survivor: it depends on BOTH replaced Tasks, so both
    // rewires converge on it within one transaction.
    let mut downstream = Task::new(
        "define supported scope",
        "record the supported scope as durable evidence",
        true,
        WorkerKind::CodexReadonly,
        readonly_scope(&repo),
        verification("scope.recorded"),
        2,
        1,
        NOW,
    )
    .unwrap();
    let downstream_dependencies = std::iter::once(&oversized_id)
        .chain(std::iter::once(&workaround_id))
        .chain(unrelated.iter())
        .map(|id| TaskDependency::completed(id.clone()))
        .collect::<Vec<_>>();
    downstream
        .strengthen_dependencies(downstream_dependencies, &BTreeSet::new())
        .unwrap();
    let downstream_id = downstream.id().clone();

    let mut writer = Task::new(
        "apply the bounded change",
        "apply the scoped change downstream of the recovered authority",
        true,
        WorkerKind::CodexWriter,
        writer_scope(&repo),
        verification("change.applied"),
        1,
        1,
        NOW,
    )
    .unwrap();
    add_dependency(&mut writer, &downstream_id);
    let writer_id = writer.id().clone();

    let criterion_id = goal.completion_criteria()[0].id().clone();
    let contract = GoalFinalVerificationSpec::new(
        1,
        vec![GoalCriterionBinding::new(
            criterion_id.clone(),
            vec![
                GoalVerificationRequirement::TaskVerified {
                    task_id: oversized_id.clone(),
                },
                GoalVerificationRequirement::TaskVerified {
                    task_id: workaround_id.clone(),
                },
            ],
        )],
    );
    let mut plan = vec![oversized, workaround];
    plan.extend(unrelated_tasks);
    plan.extend(filler);
    plan.extend([downstream, writer]);
    goal.materialize_initial_plan_with_contract(plan, contract, NOW)
        .unwrap();
    assert_eq!(goal.tasks()[&oversized_id].status(), TaskStatus::Ready);
    assert_eq!(goal.tasks()[&workaround_id].status(), TaskStatus::Ready);

    // One consumed attempt on the oversized Task, with no side effect, so it
    // carries exactly one remaining retry across the transaction.
    goal.transition_task(
        &oversized_id,
        TaskStatus::Running,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();
    goal.task_record_latest_worker_report(
        &oversized_id,
        WorkerReport::new("READONLY_BACKEND_ERROR: shape failure", Vec::new()),
    )
    .unwrap();
    goal.task_record_latest_attempt_failure_class(&oversized_id, FailureClass::HostOutputLimit)
        .unwrap();
    goal.task_bind_latest_attempt_execution(
        &oversized_id,
        None,
        None,
        None,
        Some(SideEffectClass::None),
        Some(SideEffectState::ConfirmedNotPerformed),
        Some(1),
        Some(0),
    )
    .unwrap();
    goal.transition_task(
        &oversized_id,
        TaskStatus::Retryable,
        TaskTransitionContext::default(),
        NOW,
    )
    .unwrap();

    let goal_id = goal.id().clone();
    store.create_goal(&goal).unwrap();
    ClosureFixture {
        root,
        state,
        session,
        store,
        goal_id,
        oversized: oversized_id,
        workaround: workaround_id,
        downstream: downstream_id,
        writer: writer_id,
        unrelated,
        criterion_id: criterion_id.as_str().to_owned(),
    }
}

fn load_closure(fixture: &ClosureFixture) -> Goal {
    fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .unwrap()
}

fn closure_bytes(fixture: &ClosureFixture) -> Vec<u8> {
    fs::read(
        fixture
            .state
            .join("goals")
            .join(&fixture.session.id)
            .join(format!("{}.json", fixture.goal_id.as_str())),
    )
    .unwrap()
}

fn apply_closure(fixture: &ClosureFixture, proposal: &Value) -> Result<Goal, ReplannerError> {
    let current = load_closure(fixture);
    materialize_replan_output(
        &fixture.store,
        &fixture.session,
        &fixture.goal_id,
        current.revision(),
        current.plan_revision(),
        &serde_json::to_vec(proposal).unwrap(),
    )
}

/// Arm a durable replacement authority for both replaced Tasks.
///
/// The pre-execution rejection runs first because it requires a RUNNING Goal,
/// and the post-attempt request is what moves the Goal into REPLANNING.
fn arm_closure_requests(fixture: &ClosureFixture) {
    let before = load_closure(fixture);
    goal_resume(
        &json!({
            "session_id": fixture.session.id,
            "goal_id": fixture.goal_id.as_str(),
            "pre_execution_plan_rejection": {
                "request_id": WORKAROUND_REJECTION,
                "expected_goal_revision": before.revision(),
                "expected_plan_revision": before.plan_revision(),
                "trigger_task_id": fixture.workaround.as_str(),
                "reason": "the broad reassessment must never run"
            }
        }),
        &fixture.session,
        &fixture.store,
    )
    .unwrap();
    let current = load_closure(fixture);
    goal_resume(
        &json!({
            "session_id": fixture.session.id,
            "goal_id": fixture.goal_id.as_str(),
            "failed_task_replan_requests": [{
                "request_id": OVERSIZED_REQUEST,
                "expected_goal_revision": current.revision(),
                "expected_plan_revision": current.plan_revision(),
                "trigger_task_id": fixture.oversized.as_str(),
                "reason": "the Task shape is deterministically too large for one bounded worker",
                "policy": "REQUIRE_DECOMPOSITION"
            }]
        }),
        &fixture.session,
        &fixture.store,
    )
    .unwrap();
    let current = load_closure(fixture);
    goal_resume(
        &json!({
            "session_id": fixture.session.id,
            "goal_id": fixture.goal_id.as_str(),
            "failed_task_replan_requests": [{
                "request_id": WORKAROUND_REQUEST,
                "expected_goal_revision": current.revision(),
                "expected_plan_revision": current.plan_revision(),
                "trigger_task_id": fixture.workaround.as_str(),
                "reason": "the broad reassessment is permanently superseded by the decomposition",
                "policy": "REQUIRE_DECOMPOSITION",
                "trigger_kind": "PRE_EXECUTION_REJECTION",
                "authority_request_id": WORKAROUND_REJECTION
            }]
        }),
        &fixture.session,
        &fixture.store,
    )
    .unwrap();
    let armed = load_closure(fixture);
    assert_eq!(armed.status(), GoalStatus::Replanning);
    assert_eq!(
        armed.tasks()[&fixture.oversized].status(),
        TaskStatus::NeedsReplan
    );
    assert_eq!(
        armed.tasks()[&fixture.workaround].status(),
        TaskStatus::NeedsReplan
    );
}

/// A replacement record for the oversized trigger.
fn replace_oversized(
    closure_refs: Vec<Value>,
    criterion_refs: Vec<Value>,
    fixture: &ClosureFixture,
) -> Value {
    json!({
        "replan_request_id": OVERSIZED_REQUEST,
        "old_task_id": fixture.oversized.as_str(),
        "completion_closure_task_refs": closure_refs,
        "criterion_rebindings": [{
            "criterion_id": fixture.criterion_id,
            "replacement_task_refs": criterion_refs
        }]
    })
}

/// A replacement record for the broad workaround trigger.
fn replace_workaround(
    closure_refs: Vec<Value>,
    criterion_refs: Vec<Value>,
    fixture: &ClosureFixture,
) -> Value {
    json!({
        "replan_request_id": WORKAROUND_REQUEST,
        "old_task_id": fixture.workaround.as_str(),
        "completion_closure_task_refs": closure_refs,
        "criterion_rebindings": [{
            "criterion_id": fixture.criterion_id,
            "replacement_task_refs": criterion_refs
        }]
    })
}

/// The live-shaped proposal: both replaced Tasks converge on ONE shared
/// completion closure, which is what a bounded decomposition of overlapping
/// authority legitimately produces.
fn shared_closure_proposal(fixture: &ClosureFixture) -> Value {
    let goal = load_closure(fixture);
    json!({
        "goal_id": fixture.goal_id.as_str(),
        "base_goal_revision": goal.revision(),
        "base_plan_revision": goal.plan_revision(),
        "summary": "replace both invalid Tasks with one shared bounded completion closure",
        "add_tasks": [
            read_only_task("dim-provenance", vec![]),
            read_only_task("dim-membership", vec![]),
            read_only_task("join-authority", vec![
                new_ref("dim-provenance"),
                new_ref("dim-membership")
            ])
        ],
        "replace_tasks": [
            replace_oversized(
                vec![new_ref("join-authority")],
                vec![new_ref("join-authority")],
                fixture
            ),
            replace_workaround(
                vec![new_ref("join-authority")],
                vec![new_ref("join-authority")],
                fixture
            )
        ]
    })
}

fn dependency_titles(goal: &Goal, task: &TaskId) -> Vec<String> {
    goal.tasks()[task]
        .dependencies()
        .iter()
        .map(|edge| goal.tasks()[edge.task_id()].title().to_owned())
        .collect()
}

fn total_edge_count(goal: &Goal) -> usize {
    goal.tasks()
        .values()
        .map(|task| task.dependencies().len())
        .sum()
}

/// The whole final dependency graph, normalized by Task title so two goals
/// built from independent fixtures are directly comparable.
fn normalized_graph(goal: &Goal) -> BTreeMap<String, Vec<String>> {
    goal.tasks()
        .values()
        .map(|task| {
            let mut dependencies = dependency_titles(goal, task.id());
            dependencies.sort();
            (task.title().to_owned(), dependencies)
        })
        .collect()
}

fn task_by_title<'a>(goal: &'a Goal, title: &str) -> &'a Task {
    goal.tasks()
        .values()
        .find(|task| task.title() == title)
        .unwrap_or_else(|| panic!("missing task {title}"))
}

#[test]
fn two_replaced_tasks_sharing_one_closure_converge_on_a_single_edge() {
    let fixture = closure_fixture(false, 0);
    let armed = load_closure(&fixture);
    let plan_revision_before = armed.plan_revision();
    let retry_before = armed.tasks()[&fixture.oversized].max_attempts();
    arm_closure_requests(&fixture);

    let after = apply_closure(&fixture, &shared_closure_proposal(&fixture)).unwrap();

    assert_eq!(
        after.tasks()[&fixture.oversized].status(),
        TaskStatus::Superseded
    );
    assert_eq!(
        after.tasks()[&fixture.workaround].status(),
        TaskStatus::Superseded
    );
    // The minimum regression for the live failure: the dependent converges on
    // the shared completion Task exactly once, not twice and not rejected.
    let join = task_by_title(&after, "Task join-authority").id().clone();
    assert_eq!(
        dependency_titles(&after, &fixture.downstream),
        vec!["Task join-authority"]
    );
    assert!(!has_dependency(
        &after,
        &fixture.downstream,
        &fixture.oversized
    ));
    assert!(!has_dependency(
        &after,
        &fixture.downstream,
        &fixture.workaround
    ));
    // The Writer still hangs off the surviving downstream Task.
    assert!(has_dependency(&after, &fixture.writer, &fixture.downstream));
    // A Task superseded by this very transaction is not a surviving dependent,
    // so its own dependency list is left exactly as it was.
    for superseded in [&fixture.oversized, &fixture.workaround] {
        assert_eq!(
            dependency_titles(&armed, superseded),
            dependency_titles(&after, superseded),
            "a superseded Task was rewired by its own transaction"
        );
    }
    // No active authority may retain a dependency on a superseded Task.
    for task in after.tasks().values() {
        if !task.is_active_plan_authority() {
            continue;
        }
        for edge in task.dependencies() {
            assert_ne!(
                after.tasks()[edge.task_id()].status(),
                TaskStatus::Superseded,
                "active Task {} still depends on a superseded Task",
                task.title()
            );
        }
    }
    // The final graph holds no duplicate edge.
    for task in after.tasks().values() {
        let unique = task
            .dependencies()
            .iter()
            .map(|edge| edge.task_id().clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), task.dependencies().len());
    }
    assert_eq!(after.plan_revision(), plan_revision_before + 1);
    assert!(
        after.tasks()[&fixture.oversized].max_attempts() >= retry_before,
        "replacement must not consume the trigger's remaining retry"
    );
    assert_eq!(
        after.tasks()[&fixture.oversized].attempts().len(),
        armed.tasks()[&fixture.oversized].attempts().len(),
        "old attempt history must be preserved"
    );
    assert_no_partial_replacement(&after, &fixture.oversized);
    assert_no_partial_replacement(&after, &fixture.workaround);
    // The next runnable authority is a bounded READ_ONLY replacement Task, and
    // the Writer is not runnable behind the surviving downstream Task.
    let provenance = task_by_title(&after, "Task dim-provenance").id().clone();
    assert!(has_dependency(&after, &join, &provenance));
    assert!(!has_dependency(&after, &fixture.writer, &join));
}

#[test]
fn overlapping_closures_union_into_one_dependency_set() {
    let fixture = closure_fixture(false, 0);
    arm_closure_requests(&fixture);
    let goal = load_closure(&fixture);
    // Two closures that share one member: {left, shared} and {shared, right}.
    // Each is the maximal node set of its own cone, and together the cones
    // cover every new Task.
    let proposal = json!({
        "goal_id": fixture.goal_id.as_str(),
        "base_goal_revision": goal.revision(),
        "base_plan_revision": goal.plan_revision(),
        "summary": "replace both invalid Tasks with partially overlapping closures",
        "add_tasks": [
            read_only_task("dim-one", vec![]),
            read_only_task("dim-two", vec![]),
            read_only_task("dim-three", vec![]),
            read_only_task("left-join", vec![new_ref("dim-one"), new_ref("dim-two")]),
            read_only_task("right-join", vec![new_ref("dim-two"), new_ref("dim-three")]),
            read_only_task("shared-join", vec![new_ref("dim-one"), new_ref("dim-three")])
        ],
        "replace_tasks": [
            replace_oversized(
                vec![new_ref("left-join"), new_ref("shared-join")],
                vec![new_ref("left-join"), new_ref("shared-join")],
                &fixture
            ),
            replace_workaround(
                vec![new_ref("shared-join"), new_ref("right-join")],
                vec![new_ref("shared-join"), new_ref("right-join")],
                &fixture
            )
        ]
    });
    let after = apply_closure(&fixture, &proposal).unwrap();
    let mut dependencies = dependency_titles(&after, &fixture.downstream);
    dependencies.sort();
    assert_eq!(
        dependencies,
        vec![
            "Task left-join".to_owned(),
            "Task right-join".to_owned(),
            "Task shared-join".to_owned(),
        ],
        "overlapping closures must union into one set with the shared member once"
    );
    for task in after.tasks().values() {
        let unique = task
            .dependencies()
            .iter()
            .map(|edge| edge.task_id().clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), task.dependencies().len());
    }
}

#[test]
fn replacement_rewiring_is_independent_of_the_replacement_ordering() {
    let first = closure_fixture(false, 0);
    arm_closure_requests(&first);
    let mut forward = shared_closure_proposal(&first);
    let after_forward = apply_closure(&first, &forward).unwrap();

    let second = closure_fixture(false, 0);
    arm_closure_requests(&second);
    let mut reversed = shared_closure_proposal(&second);
    let entries = reversed["replace_tasks"].as_array().unwrap().clone();
    reversed["replace_tasks"] = json!([entries[1].clone(), entries[0].clone()]);
    let after_reversed = apply_closure(&second, &reversed).unwrap();

    forward["replace_tasks"] = json!([entries[0].clone(), entries[1].clone()]);
    assert_eq!(
        normalized_graph(&after_forward),
        normalized_graph(&after_reversed),
        "the final DAG must not depend on the replacement mutation order"
    );
    assert_eq!(
        total_edge_count(&after_forward),
        total_edge_count(&after_reversed)
    );
}

#[test]
fn replacement_rewiring_preserves_every_unrelated_dependency() {
    let fixture = closure_fixture(true, 0);
    arm_closure_requests(&fixture);
    let before = load_closure(&fixture);
    let unrelated_before = dependency_titles(&before, &fixture.downstream);
    assert!(unrelated_before.contains(&"hold unrelated x".to_owned()));
    assert!(unrelated_before.contains(&"hold unrelated y".to_owned()));

    let after = apply_closure(&fixture, &shared_closure_proposal(&fixture)).unwrap();
    let mut dependencies = dependency_titles(&after, &fixture.downstream);
    dependencies.sort();
    assert_eq!(
        dependencies,
        vec![
            "Task join-authority".to_owned(),
            "hold unrelated x".to_owned(),
            "hold unrelated y".to_owned(),
        ],
        "unrelated edges must survive the transaction untouched"
    );
    for unrelated in &fixture.unrelated {
        assert!(
            has_dependency(&after, &fixture.downstream, unrelated),
            "unrelated authority {} was dropped",
            unrelated.as_str()
        );
        assert_eq!(
            after.tasks()[unrelated].status(),
            before.tasks()[unrelated].status()
        );
    }
}

#[test]
fn a_single_replacement_still_rewires_exactly_one_dependent() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let after = apply(&fixture, &decomposed_proposal(&fixture, REQUEST_ID)).unwrap();
    // B depended on A alone, so the ordinary single replacement rewires it
    // onto the closure and leaves no other edge behind.
    let mut dependencies = dependency_titles(&after, &fixture.b);
    dependencies.sort();
    assert_eq!(dependencies, vec!["Task join-authority".to_owned()]);
    assert!(has_dependency(&after, &fixture.c, &fixture.b));
    assert!(!has_dependency(&after, &fixture.b, &fixture.a));
    // An unrelated Task that this transaction never named is untouched.
    assert_eq!(
        after.tasks()[&fixture.workaround].status(),
        TaskStatus::Ready
    );
}

#[test]
fn a_collapsing_replacement_at_the_edge_ceiling_is_accepted() {
    // Existing 1022 edges plus the two internal edges of the replacement
    // closure is exactly the 1024 ceiling before rewiring. The transaction
    // removes two edges and adds one, so the final graph is 1023.
    let fixture = closure_fixture(false, 1019);
    arm_closure_requests(&fixture);
    let before = load_closure(&fixture);
    assert_eq!(total_edge_count(&before), 1022);
    let after = apply_closure(&fixture, &shared_closure_proposal(&fixture)).unwrap();
    assert_eq!(
        total_edge_count(&after),
        1023,
        "the final graph must be counted exactly, not conservatively"
    );
}

#[test]
fn a_replacement_that_would_oversize_the_final_graph_is_rejected() {
    // Existing 1021 edges plus three internal closure edges is exactly the
    // ceiling before rewiring, and this closure grows the dependent by one, so
    // the final graph would be 1025.
    let fixture = closure_fixture(false, 1018);
    arm_closure_requests(&fixture);
    let before_bytes = closure_bytes(&fixture);
    let before = load_closure(&fixture);
    assert_eq!(total_edge_count(&before), 1021);
    let goal = load_closure(&fixture);
    let proposal = json!({
        "goal_id": fixture.goal_id.as_str(),
        "base_goal_revision": goal.revision(),
        "base_plan_revision": goal.plan_revision(),
        "summary": "replace the oversized Task with a growing three-node closure",
        "add_tasks": [
            read_only_task("dim-one", vec![]),
            read_only_task("dim-two", vec![]),
            read_only_task("dim-three", vec![]),
            read_only_task("join-one", vec![new_ref("dim-one")]),
            read_only_task("join-two", vec![new_ref("dim-two")]),
            read_only_task("join-three", vec![new_ref("dim-three")])
        ],
        "replace_tasks": [replace_oversized(
            vec![
                new_ref("join-one"),
                new_ref("join-two"),
                new_ref("join-three")
            ],
            vec![
                new_ref("join-one"),
                new_ref("join-two"),
                new_ref("join-three")
            ],
            &fixture
        )]
    });
    assert!(
        apply_closure(&fixture, &proposal).is_err(),
        "an oversized final graph must be rejected"
    );
    assert_eq!(closure_bytes(&fixture), before_bytes);
    assert_eq!(load_closure(&fixture), before);
}

#[test]
fn a_late_final_dag_failure_commits_no_partial_replacement() {
    let fixture = closure_fixture(false, 0);
    arm_closure_requests(&fixture);
    let before = load_closure(&fixture);
    let before_bytes = closure_bytes(&fixture);
    let mut proposal = shared_closure_proposal(&fixture);
    // A replacement Task that depends on the very Task it replaces would make
    // the final graph invalid only after rewiring and supersession are done.
    proposal["add_tasks"][2]["dependencies"] = json!([existing_ref(&fixture.oversized)]);
    let result = apply_closure(&fixture, &proposal);
    assert!(result.is_err(), "an invalid final DAG must be rejected");
    assert_eq!(
        closure_bytes(&fixture),
        before_bytes,
        "no partial replacement may reach durable state"
    );
    let after = load_closure(&fixture);
    assert_eq!(after, before);
    assert_eq!(
        after.tasks()[&fixture.oversized].status(),
        TaskStatus::NeedsReplan
    );
    assert_eq!(
        after.tasks()[&fixture.workaround].status(),
        TaskStatus::NeedsReplan
    );
    assert_eq!(after.plan_revision(), before.plan_revision());
    assert!(
        after
            .tasks()
            .values()
            .all(|task| !task.title().starts_with("Task ")),
    );
}

#[test]
fn the_batch_rewiring_repair_does_not_weaken_unrelated_duplicate_checks() {
    // A new Task may not declare the same dependency twice.
    let fixture = closure_fixture(false, 0);
    arm_closure_requests(&fixture);
    let before_bytes = closure_bytes(&fixture);
    let mut proposal = shared_closure_proposal(&fixture);
    proposal["add_tasks"][0]["dependencies"] =
        json!([new_ref("dim-membership"), new_ref("dim-membership")]);
    assert!(apply_closure(&fixture, &proposal).is_err());
    assert_eq!(closure_bytes(&fixture), before_bytes);

    // An ordinary monotonic add may not repeat an existing edge.
    let fixture = closure_fixture(false, 0);
    arm_closure_requests(&fixture);
    let before_bytes = closure_bytes(&fixture);
    let goal = load_closure(&fixture);
    let duplicate_add = json!({
        "goal_id": fixture.goal_id.as_str(),
        "base_goal_revision": goal.revision(),
        "base_plan_revision": goal.plan_revision(),
        "summary": "add the same existing edge twice",
        "add_dependencies": [
            {"task_id": fixture.writer.as_str(), "depends_on": [fixture.downstream.as_str()]},
            {"task_id": fixture.writer.as_str(), "depends_on": [fixture.downstream.as_str()]}
        ]
    });
    assert!(apply_closure(&fixture, &duplicate_add).is_err());
    assert_eq!(closure_bytes(&fixture), before_bytes);

    // A Task may not depend on itself.
    let fixture = closure_fixture(false, 0);
    arm_closure_requests(&fixture);
    let before_bytes = closure_bytes(&fixture);
    let goal = load_closure(&fixture);
    let self_edge = json!({
        "goal_id": fixture.goal_id.as_str(),
        "base_goal_revision": goal.revision(),
        "base_plan_revision": goal.plan_revision(),
        "summary": "add a self dependency",
        "add_dependencies": [
            {"task_id": fixture.writer.as_str(), "depends_on": [fixture.writer.as_str()]}
        ]
    });
    assert!(apply_closure(&fixture, &self_edge).is_err());
    assert_eq!(closure_bytes(&fixture), before_bytes);
}

#[test]
fn replacement_without_a_durable_replan_request_authority_is_rejected() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    // No replan request was ever recorded.
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["replace_tasks"][0]["replan_request_id"] = json!("never-requested");
    expect_rejected(&fixture, &proposal);
}

#[test]
fn replacement_of_a_task_the_request_does_not_name_is_rejected() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["replace_tasks"][0]["old_task_id"] = json!(fixture.b.as_str());
    expect_rejected(&fixture, &proposal);
}

#[test]
fn replacement_pointing_at_the_replaced_task_as_its_completion_authority_is_rejected() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["replace_tasks"][0]["completion_closure_task_refs"] =
        json!([existing_ref(&fixture.a)]);
    expect_rejected(&fixture, &proposal);
}

#[test]
fn replacement_that_orphans_a_criterion_is_rejected() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["replace_tasks"][0]["criterion_rebindings"] = json!([]);
    expect_rejected(&fixture, &proposal);
}

#[test]
fn criterion_rebinding_to_a_task_outside_the_closure_is_rejected() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["replace_tasks"][0]["criterion_rebindings"][0]["replacement_task_refs"] =
        json!([new_ref("dimension-provenance")]);
    expect_rejected(&fixture, &proposal);
}

#[test]
fn replacement_introducing_a_cycle_is_rejected_atomically() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    // Make the join depend on itself's upstream, and the upstream on the join.
    proposal["add_tasks"][0]["dependencies"] = json!([new_ref("join-authority")]);
    expect_rejected(&fixture, &proposal);
}

#[test]
fn replacement_depending_on_the_superseded_task_is_rejected() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["add_tasks"][0]["dependencies"] = json!([existing_ref(&fixture.a)]);
    expect_rejected(&fixture, &proposal);
}

#[test]
fn replacement_widening_allowed_paths_is_rejected() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["add_tasks"][0]["scope"]["allowed_paths"] = json!(["/"]);
    expect_rejected(&fixture, &proposal);
}

#[test]
fn replacement_removing_required_verification_is_rejected() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["add_tasks"][0]["verification"] = json!([]);
    expect_rejected(&fixture, &proposal);
}

#[test]
fn replacement_introducing_a_forbidden_worker_capability_is_rejected() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["add_tasks"][0]["worker"] = json!("LOCAL_OPERATION");
    expect_rejected(&fixture, &proposal);
}

#[test]
fn replacement_mixed_with_ordinary_monotonic_changes_is_rejected() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["add_dependencies"] =
        json!([{"task": existing_ref(&fixture.b), "dependency": new_ref("join-authority")}]);
    expect_rejected(&fixture, &proposal);
}

#[test]
fn replacement_closure_that_is_not_the_maximal_set_is_rejected() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    // Declare an intermediate Task as the closure even though the join is above it.
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["replace_tasks"][0]["completion_closure_task_refs"] =
        json!([new_ref("dimension-provenance")]);
    expect_rejected(&fixture, &proposal);
}

#[test]
fn a_replan_request_from_an_old_plan_revision_can_never_replace() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let plan_revision_at_request = load(&fixture).plan_revision();

    // A committed ordinary replan advances the plan revision, so the durable
    // replacement authority recorded against the old revision goes stale.
    let current = load(&fixture);
    let ordinary = json!({
        "goal_id": fixture.goal_id.as_str(),
        "base_goal_revision": current.revision(),
        "base_plan_revision": current.plan_revision(),
        "summary": "add a bounded prerequisite and resolve the trigger",
        "add_tasks": [read_only_task("ordinary-prerequisite", vec![])],
        "add_dependencies": [{
            "task": existing_ref(&fixture.a),
            "dependency": new_ref("ordinary-prerequisite")
        }],
        "resolve_needs_replan": [fixture.a.as_str()]
    });
    let after_ordinary = apply(&fixture, &ordinary).unwrap();
    assert_eq!(after_ordinary.plan_revision(), plan_revision_at_request + 1);
    assert_eq!(
        after_ordinary.tasks()[&fixture.a].status(),
        TaskStatus::Pending
    );

    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["base_goal_revision"] = json!(load(&fixture).revision());
    proposal["base_plan_revision"] = json!(plan_revision_at_request);
    expect_rejected(&fixture, &proposal);
    assert_eq!(load(&fixture).failed_task_replacements().len(), 0);
}

// ---------------------------------------------------------------------------
// TDD PHASE 6 — budget-aware decomposition
// ---------------------------------------------------------------------------

#[test]
fn sizing_profile_is_deterministic_and_flags_a_broad_cross_product() {
    // 48 independent entities x 6 evidence dimensions, with no domain
    // vocabulary anywhere: purely structural counts.
    let broad = TaskSizingProfile::derive(48, 6);
    assert_eq!(broad.evidence_shape_estimate, 288);
    assert!(broad.over_budget);
    assert!(broad.evidence_shape_estimate > MAX_SINGLE_READONLY_TASK_RECORDS);
    assert_eq!(
        broad,
        TaskSizingProfile::derive(48, 6),
        "must be deterministic"
    );
    assert!(broad.guidance.iter().any(|line| line.contains("too broad")));

    let bounded = TaskSizingProfile::derive(4, 3);
    assert_eq!(bounded.evidence_shape_estimate, 12);
    assert!(!bounded.over_budget);
    assert!(
        !bounded
            .guidance
            .iter()
            .any(|line| line.contains("too broad"))
    );
}

#[test]
fn replanner_request_exposes_structured_sizing_and_compact_output_contract() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let goal = load(&fixture);
    let request = crate::replanner::replanner_request_for_goal(&goal, &fixture.session).unwrap();
    let json = serde_json::to_value(&request).unwrap();
    let sizing = &json["sizing"];
    assert!(sizing["independent_entity_count"].is_number());
    assert!(sizing["evidence_dimension_count"].is_number());
    assert!(sizing["max_single_readonly_task_records"].is_number());
    assert!(sizing["over_budget"].is_boolean());
    assert!(sizing["guidance"].as_array().unwrap().len() >= 3);
    let contract = &json["readonly_output_contract"];
    assert!(contract["max_evidence_items"].is_number());
    assert!(contract["max_summary_bytes"].is_number());
    assert!(contract["max_evidence_total_bytes"].is_number());
    assert!(contract["guidance"].as_str().unwrap().contains("32"));

    // Planner gets the same host-derived guidance for an unplanned Goal.
    let unplanned = Goal::new(
        fixture.session.id.clone(),
        fixture.repo.clone(),
        "broad structured research",
        None,
        vec![],
        vec!["cover 48 entities across 6 evidence dimensions".to_owned()],
        NOW,
    )
    .unwrap();
    let planner_json = serde_json::to_value(
        planner::planner_request_for_goal(&unplanned, &fixture.session).unwrap(),
    )
    .unwrap();
    assert!(planner_json["sizing"]["over_budget"].is_boolean());
    assert!(planner_json["readonly_output_contract"]["max_evidence_items"].is_number());
}

#[test]
fn a_size_failure_may_not_be_repaired_by_another_broad_reassessment() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    // broad_proposal is the same shape twice: a broad reassessment plus a
    // rejoin. It is not a materially smaller decomposition of the evidence.
    expect_rejected(&fixture, &broad_proposal(&fixture, REQUEST_ID));
}

#[test]
fn a_single_flat_replacement_task_cannot_repair_a_size_failure() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let goal = load(&fixture);
    let proposal = json!({
        "goal_id": fixture.goal_id.as_str(),
        "base_goal_revision": goal.revision(),
        "base_plan_revision": goal.plan_revision(),
        "summary": "one giant read-only Task again",
        "add_tasks": [read_only_task("giant", vec![])],
        "replace_tasks": [{
            "replan_request_id": REQUEST_ID,
            "old_task_id": fixture.a.as_str(),
            "completion_closure_task_refs": [new_ref("giant")],
            "criterion_rebindings": [{
                "criterion_id": fixture.criterion_id,
                "replacement_task_refs": [new_ref("giant")]
            }]
        }]
    });
    expect_rejected(&fixture, &proposal);
}

#[test]
fn replacement_objectives_are_bounded_to_the_compact_contract() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["add_tasks"][0]["objective"] =
        json!("x".repeat(planner::MAX_REPLACEMENT_OBJECTIVE_BYTES + 1));
    expect_rejected(&fixture, &proposal);
}

// ---------------------------------------------------------------------------
// TDD PHASE 7 — negative transient case
// ---------------------------------------------------------------------------

#[test]
fn a_transient_model_failure_may_never_be_routed_into_replacement() {
    let fixture = fixture(FailureClass::TransientModelFailure);
    let before = load(&fixture);
    let before_bytes = bytes(&fixture);

    // The host refuses to even record a replacement request.
    let error = goal_resume(
        &replan_args(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION"),
        &fixture.session,
        &fixture.store,
    )
    .unwrap_err();
    assert_eq!(error.code(), "INVALID_GOAL_STATE");
    assert_eq!(bytes(&fixture), before_bytes);

    // The Task keeps its bounded retry and is not superseded.
    let after = load(&fixture);
    assert_eq!(after.tasks()[&fixture.a].status(), TaskStatus::Retryable);
    assert_eq!(after.tasks()[&fixture.a].semantic_attempts_remaining(), 1);
    assert!(after.failed_task_replan_requests().is_empty());
    assert!(after.failed_task_replacements().is_empty());
    assert_eq!(after.revision(), before.revision());
}

#[test]
fn a_transient_failure_may_never_be_replaced_even_without_a_replan_request() {
    let fixture = fixture(FailureClass::TransientModelFailure);
    let goal = load(&fixture);
    let proposal = json!({
        "goal_id": fixture.goal_id.as_str(),
        "base_goal_revision": goal.revision(),
        "base_plan_revision": goal.plan_revision(),
        "summary": "supersede on a transient failure",
        "add_tasks": [
            read_only_task("x1", vec![]),
            read_only_task("x2", vec![]),
            read_only_task("x3", vec![new_ref("x1"), new_ref("x2")])
        ],
        "replace_tasks": [{
            "replan_request_id": REQUEST_ID,
            "old_task_id": fixture.a.as_str(),
            "completion_closure_task_refs": [new_ref("x3")],
            "criterion_rebindings": [{
                "criterion_id": fixture.criterion_id,
                "replacement_task_refs": [new_ref("x3")]
            }]
        }]
    });
    expect_rejected(&fixture, &proposal);
    assert_eq!(
        load(&fixture).tasks()[&fixture.a].status(),
        TaskStatus::Retryable
    );
}

#[test]
fn two_replacements_sharing_one_criterion_never_drop_a_replaced_proof() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    // The criterion requires both replaceable Tasks: the post-attempt trigger A
    // and the pristine pre-execution workaround W.
    let before_rejection = load(&fixture);
    goal_resume(
        &json!({
            "session_id": fixture.session.id,
            "goal_id": fixture.goal_id.as_str(),
            "pre_execution_plan_rejection": {
                "request_id": "reject-broad-workaround-1",
                "expected_goal_revision": before_rejection.revision(),
                "expected_plan_revision": before_rejection.plan_revision(),
                "trigger_task_id": fixture.workaround.as_str(),
                "reason": "the broad reassessment must never run"
            }
        }),
        &fixture.session,
        &fixture.store,
    )
    .unwrap();
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    request_pre_execution_replan(
        &fixture,
        "replace-broad-workaround-1",
        &fixture.workaround,
        "REQUIRE_DECOMPOSITION",
        "reject-broad-workaround-1",
    );

    // Two independent maximal closure nodes, so each replaced proof is rebound
    // to distinct authority. Overwriting instead of unioning would leave only
    // the second one bound and silently drop the first.
    let goal = load(&fixture);
    let proposal = json!({
        "goal_id": fixture.goal_id.as_str(),
        "base_goal_revision": goal.revision(),
        "base_plan_revision": goal.plan_revision(),
        "summary": "replace both invalid Tasks with two bounded closures",
        "add_tasks": [
            read_only_task("dim-one", vec![]),
            read_only_task("dim-two", vec![]),
            read_only_task("join-one", vec![new_ref("dim-one")]),
            read_only_task("join-two", vec![new_ref("dim-two")])
        ],
        "replace_tasks": [
            {
                "replan_request_id": REQUEST_ID,
                "old_task_id": fixture.a.as_str(),
                "completion_closure_task_refs": [new_ref("join-one")],
                "criterion_rebindings": [{
                    "criterion_id": fixture.criterion_id,
                    "replacement_task_refs": [new_ref("join-one")]
                }]
            },
            {
                "replan_request_id": "replace-broad-workaround-1",
                "old_task_id": fixture.workaround.as_str(),
                "completion_closure_task_refs": [new_ref("join-two")],
                "criterion_rebindings": [{
                    "criterion_id": fixture.criterion_id,
                    "replacement_task_refs": [new_ref("join-two")]
                }]
            }
        ]
    });
    let after = apply(&fixture, &proposal).unwrap();
    assert_eq!(after.tasks()[&fixture.a].status(), TaskStatus::Superseded);
    assert_eq!(
        after.tasks()[&fixture.workaround].status(),
        TaskStatus::Superseded
    );
    let spec = after.final_verification_spec().unwrap();
    let binding = spec
        .criterion_bindings()
        .iter()
        .find(|binding| binding.criterion_id().as_str() == fixture.criterion_id)
        .unwrap();
    let bound = binding
        .requirements()
        .iter()
        .map(|requirement| requirement.task_id().clone())
        .collect::<BTreeSet<_>>();
    let join_one = tasks_by_title_fragment(&after, "join-one")[0].id().clone();
    let join_two = tasks_by_title_fragment(&after, "join-two")[0].id().clone();
    // Both replaced proofs remain bound to distinct live authority.
    assert!(bound.contains(&join_one), "join-one proof was dropped");
    assert!(bound.contains(&join_two), "join-two proof was dropped");
    assert!(!bound.contains(&fixture.a));
    assert!(!bound.contains(&fixture.workaround));
    for record in after.failed_task_replacements() {
        assert_eq!(record.rebound_criterion_ids().len(), 1);
    }
    assert_no_partial_replacement(&after, &fixture.a);
    assert_no_partial_replacement(&after, &fixture.workaround);
}

#[test]
fn a_replacement_closure_may_not_widen_the_superseded_scope() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");

    // Broader allowed paths, still inside the Goal cwd: not a path-escape, so
    // only the host's explicit scope-narrowing rule can catch this.
    let mut widened = decomposed_proposal(&fixture, REQUEST_ID);
    widened["add_tasks"][0]["scope"]["allowed_paths"] = json!(["."]);
    expect_rejected(&fixture, &widened);

    // Dropping an inherited forbidden path is also a widening.
    let mut unwalled = decomposed_proposal(&fixture, REQUEST_ID);
    unwalled["add_tasks"][0]["scope"]["forbidden_paths"] = json!([]);
    expect_rejected(&fixture, &unwalled);

    // Escalating the operation kind is a widening too.
    let mut escalated = decomposed_proposal(&fixture, REQUEST_ID);
    escalated["add_tasks"][0]["scope"]["operation_kind"] = json!("LOCAL_MUTATION");
    expect_rejected(&fixture, &escalated);

    // Relaxing replay safety is likewise rejected.
    let mut relaxed = decomposed_proposal(&fixture, REQUEST_ID);
    relaxed["add_tasks"][0]["scope"]["replay_safety"] = json!("VERIFY_BEFORE_RETRY");
    expect_rejected(&fixture, &relaxed);
}

#[test]
#[cfg(unix)]
fn a_symlinked_workspace_root_evaluates_narrowing_on_the_real_path() {
    // The Replanner compares the superseded Task's stored scope against a
    // replacement scope it normalized from the canonical Goal cwd. Those two
    // spellings are the same directory only when both are the real path, which
    // is not automatic on a host whose temporary directory is itself reached
    // through a symlink. Build the fixture that way explicitly so a genuine
    // narrowing is exercised rather than left to the host's path layout.
    let fixture = symlinked_fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let after = apply(&fixture, &decomposed_proposal(&fixture, REQUEST_ID)).expect(
        "a replacement that really narrows the superseded scope must be accepted through a symlinked workspace root",
    );
    assert!(!has_dependency(&after, &fixture.b, &fixture.a));
    assert_no_partial_replacement(&after, &fixture.a);

    // The alias is not an escape hatch in the other direction either: a scope
    // that reaches past the superseded Task is still refused.
    let mut widened = decomposed_proposal(&fixture, REQUEST_ID);
    widened["add_tasks"][0]["scope"]["allowed_paths"] = json!(["."]);
    expect_rejected(&fixture, &widened);
}

#[test]
fn a_closure_member_may_not_depend_on_a_superseded_task() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    // The closure member itself names the superseded Task as a dependency.
    let mut proposal = decomposed_proposal(&fixture, REQUEST_ID);
    proposal["add_tasks"][3]["dependencies"] = json!([
        new_ref("dimension-provenance"),
        new_ref("dimension-membership"),
        new_ref("dimension-semantics"),
        existing_ref(&fixture.a)
    ]);
    expect_rejected(&fixture, &proposal);
}

#[test]
fn a_resolved_replay_forbidden_task_can_never_be_rearmed_as_ready() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    // An ordinary replan resolves the trigger back to PENDING. It must not be
    // re-armed as READY by any later readiness propagation.
    let current = load(&fixture);
    let ordinary = json!({
        "goal_id": fixture.goal_id.as_str(),
        "base_goal_revision": current.revision(),
        "base_plan_revision": current.plan_revision(),
        "summary": "add a bounded prerequisite and resolve the trigger",
        "add_tasks": [read_only_task("ordinary-prerequisite", vec![])],
        "add_dependencies": [{
            "task": existing_ref(&fixture.a),
            "dependency": new_ref("ordinary-prerequisite")
        }],
        "resolve_needs_replan": [fixture.a.as_str()]
    });
    let after = apply(&fixture, &ordinary).unwrap();
    assert_eq!(after.tasks()[&fixture.a].status(), TaskStatus::Pending);

    // Direct PENDING -> READY is refused by the durable state machine.
    let denied = fixture.store.mutate_goal_snapshot(
        &fixture.session.id,
        &fixture.goal_id,
        after.revision(),
        |goal, now| {
            goal.transition_task(
                &fixture.a,
                TaskStatus::Ready,
                TaskTransitionContext::default(),
                now,
            )
        },
    );
    assert!(denied.is_err());
    assert!(!after.tasks()[&fixture.a].unchanged_retry_permitted());
}

/// Give the ready broad workaround its own host-output-limit failure, so it
/// becomes a second, independent replaceable trigger.
fn fail_workaround(fixture: &Fixture) {
    let current = load(fixture);
    fixture
        .store
        .mutate_goal_snapshot(
            &fixture.session.id,
            &fixture.goal_id,
            current.revision(),
            |goal, now| {
                goal.transition_task(
                    &fixture.workaround,
                    TaskStatus::Running,
                    TaskTransitionContext::default(),
                    now,
                )?;
                goal.task_record_latest_worker_report(
                    &fixture.workaround,
                    WorkerReport::new("READONLY_BACKEND_ERROR: shape failure", Vec::new()),
                )?;
                goal.task_record_latest_attempt_failure_class(
                    &fixture.workaround,
                    FailureClass::HostOutputLimit,
                )?;
                goal.task_bind_latest_attempt_execution(
                    &fixture.workaround,
                    None,
                    None,
                    None,
                    Some(SideEffectClass::None),
                    Some(SideEffectState::ConfirmedNotPerformed),
                    Some(1),
                    Some(0),
                )?;
                goal.transition_task(
                    &fixture.workaround,
                    TaskStatus::Retryable,
                    TaskTransitionContext::default(),
                    now,
                )
            },
        )
        .unwrap();
}

#[test]
fn a_stale_replay_forbidden_task_never_wedges_an_unrelated_replacement() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    fail_workaround(&fixture);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    // Resolve A through an ordinary replan: it lands PENDING while still
    // carrying its HostOutputLimit attempt, so it must never be re-armed.
    let current = load(&fixture);
    apply(
        &fixture,
        &json!({
            "goal_id": fixture.goal_id.as_str(),
            "base_goal_revision": current.revision(),
            "base_plan_revision": current.plan_revision(),
            "summary": "add a bounded prerequisite and resolve the trigger",
            "add_tasks": [read_only_task("ordinary-prerequisite", vec![])],
            "add_dependencies": [{
                "task": existing_ref(&fixture.a),
                "dependency": new_ref("ordinary-prerequisite")
            }],
            "resolve_needs_replan": [fixture.a.as_str()]
        }),
    )
    .unwrap();
    let stale = load(&fixture);
    assert_eq!(stale.tasks()[&fixture.a].status(), TaskStatus::Pending);
    assert!(!stale.tasks()[&fixture.a].unchanged_retry_permitted());

    // A replacement for a *different* trigger must still commit: the stale
    // PENDING replay-forbidden Task may not fail an unrelated transaction.
    request_replan(
        &fixture,
        "replace-broad-workaround-1",
        &fixture.workaround,
        "REQUIRE_DECOMPOSITION",
    );

    let goal = load(&fixture);
    let mut proposal = decomposed_proposal(&fixture, "replace-broad-workaround-1");
    proposal["base_goal_revision"] = json!(goal.revision());
    proposal["base_plan_revision"] = json!(goal.plan_revision());
    proposal["replace_tasks"][0]["old_task_id"] = json!(fixture.workaround.as_str());
    proposal["replace_tasks"][0]["criterion_rebindings"] = json!([{
        "criterion_id": fixture.criterion_id,
        "replacement_task_refs": [new_ref("join-authority")]
    }]);
    let after = apply(&fixture, &proposal).unwrap();
    assert_eq!(
        after.tasks()[&fixture.workaround].status(),
        TaskStatus::Superseded
    );
    assert_eq!(after.failed_task_replacements().len(), 1);
    // The stale Task stayed PENDING: it was neither promoted nor lost.
    assert_eq!(after.tasks()[&fixture.a].status(), TaskStatus::Pending);
    assert!(!after.tasks()[&fixture.a].unchanged_retry_permitted());
    // The bounded replacement work is runnable, so repair liveness is preserved.
    let runnable = after
        .tasks()
        .values()
        .filter(|task| task.status() == TaskStatus::Ready)
        .collect::<Vec<_>>();
    assert!(!runnable.is_empty());
    for task in runnable {
        assert_eq!(task.worker(), WorkerKind::CodexReadonly);
    }
    assert_no_partial_replacement(&after, &fixture.workaround);
}

// ---------------------------------------------------------------------------
// TDD PHASE 8 — pre-existing safety must not regress
// ---------------------------------------------------------------------------

#[test]
fn existing_safety_invariants_hold_across_a_replacement() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    request_replan(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION");
    let before = load(&fixture);
    let before_checkpoints = before.checkpoints().len();
    let after = apply(&fixture, &decomposed_proposal(&fixture, REQUEST_ID)).unwrap();

    // Durable Goal history is append-only.
    assert!(after.checkpoints().len() > before_checkpoints);
    assert!(
        after
            .checkpoints()
            .iter()
            .any(|checkpoint| checkpoint.reason() == CheckpointReason::ReplanCommitted)
    );
    // Attempt history is preserved verbatim.
    assert_eq!(
        after.tasks()[&fixture.a].attempts(),
        before.tasks()[&fixture.a].attempts()
    );
    // Retry accounting is preserved, not replenished and not consumed.
    assert_eq!(after.tasks()[&fixture.a].max_attempts(), 2);
    assert_eq!(after.tasks()[&fixture.a].semantic_attempts_consumed(), 1);
    // No UNKNOWN side effects were introduced.
    assert!(!after.has_unknown_side_effects());
    // Writer serialization: the single Writer stays downstream of B.
    let writers = after
        .tasks()
        .values()
        .filter(|task| {
            !task.is_terminal() && task.scope().operation_kind() != TaskOperationKind::ReadOnly
        })
        .count();
    assert_eq!(writers, 1);
    // Goal is not terminal and not blocked.
    assert!(!after.is_terminal());
    assert!(after.status() == GoalStatus::Running);
    // The whole committed snapshot satisfies every durable invariant, and so
    // does the reloaded durable copy.
    after.validate().unwrap();
    load(&fixture).validate().unwrap();
    assert_eq!(load(&fixture), after);
}

#[test]
fn an_oversized_task_can_never_be_replayed_unchanged() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    let current = load(&fixture);
    let started = fixture.store.mutate_goal_snapshot(
        &fixture.session.id,
        &fixture.goal_id,
        current.revision(),
        |goal, now| {
            goal.transition_task(
                &fixture.a,
                TaskStatus::Ready,
                TaskTransitionContext::default(),
                now,
            )?;
            goal.transition_task(
                &fixture.a,
                TaskStatus::Running,
                TaskTransitionContext::default(),
                now,
            )
        },
    );
    // A is RETRYABLE after its host-output-limit attempt, so the durable
    // transition back to READY must be refused rather than replaying the shape.
    assert!(started.is_err());
    assert_eq!(
        load(&fixture).tasks()[&fixture.a].status(),
        TaskStatus::Retryable
    );
    assert_eq!(load(&fixture).tasks()[&fixture.a].attempts().len(), 1);
}

#[test]
fn goal_pause_and_cancel_still_refuse_a_replacement_goal_with_active_state() {
    let fixture = fixture(FailureClass::HostOutputLimit);
    let paused = load(&fixture);
    let result = fixture.store.mutate_goal_snapshot(
        &fixture.session.id,
        &fixture.goal_id,
        paused.revision(),
        |goal, now| {
            goal.transition_to(GoalStatus::Pausing, now)?;
            goal.add_checkpoint(CheckpointReason::Pause, now)
        },
    );
    assert!(result.is_ok());
    let cancelled = load(&fixture);
    let cancelled = fixture
        .store
        .mutate_goal_snapshot(
            &fixture.session.id,
            &fixture.goal_id,
            cancelled.revision(),
            |goal, now| goal.transition_to(GoalStatus::Cancelling, now),
        )
        .unwrap();
    assert_eq!(cancelled.status(), GoalStatus::Cancelling);
    // A replacement request against a cancelling Goal is refused.
    let error = goal_resume(
        &replan_args(&fixture, REQUEST_ID, &fixture.a, "REQUIRE_DECOMPOSITION"),
        &fixture.session,
        &fixture.store,
    )
    .unwrap_err();
    assert_eq!(error.code(), "INVALID_GOAL_STATE");
}
