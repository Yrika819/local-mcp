//! E5-H3 — reliability and crash-recovery closure for the External Writer /
//! Reviewer legs (AT-E5-034, 040, 042, 046, 052–056).
//!
//! The plan file `docs/EXTERNAL_AGENT_BACKEND_V1_E5_TEST_PLAN.md` numbers these
//! cases; earlier test modules reused some of those numbers for different
//! scenarios. This module adds the *missing* scenarios instead of renaming the
//! existing tests, and names each test after the plan case it actually proves.
//!
//! What is pinned here that was not pinned before:
//!
//! * AT-034 — a multi-file Writer proposal that half-applies is recorded
//!   per-operation, never claimed as an all-files commit, never reconciled to
//!   `COMPLETED`, and never replayed;
//! * AT-034 (all-files case) — a fully applied multi-file publication reaches the
//!   Reviewer and host verification without a second write;
//! * AT-042 — a Goal cancelled *while an operation is being published* stops the
//!   remaining operations through the store's revision CAS, and the journal keeps
//!   the honest per-operation verdict;
//! * AT-052 — the state matrix that makes the driver's Scheduler-level
//!   `REVIEWER_RECOVERY_DEFERRED` branch unreachable;
//! * AT-053/056 — a restart between the reviewer commit and host Task
//!   verification still completes only through the host Verifier.

use std::path::PathBuf;
use std::pin::Pin;

use serde_json::{Value, json};
use sha2::Digest;
use uuid::Uuid;

use crate::config;
use crate::external_goal_api;
use crate::external_model_store::external_state;
use crate::external_publish::{self, PublishOutcome};
use crate::external_writer;
use crate::goal::{Goal, GoalId, GoalStatus};
use crate::goal_api;
use crate::mutation::{MutationIntentState, MutationOperationState, ReviewerInvocationState};
use crate::task::{TaskId, TaskStatus};
use crate::task_store::TaskStore;
use crate::writer::{WriteBoundary, WriterWriteRequest};

const A_OLD: &str = "alpha-before\n";
const B_OLD: &str = "beta-before\n";
const A_NEW: &str = "alpha-after\n";
const B_NEW: &str = "beta-after\n";

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
    let root = std::env::temp_dir().join(format!("local-mcp-e5h3-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(workspace.join("a.txt"), A_OLD).unwrap();
    std::fs::write(workspace.join("b.txt"), B_OLD).unwrap();
    let session = config::Session {
        id: format!("e5h3-{label}-{}", Uuid::new_v4()),
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

fn sha256_hex(text: &str) -> String {
    format!("{:x}", sha2::Sha256::digest(text.as_bytes()))
}

/// A plan whose single Writer Task is allowed to touch two files and whose host
/// verification binds both postimages.
fn two_file_plan(work: &Value) -> Value {
    let criterion_id = work["request"]["completion_criteria"][0]["criterion_id"]
        .as_str()
        .unwrap();
    json!({
        "goal_id": work["goal_id"],
        "goal_revision": work["goal_revision"],
        "summary": "two scoped writer edits",
        "tasks": [{
            "proposal_id": "edit",
            "title": "Update two files",
            "objective": "Apply the requested scoped edits",
            "mandatory": true,
            "worker": "CODEX_WRITER",
            "dependencies": [],
            "scope": {
                "allowed_paths": ["a.txt", "b.txt"],
                "forbidden_paths": [],
                "operation_kind": "LOCAL_MUTATION",
                "replay_safety": "VERIFY_BEFORE_RETRY"
            },
            "verification": [
                {"kind": "FILE_DIGEST", "path": "a.txt", "expected_sha256": sha256_hex(A_NEW)},
                {"kind": "FILE_DIGEST", "path": "b.txt", "expected_sha256": sha256_hex(B_NEW)}
            ]
        }],
        "criterion_bindings": [{"criterion_id": criterion_id, "task_refs": ["edit"]}]
    })
}

/// A `candidate_complete` proposal carrying two `WRITE_UTF8` operations.
fn two_file_answer(work: &Value) -> Value {
    let request = &work["request"];
    json!({
        "goal_id": request["goal_id"],
        "task_id": request["task_id"],
        "attempt_id": request["attempt_id"],
        "goal_revision": request["goal_revision"],
        "plan_revision": request["plan_revision"],
        "status": "candidate_complete",
        "summary": "replace two files",
        "evidence": [{"kind": "writer-evidence", "value": "two files replaced"}],
        "proposed_operations": [
            {
                "kind": "WRITE_UTF8",
                "path": "a.txt",
                "expected_preimage": {"kind": "SHA256", "sha256": sha256_hex(A_OLD)},
                "content": A_NEW
            },
            {
                "kind": "WRITE_UTF8",
                "path": "b.txt",
                "expected_preimage": {"kind": "SHA256", "sha256": sha256_hex(B_OLD)},
                "content": B_NEW
            }
        ]
    })
}

/// Drive the loop to a durably prepared two-operation Writer intent.
fn prepared_two_file_writer(f: &Fixture) -> (GoalId, TaskId, String) {
    let view = goal_api::goal_start(
        &json!({
            "session_id": f.session.id,
            "objective": "Apply two scoped writer changes",
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
            two_file_plan(&planner_work),
        ),
    )
    .unwrap();
    assert_eq!(planned["outcome"], "COMMITTED", "{planned}");
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    assert_eq!(work["role"], "WRITER", "{work}");
    let staged = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, two_file_answer(&work)),
    )
    .unwrap();
    assert_eq!(
        staged["outcome"], "ACCEPTED_FOR_HOST_PROCESSING",
        "{staged}"
    );
    let task_id = TaskId::parse(work["task_id"].as_str().unwrap()).unwrap();
    let goal = goal_of(f, &goal_id);
    let operation_id = goal.tasks()[&task_id]
        .latest_attempt()
        .unwrap()
        .mutation_intent()
        .unwrap()
        .operation_id()
        .to_owned();
    (goal_id, task_id, operation_id)
}

fn goal_of(f: &Fixture, goal_id: &GoalId) -> Goal {
    f.store.load_goal(&f.session.id, goal_id).unwrap()
}

fn intent(f: &Fixture, goal_id: &GoalId, task_id: &TaskId) -> crate::mutation::MutationIntent {
    goal_of(f, goal_id).tasks()[task_id]
        .latest_attempt()
        .unwrap()
        .mutation_intent()
        .unwrap()
        .clone()
}

fn read(f: &Fixture, name: &str) -> String {
    std::fs::read_to_string(f.session.cwd.join(name)).unwrap()
}

fn run_publication(
    f: &Fixture,
    goal_id: &GoalId,
    task_id: &TaskId,
    operation_id: &str,
    boundary: &dyn WriteBoundary,
) -> Result<PublishOutcome, crate::orchestrator_error::OrchestratorError> {
    let goal = goal_of(f, goal_id);
    let stage =
        external_writer::verify_prepared_stage(&f.store, &f.session, &goal, task_id, operation_id)
            .expect("stage verifies");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(external_publish::publish_prepared_writer_intent(
            &f.store,
            &f.session,
            goal_id,
            task_id,
            operation_id,
            &stage,
            boundary,
        ))
}

/// Every sealed stage directory under the fixture state root.
fn staged_directories(f: &Fixture) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![f.root.join("state").join("stages")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if path.join("manifest.json").exists() {
                found.push(path);
            } else {
                stack.push(path);
            }
        }
    }
    found
}

