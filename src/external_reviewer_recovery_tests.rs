//! E5-B2 — Reviewer recovery (AT-E5-052..055).
//!
//! The reviewer leg must be resumable after a restart without ever re-running the
//! Writer, and it must refuse to review a postimage the host no longer observes.
//! `REVIEWER_RECOVERY_DEFERRED` is gone: both the normal `Applied` state and the
//! reconciled-performed recovery shape resume by beginning (or reusing) the same
//! reviewer invocation and issuing the same WorkItem.

use std::path::PathBuf;

use serde_json::{Value, json};
use sha2::Digest;
use uuid::Uuid;

use crate::config;
use crate::external_goal_api;
use crate::external_model_store::external_state;
use crate::external_publish;
use crate::goal::{Goal, GoalId, GoalStatus};
use crate::goal_api;
use crate::mutation::{MutationIntentState, MutationIntentUpdate, ReviewerInvocationState};
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
    let root = std::env::temp_dir().join(format!("local-mcp-e5b2-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(workspace.join("code.txt"), b"fn main() {}\n").unwrap();
    let session = config::Session {
        id: format!("e5b2-{label}-{}", Uuid::new_v4()),
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

fn reviewed_plan(work: &Value, content: &str) -> Value {
    let criterion_id = work["request"]["completion_criteria"][0]["criterion_id"]
        .as_str()
        .unwrap();
    json!({
        "goal_id": work["goal_id"],
        "goal_revision": work["goal_revision"],
        "summary": "one reviewed writer edit",
        "tasks": [{
            "proposal_id": "edit",
            "title": "Update code.txt",
            "objective": "Apply the requested scoped edit",
            "mandatory": true,
            "worker": "CODEX_WRITER",
            "dependencies": [],
            "scope": {
                "allowed_paths": ["code.txt"],
                "forbidden_paths": [],
                "operation_kind": "LOCAL_MUTATION",
                "replay_safety": "VERIFY_BEFORE_RETRY"
            },
            "verification": [
                {"kind": "FILE_DIGEST", "path": "code.txt", "expected_sha256": sha256_hex(content)},
                {"kind": "REVIEW_GATE", "max_blocking_findings": 0}
            ]
        }],
        "criterion_bindings": [{"criterion_id": criterion_id, "task_refs": ["edit"]}]
    })
}

fn writer_answer(work: &Value, content: &str) -> Value {
    let request = &work["request"];
    json!({
        "goal_id": request["goal_id"],
        "task_id": request["task_id"],
        "attempt_id": request["attempt_id"],
        "goal_revision": request["goal_revision"],
        "plan_revision": request["plan_revision"],
        "status": "candidate_complete",
        "summary": "replace the file",
        "evidence": [{"kind": "writer-evidence", "value": "one file replaced"}],
        "proposed_operations": [{
            "kind": "WRITE_UTF8",
            "path": "code.txt",
            "expected_preimage": {"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")},
            "content": content
        }]
    })
}

fn reviewer_answer(work: &Value, blocking_findings: u32) -> Value {
    let request = &work["request"];
    json!({
        "goal_id": request["goal_id"],
        "task_id": request["task_id"],
        "attempt_id": request["attempt_id"],
        "goal_revision": request["goal_revision"],
        "plan_revision": request["plan_revision"],
        "summary": "reviewed the applied change",
        "blocking_findings": blocking_findings,
        "evidence": [{"kind": "review", "value": "change reviewed"}]
    })
}

/// Drive to a published writer intent (reviewer invocation begun, WorkItem issued).
fn published_writer(f: &Fixture, content: &str) -> (GoalId, TaskId, Value) {
    let view = goal_api::goal_start(
        &json!({
            "session_id": f.session.id,
            "objective": "Apply one reviewed writer change",
            "model_backend": "EXTERNAL",
        }),
        &f.session,
        &f.store,
    )
    .unwrap();
    let view = serde_json::to_value(view).unwrap();
    let goal_id = GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap();
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            reviewed_plan(&planner_work, content),
        ),
    )
    .unwrap();
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    assert_eq!(work["role"], "WRITER");
    let staged = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, writer_answer(&work, content)),
    )
    .unwrap();
    assert_eq!(
        staged["outcome"], "ACCEPTED_FOR_HOST_PROCESSING",
        "{staged}"
    );
    let reviewer = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(reviewer["outcome"], "MODEL_REQUIRED", "{reviewer}");
    assert_eq!(reviewer["model_work"]["role"], "REVIEWER");
    let task_id = TaskId::parse(reviewer["model_work"]["task_id"].as_str().unwrap()).unwrap();
    (goal_id, task_id, reviewer["model_work"].clone())
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

fn file_content(f: &Fixture) -> String {
    std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap()
}

