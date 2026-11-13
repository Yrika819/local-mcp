//! E5-H2 — Writer stage race audit (deterministic, barrier-driven).
//!
//! Two clients can hold the same PENDING Writer WorkItem. Before this hardening
//! the stage id was `e5-writer:<attempt_id>`, so both candidates wrote into one
//! directory and the loser's cleanup deleted the winner's linked stage. These
//! tests pin the fixed contract:
//!
//! * every submission candidate gets its own immutable stage directory;
//! * a stage is created create-only and its manifest is written last;
//! * the loser's cleanup removes only its own unlinked stage;
//! * the winner's stage, intent and content survive a restart and verify;
//! * an identical payload from two clients still never shares a stage.
//!
//! Ordering is deterministic: the test arms a checkpoint that blocks one
//! candidate right before (or right after) its stage write, runs the other
//! candidate to completion, then releases the blocked one. Both CAS directions
//! are covered.

use std::path::PathBuf;

use serde_json::{Value, json};
use sha2::Digest;
use uuid::Uuid;

use crate::config;
use crate::external_goal_api;
use crate::external_model_store::external_state;
use crate::external_writer::{self, test_hooks};
use crate::goal::{Goal, GoalId};
use crate::goal_api;
use crate::mutation::MutationIntentState;
use crate::task_store::TaskStore;
use crate::writer_stage;

struct Fixture {
    root: PathBuf,
    session: config::Session,
    store: TaskStore,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Never clear the global checkpoint registry here: another test may own a
        // gate. Each gated test serializes on `race_guard()` and clears its own
        // gates before releasing it.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The checkpoint registry is process-global, so every test that arms a gate holds
/// this guard. Held for the whole test, including the blocked-thread joins.
static RACE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn race_guard() -> std::sync::MutexGuard<'static, ()> {
    RACE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fixture(label: &str) -> Fixture {
    let root = std::env::temp_dir().join(format!("local-mcp-e5h2-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(workspace.join("code.txt"), b"fn main() {}\n").unwrap();
    let session = config::Session {
        id: format!("e5h2-{label}-{}", Uuid::new_v4()),
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
    submission_id: &str,
    response: Value,
) -> Value {
    json!({
        "session_id": session.id,
        "goal_id": goal_id.as_str(),
        "work_id": work["work_id"],
        "submission_id": submission_id,
        "expected_goal_revision": work["goal_revision"],
        "request_digest": work["request_digest"],
        "response": response,
    })
}

fn sha256_hex(text: &str) -> String {
    format!("{:x}", sha2::Sha256::digest(text.as_bytes()))
}

fn payload_digest(response: &Value) -> String {
    crate::external_model_work::payload_digest(response)
}

fn writer_plan(work: &Value) -> Value {
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
                {"kind": "FILE_DIGEST", "path": "code.txt", "expected_sha256": sha256_hex("fn main() {}\n")}
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

/// Drive one Goal to the writer WorkItem and return it.
fn reach_writer_work_item(f: &Fixture) -> (GoalId, Value) {
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
            &Uuid::new_v4().to_string(),
            writer_plan(&planner_work),
        ),
    )
    .unwrap();
    assert_eq!(planned["outcome"], "COMMITTED", "{planned}");
    let work = goal_next(&f.session, &f.store, &goal_id)["model_work"].clone();
    assert_eq!(work["role"], "WRITER", "{work}");
    (goal_id, work)
}

/// Wait (bounded) until `count` stage directories exist.
///
/// Used to synchronize with a candidate that is parked at a checkpoint: the
/// checkpoint guarantees the write already happened, so the appearance of the
/// directory is an exact hand-off signal rather than a race-prone sleep.
fn wait_for_stages(f: &Fixture, count: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if stage_dirs(f).len() >= count {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for {count} stage directories");
}

/// Collect every stage directory under the fixture state root.
fn stage_dirs(f: &Fixture) -> Vec<PathBuf> {
    let root = f.root.join("state").join("stages");
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.join("manifest.json").exists() {
                    found.push(path);
                } else {
                    stack.push(path);
                }
            }
        }
    }
    found.sort();
    found
}

fn intent_of(goal: &Goal) -> Option<crate::mutation::MutationIntent> {
    goal.tasks()
        .values()
        .filter_map(|task| task.latest_attempt())
        .filter_map(|attempt| attempt.mutation_intent().cloned())
        .next()
}

fn stage_content(f: &Fixture, stage_dir: &std::path::Path) -> String {
    let entry = std::fs::read_dir(stage_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "content"))
        .expect("staged content file");
    let _ = f;
    std::fs::read_to_string(entry).unwrap()
}