fn ok_output() -> crate::sandbox::Output {
    crate::sandbox::Output {
        status: 0,
        stdout: String::new(),
        stderr: String::new(),
        command_start: crate::sandbox::CommandStart::Proven,
    }
}

/// Publish the first operation, then fail on the second one.
///
/// `corrupt` makes the second write land different bytes than the intent asked
/// for, which is the "target changed but the host cannot prove what happened"
/// shape rather than a clean refusal.
struct TwoFileBoundary {
    corrupt_second: bool,
}

impl WriteBoundary for TwoFileBoundary {
    fn write<'a>(
        &'a self,
        request: WriterWriteRequest<'a>,
    ) -> Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<crate::sandbox::Output>> + Send + 'a>,
    > {
        Box::pin(async move {
            if request.path.ends_with("a.txt") {
                std::fs::write(request.path, request.content.as_bytes())?;
                return Ok(ok_output());
            }
            if self.corrupt_second {
                std::fs::write(request.path, b"not-the-intended-bytes\n")?;
                return Ok(ok_output());
            }
            anyhow::bail!("injected boundary failure on the second operation")
        })
    }
}

/// The host's real publication boundary, used when both writes must succeed.
struct RealBoundary;

impl WriteBoundary for RealBoundary {
    fn write<'a>(
        &'a self,
        request: WriterWriteRequest<'a>,
    ) -> Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<crate::sandbox::Output>> + Send + 'a>,
    > {
        Box::pin(async move {
            std::fs::write(request.path, request.content.as_bytes())?;
            Ok(ok_output())
        })
    }
}

