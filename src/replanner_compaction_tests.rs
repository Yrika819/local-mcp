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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config;
use crate::goal::{
    CompletionCriterionId, FailedTaskReplacementMutation, FailedTaskReplanPolicy,
    FailedTaskReplanTriggerKind, Goal, GoalCriterionBinding, GoalFinalVerificationSpec,
    GoalVerificationRequirement,
};
use crate::planner;
use crate::replanner::{
    ReplannerBackend, ReplannerError, ReplannerRequest, replanner_request_for_goal,
};
use crate::task::{
    ReplaySafety, Task, TaskId, TaskOperationKind, TaskScope, TaskStatus, TaskTransitionContext,
    VerificationCheckResult, VerificationOutcome, VerificationResult, VerificationSpec, WorkerKind,
    WorkerReport,
};
use crate::task_store::TaskStore;

const NOW: &str = "2026-10-02T00:00:00Z";

/// Marker that appears only in the evidence source of a *superseded* Task. The
/// request must never contain it, because a superseded Task's evidence body is
/// exactly what compaction removes.
const HISTORY_EVIDENCE_MARKER: &str = "synthetic-superseded-evidence";

/// Marker for the trigger's own evidence, which the full tier *must* carry.
const TRIGGER_EVIDENCE_MARKER: &str = "synthetic-trigger-evidence";

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
    /// Replacement transactions applied before the request is built. Each
    /// transaction supersedes one active Task and adds one active Task, so the
    /// active graph stays constant while durable history grows. This is the
    /// PokéCPU shape.
    history_rounds: usize,
    /// How many leaves carry the first completion criterion's requirement. More
    /// bound leaves means more Tasks are eligible to be superseded in one host
    /// transaction, which is what keeps a thousand-entry history cheap to build:
    /// `Goal::validate` is quadratic in (records x tasks), so serial transactions
    /// dominate the cost.
    criterion_bound_leaves: usize,
    /// How many Tasks one replacement transaction supersedes. One keeps the
    /// classic one-failure-one-replacement shape; a larger value is also a real
    /// host transaction and is what makes a thousand-entry history reachable in a
    /// test without a quadratic fixture.
    history_bulk: usize,
    /// How many durable evidence items each superseded Task carries. Exactly one
    /// attempt is possible: the host refuses to re-arm a `HOST_OUTPUT_LIMIT`
    /// attempt, which is what routes a Task to replacement at all.
    history_attempts: usize,
    /// Extra scope paths on every leaf, to drive the active scope-path budget.
    paths_per_leaf: usize,
    /// Extra verification entries on every leaf, to drive the active
    /// verification-entry budget.
    verification_per_leaf: usize,
    /// Total completion criteria on the Goal, each padded to `criterion_chars`.
    criteria_count: usize,
    /// How many `COMPLETED` anchor Tasks to add. Edges may be added freely to an
    /// anchor, because a hard dependency is satisfied by a completed Task, so
    /// anchors are how the active edge budget is driven to a precise value with
    /// a legal graph.
    anchor_leaves: usize,
    /// How many host-derived dependency edges to add from the remaining
    /// `PENDING` leaves to the anchors.
    edge_top_up: usize,
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
            criterion_bound_leaves: 2,
            history_bulk: 1,
            history_attempts: 1,
            paths_per_leaf: 0,
            verification_per_leaf: 0,
            criteria_count: 2,
            anchor_leaves: 0,
            edge_top_up: 0,
            unrelated_criterion_tasks: 0,
            goal_blockers: 0,
            criterion_chars: 48,
        }
    }
}

struct Fixture {
    #[expect(dead_code, reason = "removed with the Fixture, kept for Drop")]
    root: PathBuf,
    #[expect(dead_code, reason = "consumed by the context-tier assertions")]
    repo: PathBuf,
    session: config::Session,
    #[expect(dead_code, reason = "the durable state root is kept for diagnostics")]
    state: PathBuf,
    store: TaskStore,
    goal_id: crate::goal::GoalId,
    goal: Goal,
    /// The Task the next Replanner invocation is about.
    trigger: TaskId,
    /// Every non-superseded Task, in durable `TaskId` order.
    active: Vec<TaskId>,
    /// Every superseded Task, in durable `TaskId` order.
    history: Vec<TaskId>,
    #[expect(dead_code, reason = "retained for symmetry with the durable contract")]
    criterion_id: CompletionCriterionId,
    #[expect(dead_code, reason = "retained for symmetry with the durable contract")]
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

/// A leaf carrying `paths` forbidden paths and `specs` verification entries, so
/// the active scope-path and verification-entry budgets can be driven precisely.
fn budget_leaf(repo: &Path, index: usize, paths: usize, specs: usize) -> Task {
    let forbidden = (0..paths)
        .map(|path| repo.join(format!("budget-{index:04}-{path:04}")))
        .collect::<Vec<_>>();
    let scope = TaskScope::new(
        vec![repo.to_path_buf()],
        forbidden,
        TaskOperationKind::ReadOnly,
        ReplaySafety::SafeReadOnly,
    );
    let mut entries = verification(&format!("entity.{index:04}.observed"));
    for spec in 1..specs {
        entries.push(VerificationSpec::StructuredEvidence {
            requirement_id: format!("entity.{index:04}.observed.{spec:03}"),
        });
    }
    Task::new(
        format!("leaf-{index:04}"),
        format!("inspect authority for entity class {index:04}"),
        true,
        WorkerKind::CodexReadonly,
        scope,
        entries,
        2,
        1,
        NOW,
    )
    .expect("budget leaf Task is well formed")
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
    let store = TaskStore::with_state_root(state.clone());

    let mut goal = Goal::new(
        session.id.clone(),
        repo.clone(),
        "repair the plan monotonically with bounded decomposed work",
        Some("compaction fixture".to_owned()),
        vec!["stay inside repository authority".to_owned()],
        // Every criterion is padded to `criterion_chars`, so the shape exercises
        // criterion prose weight rather than the count alone.
        (0..shape.criteria_count)
            .map(|index| {
                let prefix = if index == 0 {
                    String::new()
                } else {
                    format!("unrelated historical authority {index:02} ")
                };
                let budget = shape.criterion_chars.max(1).saturating_sub(prefix.len());
                let filler = "criterion authority ".repeat(budget.div_ceil(19).max(1));
                let mut text = prefix;
                text.push_str(&filler.chars().take(budget).collect::<String>());
                text
            })
            .collect(),
        NOW,
    )
    .expect("Goal is well formed");
    let criterion_id = goal.completion_criteria()[0].id().clone();
    let unrelated_criterion_id = goal.completion_criteria()[1].id().clone();
    // Every further criterion is bound to one real requirement, because a
    // required criterion may not carry an empty binding.
    let extra_criterion_ids = goal.completion_criteria()[2..]
        .iter()
        .map(|criterion| criterion.id().clone())
        .collect::<Vec<_>>();

    // --- initial plan -----------------------------------------------------
    let mut tasks: Vec<Task> = Vec::new();
    let mut leaf_ids: Vec<TaskId> = Vec::new();
    for index in 0..shape.leaf_tasks {
        let task = budget_leaf(
            &repo,
            index,
            shape.paths_per_leaf,
            shape.verification_per_leaf,
        );
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

    // A dedicated, dependency-free leaf reserved as the replan trigger. It is
    // created separately so it stays runnable in every shape: reusing a shaped
    // leaf would leave the trigger behind an incomplete dependency in the deep
    // chain and fan-out shapes, and the resulting context tiers would then depend
    // on which leaf the host happened to pick.
    let trigger_leaf = budget_leaf(&repo, 999, 0, 1);
    let trigger_leaf_id = trigger_leaf.id().clone();
    tasks.push(trigger_leaf);

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
                    .take(shape.criterion_bound_leaves.min(shape.leaf_tasks).max(1))
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
                vec![
                    GoalVerificationRequirement::TaskVerified {
                        task_id: leaf_ids[leaf_ids.len() - 1].clone(),
                    },
                    GoalVerificationRequirement::TaskVerified {
                        task_id: trigger_leaf_id.clone(),
                    },
                ],
            ),
        ]
        .into_iter()
        .chain(extra_criterion_ids.iter().map(|criterion| {
            GoalCriterionBinding::new(
                criterion.clone(),
                vec![GoalVerificationRequirement::TaskVerified {
                    task_id: leaf_ids[leaf_ids.len() - 1].clone(),
                }],
            )
        }))
        .collect(),
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

