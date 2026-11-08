//! E5-A failure matrix (AT-E5-032, 035–043, 045).
//!
//! Publication is the only place the External backend touches a file, so every
//! failure window is pinned here with a deterministic injection instead of a
//! timing race:
//!
//! * a failing `WriteBoundary` (before the write, after the write, or writing the
//!   wrong bytes);
//! * a failing durable commit at an exact step (`FaultPoint::BeforeReplaceOnCommit`)
//!   so "published but not journaled" and "journaled but not published" are both
//!   observable;
//! * post-preparation cancellation, and a path that changed between staging and
//!   publication.
//!
//! Invariants under test: an unknown side effect is never treated as not
//! performed, an operation is never published twice, and a Goal is never reported
//! as applied or completed on the strength of a failed publication.

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
use crate::mutation::{MutationIntentState, MutationOperationState};
use crate::task::TaskId;
use crate::task_store::{FaultPoint, TaskStore};
use crate::writer::{WriteBoundary, WriterWriteRequest};

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
    fixture_with_state(label, None)
}

fn fixture_with_state(label: &str, fault: Option<FaultPoint>) -> Fixture {
    let root = std::env::temp_dir().join(format!("local-mcp-e5f-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(workspace.join("code.txt"), b"fn main() {}\n").unwrap();
    let session = config::Session {
        id: format!("e5f-{label}-{}", Uuid::new_v4()),
        cwd: workspace.clone(),
        permitted_directories: vec![workspace],
    };
    let store = match fault {
        Some(fault) => TaskStore::with_fault(state, fault),
        None => TaskStore::with_state_root(state),
    };
    Fixture {
        root,
        session,
        store,
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

fn writer_plan(work: &Value, expected: &str) -> Value {
    let criterion_id = work["request"]["completion_criteria"][0]["criterion_id"]
        .as_str()
        .unwrap();
    json!({
        "goal_id": work["goal_id"],
        "goal_revision": work["goal_revision"],
        "summary": "one scoped writer edit",
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
                {"kind": "FILE_DIGEST", "path": "code.txt", "expected_sha256": sha256_hex(expected)}
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

/// Drive the loop to a durably prepared Writer intent.
fn prepared_writer(f: &Fixture, content: &str) -> (GoalId, TaskId, String) {
    let view = goal_api::goal_start(
        &json!({
            "session_id": f.session.id,
            "objective": "Apply one scoped writer change",
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
            writer_plan(&planner_work, "fn main() {}\n"),
        ),
    )
    .unwrap();
    assert_eq!(planned["outcome"], "COMMITTED", "{planned}");
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
    let task_id = TaskId::parse(work["task_id"].as_str().unwrap()).unwrap();
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
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

fn run_publication(
    f: &Fixture,
    goal_id: &GoalId,
    task_id: &TaskId,
    operation_id: &str,
    boundary: &dyn WriteBoundary,
) -> Result<PublishOutcome, crate::orchestrator_error::OrchestratorError> {
    run_publication_with(f, &f.store, goal_id, task_id, operation_id, boundary)
}

fn run_publication_with(
    f: &Fixture,
    store: &TaskStore,
    goal_id: &GoalId,
    task_id: &TaskId,
    operation_id: &str,
    boundary: &dyn WriteBoundary,
) -> Result<PublishOutcome, crate::orchestrator_error::OrchestratorError> {
    let goal = store.load_goal(&f.session.id, goal_id)?;
    let stage =
        external_writer::verify_prepared_stage(store, &f.session, &goal, task_id, operation_id)
            .expect("stage verifies");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(external_publish::publish_prepared_writer_intent(
            store,
            &f.session,
            goal_id,
            task_id,
            operation_id,
            &stage,
            boundary,
        ))
}

/// Every sealed stage directory (one that has a manifest) under the state root.
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

fn file_content(f: &Fixture) -> String {
    std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap()
}

fn intent(f: &Fixture, goal_id: &GoalId, task_id: &TaskId) -> crate::mutation::MutationIntent {
    goal_of(f, goal_id).tasks()[task_id]
        .latest_attempt()
        .unwrap()
        .mutation_intent()
        .unwrap()
        .clone()
}

// ---------------------------------------------------------------------------
// Write-boundary injections (AT-037, 038, 039).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum BoundaryFault {
    /// Refuse before touching the file (the helper could not run).
    FailBeforeWrite,
    /// Write the file, then report failure (the outcome is genuinely unknown).
    WriteThenFail,
    /// Write different bytes than the intent (postimage mismatch).
    WriteWrongContent,
}

struct FaultyBoundary {
    fault: BoundaryFault,
}

impl WriteBoundary for FaultyBoundary {
    fn write<'a>(
        &'a self,
        request: WriterWriteRequest<'a>,
    ) -> Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<crate::sandbox::Output>> + Send + 'a>,
    > {
        Box::pin(async move {
            match self.fault {
                BoundaryFault::FailBeforeWrite => anyhow::bail!("injected boundary failure"),
                BoundaryFault::WriteThenFail => {
                    std::fs::write(request.path, request.content.as_bytes())?;
                    anyhow::bail!("injected unknown publication outcome")
                }
                BoundaryFault::WriteWrongContent => {
                    std::fs::write(request.path, b"unexpected bytes\n")?;
                    Ok(crate::sandbox::Output {
                        status: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                        command_start: crate::sandbox::CommandStart::Proven,
                    })
                }
            }
        })
    }
}

#[test]
fn at_e5_037_a_boundary_failure_before_the_write_is_unknown_and_never_republished() {
    let f = fixture("boundary-before");
    let (goal_id, task_id, operation_id) = prepared_writer(&f, "fn main() { /* new */ }\n");
    let before = file_content(&f);

    let error = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &FaultyBoundary {
            fault: BoundaryFault::FailBeforeWrite,
        },
    );
    match error {
        Err(error) => assert!(
            !error.to_string().is_empty(),
            "a boundary error must surface, not be reported as success"
        ),
        Ok(PublishOutcome::Refused { code }) => {
            assert_eq!(code, "WRITER_PUBLICATION_UNKNOWN", "{code}")
        }
        Ok(PublishOutcome::Published { .. }) => panic!("a boundary failure cannot be a publish"),
    }
    // The file is untouched and the journal is left APPLYING (BeginOperation was
    // recorded before the helper ran), so recovery owns the decision.
    assert_eq!(file_content(&f), before);
    let intent = intent(&f, &goal_id, &task_id);
    assert_eq!(intent.state(), MutationIntentState::Applying);
    assert_eq!(
        intent.operations()[0].state(),
        MutationOperationState::Applying
    );

    // A second publication attempt must not blindly replay the unknown operation.
    let second = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &FaultyBoundary {
            fault: BoundaryFault::WriteThenFail,
        },
    );
    match second {
        Ok(PublishOutcome::Refused { code }) => assert_eq!(code, "WRITER_INTENT_NOT_PREPARED"),
        other => panic!("an APPLYING intent must not be republished: {other:?}"),
    }
    assert_eq!(file_content(&f), before);
}

#[test]
fn at_e5_038_a_write_with_an_unknown_result_is_not_treated_as_not_performed() {
    let f = fixture("boundary-unknown");
    let content = "fn main() { /* unknown */ }\n";
    let (goal_id, task_id, operation_id) = prepared_writer(&f, content);

    let outcome = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &FaultyBoundary {
            fault: BoundaryFault::WriteThenFail,
        },
    );
    assert!(
        !matches!(outcome, Ok(PublishOutcome::Published { .. })),
        "an unknown outcome must never be reported as published"
    );
    // The bytes did land, but the host refuses to claim them: the operation stays
    // APPLYING for reconciliation, never COMPLETE.
    assert_eq!(file_content(&f), content);
    let intent = intent(&f, &goal_id, &task_id);
    assert_eq!(intent.state(), MutationIntentState::Applying);
    assert_eq!(
        intent.operations()[0].state(),
        MutationOperationState::Applying,
        "no CompleteOperation may be journaled for an unproven publication"
    );
    // And a retry cannot publish a second time.
    let retry = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &FaultyBoundary {
            fault: BoundaryFault::WriteThenFail,
        },
    );
    match retry {
        Ok(PublishOutcome::Refused { code }) => assert_eq!(code, "WRITER_INTENT_NOT_PREPARED"),
        other => panic!("an unknown operation must not be replayed: {other:?}"),
    }
    assert_eq!(file_content(&f), content, "no second write happened");
}

#[test]
fn at_e5_039_a_postimage_mismatch_never_completes_the_operation() {
    let f = fixture("postimage");
    let (goal_id, task_id, operation_id) = prepared_writer(&f, "fn main() { /* intended */ }\n");

    let outcome = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &FaultyBoundary {
            fault: BoundaryFault::WriteWrongContent,
        },
    );
    match outcome {
        Ok(PublishOutcome::Refused { code }) => {
            assert_eq!(code, "WRITER_POSTIMAGE_MISMATCH", "{code}")
        }
        other => panic!("a wrong postimage must be refused, got {other:?}"),
    }
    let intent = intent(&f, &goal_id, &task_id);
    assert_eq!(intent.state(), MutationIntentState::Applying);
    assert_eq!(
        intent.operations()[0].state(),
        MutationOperationState::Applying
    );
    // The WorkItem is still SUBMITTED: the host did not claim the change.
    let goal = goal_of(&f, &goal_id);
    assert_eq!(
        external_state(&goal)
            .unwrap()
            .active_work()
            .unwrap()
            .state(),
        crate::external_model_work::WorkItemState::Submitted
    );
}

// ---------------------------------------------------------------------------
// Durable-commit injections (AT-035, 039b).
// ---------------------------------------------------------------------------

#[test]
fn at_e5_035_a_failed_intent_commit_leaves_no_intent_receipt_or_stage() {
    // The stage is written first, then the commit fails: nothing is linked.
    let f = fixture_with_state("intent-commit", Some(FaultPoint::BeforeReplaceOnCommit(2)));
    let content = "fn main() { /* unlinked */ }\n";
    let view = goal_api::goal_start(
        &json!({
            "session_id": f.session.id,
            "objective": "Apply one scoped writer change",
            "model_backend": "EXTERNAL",
        }),
        &f.session,
        &f.store,
    );
    // goal_start itself commits (attempt 1). Everything after it is also faulted by
    // a per-commit fault only on attempt 2, so drive the loop with a clean store and
    // inject the fault exactly at the writer commit instead.
    let view = serde_json::to_value(view.unwrap()).unwrap();
    let goal_id = GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap();
    let clean = TaskStore::with_state_root(f.root.join("state"));
    let planner_work = goal_next(&f.session, &clean, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &clean,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            writer_plan(&planner_work, "fn main() {}\n"),
        ),
    )
    .unwrap();
    let work = goal_next(&f.session, &clean, &goal_id)["model_work"].clone();
    assert_eq!(work["role"], "WRITER");

    // Attempt 1 of this store's counter is the writer preparation commit.
    let failing = TaskStore::with_fault(f.root.join("state"), FaultPoint::BeforeReplaceOnCommit(1));
    let error = submit(
        &f.session,
        &failing,
        &submit_args(&f.session, &goal_id, &work, writer_answer(&work, content)),
    );
    assert_eq!(error.unwrap_err(), "GOAL_PERSISTENCE_ERROR");

    // No intent, no accepted receipt, no stage left behind, workspace untouched.
    let goal = goal_of(&f, &goal_id);
    let task = goal.tasks().values().next().unwrap();
    assert!(task.latest_attempt().unwrap().mutation_intent().is_none());
    assert_eq!(
        external_state(&goal)
            .unwrap()
            .receipts()
            .iter()
            .filter(|receipt| {
                receipt.decision()
                    == crate::external_model_work::SubmissionDecision::AcceptedForHostProcessing
            })
            .count(),
        0
    );
    assert_eq!(file_content(&f), "fn main() {}\n");
    assert!(
        staged_directories(&f).is_empty(),
        "a failed intent commit must not leave a linked stage"
    );

    // The retry succeeds on a healthy store.
    let retried = submit(
        &f.session,
        &clean,
        &submit_args(&f.session, &goal_id, &work, writer_answer(&work, content)),
    )
    .unwrap();
    assert_eq!(retried["outcome"], "ACCEPTED_FOR_HOST_PROCESSING");
}

#[test]
fn at_e5_039b_a_failed_complete_operation_commit_keeps_the_operation_applying() {
    let f = fixture("complete-commit");
    let content = "fn main() { /* complete */ }\n";
    let (goal_id, task_id, operation_id) = prepared_writer(&f, content);

    // The publication loop performs: BeginOperation commit, publish, CompleteOperation
    // commit. Fault the second commit of this store instance.
    let failing = TaskStore::with_fault(f.root.join("state"), FaultPoint::BeforeReplaceOnCommit(2));
    let goal = goal_of(&f, &goal_id);
    let stage = external_writer::verify_prepared_stage(
        &failing,
        &f.session,
        &goal,
        &task_id,
        &operation_id,
    )
    .expect("stage verifies");
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(external_publish::publish_prepared_writer_intent(
            &failing,
            &f.session,
            &goal_id,
            &task_id,
            &operation_id,
            &stage,
            &crate::writer::ExecutionWriteBoundary,
        ));
    assert!(
        result.is_err(),
        "a failed journal commit must surface as an error"
    );

    // The file was published, but the journal kept the operation APPLYING: the host
    // never claims a completed operation it could not record.
    assert_eq!(file_content(&f), content);
    let persisted = intent(&f, &goal_id, &task_id);
    assert_eq!(persisted.state(), MutationIntentState::Applying);
    assert_eq!(
        persisted.operations()[0].state(),
        MutationOperationState::Applying
    );

    // A second run cannot republish: the intent is not PREPARED any more.
    let retry = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &crate::writer::ExecutionWriteBoundary,
    );
    match retry {
        Ok(PublishOutcome::Refused { code }) => assert_eq!(code, "WRITER_INTENT_NOT_PREPARED"),
        other => panic!("a partially journaled publication must not replay: {other:?}"),
    }
    assert_eq!(file_content(&f), content);
}