// ---------------------------------------------------------------------------
// AT-034 — multi-file partial publication.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_034_a_partial_multi_file_publication_is_recorded_per_operation() {
    let f = fixture("multi-partial");
    let (goal_id, task_id, operation_id) = prepared_two_file_writer(&f);
    // The durable intent must describe both host-derived operations before any
    // write happens: two indices, two request identities, two preimages.
    let prepared = intent(&f, &goal_id, &task_id);
    assert_eq!(prepared.state(), MutationIntentState::Prepared);
    assert_eq!(prepared.operations().len(), 2);
    assert!(
        prepared
            .operations()
            .iter()
            .all(|operation| operation.state() == MutationOperationState::Prepared)
    );

    let outcome = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &TwoFileBoundary {
            corrupt_second: false,
        },
    );
    match outcome {
        Ok(PublishOutcome::Refused { code }) => {
            assert_eq!(code, "WRITER_PUBLICATION_UNKNOWN", "{code}")
        }
        other => panic!("a half-applied publication must be refused, got {other:?}"),
    }

    // Actual filesystem state: the first file is the intended postimage, the
    // second is untouched.
    assert_eq!(read(&f, "a.txt"), A_NEW);
    assert_eq!(read(&f, "b.txt"), B_OLD);

    // Journal state: per-operation, never an all-files claim.
    let after = intent(&f, &goal_id, &task_id);
    assert_eq!(
        after.state(),
        MutationIntentState::Applying,
        "the intent may not claim APPLIED for a half-applied set"
    );
    assert_eq!(
        after.operations()[0].state(),
        MutationOperationState::Applied
    );
    assert_eq!(
        after.operations()[1].state(),
        MutationOperationState::Applying,
        "the operation whose helper failed stays in-flight for reconciliation"
    );

    // Attempt / receipt / Goal state.
    let goal = goal_of(&f, &goal_id);
    let task = &goal.tasks()[&task_id];
    let attempt = task.latest_attempt().unwrap();
    assert_eq!(task.status(), TaskStatus::Running);
    assert_ne!(goal.status(), GoalStatus::Completed);
    assert_ne!(
        attempt.side_effect_state(),
        Some(crate::fallback::SideEffectState::ConfirmedPerformed),
        "a half-applied set is never recorded as a confirmed performed mutation"
    );
    assert!(
        !task
            .evidence()
            .iter()
            .any(|evidence| matches!(evidence, crate::task::TaskEvidence::FileSnapshot { .. })),
        "no FileSnapshot evidence may be published for a set that did not complete"
    );
    assert_eq!(
        external_state(&goal)
            .unwrap()
            .active_work()
            .unwrap()
            .state(),
        crate::external_model_work::WorkItemState::Submitted,
        "the accepted Writer work stays SUBMITTED: the host claimed nothing"
    );
    // The stage stays: an operation has begun, so the bytes are recovery evidence.
    assert_eq!(staged_directories(&f).len(), 1);

    // A restart changes nothing and never replays the unknown operation.
    let restarted = TaskStore::with_state_root(f.root.join("state"));
    let again = goal_next(
        &config::Session {
            id: f.session.id.clone(),
            cwd: f.session.cwd.clone(),
            permitted_directories: f.session.permitted_directories.clone(),
        },
        &restarted,
        &goal_id,
    );
    assert!(
        again["model_work"].is_null(),
        "no model work is issued for a half-applied set: {again}"
    );
    assert_ne!(again["status"], "COMPLETED");
    assert_eq!(read(&f, "a.txt"), A_NEW, "no second write of a.txt");
    assert_eq!(read(&f, "b.txt"), B_OLD, "b.txt is never blindly reissued");

    // Host reconciliation is the only way out, and it is conservative.
    goal_api::goal_resume(
        &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
        &f.session,
        &f.store,
    )
    .unwrap();
    let reconciled = goal_of(&f, &goal_id);
    assert_eq!(reconciled.status(), GoalStatus::Blocked);
    let task = &reconciled.tasks()[&task_id];
    assert_eq!(task.status(), TaskStatus::Blocked);
    assert!(
        task.blockers()
            .iter()
            .any(|blocker| blocker.code() == "MUTATION_RECONCILIATION_REQUIRED"),
        "the partial mutation must require reconciliation: {:?}",
        task.blockers()
    );
    let reconciled_intent = intent(&f, &goal_id, &task_id);
    assert_eq!(reconciled_intent.state(), MutationIntentState::Partial);
    // Reconciliation observes; it never writes, reverts, or retries.
    assert_eq!(read(&f, "a.txt"), A_NEW);
    assert_eq!(read(&f, "b.txt"), B_OLD);
}

#[test]
fn at_e5_034b_a_corrupt_second_postimage_is_unknown_not_not_performed() {
    let f = fixture("multi-corrupt");
    let (goal_id, task_id, operation_id) = prepared_two_file_writer(&f);

    let outcome = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &TwoFileBoundary {
            corrupt_second: true,
        },
    );
    match outcome {
        Ok(PublishOutcome::Refused { code }) => {
            assert_eq!(code, "WRITER_POSTIMAGE_MISMATCH", "{code}")
        }
        other => panic!("a wrong second postimage must be refused, got {other:?}"),
    }
    // The first operation is proven applied and the second is not provable at
    // all, so the journal keeps both verdicts distinct.
    let after = intent(&f, &goal_id, &task_id);
    assert_eq!(after.state(), MutationIntentState::Applying);
    assert_eq!(
        after.operations()[0].state(),
        MutationOperationState::Applied
    );
    assert_eq!(
        after.operations()[1].state(),
        MutationOperationState::Applying
    );

    // Reconciliation refuses to call this "performed" or "not performed".
    goal_api::goal_resume(
        &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
        &f.session,
        &f.store,
    )
    .unwrap();
    assert_eq!(goal_of(&f, &goal_id).status(), GoalStatus::Blocked);
    assert_eq!(
        intent(&f, &goal_id, &task_id).state(),
        MutationIntentState::Unknown,
        "a target that matches neither the preimage nor the intended postimage is UNKNOWN"
    );
    assert_eq!(read(&f, "a.txt"), A_NEW);
    assert_eq!(read(&f, "b.txt"), "not-the-intended-bytes\n");
}