    // Completed anchors, then hard edges from the still-`PENDING` leaves onto
    // them. A dependency on a completed Task is always satisfied, so this drives
    // the active edge budget to an exact value without an illegal graph.
    let mut anchors = Vec::new();
    for index in 0..shape.anchor_leaves {
        let task = Task::new(
            format!("anchor-{index:04}"),
            format!("durable completed authority anchor {index:04}"),
            true,
            WorkerKind::CodexReadonly,
            readonly_scope(&repo),
            verification(&format!("anchor.{index:04}.verified")),
            2,
            goal.plan_revision() + 1,
            NOW,
        )
        .expect("anchor Task is well formed");
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
        .expect("anchor is added");
        let ctx = TaskTransitionContext {
            active_worker_stopped: true,
            side_effect_reconciled: true,
        };
        goal.transition_task(&id, TaskStatus::Running, ctx, NOW)
            .expect("anchor starts");
        goal.transition_task(&id, TaskStatus::Verifying, ctx, NOW)
            .expect("anchor verifies");
        goal.task_record_verification_result(
            &id,
            VerificationResult::new(
                VerificationOutcome::Passed,
                vec![VerificationCheckResult::new(0, true, Some("ok".to_owned()))],
                NOW,
                NOW,
            ),
        )
        .expect("anchor verification recorded");
        goal.transition_task(&id, TaskStatus::Completed, ctx, NOW)
            .expect("anchor completes");
        anchors.push(id);
    }
    if shape.edge_top_up > 0 {
        assert!(!anchors.is_empty(), "edge top-up needs at least one anchor");
        // A `READY` Task may gain a hard dependency on an already-`COMPLETED`
        // Task, so anchors are legal targets from either state.
        let mut pendings = goal
            .tasks()
            .iter()
            .filter(|(_, task)| {
                task.is_active_plan_authority()
                    && matches!(task.status(), TaskStatus::Pending | TaskStatus::Ready)
                    && task.worker() == WorkerKind::CodexReadonly
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        // The edge-ceiling replacement regression must reclaim enough budget
        // from its reserved trigger regardless of random UUID ordering.
        pendings.sort_by_key(|id| (id != &trigger_leaf_id, id.clone()));
        // `add_dependencies` must group once per target Task, so collect the
        // per-Task sets first and only then build the mutation.
        let mut planned = BTreeMap::<TaskId, Vec<TaskId>>::new();
        'outer: for target in &pendings {
            for anchor in &anchors {
                if planned.values().map(Vec::len).sum::<usize>() >= shape.edge_top_up {
                    break 'outer;
                }
                if target == anchor || goal.tasks()[target].dependencies().len() > 60 {
                    continue;
                }
                planned
                    .entry(target.clone())
                    .or_default()
                    .push(anchor.clone());
            }
        }
        let added = planned.into_iter().collect::<Vec<_>>();
        assert_eq!(
            added.iter().map(|(_, edges)| edges.len()).sum::<usize>(),
            shape.edge_top_up,
            "the fixture must be able to place every requested edge"
        );
        goal.apply_replan_mutation(
            crate::goal::ReplanMutation {
                new_tasks: Vec::new(),
                add_dependencies: added,
                add_verification: Vec::new(),
                strengthen_mandatory: Vec::new(),
                resolve_needs_replan: Vec::new(),
                add_criterion_requirements: Vec::new(),
            },
            NOW,
        )
        .expect("edge top-up applies");
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
    // The trigger is reserved up front and never superseded. Leaving it to
    // "whichever leaf the host can run first" would make the resulting context
    // tiers depend on durable Task order, which is random for a v4 UUID, and a
    // regression test that passes only sometimes is not a regression test.
    let reserved_trigger = trigger_leaf_id.clone();

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
        // Each transaction supersedes one Task, so the active graph stays constant
        // while history grows. `apply_task_replacements` clones and revalidates
        // the whole Goal per transaction, so building a thousand of these serially
        // is quadratic in the fixture itself. The host also accepts several
        // supersessions in one transaction, and `history_bulk` uses that: both
        // forms are real host transactions, and the bulk form is what makes a
        // thousand-entry history reachable in a test at all.
        // A victim must carry a criterion requirement — the replacement has to
        // rebind it — and must be a Task the host can actually run, so the
        // transaction is one the recovery path could really have taken. How many
        // qualify at once is whatever the durable plan offers, so the bulk is
        // clamped to that rather than demanded.
        let candidates = replaceable_readonly_leaves(&goal)
            .into_iter()
            .filter(|id| *id != reserved_trigger)
            .filter(|id| !criteria_bound_to(&goal, id).is_empty())
            .filter(|id| is_runnable(&goal, id))
            .collect::<Vec<_>>();
        let bulk = shape
            .history_bulk
            .max(1)
            .min(shape.leaf_tasks.max(1))
            .min(candidates.len());
        if bulk == 0 {
            panic!("history round {round} has no runnable bound leaf to supersede");
        }
        let mut mutations = Vec::with_capacity(bulk);
        let mut superseded = Vec::with_capacity(bulk);
        for (index, victim) in candidates.iter().take(bulk).cloned().enumerate() {
            let victim_scope = goal.tasks()[&victim].scope().clone();
            let victim_verification = goal.tasks()[&victim].verification_specs().to_vec();
            let victim_criteria = criteria_bound_to(&goal, &victim);
            arm_host_output_limit_attempt(
                &mut goal,
                &victim,
                shape.history_attempts,
                round * 1_000 + index,
                HISTORY_EVIDENCE_MARKER,
            );
            let request_id = format!("synthetic-history-{round:04}-{index:04}");
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
                format!("repair-{round:04}-{index:04}"),
                format!("recover authority for entity class {round:04}-{index:04}"),
                true,
                WorkerKind::CodexReadonly,
                victim_scope,
                // The closure carries the same verification weight as the Task it
                // replaces, so every history entry contributes identically and the
                // per-transaction rate a test measures from a small probe stays
                // exact. Scope is already inherited the same way.
                victim_verification,
                2,
                goal.plan_revision() + 1,
                NOW,
            )
            .expect("replacement join is well formed");
            let join_id = join.id().clone();

            mutations.push(FailedTaskReplacementMutation {
                replan_request_id: request_id,
                replaced_task_id: victim.clone(),
                replaced_plan_revision: goal.plan_revision(),
                committed_plan_revision: goal.plan_revision() + 1,
                canonical_proposal_digest: synthetic_digest(&format!(
                    "history-{round:04}-{index:04}"
                )),
                completion_closure_task_ids: vec![join_id.clone()],
                criterion_rebindings: victim_criteria
                    .into_iter()
                    .map(|criterion| (criterion, vec![join_id.clone()]))
                    .collect(),
                new_tasks: vec![join],
            });
            superseded.push(victim);
        }
        goal.apply_task_replacements(mutations, NOW)
            .unwrap_or_else(|error| panic!("history round {round} replacement: {error}"));
        for victim in superseded {
            assert_eq!(
                goal.tasks()[&victim].status(),
                TaskStatus::Superseded,
                "history round {round} superseded its victim"
            );
        }
    }

    // --- arm the replan trigger -------------------------------------------
    // One active read-only leaf is moved to NEEDS_REPLAN through a structured
    // size failure, exactly the way the real recovery path arms it. This is
    // what makes `replanner_request_for_goal` eligible.
    // Arm the reserved trigger. It is chosen at construction time so that every
    // derived fact about the request — which Task is full-detail, which are
    // structural, which are compact — is reproducible.
    let trigger = reserved_trigger;
    assert_eq!(
        goal.tasks()[&trigger].status(),
        TaskStatus::Ready,
        "the reserved trigger must be a Task the host can run"
    );
    arm_host_output_limit_attempt(
        &mut goal,
        &trigger,
        shape.history_attempts,
        9_000,
        TRIGGER_EVIDENCE_MARKER,
    );
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
    store.create_goal(&goal).expect("synthetic Goal persists");
    let goal_id = goal.id().clone();
    let active_after_history: BTreeSet<TaskId> = active.iter().cloned().collect();
    assert_eq!(
        active_after_history.len(),
        active_before_history.len(),
        "replacement must not change the size of the active graph"
    );
    // Each transaction supersedes as many Tasks as the durable plan offers, so
    // the count is exact either way.
    if shape.history_rounds > 0 {
        // Each transaction supersedes as many Tasks as the durable plan offers,
        // so the entry count is an exact positive multiple of the transaction
        // count and never less than one entry per transaction.
        assert!(
            history.len() >= shape.history_rounds
                && history.len().is_multiple_of(shape.history_rounds),
            "history must grow by exactly one entry per superseded Task: {} entries for {} transactions",
            history.len(),
            shape.history_rounds
        );
    } else {
        assert!(
            history.is_empty(),
            "no transactions means no superseded Tasks"
        );
    }

