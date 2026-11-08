//! E5-A (phase W0/W1) — external Writer issuance, validation, and private
//! durable staging (AT-E5-013..027).
//!
//! Every test asserts the safety property this phase exists for: an external
//! Writer proposal can be validated and durably prepared, and **cannot** change
//! the workspace. Publication is a separate step (E5-A2) and is represented here
//! only by its typed stop.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::Digest;
use uuid::Uuid;

use crate::config;
use crate::external_goal_api;
use crate::external_model_store::external_state;

use crate::external_writer;
use crate::goal::{Goal, GoalId, GoalStatus};
use crate::goal_api;
use crate::mutation::MutationOperationState;
use crate::task::TaskStatus;
use crate::task_store::TaskStore;

struct Fixture {
    root: PathBuf,
    managed_root: PathBuf,
    session: config::Session,
    store: TaskStore,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fixture(label: &str) -> Fixture {
    let root = std::env::temp_dir().join(format!("local-mcp-e5a-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(workspace.join("code.txt"), b"fn main() {}\n").unwrap();
    std::fs::write(workspace.join("other.txt"), b"untouched\n").unwrap();
    let session = config::Session {
        id: format!("e5a-{label}-{}", Uuid::new_v4()),
        cwd: workspace.clone(),
        permitted_directories: vec![workspace],
    };
    Fixture {
        managed_root: root.join("managed"),
        root,
        session,
        store: TaskStore::with_state_root(state),
    }
}

fn goal_next_call(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<external_goal_api::ExternalGoalNextView, goal_api::GoalApiError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(external_goal_api::goal_next(args, session, store))
}

fn goal_next(session: &config::Session, store: &TaskStore, goal_id: &GoalId) -> Value {
    serde_json::to_value(
        goal_next_call(
            &json!({"session_id": session.id, "goal_id": goal_id.as_str()}),
            session,
            store,
        )
        .unwrap(),
    )
    .unwrap()
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

fn submit(
    session: &config::Session,
    store: &TaskStore,
    args: &Value,
) -> Result<Value, goal_api::GoalApiError> {
    external_goal_api::goal_submit(args, session, store)
        .map(|view| serde_json::to_value(view).unwrap())
}

fn start_external(session: &config::Session, store: &TaskStore) -> GoalId {
    let view = goal_api::goal_start(
        &json!({
            "session_id": session.id,
            "objective": "Apply one scoped writer change",
            "model_backend": "EXTERNAL",
        }),
        session,
        store,
    )
    .unwrap();
    let view = serde_json::to_value(view).unwrap();
    GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap()
}

fn writer_task(id: &str, allowed: &[&str], verification: Value) -> Value {
    json!({
        "proposal_id": id,
        "title": "Update the target file",
        "objective": "Apply the requested scoped edit",
        "mandatory": true,
        "worker": "CODEX_WRITER",
        "dependencies": [],
        "scope": {
            "allowed_paths": allowed,
            "forbidden_paths": [],
            "operation_kind": "LOCAL_MUTATION",
            "replay_safety": "VERIFY_BEFORE_RETRY"
        },
        "verification": verification
    })
}

/// A writer plan whose single Task also asserts the exact postimage, so the host
/// verification chain can prove the mutation independently of the model.
fn writer_plan(work: &Value, allowed: &[&str], expected: Value) -> Value {
    let criterion_id = work["request"]["completion_criteria"][0]["criterion_id"]
        .as_str()
        .unwrap();
    let verification = match expected {
        Value::Null => {
            json!([{"kind": "STRUCTURED_EVIDENCE", "requirement_id": "writer-evidence"}])
        }
        digest => json!([
            {"kind": "FILE_DIGEST", "path": "code.txt", "expected_sha256": digest},
            {"kind": "STRUCTURED_EVIDENCE", "requirement_id": "writer-evidence"}
        ]),
    };
    json!({
        "goal_id": work["goal_id"],
        "goal_revision": work["goal_revision"],
        "summary": "apply one scoped edit",
        "tasks": [writer_task("edit", allowed, verification)],
        "criterion_bindings": [{"criterion_id": criterion_id, "task_refs": ["edit"]}]
    })
}

fn sha256_hex(text: &str) -> String {
    format!("{:x}", sha2::Sha256::digest(text.as_bytes()))
}

fn writer_answer(work: &Value, path: &str, content: &str, preimage: Value) -> Value {
    let request = &work["request"];
    json!({
        "goal_id": request["goal_id"],
        "task_id": request["task_id"],
        "attempt_id": request["attempt_id"],
        "goal_revision": request["goal_revision"],
        "plan_revision": request["plan_revision"],
        "status": "candidate_complete",
        "summary": "update the target file",
        "evidence": [{"kind": "writer-evidence", "value": "one file changed"}],
        "proposed_operations": [{
            "kind": "WRITE_UTF8",
            "path": path,
            "expected_preimage": preimage,
            "content": content
        }]
    })
}

/// Submit the writer plan and return the issued Writer WorkItem.
fn plan_writer_goal(f: &Fixture, goal_id: &GoalId, plan: Value) -> Value {
    let planner_work = goal_next(&f.session, &f.store, goal_id)["model_work"].clone();
    let planned = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, goal_id, &planner_work, plan),
    )
    .unwrap();
    assert_eq!(planned["outcome"], "COMMITTED", "{planned}");
    let next = goal_next(&f.session, &f.store, goal_id);
    assert_eq!(next["outcome"], "MODEL_REQUIRED", "{next}");
    let work = next["model_work"].clone();
    assert_eq!(work["role"], "WRITER");
    assert_eq!(work["output_contract"], "writer-proposal-v1");
    work
}

fn goal_of(f: &Fixture, goal_id: &GoalId) -> Goal {
    f.store.load_goal(&f.session.id, goal_id).unwrap()
}

fn workspace_digest(root: &Path) -> String {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
                continue;
            }
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            entries.push((
                String::from(relative.to_string_lossy()),
                std::fs::read(&path).unwrap(),
            ));
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = String::new();
    for (name, bytes) in entries {
        digest.push_str(&name);
        digest.push(':');
        digest.push_str(&sha256_hex(&String::from_utf8_lossy(&bytes)));
        digest.push('\n');
    }
    digest
}

// ---------------------------------------------------------------------------
// AT-E5-013..015 — issuance, lease, and read-only scope.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_013_a_writer_attempt_and_work_item_are_issued_together_without_a_model_call() {
    let f = fixture("issue");
    let goal_id = start_external(&f.session, &f.store);
    let before = workspace_digest(&f.session.cwd);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    assert_eq!(work["state"], "PENDING");
    assert!(work["task_id"].is_string());
    assert!(work["attempt_id"].is_string());
    assert_eq!(work["request"]["allowed_operations"], json!(["WRITE_UTF8"]));
    assert_eq!(work["request"]["output_contract"], "writer-proposal-v1");
    let goal = goal_of(&f, &goal_id);
    let task = goal.tasks().values().next().unwrap();
    assert_eq!(task.status(), TaskStatus::Running);
    assert!(crate::writer::holds_workspace_mutation_lease(task));
    assert_eq!(
        workspace_digest(&f.session.cwd),
        before,
        "issuance writes nothing"
    );
}

