//! Replanner Hardening V1: active-graph quota separation, bounded model
//! context, host-generated history summarization, and the hard request-size
//! ceiling.
//!
//! Two things are being proven here, and they are deliberately kept apart.
//!
//! First, measurement. Every test in the `measurement` group builds a
//! deterministic, test-owned synthetic Goal and records the *serialized byte
//! size* of the host-generated Replanner request, because a Task count proves
//! nothing about request size. The synthetic Goals are built through the real
//! `materialize_initial_plan_with_contract` and `apply_task_replacements`
//! transactions, so every one of them is a Goal the host itself would accept:
//! no committed snapshot can contain an active Task that depends on superseded
//! work, a criterion bound to a superseded Task, two active Writers, or an
//! unresolved side effect.
//!
//! Second, the invariants. `docs/REPLANNER_HARDENING_V1_DESIGN.md` is the frozen
//! contract; this file is its executable form. The negative tests matter most:
//! compaction must not become a way to smuggle an invalid plan past the host.
//! The host revalidates every candidate against the complete durable Goal, so a
//! proposal the model could not justify from its compacted context must still be
//! rejected on the full graph.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config;
use crate::goal::{
    CompletionCriterionId, FailedTaskReplacementMutation, FailedTaskReplanPolicy,
    FailedTaskReplanTriggerKind, Goal, GoalCriterionBinding, GoalFinalVerificationSpec,
    GoalVerificationRequirement,
};
use crate::replanner::{ReplannerRequest, replanner_request_for_goal};
use crate::task::{
    ReplaySafety, Task, TaskId, TaskOperationKind, TaskScope, TaskStatus, TaskTransitionContext,
    VerificationCheckResult, VerificationOutcome, VerificationResult, VerificationSpec, WorkerKind,
    WorkerReport,
};
use crate::task_store::TaskStore;

const NOW: &str = "2026-10-02T00:00:00Z";

// ===========================================================================
// Fixture
// ===========================================================================

/// How to shape one synthetic Goal.
#[derive(Clone, Debug)]
struct Shape {
    /// Total read-only leaf Tasks materialized in the initial plan.
    leaf_tasks: usize,
    /// How many of those leaves hang off a single deep chain.
    chain_depth: usize,
    /// How many leaves depend on every leaf before them (broad fan-out).
    fan_out: usize,
    /// How many leaves are already `COMPLETED` and unrelated to the trigger.
    completed_leaves: usize,
    /// Replacement rounds applied before the request is built. Each round
    /// supersedes one active Task and adds one active Task, so the active graph
    /// stays constant while durable history grows. This is the PokéCPU shape.
    history_rounds: usize,
    /// How many durable evidence items each superseded Task carries. Exactly one
    /// attempt is possible: the host refuses to re-arm a `HOST_OUTPUT_LIMIT`
    /// attempt, which is what routes a Task to replacement at all.
    history_attempts: usize,
    /// How many unrelated completed Tasks are bound to a second criterion.
    unrelated_criterion_tasks: usize,
    /// Extra durable Goal blockers to accumulate.
    goal_blockers: usize,
    /// Description length of the completion criteria, to exercise prose weight.
    criterion_chars: usize,
}

impl Default for Shape {
    fn default() -> Self {
        Self {
            leaf_tasks: 4,
            chain_depth: 0,
            fan_out: 0,
            completed_leaves: 0,
            history_rounds: 0,
            history_attempts: 1,
            unrelated_criterion_tasks: 0,
            goal_blockers: 0,
            criterion_chars: 48,
        }
    }
}