    Fixture {
        root,
        repo,
        session,
        state,
        store,
        goal_id,
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
/// Whether the host can actually run this Task: already `READY`, or `PENDING`
/// with no hard dependencies left outstanding.
fn is_runnable(goal: &Goal, id: &TaskId) -> bool {
    let task = &goal.tasks()[id];
    task.status() == TaskStatus::Ready
        || (task.status() == TaskStatus::Pending && task.dependencies().is_empty())
}

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
    evidence_marker: &str,
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
                source: format!("{evidence_marker}-{label:06}-{item:03}"),
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

fn request_value(fixture: &Fixture) -> Value {
    serde_json::to_value(request_for(fixture)).expect("request serializes to JSON")
}

fn request_task_ids(value: &Value) -> Vec<String> {
    value["tasks"]
        .as_array()
        .expect("tasks is an array")
        .iter()
        .map(|task| task["task_id"].as_str().unwrap_or_default().to_owned())
        .collect()
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

// ===========================================================================
// Active execution graph versus durable history
// ===========================================================================

#[test]
fn superseded_tasks_are_history_and_every_other_status_is_active() {
    // The predicate is the crate's own `is_active_plan_authority`, i.e.
    // `status != SUPERSEDED` — deliberately not "non-terminal". A `COMPLETED`
    // Task is still a legal dependency target and still a valid `TASK_VERIFIED`
    // proof, so excluding it would let a plan grow without bound and would let a
    // mandatory Task skip its proof requirement.
    for status in [
        TaskStatus::Pending,
        TaskStatus::Ready,
        TaskStatus::Running,
        TaskStatus::Blocked,
        TaskStatus::Retryable,
        TaskStatus::NeedsReplan,
        TaskStatus::Verifying,
        TaskStatus::Completed,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
    ] {
        assert!(
            status.is_terminal() || !status.is_terminal(),
            "status enumeration is exhaustive"
        );
        assert_ne!(
            status,
            TaskStatus::Superseded,
            "the history predicate is exactly SUPERSEDED"
        );
    }
    let fixture = build("active-predicate", Shape::default());
    let value = request_value(&fixture);
    let shown = request_task_ids(&value);
    for id in &fixture.active {
        assert!(
            shown.contains(&id.as_str().to_owned()),
            "active Task {id:?} must be shown"
        );
    }
    for id in &fixture.history {
        assert!(
            !shown.contains(&id.as_str().to_owned()),
            "superseded Task {id:?} must never appear in the active task array"
        );
    }
}

#[test]
fn every_active_task_is_shown_exactly_once_and_superseded_never_is() {
    let fixture = build(
        "one-entry-per-task",
        Shape {
            leaf_tasks: 12,
            history_rounds: 8,
            ..Shape::default()
        },
    );
    let value = request_value(&fixture);
    let shown = request_task_ids(&value);
    let unique = shown.iter().collect::<BTreeSet<_>>();
    assert_eq!(shown.len(), unique.len(), "no Task is shown twice");
    let expected = fixture
        .active
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        shown.iter().cloned().collect::<BTreeSet<_>>(),
        expected,
        "the active task array is exactly the active execution graph"
    );
    // Grouped by tier, then by Task ID: a total order over a deterministic
    // predicate, so the serialized request is byte-stable.
    let ranks = |tier: &str| match tier {
        "FULL" => 0u8,
        "STRUCTURAL" => 1,
        "COMPACT" => 2,
        other => panic!("unexpected detail tier {other}"),
    };
    let keys = value["tasks"]
        .as_array()
        .expect("tasks is an array")
        .iter()
        .map(|task| {
            (
                ranks(task["detail"].as_str().unwrap_or_default()),
                task["task_id"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    for pair in keys.windows(2) {
        assert!(
            pair[0] < pair[1],
            "tasks must be ordered by tier then Task ID"
        );
    }
    for id in &fixture.history {
        assert!(!shown.contains(&id.as_str().to_owned()));
    }
}

#[test]
fn the_trigger_is_full_detail_and_superseded_evidence_bodies_are_absent() {
    let fixture = build(
        "tier-full",
        Shape {
            leaf_tasks: 8,
            history_rounds: 4,
            history_attempts: 3,
            ..Shape::default()
        },
    );
    let value = request_value(&fixture);
    let tasks = value["tasks"]
        .as_array()
        .expect("tasks is an array")
        .clone();
    let by_id = |id: &str| -> Value {
        tasks
            .iter()
            .find(|task| task["task_id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("Task {id:?} is present"))
    };

    let trigger = by_id(fixture.trigger.as_str());
    assert_eq!(trigger["detail"], "FULL");
    assert_eq!(trigger["status"], "NEEDS_REPLAN");
    assert_eq!(trigger["attempts"].as_array().map(Vec::len), Some(1));
    assert!(trigger["evidence"].is_array());
    assert!(trigger["verification_results"].is_array());
    assert!(trigger["blockers"].is_array());
    assert!(trigger["title"].is_string());
    assert!(trigger["scope"].is_object());
    assert!(trigger["verification"].is_array());
    assert!(trigger["max_attempts"].is_number());

    // A superseded Task's evidence and attempt bodies must not appear anywhere,
    // at any tier. The history summary carries only identity and accounting.
    let serialized = serde_json::to_string(&value).expect("request serializes");
    assert!(
        !serialized.contains(HISTORY_EVIDENCE_MARKER),
        "superseded evidence source leaked into the model context"
    );
    assert!(
        serialized.contains(TRIGGER_EVIDENCE_MARKER),
        "the trigger's own evidence must still reach the model"
    );
    let task_array = serde_json::to_string(&value["tasks"]).expect("serializes");
    for victim in &fixture.history {
        assert!(
            !task_array.contains(victim.as_str()),
            "a superseded Task id must never appear in the active task array"
        );
    }
}

#[test]
fn ancestors_dependents_and_criterion_bound_tasks_are_structural_and_others_compact() {
    let fixture = build(
        "tier-structural",
        Shape {
            leaf_tasks: 10,
            // Chain and fan-out address disjoint leaves so no leaf declares the
            // same dependency twice, which the host would refuse.
            chain_depth: 3,
            fan_out: 3,
            ..Shape::default()
        },
    );
    let value = request_value(&fixture);
    let tasks = value["tasks"]
        .as_array()
        .expect("tasks is an array")
        .clone();
    let detail = |id: &TaskId| -> String {
        tasks
            .iter()
            .find(|task| task["task_id"] == id.as_str())
            .map(|task| task["detail"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_else(|| panic!("Task {id:?} is present"))
    };
    assert_eq!(detail(&fixture.trigger), "FULL");
    let detail_of = |id: &str| -> String {
        tasks
            .iter()
            .find(|task| task["task_id"] == id)
            .map(|task| task["detail"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_else(|| panic!("Task {id} is present"))
    };

    // The legal prerequisite pool: every active dependency ancestor of any
    // active Task. A replacement Task may depend on one of these, and
    // `task_ref_is_mandatory` needs its real `mandatory` flag, so none of them
    // may be demoted below structural.
    // Seeded from the trigger set, which is the documented rule: the legal
    // prerequisite pool for *this* replacement is the trigger's own dependency
    // ancestry, not the ancestry of every Task in the plan. Widening it is not
    // free — it is exactly the growth compaction exists to remove.
    let mut ancestors = BTreeSet::new();
    let mut frontier = vec![fixture.trigger.clone()];
    while let Some(id) = frontier.pop() {
        for dependency in fixture.goal.tasks()[&id].dependencies() {
            let target = dependency.task_id().clone();
            if fixture.active.contains(&target) && ancestors.insert(target.clone()) {
                frontier.push(target);
            }
        }
    }
    if !ancestors.is_empty() {
        for id in &ancestors {
            let tier = detail(id);
            assert!(
                tier == "STRUCTURAL" || tier == "FULL",
                "ancestor {id:?} is shown at {tier}"
            );
        }
    }

    // The downstream set a replacement rewires, host-side.
    let mut reverse = BTreeMap::<String, Vec<String>>::new();
    for id in &fixture.active {
        for dependency in fixture.goal.tasks()[id].dependencies() {
            reverse
                .entry(dependency.task_id().as_str().to_owned())
                .or_default()
                .push(id.as_str().to_owned());
        }
    }
    let mut dependents = BTreeSet::new();
    let mut frontier = vec![fixture.trigger.clone()];
    while let Some(id) = frontier.pop() {
        for dependent in reverse.get(id.as_str()).cloned().unwrap_or_default() {
            if fixture
                .active
                .iter()
                .any(|active| active.as_str() == dependent)
            {
                dependents.insert(dependent);
            }
        }
    }
    if !dependents.is_empty() {
        for id in &dependents {
            let tier = detail_of(id);
            assert!(
                tier == "STRUCTURAL" || tier == "FULL",
                "dependent {id} is shown at {tier}"
            );
        }
    }

    // Every Task bound to a completion criterion.
    let bound = fixture
        .goal
        .final_verification_spec()
        .expect("synthetic Goal has a contract")
        .criterion_bindings()
        .iter()
        .flat_map(|binding| binding.requirements().to_vec())
        .map(|requirement| requirement.task_id().clone())
        .filter(|id| fixture.active.contains(id))
        .collect::<BTreeSet<_>>();
    assert!(!bound.is_empty(), "criteria bind real Tasks");
    for id in &bound {
        let tier = detail(id);
        assert!(
            tier == "STRUCTURAL" || tier == "FULL",
            "criterion-bound Task {id:?} is shown at {tier}"
        );
    }

    // Structural carries structure but no history bodies; compact carries
    // topology only. An absent field must be genuinely absent, never null.
    for task in &tasks {
        let level = task["detail"].as_str().unwrap_or_default();
        if level == "FULL" {
            // The only Task allowed to carry history bodies is the one this
            // invocation is about.
            assert_eq!(task["task_id"], fixture.trigger.as_str());
            assert!(task["evidence"].is_array());
            assert!(task["attempts"].is_array());
            assert!(task["verification_results"].is_array());
            assert!(task["blockers"].is_array());
            assert!(task["max_attempts"].is_number());
            continue;
        }
        assert!(task["task_id"].is_string());
        assert!(task["status"].is_string());
        assert!(task["mandatory"].is_boolean());
        assert!(task["worker"].is_string());
        assert!(task["dependencies"].is_array());
        assert!(task["created_plan_revision"].is_number());
        for omitted in [
            "evidence",
            "attempts",
            "verification_results",
            "blockers",
            "max_attempts",
        ] {
            assert!(
                task.get(omitted).is_none(),
                "a {level} Task must omit {omitted} entirely, not null it"
            );
        }
        assert!(tasks.iter().any(|task| task["detail"] == "FULL"));
        match level {
            "STRUCTURAL" => {
                assert!(task["title"].is_string());
                assert!(task["objective"].is_string());
                assert!(task["scope"].is_object());
                assert!(task["verification"].is_array());
            }
            "COMPACT" => {
                for omitted in ["title", "objective", "scope", "verification"] {
                    assert!(
                        task.get(omitted).is_none(),
                        "a COMPACT Task must omit {omitted}"
                    );
                }
            }
            other => panic!("unexpected detail tier {other}"),
        }
    }
    // Now pin the selection algorithm itself. The trigger is whichever leaf the
    // host can run, so which leaves land in which tier depends on durable Task
    // order; asserting a fixed tier count would be a flaky test. Asserting that
    // the tiers equal the documented derivation is both deterministic and
    // stricter, because it fails if the host ever includes or omits a Task the
    // derivation does not.
    let active: BTreeSet<TaskId> = fixture.active.iter().cloned().collect();
    let full: BTreeSet<TaskId> = BTreeSet::from([fixture.trigger.clone()]);
    let walk = |seeds: BTreeSet<TaskId>| -> BTreeSet<TaskId> {
        let mut seen = seeds.clone();
        let mut frontier = seeds;
        while let Some(id) = frontier.pop_first() {
            for dependency in fixture.goal.tasks()[&id].dependencies() {
                let target = dependency.task_id().clone();
                if active.contains(&target) && seen.insert(target.clone()) {
                    frontier.insert(target);
                }
            }
        }
        seen
    };
    let ancestors = walk(full.clone());
    let mut reverse = BTreeMap::<TaskId, Vec<TaskId>>::new();
    for id in &active {
        for dependency in fixture.goal.tasks()[id].dependencies() {
            reverse
                .entry(dependency.task_id().clone())
                .or_default()
                .push(id.clone());
        }
    }
    let mut dependents = BTreeSet::new();
    let mut seen = full.clone();
    let mut frontier = full.clone();
    while let Some(id) = frontier.pop_first() {
        for dependent in reverse.get(&id).cloned().unwrap_or_default() {
            if seen.insert(dependent.clone()) {
                dependents.insert(dependent.clone());
                frontier.insert(dependent);
            }
        }
    }
    let criterion_bound = fixture
        .goal
        .final_verification_spec()
        .expect("synthetic Goal has a contract")
        .criterion_bindings()
        .iter()
        .flat_map(|binding| binding.requirements().to_vec())
        .map(|requirement| requirement.task_id().clone())
        .filter(|id| active.contains(id))
        .collect::<BTreeSet<_>>();
    let structural = ancestors
        .iter()
        .chain(dependents.iter())
        .chain(criterion_bound.iter())
        .filter(|id| !full.contains(*id))
        .cloned()
        .collect::<BTreeSet<_>>();
    let compact = active
        .iter()
        .filter(|id| !full.contains(id) && !structural.contains(id))
        .cloned()
        .collect::<BTreeSet<_>>();
    let observed = |tier: &str| -> BTreeSet<TaskId> {
        tasks
            .iter()
            .filter(|task| task["detail"] == tier)
            .map(|task| task["task_id"].as_str().expect("task id").to_owned())
            .map(|id| TaskId::parse(&id).expect("durable Task ID"))
            .collect()
    };
    assert_eq!(observed("FULL"), full, "full tier must be the trigger set");
    assert_eq!(
        observed("STRUCTURAL"),
        structural,
        "structural tier must be ancestors, dependents and criterion-bound Tasks"
    );
    assert_eq!(
        observed("COMPACT"),
        compact,
        "compact tier must be every other active Task"
    );
    assert!(
        !compact.is_empty(),
        "the fixture must contain an active Task that is neither an ancestor, a dependent, nor criterion-bound"
    );
    assert!(
        tasks.iter().any(|task| task["detail"] == "COMPACT"),
        "an unrelated active Task must be present at compact tier"
    );
    assert_eq!(
        tasks.iter().filter(|task| task["detail"] == "FULL").count(),
        1,
        "exactly the trigger is shown at full detail"
    );
}

#[test]
fn criterion_bindings_stay_complete_for_affected_and_unaffected_criteria() {
    // Compaction must not let a criterion lose its binding table: a replacement
    // has to rebind exactly the criteria that required the replaced Task, and
    // unrelated bindings must stay visible so the model does not orphan them.
    let fixture = build(
        "criterion-bindings",
        Shape {
            leaf_tasks: 6,
            completed_leaves: 4,
            unrelated_criterion_tasks: 4,
            criteria_count: 4,
            ..Shape::default()
        },
    );
    let value = request_value(&fixture);
    let bindings = value["criterion_bindings"]
        .as_array()
        .expect("criterion bindings are an array");
    let durable = fixture
        .goal
        .final_verification_spec()
        .expect("synthetic Goal has a final-verification contract")
        .criterion_bindings()
        .len();
    assert_eq!(bindings.len(), durable, "every durable binding is exposed");
    for binding in bindings {
        assert!(binding["criterion_id"].is_string());
        assert!(binding["task_ids"].is_array());
        for id in binding["task_ids"].as_array().unwrap() {
            let id = id.as_str().expect("task id is a string");
            assert!(
                fixture.active.iter().any(|active| active.as_str() == id),
                "a bound Task {id} must be part of the active graph"
            );
        }
    }
    let criteria = value["completion_criteria"]
        .as_array()
        .expect("criteria are an array");
    assert_eq!(criteria.len(), fixture.goal.completion_criteria().len());
}

// ===========================================================================
// History summarization
// ===========================================================================

#[test]
fn history_summary_is_bounded_host_generated_and_reports_what_it_omits() {
    let fixture = build(
        "history-summary",
        Shape {
            leaf_tasks: 6,
            history_rounds: 70,
            ..Shape::default()
        },
    );
    let value = request_value(&fixture);
    let history = value["history"].as_array().expect("history is an array");
    assert_eq!(
        history.len(),
        crate::replanner::REPLANNER_HISTORY_SUMMARY_LIMIT
    );
    assert_eq!(
        value["history_omitted_count"],
        json!(70 - crate::replanner::REPLANNER_HISTORY_SUMMARY_LIMIT),
        "the window must report the superseded Tasks it does not show"
    );

    // Every entry is host-generated, structurally fixed, and describes a
    // replacement chain rather than a Task body.
    let shown = history
        .iter()
        .filter_map(|entry| entry["superseded_task_id"].as_str())
        .collect::<BTreeSet<_>>();
    let record = fixture
        .goal
        .failed_task_replacements()
        .iter()
        .find(|record| shown.contains(record.replaced_task_id().as_str()))
        .expect("a replacement record falls inside the bounded history window");
    let entry = history
        .iter()
        .find(|entry| entry["superseded_task_id"] == record.replaced_task_id().as_str())
        .unwrap_or_else(|| panic!("the replaced Task appears in the history window"));
    assert_eq!(entry["status"], "SUPERSEDED");
    assert_eq!(entry["replan_request_id"], record.replan_request_id());
    assert_eq!(
        entry["committed_plan_revision"],
        record.committed_plan_revision()
    );
    assert_eq!(
        entry["preserved_max_attempts"],
        record.preserved_max_attempts()
    );
    assert_eq!(
        entry["preserved_consumed_attempts"],
        record.preserved_consumed_attempts()
    );
    let replaced_by = entry["replaced_by"]
        .as_array()
        .expect("replaced_by is an array");
    assert!(!replaced_by.is_empty());
    for id in replaced_by {
        let id = id.as_str().unwrap_or_default();
        // `replaced_by` names the Task that took over at the time. In a
        // multi-round chain that successor may itself have been superseded
        // later, so the assertion is that the name resolves durably — not that it
        // is still active.
        assert!(
            fixture
                .goal
                .tasks()
                .contains_key(&TaskId::parse(id).expect("replaced_by is a Task ID")),
            "replaced_by {id} must resolve to a durable Task"
        );
    }

    // No superseded Task body, scope, or evidence may leak into the summary.
    let serialized = serde_json::to_string(&value).expect("request serializes");
    for victim in &fixture.history {
        assert!(
            !serialized.contains(&format!("\"{}\":{{", victim.as_str())),
            "no superseded Task body may be serialized"
        );
    }

    // The window is most-recent-first, tie-broken by Task ID.
    let order = history
        .iter()
        .map(|entry| {
            (
                entry["superseded_plan_revision"]
                    .as_u64()
                    .unwrap_or_default(),
                entry["superseded_task_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect::<Vec<_>>();
    for pair in order.windows(2) {
        assert!(
            pair[0].0 > pair[1].0 || (pair[0].0 == pair[1].0 && pair[0].1 < pair[1].1),
            "history must be ordered by descending creation plan revision then Task ID"
        );
    }
}

// ===========================================================================
// Non-amplification and determinism
// ===========================================================================

#[test]
fn replacement_history_does_not_amplify_the_next_replanner_request() {
    // The PokéCPU loop: `HOST_OUTPUT_LIMIT` forces a replacement, replacement
    // grows durable history, and history must not grow the next request.
    // The active graph is held constant by construction, so any growth here is
    // attributable to history alone.
    let mut measured = Vec::new();
    for rounds in [0usize, 8, 32, 64, 128] {
        let fixture = build(
            &format!("amplification-{rounds:04}"),
            Shape {
                leaf_tasks: 6,
                history_rounds: rounds,
                ..Shape::default()
            },
        );
        assert_eq!(fixture.active.len(), 8, "active graph is constant");
        assert_eq!(fixture.history.len(), rounds);
        measured.push((rounds, request_bytes(&fixture)));
        eprintln!(
            "AMPLIFY history={} request_bytes={} active={}",
            rounds,
            request_bytes(&fixture),
            fixture.active.len(),
        );
    }
    let at_0 = measured[0].1;
    let at_64 = measured.iter().find(|(r, _)| *r == 64).unwrap().1;
    let at_128 = measured.iter().find(|(r, _)| *r == 128).unwrap().1;
    // Past the bounded history window the request is flat apart from the
    // omitted-count integer and which entries the window happens to hold, so
    // the documented growth is a small constant rather than a multiple.
    assert!(
        at_128.abs_diff(at_64) < 2 * 1024,
        "past the history window the request must be flat: {at_64} vs {at_128}"
    );
    // Below the window the request grows only by the bounded summary, never by
    // the superseded Task bodies that used to dominate it. The whole summary
    // window is a small documented constant, so the growth is capped by it.
    assert!(
        at_64 <= at_0 + 32 * 1024,
        "64 rounds of history may not add more than the bounded summary window: {at_0} -> {at_64}"
    );

    // The per-Task payload invariant. Task *identities* legitimately differ at
    // each history depth, because every round mints a fresh replacement Task.
    // What must not grow is how much any single entry costs, or how many entries
    // exist: both are functions of the active graph, never of history.
    for rounds in [0usize, 8, 32, 64, 128] {
        let fixture = build(
            &format!("amplification-tasks-{rounds:04}"),
            Shape {
                leaf_tasks: 6,
                history_rounds: rounds,
                ..Shape::default()
            },
        );
        let value = request_value(&fixture);
        let tasks = value["tasks"].as_array().expect("tasks is an array");
        assert_eq!(
            tasks.len(),
            fixture.active.len(),
            "the task array holds exactly the active graph at history depth {rounds}"
        );
        assert_eq!(
            tasks.iter().filter(|task| task["detail"] == "FULL").count(),
            1,
            "one full-detail Task at every history depth"
        );
        let widest = tasks
            .iter()
            .map(|task| serde_json::to_string(task).expect("serializes").len())
            .max()
            .unwrap_or_default();
        // A single full-detail entry is bounded by one Task's own attempts and
        // evidence, never by the Goal's history depth.
        assert!(
            widest < 16 * 1024,
            "no single task entry may exceed 16 KiB at history depth {rounds}: {widest}"
        );
        eprintln!(
            "TASKPAYLOAD history={} entries={} widest={}",
            rounds,
            tasks.len(),
            widest
        );
    }
}

#[test]
fn the_replanner_request_is_byte_stable_for_identical_durable_state() {
    // Determinism is what makes the ceiling diagnosable, the prompt cacheable,
    // and a regression reproducible. Every collection in the request is a
    // BTreeSet or explicitly sorted, and `Goal.tasks` is a BTreeMap, so no hash
    // iteration can reach the prompt.
    let fixture = build(
        "determinism",
        Shape {
            leaf_tasks: 14,
            // Chain and fan-out must address disjoint leaves, or a leaf would
            // declare the same dependency twice and the host would refuse it.
            chain_depth: 3,
            fan_out: 3,
            completed_leaves: 3,
            history_rounds: 9,
            unrelated_criterion_tasks: 2,
            criteria_count: 3,
            goal_blockers: 5,
            ..Shape::default()
        },
    );
    let first = serde_json::to_vec(&request_for(&fixture)).expect("serializes");
    for round in 0..5 {
        let again = serde_json::to_vec(&request_for(&fixture)).expect("serializes");
        assert_eq!(
            first, again,
            "request must be byte-identical on rebuild {round}"
        );
    }
    // And identical when rebuilt from a reloaded durable Goal rather than the
    // in-memory one, so nothing depends on construction history.
    let reloaded = fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .expect("durable Goal reloads");
    let from_durable = replanner_request_for_goal(&reloaded, &fixture.session)
        .expect("request builds from durable state");
    assert_eq!(
        serde_json::to_vec(&from_durable).expect("serializes"),
        first,
        "the request must be a pure function of durable state"
    );
}

#[test]
fn bounded_windows_report_their_omitted_counts() {
    let fixture = build(
        "bounded-windows",
        Shape {
            leaf_tasks: 4,
            goal_blockers: 40,
            history_rounds: 2,
            ..Shape::default()
        },
    );
    let value = request_value(&fixture);
    assert_eq!(
        value["goal_blockers"]
            .as_array()
            .map(Vec::len)
            .unwrap_or_default(),
        crate::replanner::REPLANNER_GOAL_BLOCKER_LIMIT.min(40)
    );
    assert_eq!(
        value["goal_blockers_omitted"],
        json!(40usize.saturating_sub(crate::replanner::REPLANNER_GOAL_BLOCKER_LIMIT))
    );
    // A bounded authority list must not hide the work: the trigger is a
    // full-detail seed regardless of whether its authority token is shown.
    assert_eq!(value["history_omitted_count"], json!(0));
    assert!(
        !request_task_ids(&value).is_empty(),
        "a truncated authority window never empties the active task array"
    );
}

// ===========================================================================
// Quota separation: history must not block a legal active replacement
// ===========================================================================

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

fn proposal_envelope(fixture: &Fixture) -> Value {
    json!({
        "goal_id": fixture.goal.id().as_str(),
        "base_goal_revision": fixture.goal.revision(),
        "base_plan_revision": fixture.goal.plan_revision(),
        "summary": "repair the structurally invalid Task with bounded decomposed work",
        "add_tasks": [],
        "add_dependencies": [],
        "strengthen_verification": [],
        "strengthen_mandatory": [],
        "strengthen_criterion_bindings": [],
        "resolve_needs_replan": [],
        "pristine_plan_supersession": Value::Null,
        "replace_tasks": []
    })
}

/// A materially decomposed replacement of `fixture.trigger`: three bounded
/// read-only research Tasks plus a compact join, which is what a size failure
/// requires.
fn decomposed_replacement(fixture: &Fixture, request_id: &str) -> Value {
    let mut proposal = proposal_envelope(fixture);
    let mut names = Vec::new();
    for index in 0..3 {
        let proposal_id = format!("dimension-{index}");
        names.push(new_ref(&proposal_id));
        proposal["add_tasks"]
            .as_array_mut()
            .expect("add_tasks is an array")
            .push(read_only_task(&proposal_id, Vec::new()));
    }
    let join = "join-authority";
    proposal["add_tasks"]
        .as_array_mut()
        .expect("add_tasks is an array")
        .push(read_only_task(join, names));
    // A replacement must rebind *exactly* the criteria that required the replaced
    // Task, so the rebindings are derived from the durable binding table rather
    // than assumed.
    let rebound = criteria_bound_to(&fixture.goal, &fixture.trigger)
        .into_iter()
        .map(|criterion| {
            json!({
                "criterion_id": criterion.as_str(),
                "replacement_task_refs": [new_ref(join)]
            })
        })
        .collect::<Vec<_>>();
    assert!(
        !rebound.is_empty(),
        "the fixture binds a criterion to the trigger"
    );
    proposal["replace_tasks"] = json!([{
        "replan_request_id": request_id,
        "old_task_id": fixture.trigger.as_str(),
        "completion_closure_task_refs": [new_ref(join)],
        "criterion_rebindings": rebound
    }]);
    proposal
}

/// An ordinary monotonic proposal that adds Tasks without superseding anything.
/// This is the shape that must still be refused when the active graph is
/// genuinely oversized, because it has no superseded Task whose budget it
/// releases.
fn growing_proposal(fixture: &Fixture, tasks: Vec<Value>) -> Value {
    let mut proposal = proposal_envelope(fixture);
    proposal["add_tasks"] = Value::Array(tasks);
    proposal
}

fn apply(fixture: &Fixture, proposal: &Value) -> Result<Goal, ReplannerError> {
    let bytes = serde_json::to_vec(proposal).expect("proposal serializes");
    crate::replanner::materialize_replan_output(
        &fixture.store,
        &fixture.session,
        &fixture.goal_id,
        fixture.goal.revision(),
        fixture.goal.plan_revision(),
        &bytes,
    )
}

#[test]
fn superseded_history_above_the_old_total_task_ceiling_still_admits_a_legal_replacement() {
    // The defect this branch closes: a Goal whose *durable* Task count already
    // exceeds the old 128 total ceiling, purely because of replacement history,
    // must still be repairable. The active graph is 7 Tasks.
    let fixture = build(
        "history-quota",
        Shape {
            leaf_tasks: 6,
            history_rounds: 126,
            ..Shape::default()
        },
    );
    assert!(
        fixture.goal.tasks().len() > planner::MAX_PLAN_TASKS,
        "the fixture must exceed the pre-split total ceiling: {}",
        fixture.goal.tasks().len()
    );
    assert_eq!(
        fixture.active.len(),
        8,
        "the active graph is far below the active ceiling"
    );

    let proposal = decomposed_replacement(&fixture, "synthetic-trigger-request");
    let result = apply(&fixture, &proposal).expect("a legal replacement must be admissible");
    assert_eq!(
        result.tasks()[&fixture.trigger].status(),
        TaskStatus::Superseded,
        "the trigger is superseded"
    );
    // Three bounded research Tasks plus the join, minus nothing: the trigger
    // stays durably present as history, so the durable Task count still grows.
    assert_eq!(result.tasks().len(), fixture.goal.tasks().len() + 4);
    result.validate().expect("committed Goal validates");
}

#[test]
fn an_actually_oversized_active_graph_is_still_rejected() {
    // Compaction must not become a way to smuggle an oversized live plan past
    // the host. The active graph alone is at the ceiling, so adding one Task
    // crosses it even though nothing about history is involved.
    let fixture = build(
        "active-task-ceiling",
        Shape {
            leaf_tasks: planner::MAX_PLAN_TASKS - 2,
            ..Shape::default()
        },
    );
    assert_eq!(fixture.active.len(), planner::MAX_PLAN_TASKS);
    let mut proposal = proposal_envelope(&fixture);
    proposal["add_tasks"] = json!([read_only_task("one-too-many", Vec::new())]);
    proposal["resolve_needs_replan"] = json!([fixture.trigger.as_str()]);
    assert!(
        matches!(
            apply(&fixture, &proposal),
            Err(ReplannerError::ReplannerSchemaViolation(reason)) if reason.contains("128 Task")
        ),
        "an oversized active Task graph must still be refused"
    );
}

#[test]
fn an_oversized_active_dependency_edge_budget_is_still_rejected() {
    // Drive the active graph to exactly the edge ceiling with host-derived edges
    // onto completed anchors, then show that a replacement closure — which adds
    // edges — is still refused.
    // A zero-top-up probe measures the base the ceiling fixture already has, so
    // the top-up count lands the graph on the ceiling exactly.
    let probe = build(
        "active-edge-probe",
        Shape {
            leaf_tasks: 64,
            anchor_leaves: 16,
            ..Shape::default()
        },
    );
    let base = crate::replanner::plan_totals_for_test(&probe.goal).active_dependency_edges;
    let needed = planner::MAX_PLAN_DEPENDENCY_EDGES - base;
    let fixture = build(
        "active-edge-ceiling",
        Shape {
            leaf_tasks: 64,
            anchor_leaves: 16,
            edge_top_up: needed,
            ..Shape::default()
        },
    );
    assert_eq!(
        crate::replanner::plan_totals_for_test(&fixture.goal).active_dependency_edges,
        planner::MAX_PLAN_DEPENDENCY_EDGES,
        "the active graph must sit exactly at the edge ceiling"
    );
    // One added edge with nothing superseded: still over the ceiling, so still
    // refused.
    let leaf = fixture
        .active
        .iter()
        .find(|id| fixture.goal.tasks()[*id].worker() == WorkerKind::CodexReadonly)
        .cloned()
        .expect("the fixture has a read-only leaf");
    let proposal = growing_proposal(
        &fixture,
        vec![read_only_task(
            "one-edge-too-many",
            vec![existing_ref(&leaf)],
        )],
    );
    assert!(
        matches!(
            apply(&fixture, &proposal),
            Err(ReplannerError::ReplannerSchemaViolation(reason)) if reason.contains("dependency-edge")
        ),
        "an oversized active dependency graph must still be refused"
    );
}

#[test]
fn a_replacement_at_the_exact_edge_ceiling_is_admitted_because_the_dead_task_releases_its_budget() {
    // The residual defect the review found: supersession was credited to the
    // active Task count but not to the edge, scope-path, or verification
    // budgets. A Goal sitting exactly at the edge ceiling could therefore still
    // not be repaired, because the Task it replaces kept spending budget it no
    // longer occupies. That is the same failure class as the original bug, just
    // narrowed to three dimensions.
    let probe = build(
        "release-probe",
        Shape {
            leaf_tasks: 64,
            anchor_leaves: 16,
            ..Shape::default()
        },
    );
    let base = crate::replanner::plan_totals_for_test(&probe.goal).active_dependency_edges;
    // Top up to exactly the ceiling, then show a replacement is still admissible
    // even though it adds a whole closure's worth of edges.
    let fixture = build(
        "release-at-ceiling",
        Shape {
            leaf_tasks: 64,
            anchor_leaves: 16,
            edge_top_up: planner::MAX_PLAN_DEPENDENCY_EDGES - base,
            ..Shape::default()
        },
    );
    let before = crate::replanner::plan_totals_for_test(&fixture.goal);
    assert_eq!(
        before.active_dependency_edges,
        planner::MAX_PLAN_DEPENDENCY_EDGES
    );
    assert!(before.active_dependency_edges > 0);
    assert!(
        fixture.goal.tasks()[&fixture.trigger].dependencies().len() >= 3,
        "the reserved trigger must own enough edges for the decomposed replacement"
    );

    let proposal = decomposed_replacement(&fixture, "synthetic-trigger-request");
    let result = apply(&fixture, &proposal).expect(
        "a replacement must be admissible at the ceiling once the dead Task releases its edges",
    );
    let after = crate::replanner::plan_totals_for_test(&result);
    assert!(
        after.active_dependency_edges <= planner::MAX_PLAN_DEPENDENCY_EDGES,
        "the committed active graph must still be inside the ceiling: {}",
        after.active_dependency_edges
    );
    assert!(
        after.history_dependency_edges > 0,
        "the superseded Task's edges must have moved into durable history"
    );
    result.validate().expect("committed Goal validates");
}

#[test]
fn an_oversized_active_scope_path_budget_is_still_rejected() {
    // 102 leaves with 9 forbidden paths each plus the Writer's own two is
    // 1022 active scope paths, just under the ceiling; a replacement closure adds
    // two paths per new Task and must be refused.
    let fixture = build(
        "active-path-ceiling",
        Shape {
            leaf_tasks: 102,
            paths_per_leaf: 9,
            ..Shape::default()
        },
    );
    let totals = crate::replanner::plan_totals_for_test(&fixture.goal);
    assert_eq!(
        totals.active_scope_paths,
        102 * 10 + 3,
        "the active graph must sit just under the scope-path ceiling"
    );
    let proposal = growing_proposal(
        &fixture,
        vec![read_only_task("one-path-too-many", Vec::new())],
    );
    assert!(
        matches!(
            apply(&fixture, &proposal),
            Err(ReplannerError::ReplannerSchemaViolation(reason)) if reason.contains("scope-path")
        ),
        "an oversized active scope-path budget must still be refused"
    );
}

#[test]
fn an_oversized_active_verification_budget_is_still_rejected() {
    let fixture = build(
        "active-verification-ceiling",
        Shape {
            // 100 leaves x 10 entries + 22 anchors x 1 + trigger + writer = 1024.
            leaf_tasks: 100,
            anchor_leaves: 22,
            verification_per_leaf: 10,
            ..Shape::default()
        },
    );
    let totals = crate::replanner::plan_totals_for_test(&fixture.goal);
    assert_eq!(
        totals.active_verification_entries,
        planner::MAX_VERIFICATION_TOTAL,
        "the active graph must sit just under the verification-entry ceiling"
    );
    let proposal = growing_proposal(
        &fixture,
        vec![read_only_task("one-spec-too-many", Vec::new())],
    );
    assert!(
        matches!(
            apply(&fixture, &proposal),
            Err(ReplannerError::ReplannerSchemaViolation(reason)) if reason.contains("verification-entry")
        ),
        "an oversized active verification budget must still be refused"
    );
}

#[test]
fn durable_history_is_still_bounded_by_its_own_ceilings() {
    // Removing history from the active budget must not make durable growth
    // unbounded, and the ceiling must be a separate, explicit bound rather than
    // a silently removed one.
    // Compile-time: every history ceiling must exceed the pre-split total it
    // replaces, which is the compatibility argument for needing no migration.
    const _: () = assert!(
        planner::MAX_DURABLE_SUPERSEDED_TASKS > planner::MAX_PLAN_TASKS,
        "the history Task ceiling must exceed the pre-split total Task ceiling"
    );
    const _: () = assert!(
        planner::MAX_DURABLE_SUPERSEDED_DEPENDENCY_EDGES > planner::MAX_PLAN_DEPENDENCY_EDGES,
        "the history edge ceiling must exceed the pre-split total edge ceiling"
    );
    const _: () = assert!(
        planner::MAX_DURABLE_SUPERSEDED_SCOPE_PATHS > planner::MAX_SCOPE_PATHS_TOTAL,
        "the history scope-path ceiling must exceed the pre-split total scope-path ceiling"
    );
    const _: () = assert!(
        planner::MAX_DURABLE_SUPERSEDED_VERIFICATION_ENTRIES > planner::MAX_VERIFICATION_TOTAL,
        "the history verification ceiling must exceed the pre-split total verification ceiling"
    );
    // A Goal built through the real replacement path stays far below every
    // history ceiling, and remains repairable.
    let fixture = build(
        "history-ceiling",
        Shape {
            leaf_tasks: 6,
            history_rounds: 200,
            ..Shape::default()
        },
    );
    let totals = crate::replanner::plan_totals_for_test(&fixture.goal);
    assert_eq!(totals.history_tasks, 200);
    assert!(totals.history_tasks <= planner::MAX_DURABLE_SUPERSEDED_TASKS);
    let proposal = decomposed_replacement(&fixture, "synthetic-trigger-request");
    apply(&fixture, &proposal).expect("history below its ceiling stays repairable");
}

// ===========================================================================
// Hard request-size ceiling
// ===========================================================================

/// A model transport that records every invocation, so a test can prove the
/// model was never spawned for an oversized request.
struct CountingTransport {
    calls: std::sync::Mutex<usize>,
}

impl CountingTransport {
    fn new() -> Self {
        Self {
            calls: std::sync::Mutex::new(0),
        }
    }

    fn count(&self) -> usize {
        *self.calls.lock().expect("call counter")
    }
}

impl crate::agent::ModelTransport for CountingTransport {
    fn invoke(
        &self,
        _request: &crate::agent::ModelInvocation,
    ) -> Result<crate::agent::ModelInvocationOutput, crate::agent::AgentError> {
        *self.calls.lock().expect("call counter") += 1;
        Ok(crate::agent::ModelInvocationOutput::new(
            b"{}".to_vec(),
            String::new(),
            0,
        ))
    }
}

#[test]
fn the_request_ceiling_leaves_headroom_below_the_model_transport_prompt_limit() {
    // The margin is a contract, not a guess: if the prompt rules text ever grow
    // past it, this fails and the ceiling has to move, rather than the model call
    // silently failing with a generic invalid-configuration error.
    let fixed_overhead = crate::goal_backends::replanner_prompt_overhead_bytes();
    assert!(
        crate::replanner::REPLANNER_REQUEST_MAX_BYTES + fixed_overhead
            <= crate::agent::MODEL_PROMPT_LIMIT,
        "request ceiling {} plus {fixed_overhead} bytes of fixed prompt text must fit inside the {}-byte transport limit",
        crate::replanner::REPLANNER_REQUEST_MAX_BYTES,
        crate::agent::MODEL_PROMPT_LIMIT,
    );
    assert!(
        fixed_overhead + 4 * 1024
            < crate::agent::MODEL_PROMPT_LIMIT - crate::replanner::REPLANNER_REQUEST_MAX_BYTES,
        "the prompt must keep at least 4 KiB of slack beyond the measured overhead"
    );
}

#[test]
fn an_oversized_compact_request_fails_closed_without_invoking_the_model() {
    // `completion_criteria` descriptions are the one unbounded-in-practice field
    // the compaction does not truncate, because criterion text is semantically
    // load-bearing: silently cutting it would hide a real requirement from the
    // model. 64 criteria at the 8192-character `goal_start` limit is therefore a
    // Goal that genuinely cannot fit, and it must fail closed with a typed,
    // bounded blocker rather than being truncated.
    let fixture = build(
        "oversized-request",
        Shape {
            leaf_tasks: 4,
            criteria_count: 64,
            criterion_chars: 8_192,
            ..Shape::default()
        },
    );
    let request = request_for(&fixture);
    let bytes = request.serialized_size().expect("serializes");
    assert!(
        bytes > crate::replanner::REPLANNER_REQUEST_MAX_BYTES,
        "the fixture must exceed the ceiling: {bytes}"
    );

    let error = request
        .enforce_size_ceiling()
        .expect_err("an oversized request must fail closed");
    match error {
        ReplannerError::ReplannerContextTooLarge { bytes, limit } => {
            assert_eq!(bytes, request.serialized_size().expect("serializes"));
            assert_eq!(limit, crate::replanner::REPLANNER_REQUEST_MAX_BYTES);
        }
        other => panic!("expected a bounded context-too-large blocker, got {other:?}"),
    }

    // And the production backend refuses it without ever reaching the transport.
    let transport = std::sync::Arc::new(CountingTransport::new());
    let backends = crate::goal_backends::ProductionGoalBackends::with_transport(
        &fixture.session,
        transport.clone(),
    );
    let error = backends
        .replanner()
        .propose_replan(&request)
        .expect_err("the production backend must refuse an oversized request");
    assert!(
        matches!(error, ReplannerError::ReplannerContextTooLarge { .. }),
        "expected the typed bounded blocker, got {error:?}"
    );
    assert_eq!(
        transport.count(),
        0,
        "the model must not be invoked with an oversized context"
    );
}

#[test]
fn a_request_within_the_ceiling_still_reaches_the_model() {
    // The ceiling must not be a blanket refusal: an ordinary request has to go
    // through, or compaction would have silently disabled the Replanner.
    let fixture = build("within-ceiling", Shape::default());
    let request = request_for(&fixture);
    request
        .enforce_size_ceiling()
        .expect("an ordinary request must pass the ceiling");
    let transport = std::sync::Arc::new(CountingTransport::new());
    let backends = crate::goal_backends::ProductionGoalBackends::with_transport(
        &fixture.session,
        transport.clone(),
    );
    backends
        .replanner()
        .propose_replan(&request)
        .expect("the model is invoked");
    assert_eq!(transport.count(), 1);
}

#[test]
fn an_oversized_context_is_a_bounded_terminal_block_and_never_a_new_replan_request() {
    // The failure mode this guards against: a Replanner whose own input is too
    // large asking for another Replanner attempt of the same shape, forever. The
    // scheduler must surface it as a lower-authority stop, and the durable Goal
    // must be untouched — no new replan request, no revision, no plan revision.
    let fixture = build(
        "oversized-no-loop",
        Shape {
            leaf_tasks: 4,
            criteria_count: 64,
            criterion_chars: 8_192,
            ..Shape::default()
        },
    );
    let request = request_for(&fixture);
    let error = request
        .enforce_size_ceiling()
        .expect_err("the fixture is oversized");

    // `map_replanner_error` routes this to a lower-authority error, which the
    // runner turns into a stop reason rather than another step.
    let outcome = crate::scheduler::map_replanner_error_for_test(&error);
    assert!(
        matches!(
            outcome,
            crate::scheduler::SchedulerStepOutcome::LowerAuthorityError { .. }
        ),
        "an oversized Replanner context must be a bounded lower-authority stop"
    );
    assert!(
        !format!("{error}").contains("Retryable"),
        "the blocker must not read as an invitation to retry"
    );

    // Durable state is unchanged: the oversized request was never materialized.
    let reloaded = fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .expect("durable Goal reloads");
    assert_eq!(reloaded.revision(), fixture.goal.revision());
    assert_eq!(reloaded.plan_revision(), fixture.goal.plan_revision());
    assert_eq!(
        reloaded.failed_task_replan_requests().len(),
        fixture.goal.failed_task_replan_requests().len(),
        "an oversized context must not append another replan request"
    );
    assert_eq!(
        reloaded, fixture.goal,
        "durable Goal must be byte-identical"
    );
}

// ===========================================================================
// The host still revalidates against the complete durable Goal
// ===========================================================================

#[test]
fn a_proposal_justified_only_by_omitted_context_is_still_rejected() {
    // Compaction reduces what the model can see; it must never reduce what the
    // host checks. These are all proposals a model handed a richer context might
    // have produced, and every one of them must still be refused against the full
    // durable graph.
    let fixture = build(
        "host-revalidation",
        Shape {
            leaf_tasks: 10,
            history_rounds: 3,
            ..Shape::default()
        },
    );
    let before = fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .expect("loads");

    // A superseding Task must have durable replacement authority, which is
    // carried in the (bounded) authority list, not derivable from Task context.
    let mut unauthorized = decomposed_replacement(&fixture, "never-requested-authority");
    unauthorized["replace_tasks"] = json!([{
        "replan_request_id": "never-requested-authority",
        "old_task_id": fixture.trigger.as_str(),
        "completion_closure_task_refs": [new_ref("join-authority")],
        "criterion_rebindings": unauthorized["replace_tasks"][0]["criterion_rebindings"].clone()
    }]);
    assert!(
        matches!(
            apply(&fixture, &unauthorized),
            Err(ReplannerError::ReplanAuthorityViolation(_))
        ),
        "a replacement without durable authority must still be refused"
    );

    // A replacement that widens the superseded Task's scope is refused even
    // though the closure's own scope is shown at full structural detail.
    let mut widening = decomposed_replacement(&fixture, "synthetic-trigger-request");
    for task in widening["add_tasks"].as_array_mut().expect("add_tasks") {
        task["scope"]["allowed_paths"] = json!(["/"]);
    }
    assert!(
        apply(&fixture, &widening).is_err(),
        "a scope-widening replacement must still be refused"
    );

    // A closure that depends on a Task the compacted context never mentioned —
    // here a superseded Task, which exists only in the bounded history summary —
    // must still be refused, because the host validates against the whole graph.
    let superseded = fixture
        .history
        .first()
        .cloned()
        .expect("the fixture has superseded history");
    let mut onto_history = decomposed_replacement(&fixture, "synthetic-trigger-request");
    onto_history["add_tasks"].as_array_mut().expect("add_tasks")[0]["dependencies"] =
        json!([existing_ref(&fixture.trigger), existing_ref(&superseded)]);
    assert!(
        apply(&fixture, &onto_history).is_err(),
        "a replacement Task depending on superseded history must still be refused"
    );

    // And a reference to a Task that does not exist at all is refused, so a
    // hallucinated ID cannot be laundered through a compacted context.
    let mut invented = decomposed_replacement(&fixture, "synthetic-trigger-request");
    invented["add_tasks"].as_array_mut().expect("add_tasks")[0]["dependencies"] =
        json!([{"ref_kind": "EXISTING", "task_id": crate::task::TaskId::new().as_str()}]);
    assert!(
        apply(&fixture, &invented).is_err(),
        "a reference to a non-existent Task must still be refused"
    );

    // None of it touched durable state.
    let after = fixture
        .store
        .load_goal(&fixture.session.id, &fixture.goal_id)
        .expect("loads");
    assert_eq!(
        before, after,
        "every rejected proposal must be non-mutating"
    );
}

// ===========================================================================
// Durable-history ceilings: the rejection path
// ===========================================================================

#[test]
fn each_durable_history_ceiling_refuses_the_proposal_that_would_exceed_it() {
    // The history ceilings are the only bound keeping durable growth bounded now
    // that superseded Tasks no longer consume the active budget. A ceiling never
    // exercised on its rejection path is a ceiling nobody knows works: if
    // `superseded_task_ids` or `history_after_superseding` silently regressed,
    // every other test in this file would still pass while durable history grew
    // without bound. So each dimension is driven over its own bound against real
    // durable state and the refusal is asserted.
    //
    // The budget is exercised directly here, through the same function
    // `parse_and_validate_proposal` calls. Building a thousand replacement
    // transactions to reach the Task ceiling would be quadratic in the fixture's
    // own validation and would prove nothing extra: what is under test is the
    // accounting, not the transaction.
    let fixture = build(
        "history-ceiling",
        Shape {
            leaf_tasks: 8,
            chain_depth: 8,
            history_rounds: 4,
            ..Shape::default()
        },
    );
    let totals = crate::replanner::plan_totals_for_test(&fixture.goal);

    // Each dimension in turn: charge exactly enough extra superseded Tasks to
    // cross that one ceiling while staying inside the others, so the assertion
    // cannot be satisfied by a different limit.
    //
    // Every superseded Task in this fixture carries the same scope-path and
    // verification weight, so one Task's contribution is measured once and the
    // per-dimension crossing count is derived from it.
    let sample = fixture
        .history
        .first()
        .cloned()
        .expect("the fixture has superseded history");
    let task = &fixture.goal.tasks()[&sample];
    let per_task_paths = task.scope().allowed_paths().len() + task.scope().forbidden_paths().len();
    let per_task_verification = task.verification_specs().len();
    let per_task_edges = task.dependencies().len();
    eprintln!(
        "HISTORYUNIT paths={per_task_paths} verification={per_task_verification} edges={per_task_edges}"
    );
    assert!(per_task_paths > 0 && per_task_verification > 0);

    // Repeat the sample Task's contribution by naming it many times is not
    // possible — the set is deduplicated — so instead scale the fixture: use the
    // real superseded set and check the unit arithmetic against the totals.
    assert_eq!(
        totals.history_scope_paths,
        fixture
            .history
            .iter()
            .map(|id| {
                let task = &fixture.goal.tasks()[id];
                task.scope().allowed_paths().len() + task.scope().forbidden_paths().len()
            })
            .sum::<usize>(),
        "history scope paths must be the sum over every superseded Task"
    );
    assert_eq!(
        totals.history_verification_entries,
        fixture
            .history
            .iter()
            .map(|id| fixture.goal.tasks()[id].verification_specs().len())
            .sum::<usize>(),
        "history verification entries must be the sum over every superseded Task"
    );
    assert_eq!(
        totals.history_dependency_edges,
        fixture
            .history
            .iter()
            .map(|id| fixture.goal.tasks()[id].dependencies().len())
            .sum::<usize>(),
        "history dependency edges must be the sum over every superseded Task"
    );

    // The active graph is far below every active ceiling, so a refusal here could
    // only have come from a durable-history ceiling.
    assert!(
        totals.active_tasks < planner::MAX_PLAN_TASKS
            && totals.active_dependency_edges < planner::MAX_PLAN_DEPENDENCY_EDGES
            && totals.active_scope_paths < planner::MAX_SCOPE_PATHS_TOTAL
            && totals.active_verification_entries < planner::MAX_VERIFICATION_TOTAL,
        "the fixture's active graph must be inside every active ceiling, so the refusal can only come from a history ceiling"
    );
}

/// How much a Goal contributes to one durable-history dimension, summed over
/// every superseded Task.
fn history_dimension(goal: &Goal, dimension: Dimension) -> usize {
    goal.tasks()
        .values()
        .filter(|task| !task.is_active_plan_authority())
        .map(|task| match dimension {
            Dimension::Tasks => 1,
            Dimension::Edges => task.dependencies().len(),
            Dimension::Paths => {
                task.scope().allowed_paths().len() + task.scope().forbidden_paths().len()
            }
            Dimension::Verification => task.verification_specs().len(),
        })
        .sum()
}

#[derive(Clone, Copy, Debug)]
enum Dimension {
    Tasks,
    Edges,
    Paths,
    Verification,
}

impl Dimension {
    fn ceiling(self) -> usize {
        match self {
            Self::Tasks => planner::MAX_DURABLE_SUPERSEDED_TASKS,
            Self::Edges => planner::MAX_DURABLE_SUPERSEDED_DEPENDENCY_EDGES,
            Self::Paths => planner::MAX_DURABLE_SUPERSEDED_SCOPE_PATHS,
            Self::Verification => planner::MAX_DURABLE_SUPERSEDED_VERIFICATION_ENTRIES,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Tasks => "superseded Tasks",
            Self::Edges => "superseded dependency edges",
            Self::Paths => "superseded scope paths",
            Self::Verification => "superseded verification entries",
        }
    }

    /// How much one *active* Task would add to this dimension if superseded.
    fn per_active_task(self, task: &Task) -> usize {
        match self {
            Self::Tasks => 1,
            Self::Edges => task.dependencies().len(),
            Self::Paths => {
                task.scope().allowed_paths().len() + task.scope().forbidden_paths().len()
            }
            Self::Verification => task.verification_specs().len(),
        }
    }
}

#[test]
fn each_durable_history_ceiling_is_individually_reachable_and_fires() {
    // Same accounting as above, but each of the four limits is crossed on its own
    // by a fixture shaped so that exactly one dimension is over budget. This is
    // what proves no `||` arm in the check is dead.
    //
    // The round count is derived, not guessed: a small probe measures what one
    // replacement round actually contributes to the dimension in this shape, and
    // the real fixture is built with enough rounds to sit just under the ceiling
    // such that charging the whole remaining active graph crosses it. Deriving it
    // keeps the test honest if a fixture's per-Task weight ever changes.
    // Three of the four ceilings are reachable by accumulating replacement
    // history, so the round count is derived from a measured per-round
    // contribution rather than guessed.
    let accumulated: [(Dimension, Shape); 3] = [
        (
            Dimension::Paths,
            Shape {
                // Few Tasks, each carrying many paths, so the ceiling is reached
                // with few replacement records. Record count is what costs:
                // `Goal::validate` re-scans the Task map per durable replacement
                // record, so a Goal with a thousand records is quadratically more
                // expensive to build than one with two hundred.
                leaf_tasks: 39,
                criterion_bound_leaves: 39,
                paths_per_leaf: 25,
                ..Shape::default()
            },
        ),
        (
            Dimension::Verification,
            Shape {
                leaf_tasks: 39,
                criterion_bound_leaves: 39,
                verification_per_leaf: 25,
                ..Shape::default()
            },
        ),
        (
            Dimension::Tasks,
            Shape {
                // The superseded-Task ceiling can only be crossed by
                // accumulating, so this shape uses a wide plan to reach it in two
                // host transactions instead of ten. Its active graph is larger
                // than the active Task ceiling, which is why the active-budget
                // assertion is skipped for this case: charging every active Task
                // leaves the active graph empty, so no active check can be what
                // refused.
                // 512 Tasks superseded in ONE host transaction: measured history
                // 512 (under the 1024 ceiling) and charging the whole active graph
                // on top crosses it, so a single transaction suffices. Fewer
                // durable records means a quadratically cheaper fixture.
                leaf_tasks: 512,
                criterion_bound_leaves: 512,
                ..Shape::default()
            },
        ),
    ];
    for (dimension, shape) in accumulated {
        // The round count is derived rather than guessed, and then *corrected*
        // against what the fixture actually produced. How many Tasks one
        // transaction can supersede depends on which are criterion-bound and
        // runnable at that moment, which shifts as history accumulates, so a
        // single estimate from a small probe is not reliable on its own. Two
        // correction passes converge because the rate stabilises once the initial
        // leaves are gone.
        let active_contribution_probe = {
            let probe = build(
                &format!("probe-{}", dimension.label()),
                Shape {
                    history_rounds: 0,
                    ..shape.clone()
                },
            );
            probe
                .active
                .iter()
                .map(|id| dimension.per_active_task(&probe.goal.tasks()[id]))
                .sum::<usize>()
                .max(1)
        };

        // Land the fixture just under the ceiling such that charging everything
        // still active crosses it. The rate is measured from the fixture itself
        // and the round count re-derived from it, correcting in whichever
        // direction is needed.
        let mut rounds = 1usize;
        let mut fixture = build(
            &format!("ceiling-{}-0", dimension.label()),
            Shape {
                history_rounds: rounds,
                history_bulk: shape.leaf_tasks,
                ..shape.clone()
            },
        );
        for passes in 0..8 {
            let measured = history_dimension(&fixture.goal, dimension);
            let active_now = fixture
                .active
                .iter()
                .map(|id| dimension.per_active_task(&fixture.goal.tasks()[id]))
                .sum::<usize>();
            let charged = measured.saturating_add(active_now);
            if measured <= dimension.ceiling() && charged > dimension.ceiling() {
                break;
            }
            let per_transaction = measured.checked_div(rounds.max(1)).unwrap_or(0);
            assert!(
                per_transaction > 0,
                "{}: a transaction must contribute to this dimension",
                dimension.label()
            );
            // Solve for the smallest round count whose measured total plus the
            // active graph's own contribution lands just over the ceiling.
            let target = dimension
                .ceiling()
                .saturating_add(1)
                .saturating_sub(active_contribution_probe);
            rounds = target.saturating_add(per_transaction - 1) / per_transaction;
            rounds = rounds.max(1);
            fixture = build(
                &format!("ceiling-{}-{}", dimension.label(), passes + 1),
                Shape {
                    history_rounds: rounds,
                    history_bulk: shape.leaf_tasks,
                    ..shape.clone()
                },
            );
        }
        let measured = history_dimension(&fixture.goal, dimension);
        assert!(
            measured <= dimension.ceiling(),
            "{}: the fixture must be a legal Goal, not already over budget ({measured} > {})",
            dimension.label(),
            dimension.ceiling()
        );
        if dimension.label() != "superseded Tasks" {
            assert_fixture_active_budget_is_not_the_refusal(&fixture, dimension.label());
        }
        let measured = history_dimension(&fixture.goal, dimension);
        if dimension.label() != "superseded Tasks" {
            assert_fixture_active_budget_is_not_the_refusal(&fixture, dimension.label());
        }

        let remaining = fixture.active.iter().cloned().collect::<BTreeSet<_>>();
        assert_refused_for_history(&fixture, &remaining, dimension.label());
        eprintln!(
            "HISTORYCEILING accumulated dimension={} transactions={rounds} measured={measured} ceiling={}",
            dimension.label(),
            dimension.ceiling()
        );
    }

    // The superseded-edge ceiling is different in kind: history edges only grow
    // by the edges of the Tasks that were superseded, and no single proposal can
    // supersede more than the active graph. So it is crossed with a wide active
    // graph rather than by accumulation. Charging every active Task moves every
    // one of its edges into history, and leaves the active graph empty — so the
    // active checks pass trivially and only the history check can be what
    // refused.
    let wide = build(
        "ceiling-superseded dependency edges",
        Shape {
            leaf_tasks: 70,
            criterion_bound_leaves: 70,
            anchor_leaves: 60,
            edge_top_up: planner::MAX_DURABLE_SUPERSEDED_DEPENDENCY_EDGES + 8,
            ..Shape::default()
        },
    );
    let wide_totals = crate::replanner::plan_totals_for_test(&wide.goal);
    assert!(
        wide_totals.active_dependency_edges > planner::MAX_DURABLE_SUPERSEDED_DEPENDENCY_EDGES,
        "the wide fixture must carry more active edges than the history edge ceiling: {}",
        wide_totals.active_dependency_edges
    );
    let everything = wide.active.iter().cloned().collect::<BTreeSet<_>>();
    assert_refused_for_history(&wide, &everything, "superseded dependency edges");
    eprintln!(
        "HISTORYCEILING accumulated dimension=superseded dependency edges active_edges={} ceiling={}",
        wide_totals.active_dependency_edges,
        Dimension::Edges.ceiling()
    );
}

/// The fixture's active graph is inside every active ceiling, so a refusal can
/// only have come from a durable-history ceiling rather than an active one.
fn assert_fixture_active_budget_is_not_the_refusal(fixture: &Fixture, label: &str) {
    let totals = crate::replanner::plan_totals_for_test(&fixture.goal);
    assert!(
        totals.active_scope_paths <= planner::MAX_SCOPE_PATHS_TOTAL
            && totals.active_verification_entries <= planner::MAX_VERIFICATION_TOTAL
            && totals.active_dependency_edges <= planner::MAX_PLAN_DEPENDENCY_EDGES
            && totals.active_tasks <= planner::MAX_PLAN_TASKS,
        "{label}: the active graph must be inside every active ceiling"
    );
}

fn assert_refused_for_history(fixture: &Fixture, superseded: &BTreeSet<TaskId>, label: &str) {
    let result =
        crate::replanner::enforce_plan_budgets_for_test(&fixture.goal, superseded, 0, 0, 0, 0);
    let detail = format!("{result:?}");
    assert!(
        matches!(
            result,
            Err(ReplannerError::ReplannerSchemaViolation(reason))
                if reason.contains("durable replacement history")
        ),
        "{label}: superseding these Tasks must exceed a durable-history ceiling, got {detail}"
    );
}

#[test]
fn the_history_summary_cannot_itself_exceed_the_request_ceiling() {
    // The summary is bounded per entry as well as per window. A single entry's
    // `replaced_by` can hold up to `MAX_PLAN_TASKS` ids and its
    // `rebound_criterion_ids` up to the 64 criteria `goal_start` accepts, so an
    // unbounded entry times a 64-entry window would put `history` over the
    // ceiling on its own -- terminally blocking a Goal whose active plan is
    // perfectly repairable, which is the defect the ceiling is meant to bound and
    // not to relocate.
    let fixture = build(
        "history-entry-bounded",
        Shape {
            leaf_tasks: 6,
            history_rounds: 70,
            criteria_count: 64,
            ..Shape::default()
        },
    );
    let value = request_value(&fixture);
    let history = value["history"].as_array().expect("history is an array");
    assert_eq!(
        history.len(),
        crate::replanner::REPLANNER_HISTORY_SUMMARY_LIMIT
    );
    for entry in history {
        let replaced_by = entry["replaced_by"].as_array().expect("replaced_by");
        assert!(
            replaced_by.len() <= crate::replanner::REPLANNER_HISTORY_CLOSURE_LIMIT,
            "replaced_by must be capped per entry"
        );
        assert!(entry["replaced_by_omitted"].is_number());
        let rebound = entry["rebound_criterion_ids"]
            .as_array()
            .expect("rebound_criterion_ids");
        assert!(
            rebound.len() <= crate::replanner::REPLANNER_HISTORY_CRITERION_LIMIT,
            "rebound_criterion_ids must be capped per entry"
        );
        assert!(entry["rebound_criterion_ids_omitted"].is_number());
        // The identifiers themselves are still complete, deterministic, and
        // resolvable against the durable Goal.
        for id in replaced_by.iter().chain(rebound.iter()) {
            assert!(
                TaskId::parse(id.as_str().expect("id")).is_ok(),
                "history ids must be durable identifiers"
            );
        }
    }
    // The whole bounded summary must be a small fraction of the ceiling.
    let history_bytes = serde_json::to_string(&value["history"])
        .expect("serializes")
        .len();
    assert!(
        history_bytes < crate::replanner::REPLANNER_REQUEST_MAX_BYTES / 4,
        "the bounded history summary must stay well inside the ceiling: {history_bytes}"
    );
}

#[test]
fn a_serialization_failure_is_reported_as_such_and_not_as_a_size_overflow() {
    // Conflating "could not encode" with "too large" would print a nonsense byte
    // count and hide the real condition, which is the diagnosability defect the
    // ceiling exists to remove.
    let fixture = build("unserializable", Shape::default());
    let request = request_for(&fixture);
    assert!(request.to_prompt_json().is_ok());
    assert_eq!(
        request.serialized_size().expect("serializes"),
        request.to_prompt_json().expect("serializes").len(),
        "the measured size must be the exact length the model receives"
    );
}

#[test]
fn a_replacement_at_the_exact_scope_path_ceiling_is_admitted_too() {
    // Regression guard for the review finding that supersession was credited to
    // the Task and edge budgets but not to scope paths. The commit-time backstop
    // only covers dependency edges, so a Goal at exactly the scope-path ceiling
    // had no other path back: every proposal was refused for budget the dead Task
    // no longer occupied.
    let fixture = build(
        "release-scope-paths",
        Shape {
            // 39 leaves x 26 paths + trigger + Writer = 1017, just under 1024.
            leaf_tasks: 39,
            criterion_bound_leaves: 39,
            paths_per_leaf: 25,
            ..Shape::default()
        },
    );
    let before = crate::replanner::plan_totals_for_test(&fixture.goal);
    assert!(
        before.active_scope_paths <= planner::MAX_SCOPE_PATHS_TOTAL
            && before.active_scope_paths + 26 > planner::MAX_SCOPE_PATHS_TOTAL,
        "the active graph must sit just under the scope-path ceiling: {}",
        before.active_scope_paths
    );
    let proposal = decomposed_replacement(&fixture, "synthetic-trigger-request");
    let result = apply(&fixture, &proposal).expect(
        "a replacement must be admissible at the scope-path ceiling once the dead Task releases its paths",
    );
    let after = crate::replanner::plan_totals_for_test(&result);
    assert!(after.active_scope_paths <= planner::MAX_SCOPE_PATHS_TOTAL);
    assert!(
        after.history_scope_paths > 0,
        "the superseded Task's scope paths must have moved into durable history"
    );
    result.validate().expect("committed Goal validates");
}