#[test]
fn at_e5_014_a_held_writer_lease_blocks_a_second_writer_issuance() {
    let f = fixture("lease");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let goal = goal_of(&f, &goal_id);
    let task_id = crate::task::TaskId::parse(work["task_id"].as_str().unwrap()).unwrap();
    let task = &goal.tasks()[&task_id];
    assert!(crate::writer::holds_workspace_mutation_lease(task));
    // The gate itself refuses a second writer attempt while the lease is held.
    let error = crate::writer::ensure_writer_task_ready(&goal, &task_id).unwrap_err();
    assert!(error.to_string().contains("READY Task"));
}

#[test]
fn at_e5_015_a_read_only_scope_cannot_issue_a_writer_work_item() {
    let f = fixture("read-only-scope");
    let goal_id = start_external(&f.session, &f.store);
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let criterion_id = planner_work["request"]["completion_criteria"][0]["criterion_id"]
        .as_str()
        .unwrap();
    // A READ_ONLY Task with CODEX_WRITER is refused by the planner validator, so
    // such a Task can never be materialized, and therefore never issued as a
    // writer WorkItem or staged.
    let plan = json!({
        "goal_id": planner_work["goal_id"],
        "goal_revision": planner_work["goal_revision"],
        "summary": "read only writer",
        "tasks": [{
            "proposal_id": "edit",
            "title": "Read only",
            "objective": "nothing",
            "mandatory": true,
            "worker": "CODEX_WRITER",
            "dependencies": [],
            "scope": {
                "allowed_paths": ["code.txt"],
                "forbidden_paths": [],
                "operation_kind": "READ_ONLY",
                "replay_safety": "SAFE_READ_ONLY"
            },
            "verification": [{"kind": "STRUCTURED_EVIDENCE", "requirement_id": "x"}]
        }],
        "criterion_bindings": [{"criterion_id": criterion_id, "task_refs": ["edit"]}]
    });
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &planner_work, plan),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA", "{view}");
    let goal = goal_of(&f, &goal_id);
    assert_eq!(goal.plan_revision(), 0);
    assert!(goal.tasks().is_empty());
    assert!(external_state(&goal).unwrap().active_work().is_none());
}

// ---------------------------------------------------------------------------
// AT-E5-016..021 — scope, preimage, and schema refusals.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_016_path_escapes_git_internals_and_forbidden_paths_are_refused() {
    // One fixture per probe: a refused writer proposal is a terminal pre-mutation
    // stop for that Attempt (BLOCKED or NEEDS_REPLAN), exactly like the Codex path.
    for path in ["../escape.txt", ".git/config", "other.txt", "/etc/passwd"] {
        let f = fixture(&format!("scope-{}", path.replace(['.', '/'], "_")));
        let goal_id = start_external(&f.session, &f.store);
        let work = plan_writer_goal(
            &f,
            &goal_id,
            writer_plan(
                &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
                &["code.txt"],
                Value::Null,
            ),
        );
        let before = workspace_digest(&f.session.cwd);
        let view = submit(
            &f.session,
            &f.store,
            &submit_args(
                &f.session,
                &goal_id,
                &work,
                writer_answer(&work, path, "pwned", json!({"kind": "ABSENT"})),
            ),
        )
        .unwrap();
        assert_eq!(view["outcome"], "REJECTED_SCHEMA", "{path}: {view}");
        let code = view["code"].as_str().unwrap();
        assert!(
            code == "WRITER_SCOPE_VIOLATION" || code == "WRITER_PREIMAGE_MISMATCH",
            "{path}: {code}"
        );
        assert_eq!(workspace_digest(&f.session.cwd), before, "{path}");
        let goal = goal_of(&f, &goal_id);
        let task = goal.tasks().values().next().unwrap();
        assert!(
            matches!(task.status(), TaskStatus::Blocked | TaskStatus::NeedsReplan),
            "{path}: {:?}",
            task.status()
        );
        assert!(
            task.latest_attempt().unwrap().mutation_intent().is_none(),
            "{path}"
        );
    }
}

#[test]
fn at_e5_017_a_preimage_mismatch_is_a_non_mutating_refusal() {
    let f = fixture("preimage");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            writer_answer(
                &work,
                "code.txt",
                "new body\n",
                json!({"kind": "SHA256", "sha256": sha256_hex("some other content")}),
            ),
        ),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA", "{view}");
    assert_eq!(view["code"], "WRITER_PREIMAGE_MISMATCH");
    assert!(view["receipt"]["committed_revision"].is_null());
    let goal = goal_of(&f, &goal_id);
    let state = external_state(&goal).unwrap();
    assert_eq!(state.receipts().len(), 2, "planner + writer refusal");
    assert!(state.active_work().is_none(), "refusal frees the slot");
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        "fn main() {}\n"
    );
}

#[test]
fn at_e5_018_an_absent_preimage_never_clobbers_an_existing_file() {
    let f = fixture("absent");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            writer_answer(&work, "code.txt", "clobbered\n", json!({"kind": "ABSENT"})),
        ),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA", "{view}");
    assert_eq!(view["code"], "WRITER_PREIMAGE_MISMATCH");
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        "fn main() {}\n"
    );
}

#[test]
fn at_e5_019_a_duplicate_destination_is_refused() {
    let f = fixture("duplicate");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let mut answer = writer_answer(
        &work,
        "code.txt",
        "first\n",
        json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
    );
    let second = answer["proposed_operations"][0].clone();
    answer["proposed_operations"]
        .as_array_mut()
        .unwrap()
        .push(second);
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, answer),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA", "{view}");
    assert!(
        view["code"] == "WRITER_SCOPE_VIOLATION" || view["code"] == "WRITER_OPERATION_REJECTED",
        "{view}"
    );
}

#[test]
fn at_e5_020_oversized_and_deeply_nested_payloads_are_bounded() {
    let f = fixture("oversized");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let huge = "x".repeat(crate::writer::MAX_WRITE_CONTENT_BYTES + 1);
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            writer_answer(
                &work,
                "code.txt",
                &huge,
                json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
            ),
        ),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA", "{view}");

    // The oversized proposal ended that Attempt, so the bound on operation count
    // is exercised on a fresh Goal.
    let f = fixture("oversized-count");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let mut many = writer_answer(
        &work,
        "code.txt",
        "x\n",
        json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
    );
    let template = many["proposed_operations"][0].clone();
    let mut operations = Vec::new();
    for index in 0..(crate::writer::MAX_WRITER_OPERATIONS + 1) {
        let mut operation = template.clone();
        operation["path"] = json!(format!("f{index}.txt"));
        operation["expected_preimage"] = json!({"kind": "ABSENT"});
        operations.push(operation);
    }
    many["proposed_operations"] = Value::Array(operations);
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, many),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA", "{view}");
}

#[test]
fn at_e5_021_a_non_candidate_status_carrying_operations_is_refused() {
    let f = fixture("non-candidate");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let mut answer = writer_answer(
        &work,
        "code.txt",
        "should not happen\n",
        json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
    );
    answer["status"] = json!("blocked");
    answer["proposed_operations"][0]["unexpected"] = json!(true);
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, answer),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA", "{view}");
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        "fn main() {}\n"
    );
}