struct Fixture {
    #[expect(dead_code, reason = "removed with the Fixture, kept for Drop")]
    root: PathBuf,
    #[expect(dead_code, reason = "consumed by the post-compaction assertions")]
    repo: PathBuf,
    session: config::Session,
    goal: Goal,
    /// The Task the next Replanner invocation is about.
    #[expect(dead_code, reason = "consumed by the post-compaction assertions")]
    trigger: TaskId,
    /// Every non-superseded Task, in durable `TaskId` order.
    active: Vec<TaskId>,
    /// Every superseded Task, in durable `TaskId` order.
    history: Vec<TaskId>,
    #[expect(dead_code, reason = "consumed by the post-compaction assertions")]
    criterion_id: CompletionCriterionId,
    #[expect(dead_code, reason = "consumed by the post-compaction assertions")]
    unrelated_criterion_id: CompletionCriterionId,
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

fn writer_scope(repo: &Path) -> TaskScope {
    TaskScope::new(
        vec![repo.to_path_buf()],
        vec![repo.join(".git").to_path_buf()],
        TaskOperationKind::LocalMutation,
        ReplaySafety::VerifyBeforeRetry,
    )
}

/// A durable replacement record stores a 64-hex-character canonical proposal
/// digest, and `Goal::validate` enforces that shape. Derive one deterministically
/// from a label so the fixture produces records the host itself would accept.
fn synthetic_digest(label: &str) -> String {
    Sha256::digest(label.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn verification(requirement_id: &str) -> Vec<VerificationSpec> {
    vec![VerificationSpec::StructuredEvidence {
        requirement_id: requirement_id.to_owned(),
    }]
}

fn leaf(repo: &Path, index: usize, mandatory: bool) -> Task {
    Task::new(
        format!("leaf-{index:04}"),
        format!("inspect authority for entity class {index:04}"),
        mandatory,
        WorkerKind::CodexReadonly,
        readonly_scope(repo),
        verification(&format!("entity.{index:04}.observed")),
        2,
        1,
        NOW,
    )
    .expect("leaf Task is well formed")
}

fn writer(repo: &Path) -> Task {
    Task::new(
        "apply-bounded-change",
        "apply the bounded downstream change",
        true,
        WorkerKind::CodexWriter,
        writer_scope(repo),
        verification("change.applied"),
        1,
        1,
        NOW,
    )
    .expect("writer Task is well formed")
}

/// Add every dependency in one call. `Task::strengthen_dependencies` is
/// monotonic — it rejects a candidate set that drops an existing edge — so a
/// fan-out must be expressed in a single call, not one call per edge.
fn add_dependencies(task: &mut Task, dependencies: &[&TaskId]) {
    task.strengthen_dependencies(
        dependencies
            .iter()
            .map(|id| crate::task::TaskDependency::completed((*id).clone()))
            .collect(),
        &BTreeSet::new(),
    )
    .expect("dependencies strengthen");
}

fn add_dependency(task: &mut Task, dependency: &TaskId) {
    add_dependencies(task, &[dependency]);
}

/// Build a deterministic synthetic Goal. Every parameter comes from `Shape`;
/// nothing reads the clock, the filesystem, or a random source after the
/// temporary directory is created.
fn build(label: &str, shape: Shape) -> Fixture {
    let created =
        std::env::temp_dir().join(format!("replanner-compaction-{label}-{}", Uuid::new_v4()));
    fs::create_dir_all(created.join("repo").join("src")).unwrap();
    // Resolve the root before any of it reaches the Goal: a stored Goal only ever
    // holds canonical absolute paths, and the scope-containment check compares an
    // aliased path against a canonical one.
    let root = fs::canonicalize(&created).unwrap();
    let repo = root.join("repo");
    let state = root.join("state");
    fs::write(repo.join("sentinel.txt"), b"unchanged\n").unwrap();

    let session = config::Session {
        id: format!("compaction-{}", Uuid::new_v4()),
        cwd: repo.clone(),
        permitted_directories: vec![repo.clone()],
    };
    let _store = TaskStore::with_state_root(state);

    let padded = "criterion authority ".repeat(shape.criterion_chars.div_ceil(19).max(1));
    let criterion_description: String = padded.chars().take(shape.criterion_chars.max(1)).collect();
    let mut goal = Goal::new(
        session.id.clone(),
        repo.clone(),
        "repair the plan monotonically with bounded decomposed work",
        Some("compaction fixture".to_owned()),
        vec!["stay inside repository authority".to_owned()],
        vec![
            criterion_description.clone(),
            "unrelated historical authority is durably retained".to_owned(),
        ],
        NOW,
    )
    .expect("Goal is well formed");
    let criterion_id = goal.completion_criteria()[0].id().clone();
    let unrelated_criterion_id = goal.completion_criteria()[1].id().clone();

    // --- initial plan -----------------------------------------------------
    let mut tasks: Vec<Task> = Vec::new();
    let mut leaf_ids: Vec<TaskId> = Vec::new();
    for index in 0..shape.leaf_tasks {
        let task = leaf(&repo, index, true);
        leaf_ids.push(task.id().clone());
        tasks.push(task);
    }
    // Deep chain: leaf[i] depends on leaf[i-1].
    for index in 1..shape.chain_depth.min(shape.leaf_tasks) {
        let dependency = leaf_ids[index - 1].clone();
        add_dependency(&mut tasks[index], &dependency);
    }
    // Broad fan-out: leaf[fan_out + i] depends on every one of the first
    // `fan_out` leaves.
    if shape.fan_out > 0 {
        let earlier: Vec<TaskId> = leaf_ids.iter().take(shape.fan_out).cloned().collect();
        let borrowed: Vec<&TaskId> = earlier.iter().collect();
        for task in tasks
            .iter_mut()
            .skip(shape.fan_out)
            .take(shape.leaf_tasks.min(shape.fan_out * 2) - shape.fan_out)
        {
            add_dependencies(task, &borrowed);
        }
    }

    // A single Writer downstream of the first leaf, so the plan has the one
    // active writer the serializer allows and no more.
    let mut writer_task = writer(&repo);
    add_dependency(&mut writer_task, &leaf_ids[0]);
    tasks.push(writer_task);

    let contract = GoalFinalVerificationSpec::new(
        1,
        vec![
            GoalCriterionBinding::new(
                criterion_id.clone(),
                leaf_ids
                    .iter()
                    .map(|id| GoalVerificationRequirement::TaskVerified {
                        task_id: id.clone(),
                    })
                    .collect(),
            ),
            // Seeded with one real requirement because a required criterion may
            // not start empty; the unrelated completed Tasks are added to it
            // below so the request carries bindings no replacement can touch.
            GoalCriterionBinding::new(
                unrelated_criterion_id.clone(),
                vec![GoalVerificationRequirement::TaskVerified {
                    task_id: leaf_ids[leaf_ids.len() - 1].clone(),
                }],
            ),
        ],
    );
    goal.materialize_initial_plan_with_contract(tasks, contract, NOW)
        .expect("initial plan materializes");

    // Unrelated completed Tasks bound to a second criterion, so the request has
    // criterion bindings that no replacement can possibly touch.
    for index in 0..(shape.completed_leaves + shape.unrelated_criterion_tasks) {
        let task = Task::new(
            format!("completed-{index:04}"),
            format!("durable historical authority for completed work {index:04}"),
            true,
            WorkerKind::CodexReadonly,
            readonly_scope(&repo),
            verification(&format!("completed.{index:04}.verified")),
            2,
            goal.plan_revision() + 1,
            NOW,
        )
        .expect("completed historical Task is well formed");
        let id = task.id().clone();
        goal.apply_replan_mutation(
            crate::goal::ReplanMutation {
                new_tasks: vec![task],
                add_dependencies: Vec::new(),
                add_verification: Vec::new(),
                strengthen_mandatory: Vec::new(),
                resolve_needs_replan: Vec::new(),
                add_criterion_requirements: vec![(
                    unrelated_criterion_id.clone(),
                    vec![GoalVerificationRequirement::TaskVerified {
                        task_id: id.clone(),
                    }],
                )],
            },
            NOW,
        )
        .expect("unrelated completed Task is added");
        // Complete it through the host Verifier's own shape: a `COMPLETED` Task
        // must carry a passing host verification result, so a hand-rolled status
        // flip would build a Goal the host rejects on the next load.
        let ctx = TaskTransitionContext {
            active_worker_stopped: true,
            side_effect_reconciled: true,
        };
        goal.transition_task(&id, TaskStatus::Running, ctx, NOW)
            .expect("unrelated Task starts");
        goal.transition_task(&id, TaskStatus::Verifying, ctx, NOW)
            .expect("unrelated Task verifies");
        goal.task_record_verification_result(
            &id,
            VerificationResult::new(
                VerificationOutcome::Passed,
                vec![VerificationCheckResult::new(0, true, Some("ok".to_owned()))],
                NOW,
                NOW,
            ),
        )
        .expect("unrelated verification recorded");
        goal.transition_task(&id, TaskStatus::Completed, ctx, NOW)
            .expect("unrelated Task completes");
    }

    for index in 0..shape.goal_blockers {
        goal.add_blocker(crate::goal::GoalBlocker::new(
            format!("SYNTHETIC_{index:03}"),
            format!("synthetic durable goal blocker {index:03}"),
            false,
        ))
        .expect("goal blocker added");
    }

    // The whole point of the history sweep: replacement must not grow the active
    // graph. Recorded here so the final assertion can prove it mechanically.
    let active_before_history: Vec<TaskId> = goal
        .tasks()
        .iter()
        .filter(|(_, task)| task.is_active_plan_authority())
        .map(|(id, _)| id.clone())
        .collect();

    // --- history rounds ---------------------------------------------------
    // Each round drives the complete real recovery path: the victim runs one
    // bounded attempt that exceeds the host output limit, the host records a
    // durable failed-task replan request against it, and the host commits a
    // replacement transaction that supersedes it and adds one closure Task. The
    // *active* graph therefore stays constant while durable history grows by one
    // per round. This is the PokéCPU amplification shape, produced by the host's
    // own transaction rather than by writing durable state directly.
    for round in 0..shape.history_rounds {
        // Always the first replaceable leaf: each round supersedes one and adds
        // one, so the replaceable set is size-stable and this never runs dry.
        let victim = replaceable_readonly_leaves(&goal)
            .first()
            .cloned()
            .unwrap_or_else(|| panic!("history round {round} has no replaceable leaf"));
        let victim_scope = goal.tasks()[&victim].scope().clone();
        let victim_criteria = criteria_bound_to(&goal, &victim);
        arm_host_output_limit_attempt(&mut goal, &victim, shape.history_attempts, round);
        let request_id = format!("synthetic-history-{round:04}");
        goal.request_failed_task_replan(
            request_id.clone(),
            goal.revision(),
            goal.plan_revision(),
            victim.clone(),
            "response exceeded host output limit".to_owned(),
            FailedTaskReplanPolicy::RequireDecomposition,
            FailedTaskReplanTriggerKind::PostAttemptFailure,
            None,
            NOW,
        )
        .unwrap_or_else(|error| panic!("history round {round} replan request: {error}"));

        let join = Task::new(
            format!("repair-{round:04}"),
            format!("recover authority for entity class {round:04}"),
            true,
            WorkerKind::CodexReadonly,
            victim_scope,
            verification(&format!("repair.{round:04}.recovered")),
            2,
            goal.plan_revision() + 1,
            NOW,
        )
        .expect("replacement join is well formed");
        let join_id = join.id().clone();

        let mutation = FailedTaskReplacementMutation {
            replan_request_id: request_id,
            replaced_task_id: victim.clone(),
            replaced_plan_revision: goal.plan_revision(),
            committed_plan_revision: goal.plan_revision() + 1,
            // A record's digest must be exactly 64 hex characters, so derive it
            // deterministically instead of inventing a label-shaped string.
            canonical_proposal_digest: synthetic_digest(&format!("history-{round:04}")),
            completion_closure_task_ids: vec![join_id.clone()],
            criterion_rebindings: victim_criteria
                .into_iter()
                .map(|criterion| (criterion, vec![join_id.clone()]))
                .collect(),
            new_tasks: vec![join],
        };
        goal.apply_task_replacements(vec![mutation], NOW)
            .unwrap_or_else(|error| panic!("history round {round} replacement: {error}"));
        assert_eq!(
            goal.tasks()[&victim].status(),
            TaskStatus::Superseded,
            "history round {round} superseded its victim"
        );
    }

    // --- arm the replan trigger -------------------------------------------
    // One active read-only leaf is moved to NEEDS_REPLAN through a structured
    // size failure, exactly the way the real recovery path arms it. This is
    // what makes `replanner_request_for_goal` eligible.
    // The trigger has to be a Task the host can actually run, so prefer an
    // already-`READY` leaf and otherwise promote a dependency-free closure Task.
    // A leaf still behind an incomplete dependency is not a legal trigger, and
    // inventing one would build a Goal the host itself would never present.
    let trigger = replaceable_readonly_leaves(&goal)
        .into_iter()
        .find(|id| goal.tasks()[id].status() == TaskStatus::Ready)
        .or_else(|| {
            replaceable_readonly_leaves(&goal)
                .into_iter()
                .find(|id| goal.tasks()[id].dependencies().is_empty())
        })
        .unwrap_or_else(|| panic!("no runnable read-only leaf remains to arm"));
    arm_host_output_limit_attempt(&mut goal, &trigger, shape.history_attempts, 9_000);
    goal.request_failed_task_replan(
        "synthetic-trigger-request".to_owned(),
        goal.revision(),
        goal.plan_revision(),
        trigger.clone(),
        "response exceeded host output limit".to_owned(),
        FailedTaskReplanPolicy::RequireDecomposition,
        FailedTaskReplanTriggerKind::PostAttemptFailure,
        None,
        NOW,
    )
    .expect("replan request armed");

    let active: Vec<TaskId> = goal
        .tasks()
        .iter()
        .filter(|(_, task)| task.is_active_plan_authority())
        .map(|(id, _)| id.clone())
        .collect();
    let history: Vec<TaskId> = goal
        .tasks()
        .iter()
        .filter(|(_, task)| !task.is_active_plan_authority())
        .map(|(id, _)| id.clone())
        .collect();

    goal.validate()
        .unwrap_or_else(|error| panic!("synthetic Goal must satisfy host validation: {error}"));
    let active_after_history: BTreeSet<TaskId> = active.iter().cloned().collect();
    assert_eq!(
        active_after_history.len(),
        active_before_history.len(),
        "replacement must not change the size of the active graph"
    );
    assert_eq!(
        history.len(),
        shape.history_rounds,
        "each round contributes exactly one superseded Task"
    );

    Fixture {
        root,
        repo,
        session,
        goal,
        trigger,
        active,
        history,
        criterion_id,
        unrelated_criterion_id,
    }
}

/// Run exactly one bounded attempt on `task_id` that exceeds the host output
/// limit, then append `evidence_items` durable evidence items to it.
///
/// Exactly one attempt, because the host refuses to re-arm a `HOST_OUTPUT_LIMIT`
/// attempt: an unchanged retry is structurally unable to succeed, which is
/// precisely what routes the Task to replacement. Evidence is append-only while
/// the Task is non-terminal, so it is how an evidence-heavy historical Task is
/// expressed without inventing a second attempt the host would reject.
fn arm_host_output_limit_attempt(
    goal: &mut Goal,
    task_id: &TaskId,
    evidence_items: usize,
    label: usize,
) {
    if goal.tasks()[task_id].status() == TaskStatus::Pending {
        goal.transition_task(
            task_id,
            TaskStatus::Ready,
            TaskTransitionContext::default(),
            NOW,
        )
        .expect("task becomes runnable");
    }
    {
        goal.transition_task(
            task_id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .expect("task starts");
        goal.task_record_latest_worker_report(
            task_id,
            WorkerReport::new(
                "READONLY_BACKEND_ERROR: response exceeded host output limit",
                Vec::new(),
            ),
        )
        .expect("worker report recorded");
        goal.task_record_latest_attempt_failure_class(
            task_id,
            crate::failure_class::FailureClass::HostOutputLimit,
        )
        .expect("failure classified");
        goal.task_bind_latest_attempt_execution(
            task_id,
            None,
            None,
            None,
            Some(crate::fallback::SideEffectClass::None),
            Some(crate::fallback::SideEffectState::ConfirmedNotPerformed),
            Some(1),
            Some(0),
        )
        .expect("attempt execution bound");
        goal.transition_task(
            task_id,
            TaskStatus::Retryable,
            TaskTransitionContext::default(),
            NOW,
        )
        .expect("task is retryable");
    }
    for item in 0..evidence_items {
        goal.task_add_evidence(
            task_id,
            crate::task::TaskEvidence::StructuredObservation {
                requirement_id: format!("evidence.{label:06}.{item:03}"),
                source: format!("synthetic-evidence-{label:06}-{item:03}"),
                passed: false,
                detail: "x".repeat(256),
            },
        )
        .expect("evidence recorded");
    }
}

/// Every active read-only Task a host replacement transaction may legally
/// supersede, in durable `TaskId` order. This is exactly the status set
/// `Goal::apply_task_replacements` accepts, so the fixture can never build a
/// Goal the host itself would refuse.
fn replaceable_readonly_leaves(goal: &Goal) -> Vec<TaskId> {
    goal.tasks()
        .iter()
        .filter(|(_, task)| {
            task.is_active_plan_authority()
                && task.worker() == WorkerKind::CodexReadonly
                && matches!(
                    task.status(),
                    TaskStatus::Pending
                        | TaskStatus::Ready
                        | TaskStatus::Retryable
                        | TaskStatus::Blocked
                        | TaskStatus::NeedsReplan
                )
        })
        .map(|(id, _)| id.clone())
        .collect()
}

fn criteria_bound_to(goal: &Goal, task: &TaskId) -> Vec<CompletionCriterionId> {
    goal.final_verification_spec()
        .map(|spec| {
            spec.criterion_bindings()
                .iter()
                .filter(|binding| {
                    binding
                        .requirements()
                        .iter()
                        .any(|requirement| requirement.task_id() == task)
                })
                .map(|binding| binding.criterion_id().clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Build the request and record its serialized size. This is the measurement
/// the whole branch is about: bytes, not Task counts.
fn request_for(fixture: &Fixture) -> ReplannerRequest {
    replanner_request_for_goal(&fixture.goal, &fixture.session).expect("request builds")
}

fn request_bytes(fixture: &Fixture) -> usize {
    serde_json::to_vec(&request_for(fixture))
        .expect("request serializes")
        .len()
}

// ===========================================================================
// Baseline measurement (pre-change reference, kept as a live regression)
// ===========================================================================

#[test]
fn measurement_baseline_shapes_report_serialized_request_bytes() {
    // These are the PokéCPU-shaped and extreme shapes. The assertions are
    // deliberately loose here: the *hard* bounds are asserted in the ceiling and
    // non-amplification groups. This test exists so the numbers are recorded on
    // every run and a silent order-of-magnitude regression is visible in review.
    struct Case {
        label: &'static str,
        shape: Shape,
    }
    let cases = [
        Case {
            label: "small",
            shape: Shape::default(),
        },
        Case {
            label: "pokecpu-36",
            shape: Shape {
                leaf_tasks: 36,
                ..Shape::default()
            },
        },
        Case {
            label: "pokecpu-36-with-history",
            shape: Shape {
                leaf_tasks: 36,
                history_rounds: 24,
                ..Shape::default()
            },
        },
        Case {
            label: "history-heavy",
            shape: Shape {
                leaf_tasks: 6,
                history_rounds: 64,
                history_attempts: 3,
                ..Shape::default()
            },
        },
        Case {
            label: "deep-chain",
            shape: Shape {
                leaf_tasks: 24,
                chain_depth: 24,
                ..Shape::default()
            },
        },
        Case {
            label: "wide-fan-out",
            shape: Shape {
                leaf_tasks: 24,
                fan_out: 12,
                ..Shape::default()
            },
        },
        Case {
            label: "many-unrelated-completed",
            shape: Shape {
                leaf_tasks: 6,
                completed_leaves: 20,
                unrelated_criterion_tasks: 20,
                ..Shape::default()
            },
        },
        Case {
            label: "blocker-and-criterion-weight",
            shape: Shape {
                leaf_tasks: 6,
                goal_blockers: 40,
                criterion_chars: 4_000,
                ..Shape::default()
            },
        },
    ];

    for case in cases {
        let fixture = build(case.label, case.shape);
        let bytes = request_bytes(&fixture);
        let durable = serde_json::to_vec(&fixture.goal)
            .expect("goal serializes")
            .len();
        eprintln!(
            "MEASURE label={} active={} history={} request_bytes={} durable_goal_bytes={} ratio={:.3}",
            case.label,
            fixture.active.len(),
            fixture.history.len(),
            bytes,
            durable,
            bytes as f64 / durable as f64,
        );
        assert!(bytes > 0, "{} produced an empty request", case.label);
    }
}