// ---------------------------------------------------------------------------
// Path authority and cancellation (AT-032, 042, 043, 045).
// ---------------------------------------------------------------------------

#[test]
fn at_e5_032_a_target_that_became_a_directory_is_refused_before_publication() {
    let f = fixture("authority");
    let (goal_id, task_id, operation_id) = prepared_writer(&f, "fn main() { /* blocked */ }\n");
    // Replace the file with a directory: the path no longer resolves to a regular
    // target, so the pre-publication authority check must refuse.
    std::fs::remove_file(f.session.cwd.join("code.txt")).unwrap();
    std::fs::create_dir(f.session.cwd.join("code.txt")).unwrap();

    let outcome = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &crate::writer::ExecutionWriteBoundary,
    );
    match outcome {
        Ok(PublishOutcome::Refused { code }) => assert!(
            code == "WRITER_PATH_AUTHORITY_CHANGED" || code == "WRITER_PREIMAGE_CHANGED",
            "{code}"
        ),
        other => panic!("a changed path authority must refuse publication, got {other:?}"),
    }
    // Nothing was begun, so the journal still proves no write.
    let persisted = intent(&f, &goal_id, &task_id);
    assert_eq!(persisted.state(), MutationIntentState::Prepared);
    assert_eq!(
        persisted.operations()[0].state(),
        MutationOperationState::Prepared
    );
}