// ---------------------------------------------------------------------------
// AT-E5-022..025 — identity, revision, and idempotency.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_022_a_foreign_or_missing_attempt_is_refused_without_an_oracle() {
    let f = fixture("identity");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let mut answer = writer_answer(
        &work,
        "code.txt",
        "x\n",
        json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
    );
    answer["attempt_id"] = json!(Uuid::new_v4().to_string());
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, answer),
    )
    .unwrap();
    assert_eq!(view["outcome"], "REJECTED_SCHEMA", "{view}");
    assert_eq!(view["code"], "WRITER_IDENTITY_MISMATCH");

    // A completely unknown WorkItem is refused as an unknown work id.
    let mut args = submit_args(
        &f.session,
        &goal_id,
        &work,
        writer_answer(
            &work,
            "code.txt",
            "x\n",
            json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
        ),
    );
    args["work_id"] = json!(Uuid::new_v4().to_string());
    let view = submit(&f.session, &f.store, &args).unwrap();
    assert_eq!(view["outcome"], "REJECTED_AUTHORITY");
    assert_eq!(view["code"], "UNKNOWN_WORK_ID");
}

#[test]
fn at_e5_023_a_stale_revision_is_refused_and_stages_nothing() {
    let f = fixture("stale-revision");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let mut args = submit_args(
        &f.session,
        &goal_id,
        &work,
        writer_answer(
            &work,
            "code.txt",
            "x\n",
            json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
        ),
    );
    args["expected_goal_revision"] = json!(1);
    let view = submit(&f.session, &f.store, &args).unwrap();
    assert_eq!(view["outcome"], "REJECTED_STALE_REVISION", "{view}");
    let goal = goal_of(&f, &goal_id);
    assert!(external_state(&goal).unwrap().receipts().len() == 1);
    let stages = f.root.join("state").join("stages");
    assert!(
        !stages.exists() || std::fs::read_dir(&stages).unwrap().next().is_none(),
        "a stale revision must not create a stage"
    );
}

#[test]
fn at_e5_024_the_same_submission_replays_without_a_second_stage() {
    let f = fixture("replay");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let args = submit_args(
        &f.session,
        &goal_id,
        &work,
        writer_answer(
            &work,
            "code.txt",
            "replayed\n",
            json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
        ),
    );
    let first = submit(&f.session, &f.store, &args).unwrap();
    assert_eq!(first["outcome"], "ACCEPTED_FOR_HOST_PROCESSING", "{first}");
    assert_eq!(first["code"], "WRITE_INTENT_PREPARED");
    let goal_after_first = goal_of(&f, &goal_id);
    let revision = goal_after_first.revision();
    assert_eq!(
        external_state(&goal_after_first)
            .unwrap()
            .active_work()
            .unwrap()
            .state(),
        crate::external_model_work::WorkItemState::Submitted
    );

    let replay = submit(&f.session, &f.store, &args).unwrap();
    assert_eq!(
        replay["outcome"], "REPLAY_OF_ACCEPTED_SUBMISSION",
        "{replay}"
    );
    assert_eq!(replay["receipt"], first["receipt"]);
    assert_eq!(goal_of(&f, &goal_id).revision(), revision);
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        "fn main() {}\n",
        "preparation never mutates the workspace"
    );
}

#[test]
fn at_e5_025_the_same_id_with_a_different_payload_is_a_conflict() {
    let f = fixture("conflict");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let args = submit_args(
        &f.session,
        &goal_id,
        &work,
        writer_answer(
            &work,
            "code.txt",
            "first\n",
            json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
        ),
    );
    let _ = submit(&f.session, &f.store, &args).unwrap();
    let mut conflicting = args.clone();
    conflicting["response"] = writer_answer(
        &work,
        "code.txt",
        "second\n",
        json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
    );
    let view = submit(&f.session, &f.store, &conflicting).unwrap();
    assert_eq!(view["outcome"], "CONFLICTING_DUPLICATE", "{view}");
}

// ---------------------------------------------------------------------------
// AT-E5-026 / 027 — stage durability, privacy, and the publication stop.
// ---------------------------------------------------------------------------

#[test]
fn at_e5_026_an_interrupted_stage_leaves_the_workspace_untouched() {
    let f = fixture("stage-crash");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let before = workspace_digest(&f.session.cwd);
    // A fault before the durable replace makes the stage write fail; the commit
    // never happens, so no intent and no receipt exist.
    let failing = TaskStore::with_fault(
        f.root.join("state"),
        crate::task_store::FaultPoint::BeforeReplace,
    );
    let args = submit_args(
        &f.session,
        &goal_id,
        &work,
        writer_answer(
            &work,
            "code.txt",
            "crashed\n",
            json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
        ),
    );
    let error = submit(&f.session, &failing, &args).unwrap_err();
    assert_eq!(error.code(), "GOAL_PERSISTENCE_ERROR");
    let goal = goal_of(&f, &goal_id);
    assert_eq!(external_state(&goal).unwrap().receipts().len(), 1);
    assert!(
        goal.tasks().values().all(|task| task
            .latest_attempt()
            .unwrap()
            .mutation_intent()
            .is_none())
    );
    assert_eq!(workspace_digest(&f.session.cwd), before);
}

#[test]
fn at_e5_027_the_stage_is_private_bounded_and_hash_verified() {
    let f = fixture("stage-verify");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let submitted = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            writer_answer(
                &work,
                "code.txt",
                "prepared body\n",
                json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
            ),
        ),
    )
    .unwrap();
    assert_eq!(submitted["outcome"], "ACCEPTED_FOR_HOST_PROCESSING");

    let goal = goal_of(&f, &goal_id);
    let task = goal.tasks().values().next().unwrap();
    let attempt = task.latest_attempt().unwrap();
    let intent = attempt.mutation_intent().expect("prepared intent");
    assert_eq!(intent.operations().len(), 1);
    assert_eq!(
        intent.operations()[0].state(),
        MutationOperationState::Prepared,
        "no operation may be begun by preparation"
    );
    assert_eq!(
        intent.operations()[0].intended_after_sha256(),
        sha256_hex("prepared body\n")
    );

    // The sealed stage verifies against the durable intent.
    let task_id = task.id().clone();
    let stage = external_writer::verify_prepared_stage(
        &f.store,
        &f.session,
        &goal,
        &task_id,
        intent.operation_id(),
    )
    .expect("stage verifies");
    assert_eq!(stage.operation_count(), 1);

    // The raw model JSON is not stored anywhere in the durable document, and the
    // stage holds only the validated content bytes.
    let document = serde_json::to_string(&goal).unwrap();
    assert!(!document.contains("prepared body"));
    assert!(!document.contains("proposed_operations"));

    // Tampering with the staged bytes is detected, not repaired.
    let stage_root = crate::writer_stage::stage_directory(
        f.store.state_root(),
        &f.session.id,
        goal.id().as_str(),
        intent.operation_id(),
    );
    let content_file = std::fs::read_dir(&stage_root)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "content"))
        .expect("staged content file");
    std::fs::write(&content_file, b"tampered").unwrap();
    let error = external_writer::verify_prepared_stage(
        &f.store,
        &f.session,
        &goal,
        &task_id,
        intent.operation_id(),
    )
    .unwrap_err();
    assert_eq!(error.code(), "WRITER_STAGE_INTEGRITY_MISMATCH");
}