// ---------------------------------------------------------------------------
// AT-052 — the reviewer WorkItem survives a restart with the same identity.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_052_a_restart_redelivers_the_same_reviewer_work_item() {
    let f = fixture("restart");
    let content = "fn main() { /* reviewed once */ }\n";
    let (goal_id, task_id, work) = published_writer(&f, content);
    let work_id = work["work_id"].as_str().unwrap().to_owned();
    let digest = work["request_digest"].as_str().unwrap().to_owned();

    let restarted = TaskStore::with_state_root(f.root.join("state"));
    let again = goal_next(&f.session, &restarted, &goal_id);
    assert_eq!(again["outcome"], "MODEL_REQUIRED", "{again}");
    assert_eq!(again["model_work"]["work_id"].as_str().unwrap(), work_id);
    assert_eq!(
        again["model_work"]["request_digest"].as_str().unwrap(),
        digest
    );
    assert_eq!(again["model_work"]["role"], "REVIEWER");
    assert_eq!(again["host_steps_executed"], 0);

    let goal = restarted.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(goal.tasks()[&task_id].attempts().len(), 1);
    assert_eq!(
        goal.tasks()[&task_id].status(),
        TaskStatus::Running,
        "the writer leg is not re-run"
    );
    assert_eq!(
        intent(&f, &goal_id, &task_id).reviewer_state(),
        ReviewerInvocationState::Invoking
    );
    assert_eq!(file_content(&f), content);
}

// ---------------------------------------------------------------------------
// AT-053 — duplicate reviewer submissions produce one receipt and one result.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_053_a_duplicate_reviewer_submission_records_one_result() {
    let f = fixture("duplicate");
    let content = "fn main() { /* duplicate review */ }\n";
    let (goal_id, task_id, work) = published_writer(&f, content);
    let submission = Uuid::new_v4().to_string();
    let mut args = submit_args(&f.session, &goal_id, &work, reviewer_answer(&work, 0));
    args["submission_id"] = json!(submission);
    let first = submit(&f.session, &f.store, &args).unwrap();
    assert_eq!(first["outcome"], "COMMITTED", "{first}");

    // Replay after a restart: served from the receipt, no second ReviewResult.
    let restarted = TaskStore::with_state_root(f.root.join("state"));
    let replay = submit(&f.session, &restarted, &args).unwrap();
    assert_eq!(
        replay["outcome"], "REPLAY_OF_ACCEPTED_SUBMISSION",
        "{replay}"
    );
    let goal = restarted.load_goal(&f.session.id, &goal_id).unwrap();
    let task = &goal.tasks()[&task_id];
    let review_results = task
        .evidence()
        .iter()
        .filter(|evidence| matches!(evidence, crate::task::TaskEvidence::ReviewResult { .. }))
        .count();
    assert_eq!(review_results, 1);
    assert_eq!(
        intent(&f, &goal_id, &task_id).reviewer_state(),
        ReviewerInvocationState::Persisted
    );
    assert_eq!(task.status(), TaskStatus::Verifying);
}

// ---------------------------------------------------------------------------
// AT-054 — a rejected review never re-runs the Writer.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_054_a_rejected_review_never_reruns_the_writer() {
    let f = fixture("rejected");
    let content = "fn main() { /* rejected */ }\n";
    let (goal_id, task_id, work) = published_writer(&f, content);
    let rejected = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, reviewer_answer(&work, 3)),
    )
    .unwrap();
    assert_eq!(rejected["outcome"], "REJECTED_SCHEMA", "{rejected}");
    assert_eq!(rejected["code"], "REVIEW_GATE_BLOCKED");

    let restarted = TaskStore::with_state_root(f.root.join("state"));
    let goal = restarted.load_goal(&f.session.id, &goal_id).unwrap();
    let task = &goal.tasks()[&task_id];
    assert_eq!(task.status(), TaskStatus::Blocked);
    assert_eq!(task.attempts().len(), 1, "no second Writer Attempt");
    assert_ne!(goal.status(), GoalStatus::Completed);

    // A restart does not ask the client to publish or write again.
    let after = goal_next(&f.session, &restarted, &goal_id);
    assert_ne!(after["outcome"], "MODEL_REQUIRED", "{after}");
    assert!(after["model_work"].is_null(), "{after}");
    assert_eq!(file_content(&f), content);
}