#[test]
fn at_e5_042_a_cancelled_goal_never_publishes_its_prepared_intent() {
    let f = fixture("cancel-prepared");
    let (goal_id, task_id, operation_id) = prepared_writer(&f, "fn main() { /* cancelled */ }\n");
    goal_api::goal_cancel(
        &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
        &f.session,
        &f.store,
    )
    .unwrap();
    let before = file_content(&f);
    let outcome = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &crate::writer::ExecutionWriteBoundary,
    );
    match outcome {
        Ok(PublishOutcome::Refused { code }) => assert!(
            code == "WRITER_GOAL_NOT_ACTIVE"
                || code == "WRITER_TASK_NOT_RUNNING"
                || code == "WRITER_LEASE_LOST",
            "{code}"
        ),
        other => panic!("a cancelled Goal must not publish, got {other:?}"),
    }
    assert_eq!(file_content(&f), before);
    let goal = goal_of(&f, &goal_id);
    assert!(matches!(
        goal.status(),
        GoalStatus::Cancelled | GoalStatus::Cancelling
    ));
    assert_eq!(
        external_state(&goal).unwrap().active_work(),
        None,
        "cancellation invalidated the WorkItem in the same commit"
    );
}

#[test]
fn at_e5_043_a_cancelled_goal_with_an_applying_intent_keeps_its_stage_and_refuses_publication() {
    let f = fixture("cancel-applying");
    let content = "fn main() { /* unknown then cancelled */ }\n";
    let (goal_id, task_id, operation_id) = prepared_writer(&f, content);

    // Leave the journal APPLYING through an unknown publication.
    let unknown = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &FaultyBoundary {
            fault: BoundaryFault::WriteThenFail,
        },
    );
    assert!(!matches!(unknown, Ok(PublishOutcome::Published { .. })));
    let stage_dir = crate::writer_stage::stage_directory(
        f.store.state_root(),
        &f.session.id,
        goal_id.as_str(),
        &operation_id,
    );
    assert!(stage_dir.exists(), "the stage is recovery evidence");

    goal_api::goal_cancel(
        &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
        &f.session,
        &f.store,
    )
    .unwrap();

    let outcome = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &crate::writer::ExecutionWriteBoundary,
    );
    match outcome {
        Ok(PublishOutcome::Refused { code }) => assert!(
            code == "WRITER_GOAL_NOT_ACTIVE"
                || code == "WRITER_TASK_NOT_RUNNING"
                || code == "WRITER_LEASE_LOST"
                || code == "WRITER_INTENT_NOT_PREPARED",
            "{code}"
        ),
        other => panic!("an unknown side effect must not be retried after cancel: {other:?}"),
    }
    // No blind cleanup: the stage survives cancellation because the outcome is
    // unknown and reconciliation still needs it.
    assert!(
        stage_dir.exists(),
        "a stage with an unknown side effect is kept"
    );
    assert_eq!(file_content(&f), content);
}