/// A writer plan whose Task also asserts the exact postimage and a review gate.
fn reviewed_writer_plan(work: &Value, content: &str) -> Value {
    let criterion_id = work["request"]["completion_criteria"][0]["criterion_id"]
        .as_str()
        .unwrap();
    json!({
        "goal_id": work["goal_id"],
        "goal_revision": work["goal_revision"],
        "summary": "apply one reviewed scoped edit",
        "tasks": [{
            "proposal_id": "edit",
            "title": "Update the target file",
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
        "evidence": [{"kind": "review-evidence", "value": "change reviewed"}]
    })
}

#[test]
fn at_e5_028_to_034_publication_and_review_reach_host_goal_completion() {
    let f = fixture("publication-e2e");
    crate::model_accounting::reset_for_test();
    let goal_id = start_external(&f.session, &f.store);
    let content = "fn main() { println!(\"e5\"); }\n";
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let planned = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            reviewed_writer_plan(&planner_work, content),
        ),
    )
    .unwrap();
    assert_eq!(planned["outcome"], "COMMITTED", "{planned}");

    let writer_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    assert_eq!(writer_work["role"], "WRITER");
    let staged = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &writer_work,
            writer_answer(
                &writer_work,
                "code.txt",
                content,
                json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
            ),
        ),
    )
    .unwrap();
    assert_eq!(
        staged["outcome"], "ACCEPTED_FOR_HOST_PROCESSING",
        "{staged}"
    );

    // One bounded goal_next publishes the prepared intent, begins the reviewer
    // invocation, and hands back the reviewer WorkItem.
    let reviewed = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(reviewed["outcome"], "MODEL_REQUIRED", "{reviewed}");
    assert_eq!(reviewed["model_work"]["role"], "REVIEWER", "{reviewed}");
    assert_eq!(
        reviewed["model_work"]["output_contract"],
        "reviewer-verdict-v1"
    );
    let reviewer_work = reviewed["model_work"].clone();
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        content,
        "the host published the exact staged bytes"
    );

    let verdict = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &reviewer_work,
            reviewer_answer(&reviewer_work, 0),
        ),
    )
    .unwrap();
    assert_eq!(verdict["outcome"], "COMMITTED", "{verdict}");
    let goal = goal_of(&f, &goal_id);
    let task = goal.tasks().values().next().unwrap();
    assert_eq!(
        task.status(),
        TaskStatus::Verifying,
        "review never completes a Task"
    );

    let finished = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(finished["status"], "COMPLETED", "{finished}");
    let goal = goal_of(&f, &goal_id);
    assert_eq!(goal.status(), GoalStatus::Completed);
    let task = goal.tasks().values().next().unwrap();
    assert_eq!(task.status(), TaskStatus::Completed);
    assert!(
        task.latest_attempt()
            .unwrap()
            .mutation_intent()
            .is_some_and(|intent| {
                intent.state() == crate::mutation::MutationIntentState::Applied
                    && intent.reviewer_state()
                        == crate::mutation::ReviewerInvocationState::Persisted
            })
    );
    assert_eq!(
        crate::model_accounting::snapshot_for_test().codex_spawn_count,
        0
    );
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        content
    );
    // A second goal_next never republishes.
    let terminal = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(terminal["outcome"], "TERMINAL", "{terminal}");
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        content
    );
}

#[test]
fn at_e5_029_a_review_gate_failure_blocks_the_writer_task() {
    let f = fixture("review-gate");
    let goal_id = start_external(&f.session, &f.store);
    let content = "fn main() { println!(\"gate\"); }\n";
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            reviewed_writer_plan(&planner_work, content),
        ),
    )
    .unwrap();
    let writer_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &writer_work,
            writer_answer(
                &writer_work,
                "code.txt",
                content,
                json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
            ),
        ),
    )
    .unwrap();
    let reviewer_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    assert_eq!(reviewer_work["role"], "REVIEWER");
    let verdict = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &reviewer_work,
            reviewer_answer(&reviewer_work, 3),
        ),
    )
    .unwrap();
    assert_eq!(verdict["outcome"], "REJECTED_SCHEMA", "{verdict}");
    assert_eq!(verdict["code"], "REVIEW_GATE_BLOCKED");
    let goal = goal_of(&f, &goal_id);
    let task = goal.tasks().values().next().unwrap();
    assert_eq!(task.status(), TaskStatus::Blocked);
    assert_ne!(goal.status(), GoalStatus::Completed);
}

#[test]
fn at_e5_031_a_changed_postimage_stops_the_review() {
    let f = fixture("stale-postimage");
    let goal_id = start_external(&f.session, &f.store);
    let content = "fn main() { println!(\"stale\"); }\n";
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            reviewed_writer_plan(&planner_work, content),
        ),
    )
    .unwrap();
    let writer_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &writer_work,
            writer_answer(
                &writer_work,
                "code.txt",
                content,
                json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
            ),
        ),
    )
    .unwrap();
    let reviewer_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    assert_eq!(reviewer_work["role"], "REVIEWER");
    // An external edit after publication invalidates the review request.
    std::fs::write(f.session.cwd.join("code.txt"), b"changed by someone else\n").unwrap();
    let verdict = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &reviewer_work,
            reviewer_answer(&reviewer_work, 0),
        ),
    )
    .unwrap();
    assert_eq!(verdict["outcome"], "REJECTED_SCHEMA", "{verdict}");
    assert_eq!(verdict["code"], "REVIEWER_STALE_POSTIMAGE");
    let goal = goal_of(&f, &goal_id);
    assert_ne!(goal.status(), GoalStatus::Completed);
}

#[test]
fn at_e5_030_a_preimage_that_changed_after_staging_blocks_publication() {
    let f = fixture("preimage-changed");
    let goal_id = start_external(&f.session, &f.store);
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            reviewed_writer_plan(&planner_work, "new content\n"),
        ),
    )
    .unwrap();
    let writer_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &writer_work,
            writer_answer(
                &writer_work,
                "code.txt",
                "new content\n",
                json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
            ),
        ),
    )
    .unwrap();
    // A concurrent edit between staging and publication must stop the host.
    std::fs::write(f.session.cwd.join("code.txt"), b"edited after staging\n").unwrap();
    let view = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(view["outcome"], "BLOCKED", "{view}");
    assert_eq!(view["blockers"][0]["code"], "WRITER_PREIMAGE_CHANGED");
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        "edited after staging\n",
        "a stale preimage is never overwritten"
    );
    let goal = goal_of(&f, &goal_id);
    let task = goal.tasks().values().next().unwrap();
    // Nothing was begun for the operation, so the journal still proves no write.
    assert!(
        task.latest_attempt()
            .unwrap()
            .mutation_intent()
            .unwrap()
            .operations()
            .iter()
            .all(|operation| operation.state() == MutationOperationState::Prepared)
    );
}