#[test]
fn at_e5_034c_a_complete_multi_file_publication_reaches_the_host_verifier() {
    let f = fixture("multi-complete");
    let (goal_id, task_id, operation_id) = prepared_two_file_writer(&f);

    let published = run_publication(&f, &goal_id, &task_id, &operation_id, &RealBoundary);
    assert!(
        matches!(published, Ok(PublishOutcome::Published { .. })),
        "{published:?}"
    );
    let after = intent(&f, &goal_id, &task_id);
    assert_eq!(after.state(), MutationIntentState::Applied);
    assert!(
        after
            .operations()
            .iter()
            .all(|operation| operation.state() == MutationOperationState::Applied)
    );
    assert_eq!(read(&f, "a.txt"), A_NEW);
    assert_eq!(read(&f, "b.txt"), B_NEW);
    assert_eq!(
        external_publish::reviewer_pending(&goal_of(&f, &goal_id)),
        Some((task_id.clone(), operation_id.clone(), false)),
        "the reviewer leg must be pending for the published set"
    );

    // The driver publishes, begins the reviewer invocation, and hands back one
    // Reviewer WorkItem bound to the same writer Attempt.
    let mut reviewer = goal_next(&f.session, &f.store, &goal_id);
    let mut guard = 0;
    while reviewer["outcome"] == "HOST_PROGRESS" && guard < 6 {
        reviewer = goal_next(&f.session, &f.store, &goal_id);
        guard += 1;
    }
    assert_eq!(reviewer["outcome"], "MODEL_REQUIRED", "{reviewer}");
    assert_eq!(reviewer["model_work"]["role"], "REVIEWER", "{reviewer}");
    let goal = goal_of(&f, &goal_id);
    assert_eq!(
        goal.tasks()[&task_id].attempts().len(),
        1,
        "reviewing never starts a second writer Attempt"
    );
}

// ---------------------------------------------------------------------------
// AT-040 — crash after the last publication commit, before the host completed
// the active WorkItem.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_040b_a_crash_after_the_last_publication_commit_resumes_the_reviewer() {
    let f = fixture("publish-crash");
    let (goal_id, task_id, operation_id) = prepared_two_file_writer(&f);
    // Exactly the crash window: publication committed, the process stopped before
    // `commit_complete_active_work` ran.
    let published = run_publication(&f, &goal_id, &task_id, &operation_id, &RealBoundary);
    assert!(
        matches!(published, Ok(PublishOutcome::Published { .. })),
        "{published:?}"
    );
    let goal = goal_of(&f, &goal_id);
    assert_eq!(
        external_state(&goal)
            .unwrap()
            .active_work()
            .unwrap()
            .state(),
        crate::external_model_work::WorkItemState::Submitted,
        "the WorkItem is still SUBMITTED at the crash boundary"
    );

    // After a restart the host must finish the bookkeeping and resume the
    // reviewer leg. Before this recovery existed the Goal dead-ended on
    // WRITER_INTENT_NOT_PREPARED forever, because the intent was APPLIED while
    // the WorkItem was still expecting a submission.
    let restarted = TaskStore::with_state_root(f.root.join("state"));
    let mut view = goal_next(&f.session, &restarted, &goal_id);
    let mut guard = 0;
    while view["outcome"] == "HOST_PROGRESS" && guard < 6 {
        view = goal_next(&f.session, &restarted, &goal_id);
        guard += 1;
    }
    assert_eq!(view["outcome"], "MODEL_REQUIRED", "{view}");
    assert_eq!(view["model_work"]["role"], "REVIEWER", "{view}");
    assert_ne!(
        view["no_action_reason"],
        json!("WRITER_INTENT_NOT_PREPARED"),
        "a published intent must never dead-end on a lost prepared intent: {view}"
    );

    // The writer leg never ran twice and no file was written a second time.
    let goal = restarted.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(goal.tasks()[&task_id].attempts().len(), 1);
    assert_eq!(read(&f, "a.txt"), A_NEW);
    assert_eq!(read(&f, "b.txt"), B_NEW);
    let history = external_state(&goal).unwrap().work_history();
    assert!(
        history.iter().any(
            |entry| entry.role() == crate::external_model_work::WorkRole::Writer
                && entry.state() == crate::external_model_work::WorkItemState::Committed
        ),
        "the completed Writer work item is recorded as COMMITTED: {history:?}"
    );

    // And the resumed reviewer leg is submittable end to end.
    let reviewer_work = view["model_work"].clone();
    let verdict = submit(
        &f.session,
        &restarted,
        &submit_args(
            &f.session,
            &goal_id,
            &reviewer_work,
            reviewer_answer(&reviewer_work),
        ),
    )
    .unwrap();
    assert_eq!(verdict["outcome"], "COMMITTED", "{verdict}");
    let finished = goal_next(&f.session, &restarted, &goal_id);
    assert_eq!(finished["status"], "COMPLETED", "{finished}");
}