#[test]
fn at_e5_040_a_published_intent_reaches_the_reviewer_without_rerunning_the_writer() {
    let f = fixture("reviewer-resume");
    let content = "fn main() { /* reviewed */ }\n";
    let (goal_id, task_id, _operation_id) = prepared_writer(&f, content);
    // Publish through the real host driver path (goal_next), which also begins the
    // reviewer invocation and hands back the reviewer WorkItem.
    let reviewed = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(reviewed["outcome"], "MODEL_REQUIRED", "{reviewed}");
    assert_eq!(reviewed["model_work"]["role"], "REVIEWER");
    assert_eq!(file_content(&f), content);

    // A restart keeps the same reviewer WorkItem: the writer is never re-run.
    let restarted = TaskStore::with_state_root(f.root.join("state"));
    let work_id = reviewed["model_work"]["work_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let args = json!({"session_id": f.session.id, "goal_id": goal_id.as_str()});
    let again = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(external_goal_api::goal_next(&args, &f.session, &restarted))
        .unwrap();
    let again = serde_json::to_value(again).unwrap();
    if again["outcome"] == "MODEL_REQUIRED" {
        assert_eq!(again["model_work"]["work_id"].as_str().unwrap(), work_id);
        assert_eq!(again["model_work"]["role"], "REVIEWER");
    }
    let goal = restarted.load_goal(&f.session.id, &goal_id).unwrap();
    let attempt = goal.tasks()[&task_id].latest_attempt().unwrap();
    assert_eq!(
        goal.tasks()[&task_id].attempts().len(),
        1,
        "the reviewer leg must not create a second writer Attempt"
    );
    assert_eq!(
        attempt.mutation_intent().unwrap().reviewer_state(),
        crate::mutation::ReviewerInvocationState::Invoking
    );
    assert_eq!(file_content(&f), content, "no re-publication");
}

#[test]
fn at_e5_045_publication_writes_a_regular_file_with_the_exact_bytes() {
    let f = fixture("bytes");
    let content = "fn main() { /* exact bytes */ }\n";
    let (goal_id, task_id, operation_id) = prepared_writer(&f, content);
    let published = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &crate::writer::ExecutionWriteBoundary,
    );
    assert!(
        matches!(published, Ok(PublishOutcome::Published { .. })),
        "{published:?}"
    );
    let path = f.session.cwd.join("code.txt");
    let metadata = std::fs::symlink_metadata(&path).unwrap();
    assert!(metadata.file_type().is_file(), "a regular file is written");
    assert_eq!(std::fs::read(&path).unwrap(), content.as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // The publisher replaces the file content, not the mode contract it already
        // documents; assert the mode is not widened to group/other-writable.
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode & 0o022,
            0,
            "publication must not add group/other write"
        );
    }
    let intent = intent(&f, &goal_id, &task_id);
    assert_eq!(intent.state(), MutationIntentState::Applied);
    assert_eq!(
        intent.operations()[0].state(),
        MutationOperationState::Applied
    );
}