#[test]
fn at_e5_044_two_clients_publish_exactly_once() {
    let f = fixture("two-clients");
    let goal_id = start_external(&f.session, &f.store);
    let content = "fn main() { println!(\"once\"); }\n";
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            reviewed_writer_plan(&planner_work, content),
        ),
    )
    .unwrap();
    let writer_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &writer_work,
            writer_answer(
                &writer_work,
                "code.txt",
                content,
                json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
            ),
        ),
    )
    .unwrap();

    let mut handles = Vec::new();
    for _ in 0..2 {
        let session = f.session.clone();
        let store = f.store.clone();
        let args = json!({"session_id": f.session.id, "goal_id": goal_id.as_str()});
        handles.push(std::thread::spawn(move || {
            goal_next_call(&args, &session, &store)
                .map(|view| {
                    serde_json::to_value(view).unwrap()["outcome"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                })
                .unwrap_or_else(|error| error.code().to_owned())
        }));
    }
    let outcomes: Vec<String> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert!(
        outcomes.iter().any(|outcome| outcome == "MODEL_REQUIRED"),
        "{outcomes:?}"
    );
    for outcome in &outcomes {
        assert!(
            !outcome.starts_with("GOAL_PERSISTENCE") && outcome != "GOAL_STATE_CORRUPT",
            "{outcomes:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        content
    );
    // Exactly one publication: the journal has one Applied operation.
    let goal = goal_of(&f, &goal_id);
    let intent = goal
        .tasks()
        .values()
        .next()
        .unwrap()
        .latest_attempt()
        .unwrap()
        .mutation_intent()
        .unwrap();
    assert_eq!(intent.operations().len(), 1);
    assert_eq!(
        intent.operations()[0].state(),
        MutationOperationState::Applied
    );
}

#[test]
fn at_e5_027c_cancelling_a_prepared_writer_goal_never_applies_it() {
    let f = fixture("cancel-prepared");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            writer_answer(
                &work,
                "code.txt",
                "never applied\n",
                json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
            ),
        ),
    )
    .unwrap();
    goal_api::goal_cancel(
        &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
        &f.session,
        &f.store,
    )
    .unwrap();
    let goal = goal_of(&f, &goal_id);
    assert!(matches!(
        goal.status(),
        GoalStatus::Cancelled | GoalStatus::Cancelling
    ));
    assert!(external_state(&goal).unwrap().active_work().is_none());
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        "fn main() {}\n"
    );
    let after = goal_next(&f.session, &f.store, &goal_id);
    // The Task still holds a live Attempt/lease, so cancellation rests in
    // CANCELLING until the host reconciles; either way the Goal is not active
    // and the prepared intent is never applied.
    assert_ne!(after["status"], "RUNNING", "{after}");
    assert!(after["model_work"].is_null(), "{after}");
    assert_ne!(after["result_kind"], "COMPLETED");
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        "fn main() {}\n"
    );
}

#[test]
fn at_e5_027d_a_writer_submission_cannot_be_laundered_through_another_work_item() {
    let f = fixture("launder");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let args = submit_args(
        &f.session,
        &goal_id,
        &work,
        writer_answer(
            &work,
            "code.txt",
            "first\n",
            json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
        ),
    );
    let _ = submit(&f.session, &f.store, &args).unwrap();
    // Reuse the submission id against a synthetic second WorkItem id.
    let mut laundered = args.clone();
    laundered["work_id"] = json!(Uuid::new_v4().to_string());
    let view = submit(&f.session, &f.store, &laundered).unwrap();
    assert_eq!(view["outcome"], "REJECTED_AUTHORITY", "{view}");
    assert_eq!(view["code"], "SUBMISSION_WORK_ID_MISMATCH");
}

#[test]
fn at_e5_027e_a_non_mutating_writer_status_is_recorded_without_a_stage() {
    let f = fixture("non-mutating");
    let goal_id = start_external(&f.session, &f.store);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let mut answer = writer_answer(
        &work,
        "code.txt",
        "ignored\n",
        json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
    );
    answer["status"] = json!("needs_replan");
    answer["proposed_operations"] = json!([]);
    let view = submit(
        &f.session,
        &f.store,
        &submit_args(&f.session, &goal_id, &work, answer),
    )
    .unwrap();
    assert_eq!(view["outcome"], "COMMITTED", "{view}");
    assert_eq!(view["code"], "WRITER_NEEDS_REPLAN");
    let goal = goal_of(&f, &goal_id);
    let task = goal.tasks().values().next().unwrap();
    assert_eq!(task.status(), TaskStatus::NeedsReplan);
    assert!(task.latest_attempt().unwrap().mutation_intent().is_none());
    let stages = f.root.join("state").join("stages");
    assert!(!stages.exists() || std::fs::read_dir(&stages).unwrap().next().is_none());
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        "fn main() {}\n"
    );
    // The prepared-intent helper never treats a repudiated attempt as publishable.
    assert!(external_writer::pending_prepared_intent(&goal, task.id()).is_none());
}

// ---------------------------------------------------------------------------
// Authorization B (AT-GW) — the grant gate around the existing Writer path.
// ---------------------------------------------------------------------------