// ---------------------------------------------------------------------------
// The core race: two candidates, opposite CAS orders.
// ---------------------------------------------------------------------------

#[test]
fn a_staged_candidate_that_loses_the_cas_never_deletes_the_winners_stage() {
    // Candidate A stages first (its directory exists), then candidate B stages and
    // commits first. A then loses the CAS. The bug this pins: A's cleanup used to
    // delete the *shared* stage directory, destroying the winner's linked bytes.
    let _serial = race_guard();
    let f = fixture("staged-then-loses");
    let (goal_id, work) = reach_writer_work_item(&f);

    let a_content = "fn main() { /* candidate a */ }\n";
    let b_content = "fn main() { /* candidate b */ }\n";
    let a_answer = writer_answer(&work, a_content);
    let b_answer = writer_answer(&work, b_content);
    let a_digest = payload_digest(&a_answer);

    // Block A immediately after its stage write, so its stage exists while B runs.
    let _gate = test_hooks::arm(&a_digest, test_hooks::Phase::AfterStage);
    let a_submit = {
        let session = f.session.clone();
        let store = f.store.clone();
        let args = submit_args(
            &f.session,
            &goal_id,
            &work,
            &Uuid::new_v4().to_string(),
            a_answer.clone(),
        );
        std::thread::spawn(move || submit(&session, &store, &args))
    };

    // B commits while A is parked after staging (the directory existing proves A
    // already wrote it and is now blocked at the checkpoint).
    wait_for_stages(&f, 1);
    let b = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            &Uuid::new_v4().to_string(),
            b_answer,
        ),
    )
    .unwrap();
    assert_eq!(b["outcome"], "ACCEPTED_FOR_HOST_PROCESSING", "{b}");

    let stage_count_while_a_parked = stage_dirs(&f).len();
    assert_eq!(
        stage_count_while_a_parked, 2,
        "while A is parked both stages exist"
    );

    test_hooks::release(&a_digest, test_hooks::Phase::AfterStage);
    let a = a_submit.join().unwrap();
    test_hooks::clear();

    let a_code = match &a {
        Ok(view) => view["outcome"].as_str().unwrap_or_default().to_owned(),
        Err(code) => code.clone(),
    };
    assert_ne!(
        a_code, "ACCEPTED_FOR_HOST_PROCESSING",
        "two candidates must never both commit"
    );
    assert_eq!(a_code, "REJECTED_STALE_REVISION", "{a:?}");

    // The winner (B) owns the only surviving stage, and its bytes are B's.
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    let intent = intent_of(&goal).expect("winner intent");
    assert_eq!(intent.state(), MutationIntentState::Prepared);
    let task_id = goal.tasks().keys().next().unwrap().clone();
    let stage = external_writer::verify_prepared_stage(
        &f.store,
        &f.session,
        &goal,
        &task_id,
        intent.operation_id(),
    )
    .expect("winner stage verifies");
    assert_eq!(stage.operation_count(), 1);
    let winner_dir = writer_stage::stage_directory(
        f.store.state_root(),
        &f.session.id,
        goal_id.as_str(),
        intent.operation_id(),
    );
    assert_eq!(
        stage_content(&f, &winner_dir),
        b_content,
        "winner bytes intact"
    );
    assert_eq!(
        stage_dirs(&f).len(),
        1,
        "the losing candidate clean up only its own unlinked stage"
    );

    let state = external_state(&goal).unwrap();
    let accepted = state
        .receipts()
        .iter()
        .filter(|receipt| {
            receipt.decision()
                == crate::external_model_work::SubmissionDecision::AcceptedForHostProcessing
        })
        .count();
    assert_eq!(accepted, 1, "exactly one writer acceptance");
}