// ---------------------------------------------------------------------------
// Authorization B reliability (AT-GW-017/018/019/020/023/024).
// ---------------------------------------------------------------------------

fn grant_usage(f: &Fixture, goal_id: &GoalId) -> (u32, u64) {
    let ledger = crate::authorization_store::load_ledger(&f.store, &f.session.id, goal_id.as_str())
        .unwrap()
        .expect("grant ledger exists");
    let grant = ledger.latest_grant().expect("grant exists");
    (grant.usage().operations(), grant.usage().bytes())
}

#[test]
fn at_gw_017_a_crash_after_the_charge_before_begin_operation_never_double_charges() {
    let f = fixture("gw-charge");
    let content = "fn main() { /* charged once */ }\n";
    let (goal_id, task_id, operation_id) = prepared_writer(&f, content);

    // Attempt 1 of this store instance is the BeginOperation commit, so the
    // operation is charged but never begins.
    let failing = TaskStore::with_fault(f.root.join("state"), FaultPoint::BeforeReplaceOnCommit(1));
    let boundary = crate::writer::ExecutionWriteBoundary;
    let result = run_publication_with(&f, &failing, &goal_id, &task_id, &operation_id, &boundary);
    assert!(result.is_err(), "the BeginOperation commit must fail");
    assert_eq!(file_content(&f), "fn main() {}\n");
    assert_eq!(
        grant_usage(&f, &goal_id).0,
        1,
        "the charge happened before the side effect and is never refunded"
    );
    assert_eq!(
        intent(&f, &goal_id, &task_id).state(),
        MutationIntentState::Prepared,
        "no operation began"
    );

    // Retrying with a healthy store re-uses the charge and publishes once.
    let clean = TaskStore::with_state_root(f.root.join("state"));
    let goal = clean.load_goal(&f.session.id, &goal_id).unwrap();
    let stage =
        external_writer::verify_prepared_stage(&clean, &f.session, &goal, &task_id, &operation_id)
            .expect("stage verifies");
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(external_publish::publish_prepared_writer_intent(
            &clean,
            &f.session,
            &goal_id,
            &task_id,
            &operation_id,
            &stage,
            &boundary,
        ))
        .expect("publication");
    assert!(
        matches!(outcome, PublishOutcome::Published { .. }),
        "{outcome:?}"
    );
    assert_eq!(file_content(&f), content);
    assert_eq!(
        grant_usage(&f, &goal_id).0,
        1,
        "one operation id is charged exactly once"
    );
}