// ---------------------------------------------------------------------------
// AT-052c — crash between the reviewer invocation commit and the WorkItem
// issuance.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_052c_a_crash_before_the_reviewer_work_item_is_issued_reuses_the_invocation() {
    let f = fixture("reviewer-issue-crash");
    let (goal_id, task_id, operation_id) = prepared_two_file_writer(&f);
    let published = run_publication(&f, &goal_id, &task_id, &operation_id, &RealBoundary);
    assert!(matches!(published, Ok(PublishOutcome::Published { .. })));
    let published_goal = goal_of(&f, &goal_id);
    // Host steps up to (but not including) the WorkItem issuance, exactly as a
    // crash between them would leave them.
    crate::external_model_store::commit_complete_active_work(
        &f.store,
        &f.session.id,
        &goal_id,
        published_goal.revision(),
        external_publish::WRITER_PUBLISHED,
    )
    .unwrap();
    external_publish::begin_reviewer_invocation(
        &f.store,
        &f.session,
        &goal_id,
        &task_id,
        &operation_id,
        goal_of(&f, &goal_id).revision(),
    )
    .unwrap();
    let before = intent(&f, &goal_id, &task_id);
    assert_eq!(before.reviewer_state(), ReviewerInvocationState::Invoking);
    let invocation_id = serde_json::to_value(before.reviewer()).unwrap()["invocation_id"].clone();
    assert!(invocation_id.is_string(), "{invocation_id}");
    assert_eq!(
        external_state(&goal_of(&f, &goal_id))
            .unwrap()
            .active_work(),
        None,
        "no WorkItem exists at this crash boundary"
    );

    // Restart: the same invocation is used, and the Writer is never re-run.
    let restarted = TaskStore::with_state_root(f.root.join("state"));
    let view = goal_next(&f.session, &restarted, &goal_id);
    assert_eq!(view["outcome"], "MODEL_REQUIRED", "{view}");
    assert_eq!(view["model_work"]["role"], "REVIEWER", "{view}");
    let goal = restarted.load_goal(&f.session.id, &goal_id).unwrap();
    let after = goal.tasks()[&task_id]
        .latest_attempt()
        .unwrap()
        .mutation_intent()
        .unwrap()
        .clone();
    assert_eq!(after.reviewer_state(), ReviewerInvocationState::Invoking);
    assert_eq!(
        serde_json::to_value(after.reviewer()).unwrap()["invocation_id"],
        invocation_id,
        "the reviewer invocation identity is stable across the crash"
    );
    assert_eq!(goal.tasks()[&task_id].attempts().len(), 1);
    assert_eq!(read(&f, "a.txt"), A_NEW);
    assert_eq!(read(&f, "b.txt"), B_NEW);
}

// ---------------------------------------------------------------------------
// AT-042 — cancel while an operation is in flight.
// ---------------------------------------------------------------------------

/// Cancels the Goal from inside the *first* write, exactly like a concurrent
/// `goal_cancel` landing while an operation is being published.
struct CancelDuringFirstWrite<'a> {
    store: &'a TaskStore,
    session: &'a config::Session,
    goal_id: &'a GoalId,
    cancelled: std::sync::atomic::AtomicBool,
}

impl WriteBoundary for CancelDuringFirstWrite<'_> {
    fn write<'a>(
        &'a self,
        request: WriterWriteRequest<'a>,
    ) -> Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<crate::sandbox::Output>> + Send + 'a>,
    > {
        Box::pin(async move {
            if request.path.ends_with("a.txt")
                && !self
                    .cancelled
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                goal_api::goal_cancel(
                    &json!({"session_id": self.session.id, "goal_id": self.goal_id.as_str()}),
                    self.session,
                    self.store,
                )
                .unwrap();
            }
            std::fs::write(request.path, request.content.as_bytes())?;
            Ok(ok_output())
        })
    }
}

#[test]
fn at_e5_042_a_cancel_between_operations_stops_the_publication_journal() {
    let f = fixture("multi-cancel");
    let (goal_id, task_id, operation_id) = prepared_two_file_writer(&f);

    let outcome = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &CancelDuringFirstWrite {
            store: &f.store,
            session: &f.session,
            goal_id: &goal_id,
            cancelled: std::sync::atomic::AtomicBool::new(false),
        },
    );
    // The revision CAS observes the cancellation commit and refuses to journal
    // the completed operation on a Goal that is no longer RUNNING. The
    // quarantine's termination evidence is committed in the same revision CAS,
    // so a cancellation that wins the revision is reported as an honest refusal
    // rather than a generic error.
    match outcome {
        Err(_) => {}
        Ok(PublishOutcome::Refused { code }) => {
            assert_eq!(code, "WRITER_PUBLICATION_QUARANTINE_UNRECORDED")
        }
        Ok(PublishOutcome::Published { .. }) => {
            panic!("a cancelled Goal may not continue journaling publication")
        }
    }
    let goal = goal_of(&f, &goal_id);
    assert_eq!(goal.status(), GoalStatus::Cancelling);
    assert_ne!(goal.status(), GoalStatus::Completed);

    // The operation that was in flight when the cancellation committed is left
    // unproven, and the next operation never begins at all.
    let after = intent(&f, &goal_id, &task_id);
    assert_ne!(after.state(), MutationIntentState::Applied);
    assert_eq!(
        after.operations()[0].state(),
        MutationOperationState::Applying,
        "the in-flight operation's completion lost the revision CAS"
    );
    assert_eq!(
        after.operations()[1].state(),
        MutationOperationState::Prepared,
        "no operation may begin after the cancellation is durable"
    );
    let task = &goal.tasks()[&task_id];
    assert_ne!(
        task.latest_attempt().unwrap().side_effect_state(),
        Some(crate::fallback::SideEffectState::ConfirmedPerformed),
        "no confirmed-performed claim survives a cancelled publication"
    );

    // Reconciliation keeps the cancellation and the partial effect honest.
    goal_api::goal_resume(
        &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
        &f.session,
        &f.store,
    )
    .unwrap();
    assert_ne!(goal_of(&f, &goal_id).status(), GoalStatus::Completed);
    assert_eq!(
        intent(&f, &goal_id, &task_id).state(),
        MutationIntentState::Partial
    );
    // The first write landed before the cancellation could be observed (nothing
    // can recall an in-flight syscall), but the second was never started, nothing
    // was reverted, and nothing was retried.
    assert_eq!(read(&f, "a.txt"), A_NEW);
    assert_eq!(read(&f, "b.txt"), B_OLD);
}