#[test]
fn a_late_candidate_is_refused_before_it_can_stage_anything() {
    // Opposite CAS direction: the first candidate commits, the second arrives with
    // a stale goal revision and is refused by the pre-check, so it never writes a
    // stage at all.
    let f = fixture("late-candidate");
    let (goal_id, work) = reach_writer_work_item(&f);
    let first = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            &Uuid::new_v4().to_string(),
            writer_answer(&work, "fn main() { /* first */ }\n"),
        ),
    )
    .unwrap();
    assert_eq!(first["outcome"], "ACCEPTED_FOR_HOST_PROCESSING");
    let stages_before = stage_dirs(&f);

    let second = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            &Uuid::new_v4().to_string(),
            writer_answer(&work, "fn main() { /* second */ }\n"),
        ),
    )
    .unwrap();
    // The WorkItem is already SUBMITTED, so the binding check refuses before the
    // revision check; either typed refusal is correct and neither writes.
    let code = second["code"].as_str().unwrap_or_default();
    assert!(
        code == "WORK_ITEM_NOT_PENDING" || code == "GOAL_REVISION_STALE",
        "unexpected late-candidate refusal: {second}"
    );
    assert!(second["receipt"].is_null());
    assert_eq!(
        stage_dirs(&f),
        stages_before,
        "a refused candidate must not create or remove a stage"
    );
}