#[test]
fn at_gw_018_a_boundary_failure_after_begin_is_unknown_and_consumes_one_charge() {
    let f = fixture("gw-unknown");
    let content = "fn main() { /* unknown */ }\n";
    let (goal_id, task_id, operation_id) = prepared_writer(&f, content);
    let boundary = FaultyBoundary {
        fault: BoundaryFault::FailBeforeWrite,
    };
    let _ = run_publication(&f, &goal_id, &task_id, &operation_id, &boundary);
    assert_eq!(file_content(&f), "fn main() {}\n");
    assert_eq!(grant_usage(&f, &goal_id).0, 1);
    assert_eq!(
        intent(&f, &goal_id, &task_id).state(),
        MutationIntentState::Applying,
        "an unknown side effect stays journaled as applying"
    );

    // A second publication attempt must not write or charge again.
    let result = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &crate::writer::ExecutionWriteBoundary,
    )
    .expect("publication call");
    assert!(
        matches!(result, PublishOutcome::Refused { .. }),
        "an applying intent is never re-published: {result:?}"
    );
    assert_eq!(file_content(&f), "fn main() {}\n");
    assert_eq!(grant_usage(&f, &goal_id).0, 1);
}

#[test]
fn at_gw_019_a_grant_reissue_invalidates_the_prepared_work_item() {
    let f = fixture("gw-regrant");
    let content = "fn main() { /* old grant */ }\n";
    let (goal_id, task_id, operation_id) = prepared_writer(&f, content);
    assert_eq!(grant_usage(&f, &goal_id).0, 0);

    crate::authorization_store::revoke_active(&f.store, &f.session.id, goal_id.as_str(), None)
        .unwrap();
    let goal = goal_of(&f, &goal_id);
    crate::authorization_store::test_support::install_grant(&f.store, &goal, &["**"], 8, 8192, 600);

    let result = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &crate::writer::ExecutionWriteBoundary,
    )
    .expect("publication call");
    assert!(
        matches!(result, PublishOutcome::Refused { .. }),
        "the old WorkItem binding must not use the new grant: {result:?}"
    );
    assert_eq!(file_content(&f), "fn main() {}\n");
    // The new grant was not charged by the refused old binding.
    assert_eq!(grant_usage(&f, &goal_id).0, 0);
}