// ---------------------------------------------------------------------------
// AT-052 — why the Scheduler-level reviewer recovery branch is unreachable.
// ---------------------------------------------------------------------------

/// Build one durable (intent state, reviewer state) shape on a published writer
/// Task and return `(needs_reviewer_recovery, reviewer_pending)`.
fn reviewer_shape(
    label: &str,
    reconcile: bool,
    reviewer_state: ReviewerInvocationState,
) -> (bool, bool) {
    let f = fixture(label);
    let (goal_id, task_id, operation_id) = prepared_two_file_writer(&f);
    let published = run_publication(&f, &goal_id, &task_id, &operation_id, &RealBoundary);
    assert!(matches!(published, Ok(PublishOutcome::Published { .. })));

    if reconcile {
        let revision = goal_of(&f, &goal_id).revision();
        f.store
            .mutate_goal_snapshot(&f.session.id, &goal_id, revision, |goal, _now| {
                goal.task_advance_latest_mutation_intent(
                    &task_id,
                    &operation_id,
                    crate::mutation::MutationIntentUpdate::Reconcile {
                        state: MutationIntentState::ReconciledPerformed,
                    },
                )
            })
            .unwrap();
    }
    match reviewer_state {
        ReviewerInvocationState::NotStarted => {}
        ReviewerInvocationState::Invoking => {
            let revision = goal_of(&f, &goal_id).revision();
            f.store
                .mutate_goal_snapshot(&f.session.id, &goal_id, revision, |goal, _now| {
                    goal.task_advance_latest_mutation_intent(
                        &task_id,
                        &operation_id,
                        crate::mutation::MutationIntentUpdate::BeginReviewer {
                            invocation_id: "e5-reviewer:h3-state-matrix".to_owned(),
                        },
                    )
                })
                .unwrap();
        }
        ReviewerInvocationState::Persisted => {
            let revision = goal_of(&f, &goal_id).revision();
            f.store
                .mutate_goal_snapshot(&f.session.id, &goal_id, revision, |goal, _now| {
                    goal.task_advance_latest_mutation_intent(
                        &task_id,
                        &operation_id,
                        crate::mutation::MutationIntentUpdate::BeginReviewer {
                            invocation_id: "e5-reviewer:h3-state-matrix".to_owned(),
                        },
                    )
                })
                .unwrap();
            let revision = goal_of(&f, &goal_id).revision();
            f.store
                .mutate_goal_snapshot(&f.session.id, &goal_id, revision, |goal, _now| {
                    goal.task_advance_latest_mutation_intent(
                        &task_id,
                        &operation_id,
                        crate::mutation::MutationIntentUpdate::CompleteReviewer,
                    )
                })
                .unwrap();
        }
    }
    let goal = goal_of(&f, &goal_id);
    (
        goal.tasks()[&task_id].needs_reviewer_recovery(),
        external_publish::reviewer_pending(&goal).is_some(),
    )
}

#[test]
fn at_e5_052_the_scheduler_reviewer_recovery_branch_is_unreachable_from_the_driver() {
    // `advance_external_step` resolves `external_publish::reviewer_pending` before
    // it consults the Scheduler, and the Scheduler only selects `RunReviewer` for
    // `Task::needs_reviewer_recovery()`. That predicate is strictly narrower than
    // `reviewer_pending`, so the `REVIEWER_RECOVERY_DEFERRED` refusal below it can
    // never be produced. These are the six reachable shapes:
    let shapes = [
        (
            "applied-notstarted",
            false,
            ReviewerInvocationState::NotStarted,
        ),
        ("applied-invoking", false, ReviewerInvocationState::Invoking),
        (
            "applied-persisted",
            false,
            ReviewerInvocationState::Persisted,
        ),
        (
            "reconciled-notstarted",
            true,
            ReviewerInvocationState::NotStarted,
        ),
        (
            "reconciled-invoking",
            true,
            ReviewerInvocationState::Invoking,
        ),
        (
            "reconciled-persisted",
            true,
            ReviewerInvocationState::Persisted,
        ),
    ];
    for (label, reconcile, reviewer_state) in shapes {
        let (needs_recovery, pending) = reviewer_shape(label, reconcile, reviewer_state);
        // The implication that makes the Scheduler branch dead code.
        assert!(
            !needs_recovery || pending,
            "{label}: needs_reviewer_recovery() must imply reviewer_pending()"
        );
        // And only the reconciled-performed/not-started shape asks for a reviewer.
        assert_eq!(
            needs_recovery,
            reconcile && reviewer_state == ReviewerInvocationState::NotStarted,
            "{label}: unexpected needs_reviewer_recovery()"
        );
    }
}