fn scoped_fixture(
    label: &str,
    paths: &[&str],
    max_operations: u32,
    max_bytes: u64,
) -> (Fixture, GoalId) {
    let root = std::env::temp_dir().join(format!("local-mcp-gw-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(workspace.join("code.txt"), b"fn main() {}\n").unwrap();
    std::fs::write(workspace.join("other.txt"), b"untouched\n").unwrap();
    let session = config::Session {
        id: format!("gw-{label}-{}", Uuid::new_v4()),
        cwd: workspace.clone(),
        permitted_directories: vec![workspace],
    };
    let store = TaskStore::with_state_root(state);
    let view = goal_api::goal_start(
        &json!({
            "session_id": session.id,
            "objective": "Authorization B writer gate",
            "model_backend": "EXTERNAL",
            "write_scope": {
                "allowed_paths": paths,
                "allowed_operations": ["WRITE_UTF8"],
                "max_operations": max_operations,
                "max_bytes": max_bytes,
                "expires_in_seconds": 600
            }
        }),
        &session,
        &store,
    )
    .unwrap();
    let view = serde_json::to_value(view).unwrap();
    let goal_id = GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap();
    (
        Fixture {
            managed_root: root.join("managed"),
            root,
            session,
            store,
        },
        goal_id,
    )
}

fn install_grant(f: &Fixture, goal_id: &GoalId, paths: &[&str], ops: u32, bytes: u64) {
    let goal = f.store.load_goal(&f.session.id, goal_id).unwrap();
    crate::authorization_store::test_support::install_grant(
        &f.store, &goal, paths, ops, bytes, 600,
    );
}

fn writer_answer_multi(work: &Value, operations: &[(&str, &str, Value)]) -> Value {
    let request = &work["request"];
    json!({
        "goal_id": request["goal_id"],
        "task_id": request["task_id"],
        "attempt_id": request["attempt_id"],
        "goal_revision": request["goal_revision"],
        "plan_revision": request["plan_revision"],
        "status": "candidate_complete",
        "summary": "update the target files",
        "evidence": [{"kind": "writer-evidence", "value": "files changed"}],
        "proposed_operations": operations
            .iter()
            .map(|(path, content, preimage)| json!({
                "kind": "WRITE_UTF8",
                "path": path,
                "expected_preimage": preimage,
                "content": content
            }))
            .collect::<Vec<_>>()
    })
}

fn code_preimage(f: &Fixture) -> Value {
    let content = std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap();
    json!({"kind": "SHA256", "sha256": sha256_hex(&content)})
}

fn other_preimage(f: &Fixture) -> Value {
    let content = std::fs::read_to_string(f.session.cwd.join("other.txt")).unwrap();
    json!({"kind": "SHA256", "sha256": sha256_hex(&content)})
}

#[test]
fn at_gw_001_no_grant_blocks_writer_issuance_without_writing() {
    let (f, goal_id) = scoped_fixture("nogrant", &["code.txt"], 4, 4096);
    let before = workspace_digest(&f.session.cwd);
    let next = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(next["outcome"], "BLOCKED", "{next}");
    assert_eq!(
        next["blockers"][0]["code"], "WRITE_AUTHORIZATION_UNAVAILABLE",
        "{next}"
    );
    assert!(
        next["model_work"].is_null(),
        "no work may be issued: {next}"
    );
    assert_eq!(workspace_digest(&f.session.cwd), before);

    // Granting the exact scope unblocks the real Writer path.
    install_grant(&f, &goal_id, &["code.txt"], 4, 4096);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let goal = goal_of(&f, &goal_id);
    let binding = crate::external_model_store::external_state(&goal)
        .unwrap()
        .active_work()
        .and_then(|item| item.authorization())
        .expect("writer WorkItem carries the grant binding");
    assert_eq!(binding.authorization_epoch(), 1);
    assert_eq!(work["role"], "WRITER");
}

#[test]
fn at_gw_015_a_revoked_grant_blocks_the_writer_path_end_to_end() {
    let (f, goal_id) = scoped_fixture("revoked", &["code.txt"], 4, 4096);
    install_grant(&f, &goal_id, &["code.txt"], 4, 4096);
    let before = workspace_digest(&f.session.cwd);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let submitted = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            writer_answer(
                &work,
                "code.txt",
                "fn main() { changed(); }\n",
                code_preimage(&f),
            ),
        ),
    )
    .unwrap();
    assert_eq!(
        submitted["outcome"], "ACCEPTED_FOR_HOST_PROCESSING",
        "{submitted}"
    );

    crate::authorization_store::revoke_active(&f.store, &f.session.id, goal_id.as_str(), None)
        .unwrap();
    let next = goal_next(&f.session, &f.store, &goal_id);
    assert!(
        matches!(
            next["outcome"].as_str(),
            Some("BLOCKED") | Some("NO_ACTION_WITH_REASON")
        ),
        "revocation must stop the writer path: {next}"
    );
    // The prepared slot is durably released and the Task records the exact
    // authorization code; the Goal is not left spinning on the revoked grant.
    let goal = goal_of(&f, &goal_id);
    assert!(
        crate::external_model_store::external_state(&goal)
            .unwrap()
            .active_work()
            .is_none(),
        "a revoked prepared slot is released"
    );
    let task = goal.tasks().values().next().unwrap();
    assert!(
        task.blockers()
            .iter()
            .any(|blocker| blocker.code() == "WRITE_AUTHORIZATION_REVOKED"),
        "the Task records the authorization stop: {:?}",
        task.blockers()
    );
    assert_eq!(
        workspace_digest(&f.session.cwd),
        before,
        "a revoked grant never publishes"
    );
}

#[test]
fn at_gw_009_an_out_of_scope_operation_is_refused_before_staging() {
    let (f, goal_id) = scoped_fixture("scope", &["code.txt"], 4, 4096);
    install_grant(&f, &goal_id, &["code.txt"], 4, 4096);
    let before = workspace_digest(&f.session.cwd);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["other.txt"],
            Value::Null,
        ),
    );
    let submitted = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            writer_answer(&work, "other.txt", "rewritten\n", other_preimage(&f)),
        ),
    )
    .unwrap();
    assert_eq!(submitted["outcome"], "REJECTED_AUTHORITY", "{submitted}");
    assert_eq!(
        submitted["code"], "WRITE_AUTHORIZATION_SCOPE_MISMATCH",
        "{submitted}"
    );
    assert_eq!(workspace_digest(&f.session.cwd), before);
}

#[test]
fn at_gw_013_the_grant_operation_budget_bounds_a_multi_file_proposal() {
    let (f, goal_id) = scoped_fixture("budget", &["code.txt", "other.txt"], 1, 4096);
    install_grant(&f, &goal_id, &["code.txt", "other.txt"], 1, 4096);
    let before = workspace_digest(&f.session.cwd);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt", "other.txt"],
            Value::Null,
        ),
    );
    let submitted = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            writer_answer_multi(
                &work,
                &[
                    ("code.txt", "fn main() { changed(); }\n", code_preimage(&f)),
                    ("other.txt", "rewritten\n", other_preimage(&f)),
                ],
            ),
        ),
    )
    .unwrap();
    assert_eq!(submitted["outcome"], "REJECTED_AUTHORITY", "{submitted}");

    assert_eq!(submitted["code"], "WRITE_BUDGET_EXHAUSTED", "{submitted}");
    assert_eq!(workspace_digest(&f.session.cwd), before);
}

#[test]
fn at_e5_074_a_dirty_primary_refuses_external_managed_preparation() {
    let (f, goal_id, _base) = managed_fixture("dirty");
    // A grant bound to the requested managed identity lets goal_next reach the
    // host preparation step instead of stopping at the authorization gate.
    install_grant(&f, &goal_id, &["code.txt"], 8, 8192);
    std::fs::write(
        f.session.cwd.join("code.txt"),
        b"dirty primary
",
    )
    .unwrap();

    let next = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(next["outcome"], "BLOCKED", "{next}");
    assert_eq!(
        next["blockers"][0]["code"], "MANAGED_WORKSPACE_BLOCKED",
        "{next}"
    );
    assert!(
        !f.managed_root
            .join(&f.session.id)
            .join(goal_id.as_str())
            .exists(),
        "no worktree may be created for a dirty primary"
    );
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        "dirty primary
"
    );
}

// ---------------------------------------------------------------------------
// E5-D / D2–D4 — EXTERNAL Writer, Reviewer and host evidence in a managed
// worktree. AT-E5-072, 077, 078, 079, 082.
// ---------------------------------------------------------------------------

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|error| panic!("could not run git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned()
}

struct AllowAllApprovals;

impl crate::managed_worktree_prepare::ManagedCreationApprover for AllowAllApprovals {
    fn approve(
        &self,
        _request: &crate::managed_worktree_prepare::ManagedCreationApproval,
    ) -> crate::managed_worktree_prepare::ManagedApprovalFuture<'_> {
        Box::pin(async { Ok(true) })
    }
}