#[test]
fn at_gw_020_a_schema_1_external_goal_migrates_without_inventing_a_grant() {
    let f = fixture("gw-schema1");
    let goal_id = goal_api::goal_start(
        &json!({
            "session_id": f.session.id,
            "objective": "legacy external goal",
            "model_backend": "EXTERNAL",
        }),
        &f.session,
        &f.store,
    )
    .map(|view| serde_json::to_value(view).unwrap())
    .map(|view| GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap())
    .unwrap();
    let path = f.store.goal_path_for_test(&f.session.id, &goal_id).unwrap();
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    let mut document = serde_json::to_value(&goal).unwrap();
    let external = document["external_model"].as_object_mut().unwrap();
    external.insert("schema_version".to_owned(), json!(1));
    external.remove("write_scope");
    external.remove("authorization_anchor");
    std::fs::write(&path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

    let migrated = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    let state = crate::external_model_store::external_state(&migrated).unwrap();
    assert_eq!(
        state.schema_version(),
        crate::external_model_work::EXTERNAL_MODEL_SCHEMA_VERSION
    );
    assert!(state.write_scope().is_none());
    assert_eq!(state.authorization_anchor().highest_epoch(), 0);
    assert_eq!(
        crate::authorization_store::usable_grant(&f.store, &migrated)
            .unwrap_err()
            .code(),
        "WRITE_AUTHORIZATION_REQUIRED"
    );
}

#[cfg(unix)]
#[test]
fn at_gw_009_a_symlink_swap_is_refused_before_the_budget_charge() {
    let f = fixture("gw-symlink");
    let content = "fn main() { /* never */ }\n";
    let (goal_id, task_id, operation_id) = prepared_writer(&f, content);
    let outside = f.root.join("outside.txt");
    std::fs::write(&outside, b"outside\n").unwrap();
    std::fs::remove_file(f.session.cwd.join("code.txt")).unwrap();
    std::os::unix::fs::symlink(&outside, f.session.cwd.join("code.txt")).unwrap();

    let result = run_publication(
        &f,
        &goal_id,
        &task_id,
        &operation_id,
        &crate::writer::ExecutionWriteBoundary,
    )
    .expect("publication call");
    assert!(
        matches!(result, PublishOutcome::Refused { .. }),
        "a swapped symlink must be refused: {result:?}"
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside\n");
    assert_eq!(
        grant_usage(&f, &goal_id).0,
        0,
        "a refused path check never charges the grant"
    );
}