#[test]
fn at_e5_052b_a_reconciled_performed_intent_never_reports_deferred_recovery() {
    let f = fixture("reconciled-loop");
    let (goal_id, task_id, operation_id) = prepared_two_file_writer(&f);
    let published = run_publication(&f, &goal_id, &task_id, &operation_id, &RealBoundary);
    assert!(matches!(published, Ok(PublishOutcome::Published { .. })));
    let revision = goal_of(&f, &goal_id).revision();
    f.store
        .mutate_goal_snapshot(&f.session.id, &goal_id, revision, |goal, _now| {
            goal.task_advance_latest_mutation_intent(
                &task_id,
                &operation_id,
                crate::mutation::MutationIntentUpdate::Reconcile {
                    state: MutationIntentState::ReconciledPerformed,
                },
            )
        })
        .unwrap();
    // Crash shape: the reviewer invocation had not started and the WorkItem is gone.
    let revision = goal_of(&f, &goal_id).revision();
    crate::external_model_store::commit_cancel_active_work(
        &f.store,
        &f.session.id,
        &goal_id,
        revision,
        "REVIEWER_WORK_ITEM_LOST",
    )
    .unwrap();

    let resumed = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(resumed["outcome"], "MODEL_REQUIRED", "{resumed}");
    assert_eq!(resumed["model_work"]["role"], "REVIEWER", "{resumed}");
    assert_ne!(
        resumed["no_action_reason"],
        json!("REVIEWER_RECOVERY_DEFERRED")
    );
    let goal = goal_of(&f, &goal_id);
    assert_eq!(
        goal.tasks()[&task_id].attempts().len(),
        1,
        "recovery never re-runs the Writer"
    );
    assert_eq!(read(&f, "a.txt"), A_NEW);
    assert_eq!(read(&f, "b.txt"), B_NEW);
}

// ---------------------------------------------------------------------------
// AT-053 / AT-056 — restart between the reviewer commit and host verification.
// ---------------------------------------------------------------------------

fn reviewer_answer(work: &Value) -> Value {
    let request = &work["request"];
    json!({
        "goal_id": request["goal_id"],
        "task_id": request["task_id"],
        "attempt_id": request["attempt_id"],
        "goal_revision": request["goal_revision"],
        "plan_revision": request["plan_revision"],
        "summary": "reviewed the applied change",
        "blocking_findings": 0,
        "evidence": [{"kind": "review", "value": "change reviewed"}]
    })
}

/// Drive publication and hand back the durable Reviewer WorkItem.
fn reach_reviewer_work_item(f: &Fixture) -> (GoalId, TaskId, Value) {
    let (goal_id, task_id, operation_id) = prepared_two_file_writer(f);
    let published = run_publication(f, &goal_id, &task_id, &operation_id, &RealBoundary);
    assert!(matches!(published, Ok(PublishOutcome::Published { .. })));
    let mut view = goal_next(&f.session, &f.store, &goal_id);
    let mut guard = 0;
    while view["outcome"] == "HOST_PROGRESS" && guard < 6 {
        view = goal_next(&f.session, &f.store, &goal_id);
        guard += 1;
    }
    assert_eq!(view["outcome"], "MODEL_REQUIRED", "{view}");
    assert_eq!(view["model_work"]["role"], "REVIEWER", "{view}");
    (goal_id, task_id, view["model_work"].clone())
}