fn managed_fixture(label: &str) -> (Fixture, GoalId, String) {
    let raw_root =
        std::env::temp_dir().join(format!("local-mcp-gw-managed-{label}-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&raw_root).unwrap();
    let root = std::fs::canonicalize(&raw_root).unwrap();
    let primary = root.join("primary");
    let managed = root.join("managed");
    let state = root.join("state");
    std::fs::create_dir_all(&primary).unwrap();
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    git(&primary, &["init", "-q", "."]);
    git(&primary, &["config", "core.autocrlf", "false"]);
    git(
        &primary,
        &["config", "user.email", "managed@example.invalid"],
    );
    git(&primary, &["config", "user.name", "Managed E2E"]);
    git(&primary, &["config", "commit.gpgsign", "false"]);
    std::fs::write(primary.join("code.txt"), b"fn main() {}\n").unwrap();
    git(&primary, &["add", "code.txt"]);
    git(&primary, &["commit", "-q", "-m", "base"]);
    let base = git(&primary, &["rev-parse", "HEAD"]);

    let session = config::Session {
        id: format!("gw-managed-{label}-{}", Uuid::new_v4()),
        cwd: primary.clone(),
        permitted_directories: vec![primary],
    };
    let store = TaskStore::with_state_root(state);
    let mut fixture = Fixture {
        root: root.clone(),
        managed_root: managed.clone(),
        session,
        store,
    };
    let view = goal_api::goal_start_with_managed_root(
        &json!({
            "session_id": fixture.session.id,
            "objective": "Managed Authorization B E2E",
            "workspace_mode": "MANAGED_WORKTREE",
            "model_backend": "EXTERNAL",
            "write_scope": {
                "allowed_paths": ["code.txt"],
                "allowed_operations": ["WRITE_UTF8"],
                "max_operations": 8,
                "max_bytes": 8192,
                "expires_in_seconds": 600
            }
        }),
        &fixture.session,
        &fixture.store,
        &crate::goal_api::ManagedRootSource::fixed(managed.clone()),
    )
    .unwrap();
    let view = serde_json::to_value(view).unwrap();
    let goal_id = GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap();
    let canonical_managed = std::fs::canonicalize(&fixture.managed_root).unwrap();
    if !fixture
        .session
        .permitted_directories
        .contains(&canonical_managed)
    {
        fixture
            .session
            .permitted_directories
            .push(canonical_managed);
        fixture.session.permitted_directories.sort();
    }
    (fixture, goal_id, base)
}

fn prepare_managed_external(f: &Fixture, goal_id: &GoalId) {
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(
            crate::managed_worktree_prepare::prepare_managed_workspace_with_policy(
                &f.store,
                &f.session,
                goal_id,
                &crate::managed_worktree_observe::HostGit::new(),
                &crate::managed_worktree_create::HostWorktreeCreator::new(),
                &AllowAllApprovals,
                false,
            ),
        )
        .expect("managed preparation");
    assert!(
        matches!(
            outcome,
            crate::managed_worktree_prepare::ManagedWorkspacePreparation::Active { .. }
        ),
        "managed workspace did not become ACTIVE"
    );
}

#[test]
fn at_e5_072_082_external_managed_writer_reaches_host_evidence_without_touching_primary() {
    // The managed host-step chain is deep; run it on an explicit stack so the
    // test exercises the code, not the harness thread's default stack limit.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(at_e5_072_082_body)
        .unwrap()
        .join()
        .unwrap();
}

fn at_e5_072_082_body() {
    let (f, goal_id, base) = managed_fixture("e2e");
    prepare_managed_external(&f, &goal_id);
    install_grant(&f, &goal_id, &["code.txt"], 8, 8192);
    crate::model_accounting::reset_for_test();

    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    let managed_root = goal
        .execution_root()
        .expect("managed execution root")
        .to_path_buf();
    assert_ne!(managed_root, f.session.cwd);
    assert!(
        std::fs::canonicalize(&managed_root)
            .unwrap()
            .starts_with(std::fs::canonicalize(&f.managed_root).unwrap())
    );

    let content = "fn main() { println!(\"managed\"); }\n";
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    let planned = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            reviewed_writer_plan(&planner_work, content),
        ),
    )
    .unwrap();
    assert_eq!(planned["outcome"], "COMMITTED", "{planned}");

    let writer_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    assert_eq!(writer_work["role"], "WRITER", "{writer_work}");
    let staged = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &writer_work,
            writer_answer(
                &writer_work,
                "code.txt",
                content,
                json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
            ),
        ),
    )
    .unwrap();
    assert_eq!(
        staged["outcome"], "ACCEPTED_FOR_HOST_PROCESSING",
        "{staged}"
    );

    let reviewed = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(reviewed["outcome"], "MODEL_REQUIRED", "{reviewed}");
    assert_eq!(reviewed["model_work"]["role"], "REVIEWER", "{reviewed}");
    // AT-E5-077: the write happened in the managed root only.
    assert_eq!(
        std::fs::read_to_string(managed_root.join("code.txt")).unwrap(),
        content
    );
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        "fn main() {}\n"
    );

    let reviewer_work = reviewed["model_work"].clone();
    let verdict = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &reviewer_work,
            reviewer_answer(&reviewer_work, 0),
        ),
    )
    .unwrap();
    assert_eq!(verdict["outcome"], "COMMITTED", "{verdict}");
    let finished = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(finished["status"], "COMPLETED", "{finished}");

    // AT-E5-078: durable managed evidence names the exact worktree and head.
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    let evidence = goal
        .managed_workspace_verification_evidence()
        .expect("managed verification evidence");
    assert_eq!(evidence.observed_head(), base);
    assert!(!evidence.workspace_snapshot_digest().is_empty());
    let record = goal.managed_worktree().expect("worktree record");
    assert_eq!(record.worktree_root(), managed_root);

    // The result projection re-observes the managed worktree and matches the
    // durable evidence (identity is not conflated with the content snapshot).
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(goal_api::goal_result_async(
            &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
            &f.session,
            &f.store,
        ))
        .unwrap();
    let result = serde_json::to_value(result).unwrap();
    let workspace = result["managed_workspace"]
        .as_object()
        .expect("managed workspace result");
    assert_eq!(
        workspace["worktree_path"],
        managed_root.to_string_lossy().as_ref()
    );
    assert!(
        workspace["changed_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path == "code.txt"),
        "{result}"
    );

    // AT-E5-082: no automatic merge, commit, push, or new primary ref.
    assert_eq!(git(&f.session.cwd, &["rev-parse", "HEAD"]), base);
    assert_eq!(git(&f.session.cwd, &["status", "--porcelain"]), "");
    assert_eq!(git(&managed_root, &["rev-parse", "HEAD"]), base);
    assert_eq!(
        crate::model_accounting::snapshot_for_test().codex_spawn_count,
        0
    );
}