#[test]
fn two_concurrent_candidates_stage_into_distinct_immutable_directories() {
    // Order-independent contract: whichever candidate wins, exactly one is
    // accepted, exactly one stage survives, and the surviving stage holds the
    // accepted candidate's bytes. The winner's stage id is candidate-unique
    // (attempt part + nonce), which is what makes that possible.
    let _serial = race_guard();
    let f = fixture("distinct");
    let (goal_id, work) = reach_writer_work_item(&f);
    let first_content = "fn main() { /* distinct first */ }
";
    let second_content = "fn main() { /* distinct second */ }
";
    let digest = payload_digest(&writer_answer(&work, first_content));

    let _gate = test_hooks::arm(&digest, test_hooks::Phase::AfterStage);
    let blocked = {
        let session = f.session.clone();
        let store = f.store.clone();
        let args = submit_args(
            &f.session,
            &goal_id,
            &work,
            &Uuid::new_v4().to_string(),
            writer_answer(&work, first_content),
        );
        std::thread::spawn(move || submit(&session, &store, &args))
    };
    let other = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            &Uuid::new_v4().to_string(),
            writer_answer(&work, second_content),
        ),
    );
    test_hooks::release(&digest, test_hooks::Phase::AfterStage);
    let blocked = blocked.join().unwrap();
    test_hooks::clear();

    let outcomes = [
        blocked
            .as_ref()
            .map(|view| view["outcome"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_else(|code| code.clone()),
        other
            .as_ref()
            .map(|view| view["outcome"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_else(|code| code.clone()),
    ];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.as_str() == "ACCEPTED_FOR_HOST_PROCESSING")
            .count(),
        1,
        "exactly one candidate commits: {outcomes:?}"
    );

    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    let intent = intent_of(&goal).expect("winner intent");
    assert_eq!(
        intent.operation_id().matches(':').count(),
        2,
        "the stage id must carry an attempt part and a candidate nonce: {}",
        intent.operation_id()
    );
    assert_eq!(stage_dirs(&f).len(), 1, "one immutable stage survives");
    let winner_dir = writer_stage::stage_directory(
        f.store.state_root(),
        &f.session.id,
        goal_id.as_str(),
        intent.operation_id(),
    );
    let surviving = stage_content(&f, &winner_dir);
    assert!(
        surviving == first_content || surviving == second_content,
        "the surviving stage holds one candidate's exact bytes"
    );
    // The durable intent's intended digest matches the surviving bytes.
    assert_eq!(
        intent.operations()[0].intended_after_sha256(),
        sha256_hex(&surviving)
    );
}

#[test]
fn an_identical_payload_from_two_clients_never_shares_a_stage() {
    let _serial = race_guard();
    let f = fixture("identical");
    let (goal_id, work) = reach_writer_work_item(&f);
    let answer = writer_answer(&work, "fn main() { /* same */ }\n");
    let digest = payload_digest(&answer);

    // Both candidates share this payload digest, so both park at the same gate.
    // A helper thread releases them together; the CAS then picks exactly one.
    let _gate = test_hooks::arm(&digest, test_hooks::Phase::AfterStage);
    let release_digest = digest.clone();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        test_hooks::release(&release_digest, test_hooks::Phase::AfterStage);
    });

    let first = {
        let session = f.session.clone();
        let store = f.store.clone();
        let args = submit_args(
            &f.session,
            &goal_id,
            &work,
            &Uuid::new_v4().to_string(),
            answer.clone(),
        );
        std::thread::spawn(move || submit(&session, &store, &args))
    };
    let second = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            &Uuid::new_v4().to_string(),
            answer.clone(),
        ),
    );
    let first = first.join().unwrap();
    releaser.join().unwrap();
    test_hooks::clear();

    let outcomes = [
        first
            .as_ref()
            .map(|view| view["outcome"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_else(|code| code.clone()),
        second
            .as_ref()
            .map(|view| view["outcome"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_else(|code| code.clone()),
    ];
    let accepted = outcomes
        .iter()
        .filter(|outcome| outcome.as_str() == "ACCEPTED_FOR_HOST_PROCESSING")
        .count();
    assert_eq!(
        accepted, 1,
        "exactly one candidate is accepted: {outcomes:?}"
    );
    assert_eq!(stage_dirs(&f).len(), 1, "exactly one stage survives");
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
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
        1
    );
}

#[test]
fn a_winner_stage_survives_a_restart_and_restores_the_same_content() {
    let f = fixture("restart");
    let (goal_id, work) = reach_writer_work_item(&f);
    let content = "fn main() { /* restarted */ }\n";
    let accepted = submit(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            &Uuid::new_v4().to_string(),
            writer_answer(&work, content),
        ),
    )
    .unwrap();
    assert_eq!(accepted["outcome"], "ACCEPTED_FOR_HOST_PROCESSING");

    let restarted = TaskStore::with_state_root(f.root.join("state"));
    let goal = restarted.load_goal(&f.session.id, &goal_id).unwrap();
    let intent = intent_of(&goal).unwrap();
    let task_id = goal.tasks().keys().next().unwrap().clone();
    let stage = external_writer::verify_prepared_stage(
        &restarted,
        &f.session,
        &goal,
        &task_id,
        intent.operation_id(),
    )
    .expect("stage verifies after restart");
    let dir = writer_stage::stage_directory(
        restarted.state_root(),
        &f.session.id,
        goal_id.as_str(),
        intent.operation_id(),
    );
    assert_eq!(stage_content(&f, &dir), content);
    assert_eq!(stage.operation_count(), 1);
}

#[test]
fn a_replay_never_creates_a_second_stage() {
    let f = fixture("replay");
    let (goal_id, work) = reach_writer_work_item(&f);
    let submission = Uuid::new_v4().to_string();
    let args = submit_args(
        &f.session,
        &goal_id,
        &work,
        &submission,
        writer_answer(&work, "fn main() { /* replayed */ }\n"),
    );
    let first = submit(&f.session, &f.store, &args).unwrap();
    assert_eq!(first["outcome"], "ACCEPTED_FOR_HOST_PROCESSING");
    let dirs_before = stage_dirs(&f).len();
    let replay = submit(&f.session, &f.store, &args).unwrap();
    assert_eq!(replay["outcome"], "REPLAY_OF_ACCEPTED_SUBMISSION");
    assert_eq!(stage_dirs(&f).len(), dirs_before);
}

#[test]
fn a_stage_id_is_immutable_once_sealed() {
    // Direct contract pin on the stage primitive: a second write for the same id
    // is refused and never rewrites the sealed bytes.
    let root = f_root("immutable-direct");
    let identity = crate::writer_stage::StageIdentity {
        session_id: "s".to_owned(),
        goal_id: Uuid::new_v4().to_string(),
        task_id: Uuid::new_v4().to_string(),
        attempt_id: Uuid::new_v4().to_string(),
        plan_revision: 1,
        operation_id: format!("e5-writer:{}:{}", Uuid::new_v4(), Uuid::new_v4()),
        scope_identity: "scope".to_owned(),
    };
    let stage = writer_stage::write_stage(&root, &identity, &[(0, "sealed".to_owned())]).unwrap();
    let error =
        writer_stage::write_stage(&root, &identity, &[(0, "rewritten".to_owned())]).unwrap_err();
    assert_eq!(error.code(), "WRITER_STAGE_ALREADY_EXISTS");
    let dir = writer_stage::stage_directory(&root, "s", &identity.goal_id, &identity.operation_id);
    assert_eq!(stage_content_plain(&dir), "sealed");
    assert_eq!(stage.operation_count(), 1);
    let _ = std::fs::remove_dir_all(&root);
}

fn f_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("local-mcp-e5h2-{label}-{}", Uuid::new_v4()))
}

fn stage_content_plain(stage_dir: &std::path::Path) -> String {
    let entry = std::fs::read_dir(stage_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "content"))
        .expect("staged content file");
    std::fs::read_to_string(entry).unwrap()
}