// ---------------------------------------------------------------------------
// AT-055 — a changed postimage is never put under review.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_055_a_changed_postimage_is_not_put_under_review() {
    let f = fixture("stale-issue");
    let content = "fn main() { /* published */ }\n";
    let (goal_id, task_id, work) = published_writer(&f, content);
    // The reviewer WorkItem exists; a concurrent edit invalidates its target.
    std::fs::write(
        f.session.cwd.join("code.txt"),
        b"fn main() { /* edited */ }\n",
    )
    .unwrap();

    // Issuing another review for this intent must refuse rather than ask for a
    // review of bytes the host did not publish.
    let request =
        crate::writer::build_external_reviewer_request(&goal_of(&f, &goal_id), &task_id).unwrap();
    assert!(crate::writer::reviewer_request_postimage_is_stale(&request));

    let submitted = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, reviewer_answer(&work, 0)),
    )
    .unwrap();
    assert_eq!(submitted["outcome"], "REJECTED_SCHEMA", "{submitted}");
    assert_eq!(submitted["code"], "REVIEWER_STALE_POSTIMAGE");
    let goal = goal_of(&f, &goal_id);
    assert_eq!(goal.tasks()[&task_id].status(), TaskStatus::Blocked);
    assert_ne!(goal.status(), GoalStatus::Completed);
}

// ---------------------------------------------------------------------------
// Recovery shape: a reconciled-performed intent resumes the reviewer.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_052b_a_reconciled_performed_intent_resumes_the_reviewer_leg() {
    let f = fixture("reconciled");
    let content = "fn main() { /* reconciled */ }\n";
    let (goal_id, task_id, _work) = published_writer(&f, content);
    let operation_id = intent(&f, &goal_id, &task_id).operation_id().to_owned();

    // Put the journal into the recovery shape the Scheduler recognises
    // (RECONCILED_PERFORMED with the reviewer not yet started) and drop the
    // reviewer WorkItem, as a crash after reconciliation would leave it.
    let revision = goal_of(&f, &goal_id).revision();
    f.store
        .mutate_goal_snapshot(&f.session.id, &goal_id, revision, |goal, _now| {
            goal.task_advance_latest_mutation_intent(
                &task_id,
                &operation_id,
                MutationIntentUpdate::Reconcile {
                    state: MutationIntentState::ReconciledPerformed,
                },
            )
        })
        .unwrap();
    let goal = goal_of(&f, &goal_id);
    let revision = goal.revision();
    crate::external_model_store::commit_cancel_active_work(
        &f.store,
        &f.session.id,
        &goal_id,
        revision,
        "REVIEWER_WORK_ITEM_LOST",
    )
    .unwrap();

    // `goal_next` must resume the reviewer leg instead of reporting
    // REVIEWER_RECOVERY_DEFERRED.
    let resumed = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(resumed["outcome"], "MODEL_REQUIRED", "{resumed}");
    assert_eq!(resumed["model_work"]["role"], "REVIEWER", "{resumed}");
    assert_ne!(
        resumed["no_action_reason"],
        json!("REVIEWER_RECOVERY_DEFERRED")
    );
    let goal = goal_of(&f, &goal_id);
    assert_eq!(goal.tasks()[&task_id].attempts().len(), 1);
    assert_eq!(file_content(&f), content);

    // And the resumed WorkItem is submittable end to end.
    let verdict = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &resumed["model_work"],
            reviewer_answer(&resumed["model_work"], 0),
        ),
    )
    .unwrap();
    assert_eq!(verdict["outcome"], "COMMITTED", "{verdict}");
    let finished = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(finished["status"], "COMPLETED", "{finished}");
    assert_eq!(
        external_state(&goal_of(&f, &goal_id))
            .unwrap()
            .active_work(),
        None
    );
}

#[test]
fn at_e5_052c_a_reviewer_work_item_is_never_issued_for_an_unpublished_intent() {
    // AT-046: before the host published, no reviewer WorkItem may exist.
    let f = fixture("before-publish");
    let content = "fn main() { /* staged only */ }\n";
    let view = goal_api::goal_start(
        &json!({
            "session_id": f.session.id,
            "objective": "Apply one reviewed writer change",
            "model_backend": "EXTERNAL",
        }),
        &f.session,
        &f.store,
    )
    .unwrap();
    let view = serde_json::to_value(view).unwrap();
    let goal_id = GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap();
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            reviewed_plan(&planner_work, content),
        ),
    )
    .unwrap();
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, writer_answer(&work, content)),
    )
    .unwrap();

    let goal = goal_of(&f, &goal_id);
    let task_id = goal.tasks().keys().next().unwrap().clone();
    // The reviewed plan exists and the writer intent is prepared, but publication
    // has not happened: no reviewer request can be built for it.
    assert_eq!(
        intent(&f, &goal_id, &task_id).state(),
        MutationIntentState::Prepared
    );
    assert!(crate::writer::build_external_reviewer_request(&goal, &task_id).is_err());
    assert_eq!(external_publish::reviewer_pending(&goal), None);
}