#[cfg(not(windows))]
#[test]
fn at_e5_073_the_external_driver_prepares_a_clean_managed_goal() {
    // Windows gates managed creation behind the interactive approval IPC; the
    // Unix policy has no such gate, so this drives the production driver path.
    let (f, goal_id, base) = managed_fixture("driver-prepare");
    install_grant(&f, &goal_id, &["code.txt"], 8, 8192);
    let next = goal_next(&f.session, &f.store, &goal_id);
    assert!(
        matches!(
            next["outcome"].as_str(),
            Some("HOST_PROGRESS") | Some("MODEL_REQUIRED")
        ),
        "the driver must prepare the worktree, got {next}"
    );
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    let root = goal.execution_root().expect("managed execution root");
    assert!(root.exists());
    assert_eq!(git(root, &["rev-parse", "HEAD"]), base);
    assert_eq!(git(&f.session.cwd, &["status", "--porcelain"]), "");
}

#[test]
fn at_gw_013d_goal_next_resumes_an_already_charged_operation_at_the_ceiling() {
    let (f, goal_id) = scoped_fixture("resume-ceiling", &["code.txt"], 1, 4096);
    install_grant(&f, &goal_id, &["code.txt"], 1, 4096);
    let content = "fn main() { println!(\"resume\"); }\n";
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    let submitted = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            writer_answer(
                &work,
                "code.txt",
                content,
                json!({"kind": "SHA256", "sha256": sha256_hex("fn main() {}\n")}),
            ),
        ),
    )
    .unwrap();
    assert_eq!(
        submitted["outcome"], "ACCEPTED_FOR_HOST_PROCESSING",
        "{submitted}"
    );

    // Charge the exact operation but stop before BeginOperation (the crash
    // window), leaving the one-operation grant at its ceiling.
    let goal = goal_of(&f, &goal_id);
    let task_id = crate::task::TaskId::parse(work["task_id"].as_str().unwrap()).unwrap();
    let binding = crate::external_model_store::external_state(&goal)
        .unwrap()
        .active_work()
        .and_then(|item| item.authorization())
        .cloned()
        .expect("writer WorkItem binding");
    let operation_id = goal.tasks()[&task_id]
        .latest_attempt()
        .and_then(|attempt| attempt.mutation_intent())
        .map(|intent| intent.operation_id().to_owned())
        .expect("prepared intent");
    drop(
        crate::authorization_store::acquire_publication_fence(
            &f.store,
            &goal,
            &binding,
            &format!("{operation_id}#0"),
            content.len() as u64,
            |_| Ok(()),
        )
        .unwrap(),
    );

    // The public entry point must resume this exact operation, not block the
    // Goal because the grant is at its ceiling.
    let next = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(next["outcome"], "MODEL_REQUIRED", "{next}");
    assert_eq!(next["model_work"]["role"], "REVIEWER", "{next}");
    assert_eq!(
        std::fs::read_to_string(f.session.cwd.join("code.txt")).unwrap(),
        content
    );
}

#[test]
fn at_gw_013e_a_budget_refused_writer_is_durably_blocked_instead_of_reselected() {
    let f = fixture("writer-block");
    let goal_id = start_external(&f.session, &f.store);
    let planner_work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &planner_work,
            writer_plan(&planner_work, &["code.txt"], Value::Null),
        ),
    )
    .unwrap();
    let goal = goal_of(&f, &goal_id);
    let task_id = goal
        .tasks()
        .keys()
        .next()
        .expect("planned writer task")
        .clone();
    assert_eq!(goal.tasks()[&task_id].status(), TaskStatus::Ready);

    let blocked = crate::external_publish::record_writer_authorization_block(
        &f.store,
        &f.session,
        &goal_id,
        &task_id,
        "WRITE_BUDGET_EXHAUSTED",
    )
    .unwrap();
    assert!(blocked.is_some());
    assert_eq!(
        goal_of(&f, &goal_id).tasks()[&task_id].status(),
        TaskStatus::Cancelled
    );

    // The Scheduler must not hand back the same Writer WorkItem forever.
    let next = goal_next(&f.session, &f.store, &goal_id);
    assert!(
        !(next["outcome"] == "MODEL_REQUIRED" && next["model_work"]["role"] == "WRITER"),
        "a blocked writer must not be reissued: {next}"
    );
}

#[test]
fn at_gw_025b_an_expired_grant_still_allows_read_only_progress() {
    let (f, goal_id) = scoped_fixture("expired-readonly", &["code.txt"], 4, 4096);
    install_grant(&f, &goal_id, &["code.txt"], 4, 4096);
    let before = workspace_digest(&f.session.cwd);

    let mut ledger =
        crate::authorization_store::load_ledger(&f.store, &f.session.id, goal_id.as_str())
            .unwrap()
            .unwrap();
    let now = crate::authorization::now_unix_seconds();
    ledger.force_grant_window(
        &crate::authorization::format_utc_rfc3339(now - 120),
        &crate::authorization::format_utc_rfc3339(now - 60),
    );
    crate::authorization_store::save_ledger(&f.store, &ledger).unwrap();

    // The grant cannot authorize a write, but the Planner is read-only work and
    // must still be issued: an expired grant is not a whole-Goal stop.
    let next = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(next["outcome"], "MODEL_REQUIRED", "{next}");
    assert_eq!(next["model_work"]["role"], "PLANNER", "{next}");
    assert_eq!(workspace_digest(&f.session.cwd), before);
}

#[test]
fn at_gw_025c_a_pending_writer_under_a_lapsed_grant_is_released() {
    let (f, goal_id) = scoped_fixture("pending-lapse", &["code.txt"], 4, 4096);
    install_grant(&f, &goal_id, &["code.txt"], 4, 4096);
    let work = plan_writer_goal(
        &f,
        &goal_id,
        writer_plan(
            &goal_next(&f.session, &f.store, &goal_id)["model_work"].clone(),
            &["code.txt"],
            Value::Null,
        ),
    );
    assert_eq!(work["role"], "WRITER", "{work}");

    // The grant lapses before the client submits the issued request.
    let mut ledger =
        crate::authorization_store::load_ledger(&f.store, &f.session.id, goal_id.as_str())
            .unwrap()
            .unwrap();
    let now = crate::authorization::now_unix_seconds();
    ledger.force_grant_window(
        &crate::authorization::format_utc_rfc3339(now - 120),
        &crate::authorization::format_utc_rfc3339(now - 60),
    );
    crate::authorization_store::save_ledger(&f.store, &ledger).unwrap();

    let next = goal_next(&f.session, &f.store, &goal_id);
    assert!(
        !(next["outcome"] == "MODEL_REQUIRED" && next["model_work"]["role"] == "WRITER"),
        "a lapsed writer request must not be handed back forever: {next}"
    );
    let goal = goal_of(&f, &goal_id);
    assert!(
        crate::external_model_store::external_state(&goal)
            .unwrap()
            .active_work()
            .is_none(),
        "the dead writer slot is released"
    );
    let task = goal.tasks().values().next().unwrap();
    assert_eq!(task.status(), TaskStatus::Blocked);
    assert!(
        task.blockers()
            .iter()
            .any(|blocker| blocker.code().starts_with("WRITE_AUTHORIZATION_")
                || blocker.code() == "WRITE_BUDGET_EXHAUSTED")
    );
}