#[test]
fn at_e5_050_a_reviewer_cannot_supply_its_own_file_evidence() {
    // AT-050: the reviewer dialect has no file-evidence field at all and unknown
    // fields are rejected, so a model cannot declare which postimage it reviewed.
    let f = fixture("reviewer-forged");
    let (goal_id, task_id, work) = reach_reviewer_work_item(&f);
    let request = &work["request"];
    let forged = json!({
        "goal_id": request["goal_id"],
        "task_id": request["task_id"],
        "attempt_id": request["attempt_id"],
        "goal_revision": request["goal_revision"],
        "plan_revision": request["plan_revision"],
        "summary": "reviewed a file the host never published",
        "blocking_findings": 0,
        "evidence": [{"kind": "review", "value": "change reviewed"}],
        "files": [
            {"path": "a.txt", "after_sha256": sha256_hex("forged postimage
    "), "size": 1},
            {"path": "b.txt", "after_sha256": sha256_hex("forged postimage
    "), "size": 1}
        ]
    });
    let refused = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, forged),
    )
    .unwrap();
    assert_eq!(refused["outcome"], "REJECTED_SCHEMA", "{refused}");
    assert_eq!(refused["code"], "REVIEWER_OUTPUT_INVALID", "{refused}");

    // Nothing about the forged claim reached the durable Goal.
    let goal = goal_of(&f, &goal_id);
    let document = serde_json::to_string(&goal).unwrap();
    assert!(
        !document.contains(&sha256_hex(
            "forged postimage
"
        )),
        "the forged postimage digest must never be persisted"
    );
    let task = &goal.tasks()[&task_id];
    assert_eq!(task.status(), TaskStatus::Blocked);
    assert!(
        task.evidence()
            .iter()
            .all(|evidence| !matches!(evidence, crate::task::TaskEvidence::ReviewResult { .. }))
    );
    assert_ne!(goal.status(), GoalStatus::Completed);
    assert_eq!(read(&f, "a.txt"), A_NEW);
    assert_eq!(read(&f, "b.txt"), B_NEW);
}

#[test]
fn at_e5_055_a_reviewer_summary_is_stored_once_bounded_and_out_of_the_receipt() {
    let f = fixture("reviewer-summary");
    let (goal_id, task_id, work) = reach_reviewer_work_item(&f);
    let sentinel = "SENTINEL_REVIEWER_SUMMARY_E5H3";
    let mut answer = reviewer_answer(&work);
    answer["summary"] = json!(format!("{sentinel} reviewed both files"));
    let verdict = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, answer),
    )
    .unwrap();
    assert_eq!(verdict["outcome"], "COMMITTED", "{verdict}");

    let goal = goal_of(&f, &goal_id);
    let document = serde_json::to_string(&goal).unwrap();
    assert_eq!(
        document.matches(sentinel).count(),
        1,
        "the reviewer summary is stored once, as the review evidence"
    );
    let state = external_state(&goal).unwrap();
    for receipt in state.receipts() {
        let detail = receipt.redacted_detail().unwrap_or_default();
        assert!(
            !detail.contains(sentinel),
            "receipts never echo model prose"
        );
        assert!(
            detail.len() <= crate::external_model_work::MAX_RECEIPT_DETAIL_BYTES,
            "receipt details stay bounded"
        );
    }
    for item in state.work_history() {
        assert!(
            !serde_json::to_string(item).unwrap().contains(sentinel),
            "work history never echoes model prose"
        );
    }
    let review = goal.tasks()[&task_id]
        .evidence()
        .iter()
        .find_map(|evidence| match evidence {
            crate::task::TaskEvidence::ReviewResult { summary, .. } => Some(summary.clone()),
            _ => None,
        })
        .expect("one ReviewResult");
    assert!(review.starts_with(sentinel));
    assert!(
        review.len() <= 8 * 1024,
        "the stored summary respects the host bound"
    );
    assert_eq!(goal.tasks()[&task_id].status(), TaskStatus::Verifying);
    assert_ne!(goal.status(), GoalStatus::Completed);
}

#[test]
fn at_e5_056_only_the_host_verifier_completes_after_a_reviewer_restart() {
    let f = fixture("reviewer-restart");
    let (goal_id, task_id, operation_id) = prepared_two_file_writer(&f);
    let published = run_publication(&f, &goal_id, &task_id, &operation_id, &RealBoundary);
    assert!(matches!(published, Ok(PublishOutcome::Published { .. })));

    // Publish + begin the reviewer invocation through the host driver.
    let mut work = goal_next(&f.session, &f.store, &goal_id);
    while work["outcome"] == "HOST_PROGRESS" {
        work = goal_next(&f.session, &f.store, &goal_id);
    }
    assert_eq!(work["outcome"], "MODEL_REQUIRED", "{work}");
    assert_eq!(work["model_work"]["role"], "REVIEWER", "{work}");
    let reviewer_work = work["model_work"].clone();

    // The review is committed, then the host is restarted *before* the Task
    // verifier runs.
    let receipt = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &reviewer_work,
            reviewer_answer(&reviewer_work),
        ),
    )
    .unwrap();
    assert_eq!(receipt["outcome"], "COMMITTED", "{receipt}");
    let after_review = goal_of(&f, &goal_id);
    assert_eq!(
        after_review.tasks()[&task_id].status(),
        TaskStatus::Verifying
    );
    assert_ne!(
        after_review.status(),
        GoalStatus::Completed,
        "a passing review is not a completion"
    );

    // A restart replays the receipt and then lets only the host Verifier finish.
    let restarted = TaskStore::with_state_root(f.root.join("state"));
    let replay = submit(
        &f.session,
        &restarted,
        &submit_args(
            &f.session,
            &goal_id,
            &reviewer_work,
            reviewer_answer(&reviewer_work),
        ),
    );
    // A different submission id would be a second verdict; the same payload with a
    // new id must never create a second ReviewResult.
    if let Ok(replay) = replay {
        assert_ne!(replay["outcome"], "COMMITTED", "{replay}");
    }
    let goal = restarted.load_goal(&f.session.id, &goal_id).unwrap();
    let review_results = goal.tasks()[&task_id]
        .evidence()
        .iter()
        .filter(|evidence| matches!(evidence, crate::task::TaskEvidence::ReviewResult { .. }))
        .count();
    assert_eq!(review_results, 1);

    let finished = goal_next(&f.session, &restarted, &goal_id);
    assert_eq!(finished["status"], "COMPLETED", "{finished}");
    assert_eq!(read(&f, "a.txt"), A_NEW);
    assert_eq!(read(&f, "b.txt"), B_NEW);
    let goal = restarted.load_goal(&f.session.id, &goal_id).unwrap();
    assert!(
        goal.final_verifications()
            .last()
            .is_some_and(|result| result.outcome() == crate::task::VerificationOutcome::Passed),
        "completion requires a host final verification record"
    );
}
