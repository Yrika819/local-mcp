//! E3-C — External Agent Backend V1 recovery, compatibility, and security
//! regression tests.
//!
//! Three families, all deterministic and all inside an isolated temp root:
//!
//! * **Crash recovery.** A failed durable commit must leave the previous
//!   revision intact, never double-issue a WorkItem, never double-create a
//!   receipt, and never re-run an unknown side effect.
//! * **Compatibility.** Pre-E3 GoalStore documents still read as CODEX, and an
//!   EXTERNAL document whose authoritative mode field is missing is refused
//!   rather than silently downgraded.
//! * **Security.** Session isolation, no writer/approval path reachable from
//!   `goal_submit`, no file mutation, no Codex fallback, and a mechanically
//!   asserted Codex spawn count of zero.
//!
//! The process is never killed and no real repository is touched: crash points
//! are injected through the store's existing test fault hook, which is the same
//! boundary a real process death interrupts.

use std::path::PathBuf;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config;
use crate::external_goal_api;
use crate::external_model_store::{commit_stop, external_state};
use crate::external_model_work::{ExternalStop, SubmissionDecision, WorkItemState};
use crate::goal::{GOAL_SCHEMA_VERSION, Goal, GoalId, GoalStatus};
use crate::goal_api;
use crate::model_accounting;
use crate::model_backend::ModelBackend;
use crate::task_store::{FaultPoint, TaskStore, utc_now_rfc3339};

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
    let root = std::env::temp_dir().join(format!("local-mcp-e3c-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let session = config::Session {
        id: format!("e3c-{label}-{}", Uuid::new_v4()),
        cwd: workspace.clone(),
        permitted_directories: vec![workspace],
    };
    let store = TaskStore::with_state_root(state);
    Fixture {
        root,
        session,
        store,
    }
}

fn start_external(session: &config::Session, store: &TaskStore) -> GoalId {
    let view = goal_api::goal_start(
        &json!({
            "session_id": session.id,
            "objective": "Recoverable external model plan",
            "model_backend": "EXTERNAL",
        }),
        session,
        store,
    )
    .unwrap();
    let view = serde_json::to_value(view).unwrap();
    GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap()
}

fn goal_next(session: &config::Session, store: &TaskStore, goal_id: &GoalId) -> Value {
    serde_json::to_value(
        external_goal_api::goal_next(
            &json!({"session_id": session.id, "goal_id": goal_id.as_str()}),
            session,
            store,
        )
        .unwrap(),
    )
    .unwrap()
}

fn valid_proposal(work: &Value) -> Value {
    let criterion_id = work["request"]["completion_criteria"][0]["criterion_id"]
        .as_str()
        .unwrap();
    json!({
        "goal_id": work["goal_id"],
        "goal_revision": work["goal_revision"],
        "summary": "Read-only inspection",
        "tasks": [{
            "proposal_id": "inspect-1",
            "title": "Inspect",
            "objective": "Collect compact structured evidence",
            "mandatory": true,
            "worker": "CODEX_READONLY",
            "dependencies": [],
            "scope": {
                "allowed_paths": ["."],
                "forbidden_paths": [],
                "operation_kind": "READ_ONLY",
                "replay_safety": "SAFE_READ_ONLY"
            },
            "verification": [{"kind": "STRUCTURED_EVIDENCE", "requirement_id": "evidence-1"}]
        }],
        "criterion_bindings": [{"criterion_id": criterion_id, "task_refs": ["inspect-1"]}]
    })
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

fn submit_outcome(
    session: &config::Session,
    store: &TaskStore,
    args: &Value,
) -> Result<String, String> {
    match external_goal_api::goal_submit(args, session, store) {
        Ok(view) => Ok(serde_json::to_value(view).unwrap()["outcome"]
            .as_str()
            .unwrap()
            .to_owned()),
        Err(error) => Err(error.code().to_owned()),
    }
}

fn tree_digest(root: &std::path::Path) -> String {
    let mut entries = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                stack.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                let digest = format!("{:x}", Sha256::digest(&bytes));
                entries.push(format!(
                    "{}:{digest}",
                    path.strip_prefix(root).unwrap().display()
                ));
            }
        }
    }
    entries.sort();
    format!("{:x}", Sha256::digest(entries.join("\n").as_bytes()))
}

// ---------------------------------------------------------------------------
// Crash recovery.
// ---------------------------------------------------------------------------

#[test]
fn a_failed_issuance_commit_reports_no_work_item_and_leaves_the_goal_intact() {
    let f = fixture("issue-crash");
    let goal_id = start_external(&f.session, &f.store);
    let before = serde_json::to_value(f.store.load_goal(&f.session.id, &goal_id).unwrap()).unwrap();

    let failing = TaskStore::with_fault(f.root.join("state"), FaultPoint::BeforeReplace);
    let error = external_goal_api::goal_next(
        &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
        &f.session,
        &failing,
    )
    .unwrap_err();
    assert_eq!(error.code(), "GOAL_PERSISTENCE_ERROR");

    let clean = TaskStore::with_state_root(f.root.join("state"));
    let after = serde_json::to_value(clean.load_goal(&f.session.id, &goal_id).unwrap()).unwrap();
    assert_eq!(before, after, "a failed issuance must not be observable");
    // The next call issues cleanly, and it is the only WorkItem.
    let view = goal_next(&f.session, &clean, &goal_id);
    assert_eq!(view["outcome"], "MODEL_REQUIRED");
    assert_eq!(view["model_work"]["issue_counter"], 1);
}

#[test]
fn a_failed_receipt_commit_keeps_the_work_item_pending_and_the_retry_succeeds() {
    let f = fixture("receipt-crash");
    let goal_id = start_external(&f.session, &f.store);
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = next["model_work"].clone();
    let revision = f
        .store
        .load_goal(&f.session.id, &goal_id)
        .unwrap()
        .revision();
    let submission = Uuid::new_v4().to_string();
    let args = submit_args(
        &f.session,
        &goal_id,
        &work,
        &submission,
        valid_proposal(&work),
    );

    let failing = TaskStore::with_fault(f.root.join("state"), FaultPoint::BeforeReplace);
    let code = submit_outcome(&f.session, &failing, &args).unwrap_err();
    assert_eq!(code, "GOAL_PERSISTENCE_ERROR");

    let clean = TaskStore::with_state_root(f.root.join("state"));
    let goal = clean.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(goal.revision(), revision, "no partial commit is visible");
    let state = external_state(&goal).unwrap();
    assert_eq!(state.receipts().len(), 0);
    assert_eq!(state.active_work().unwrap().state(), WorkItemState::Pending);

    // The submission was never accepted, so retrying it is not a replay.
    let outcome = submit_outcome(&f.session, &clean, &args).unwrap();
    assert_eq!(outcome, "ACCEPTED_FOR_HOST_PROCESSING");
    let goal = clean.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(external_state(&goal).unwrap().receipts().len(), 1);
}

#[test]
fn a_committed_receipt_survives_a_restart_and_replays_without_new_effects() {
    let f = fixture("receipt-restart");
    let goal_id = start_external(&f.session, &f.store);
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = next["model_work"].clone();
    let submission = Uuid::new_v4().to_string();
    let args = submit_args(
        &f.session,
        &goal_id,
        &work,
        &submission,
        valid_proposal(&work),
    );
    assert_eq!(
        submit_outcome(&f.session, &f.store, &args).unwrap(),
        "ACCEPTED_FOR_HOST_PROCESSING"
    );

    let restarted = TaskStore::with_state_root(f.root.join("state"));
    let revision = restarted
        .load_goal(&f.session.id, &goal_id)
        .unwrap()
        .revision();
    assert_eq!(
        submit_outcome(&f.session, &restarted, &args).unwrap(),
        "REPLAY_OF_ACCEPTED_SUBMISSION"
    );
    let goal = restarted.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(goal.revision(), revision);
    assert_eq!(external_state(&goal).unwrap().receipts().len(), 1);
}

#[test]
fn an_issued_work_item_is_redelivered_identically_after_a_restart() {
    let f = fixture("redeliver");
    let goal_id = start_external(&f.session, &f.store);
    let first = goal_next(&f.session, &f.store, &goal_id);

    let restarted = TaskStore::with_state_root(f.root.join("state"));
    let second = goal_next(&f.session, &restarted, &goal_id);
    assert_eq!(
        second["model_work"]["work_id"],
        first["model_work"]["work_id"]
    );
    assert_eq!(
        second["model_work"]["request_digest"],
        first["model_work"]["request_digest"]
    );
    assert_eq!(second["host_steps_executed"], 0);
}

#[test]
fn concurrent_submissions_serialize_to_exactly_one_accepted_receipt() {
    let f = fixture("concurrent");
    let goal_id = start_external(&f.session, &f.store);
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = next["model_work"].clone();

    let mut handles = Vec::new();
    for _ in 0..2 {
        let session = f.session.clone();
        let store = f.store.clone();
        let args = submit_args(
            &f.session,
            &goal_id,
            &work,
            &Uuid::new_v4().to_string(),
            valid_proposal(&work),
        );
        handles.push(std::thread::spawn(move || {
            external_goal_api::goal_submit(&args, &session, &store)
                .map(|view| {
                    serde_json::to_value(view).unwrap()["outcome"]
                        .as_str()
                        .unwrap()
                        .to_owned()
                })
                .unwrap_or_else(|error| error.code().to_owned())
        }));
    }
    let outcomes: Vec<String> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let accepted = outcomes
        .iter()
        .filter(|outcome| outcome.as_str() == "ACCEPTED_FOR_HOST_PROCESSING")
        .count();
    assert_eq!(accepted, 1, "outcomes were {outcomes:?}");
    for outcome in &outcomes {
        assert!(
            !outcome.starts_with("GOAL_PERSISTENCE") && outcome != "GOAL_STATE_CORRUPT",
            "a race must be refused, not corrupt the store: {outcomes:?}"
        );
    }
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(external_state(&goal).unwrap().receipts().len(), 1);
}

#[test]
fn cancel_then_late_submission_is_refused_and_never_revives_the_goal() {
    let f = fixture("late-submit");
    let goal_id = start_external(&f.session, &f.store);
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = next["model_work"].clone();
    goal_api::goal_cancel(
        &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
        &f.session,
        &f.store,
    )
    .unwrap();

    let args = submit_args(
        &f.session,
        &goal_id,
        &work,
        &Uuid::new_v4().to_string(),
        valid_proposal(&work),
    );
    let view =
        serde_json::to_value(external_goal_api::goal_submit(&args, &f.session, &f.store).unwrap())
            .unwrap();
    assert_eq!(view["outcome"], "CANCELLED_STALE");
    assert_eq!(view["code"], "GOAL_CANCELLED");
    assert!(view["receipt"].is_null());
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(goal.status(), GoalStatus::Cancelled);
    assert!(external_state(&goal).unwrap().active_work().is_none());
    assert_eq!(external_state(&goal).unwrap().receipts().len(), 0);
}

#[test]
fn a_superseded_work_item_is_invalidated_instead_of_dead_ending_the_goal() {
    let f = fixture("revision-invalidated");
    let goal_id = start_external(&f.session, &f.store);
    let first = goal_next(&f.session, &f.store, &goal_id);
    let first_work_id = first["model_work"]["work_id"].as_str().unwrap().to_owned();

    // A host-only transition advances the Goal revision while the request is
    // outstanding, exactly as a pause/resume or recovery step would.
    let revision = f
        .store
        .load_goal(&f.session.id, &goal_id)
        .unwrap()
        .revision();
    commit_stop(
        &f.store,
        &f.session.id,
        &goal_id,
        revision,
        ExternalStop::new("HOST_TRANSITION", None, &utc_now_rfc3339()),
    )
    .unwrap();

    let progress = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(progress["outcome"], "HOST_PROGRESS");
    assert_eq!(
        progress["progress"]["code"],
        "WORK_ITEM_REVISION_INVALIDATED"
    );
    assert!(progress["model_work"].is_null());

    let fresh = goal_next(&f.session, &f.store, &goal_id);
    assert_eq!(fresh["outcome"], "MODEL_REQUIRED");
    assert_ne!(
        fresh["model_work"]["work_id"].as_str().unwrap(),
        first_work_id
    );
    assert_eq!(fresh["model_work"]["issue_counter"], 2);
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    assert_eq!(external_state(&goal).unwrap().work_history().len(), 1);
    assert_eq!(
        external_state(&goal).unwrap().work_history()[0].state(),
        WorkItemState::Cancelled
    );
}

// ---------------------------------------------------------------------------
// Compatibility.
// ---------------------------------------------------------------------------

#[test]
fn a_pre_e3_schema_five_document_migrates_to_codex_and_gains_no_external_state() {
    let f = fixture("schema5");
    let now = utc_now_rfc3339();
    let goal = Goal::new(
        f.session.id.clone(),
        f.session.cwd.clone(),
        "Legacy document",
        None,
        Vec::new(),
        Vec::new(),
        &now,
    )
    .unwrap();
    f.store.create_goal(&goal).unwrap();
    let path = f
        .store
        .goal_path_for_test(&f.session.id, goal.id())
        .unwrap();

    // Rewrite the durable document as a schema-5 record: the managed-workspace
    // verification evidence field did not exist yet.
    let mut document: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    document["schema_version"] = Value::from(5);
    document
        .as_object_mut()
        .unwrap()
        .remove("managed_workspace_verification_evidence");
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();

    let reloaded = f.store.load_goal(&f.session.id, goal.id()).unwrap();
    assert_eq!(reloaded.schema_version(), GOAL_SCHEMA_VERSION);
    assert_eq!(reloaded.model_backend(), ModelBackend::Codex);
    assert!(reloaded.external_model().is_none());
}

#[test]
fn dropping_the_mode_field_from_an_external_document_is_refused_not_downgraded() {
    let f = fixture("downgrade-guard");
    let goal_id = start_external(&f.session, &f.store);
    let _ = goal_next(&f.session, &f.store, &goal_id);
    let path = f.store.goal_path_for_test(&f.session.id, &goal_id).unwrap();
    let document: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(document["model_backend"], "EXTERNAL");
    assert!(document.get("external_model").is_some());

    // Removing only the mode key must not resurrect a CODEX Goal that silently
    // discards live external state.
    let mut stripped = document.clone();
    stripped.as_object_mut().unwrap().remove("model_backend");
    std::fs::write(&path, serde_json::to_vec(&stripped).unwrap()).unwrap();
    let error = f.store.load_goal(&f.session.id, &goal_id).unwrap_err();
    assert!(
        matches!(
            error,
            crate::orchestrator_error::OrchestratorError::CorruptGoal(_)
        ),
        "unexpected error: {error}"
    );

    // Restoring the document restores the exact durable state, including the
    // pending WorkItem.
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let restored = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    assert!(external_state(&restored).unwrap().active_work().is_some());
}

// ---------------------------------------------------------------------------
// Security.
// ---------------------------------------------------------------------------

#[test]
fn goal_submit_never_mutates_the_workspace() {
    let f = fixture("no-write");
    std::fs::write(f.session.cwd.join("tracked.txt"), b"original\n").unwrap();
    let goal_id = start_external(&f.session, &f.store);
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = next["model_work"].clone();
    let before = tree_digest(&f.session.cwd);

    let outcome = submit_outcome(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            &Uuid::new_v4().to_string(),
            valid_proposal(&work),
        ),
    )
    .unwrap();
    assert_eq!(outcome, "ACCEPTED_FOR_HOST_PROCESSING");
    assert_eq!(tree_digest(&f.session.cwd), before);
}

#[test]
fn the_external_submission_path_has_no_writer_or_approval_reachability() {
    // A source-level guard: the external modules must not name the mutation,
    // approval, or publish boundaries. `goal_submit` records intent; it never
    // gains write authority (ADR-07 / TM-03).
    for path in [
        "src/external_goal_api.rs",
        "src/external_goal_driver.rs",
        "src/external_model_store.rs",
        "src/external_model_work.rs",
    ] {
        let source = std::fs::read_to_string(path).unwrap();
        for needle in [
            "workspace_publish",
            "WriteBoundary",
            "proposed_operations",
            "approvals::",
            "without_sandbox",
        ] {
            assert!(
                !source.contains(needle),
                "{path} must not reach the host mutation/approval boundary via {needle}"
            );
        }
    }
}

#[test]
fn codex_fallback_is_refused_for_an_external_goal() {
    let f = fixture("fallback-guard");
    let now = utc_now_rfc3339();
    let codex = Goal::new(
        f.session.id.clone(),
        f.session.cwd.clone(),
        "Codex goal",
        None,
        Vec::new(),
        Vec::new(),
        &now,
    )
    .unwrap();
    let external = codex.clone().with_creation_backend(ModelBackend::External);
    assert_eq!(
        external_goal_api::codex_fallback_decision(Some(&codex)),
        external_goal_api::CodexFallbackDecision::Allowed
    );
    assert_eq!(
        external_goal_api::codex_fallback_decision(None),
        external_goal_api::CodexFallbackDecision::Allowed
    );
    assert_eq!(
        external_goal_api::codex_fallback_decision(Some(&external)),
        external_goal_api::CodexFallbackDecision::RefusedForExternalBackend
    );
}

#[test]
fn a_cross_session_goal_reference_is_indistinguishable_from_a_missing_goal() {
    let a = fixture("isolation-a");
    let goal_id = start_external(&a.session, &a.store);
    let b = config::Session {
        id: format!("e3c-isolation-b-{}", Uuid::new_v4()),
        cwd: a.session.cwd.clone(),
        permitted_directories: a.session.permitted_directories.clone(),
    };
    let foreign = external_goal_api::goal_next(
        &json!({"session_id": b.id, "goal_id": goal_id.as_str()}),
        &b,
        &a.store,
    )
    .unwrap_err();
    let missing = external_goal_api::goal_next(
        &json!({"session_id": b.id, "goal_id": Uuid::new_v4().to_string()}),
        &b,
        &a.store,
    )
    .unwrap_err();
    assert_eq!(foreign.code(), missing.code());
    assert_eq!(foreign.code(), "GOAL_NOT_FOUND");
    assert_eq!(
        serde_json::to_value(&foreign).unwrap(),
        serde_json::to_value(&missing).unwrap()
    );

    // The owner's durable state is untouched by the failed probe.
    let goal = a.store.load_goal(&a.session.id, &goal_id).unwrap();
    assert!(external_state(&goal).unwrap().active_work().is_none());
    assert_eq!(external_state(&goal).unwrap().issue_counter(), 0);
}

#[test]
fn the_external_lifecycle_never_records_a_codex_spawn_or_model_call() {
    let f = fixture("accounting");
    model_accounting::reset_for_test();
    let goal_id = start_external(&f.session, &f.store);
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = next["model_work"].clone();
    let _ = submit_outcome(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            &Uuid::new_v4().to_string(),
            json!({}),
        ),
    );
    let _ = goal_next(&f.session, &f.store, &goal_id);
    let _ = external_goal_api::external_goal_run_outcome(
        &f.store.load_goal(&f.session.id, &goal_id).unwrap(),
    );

    let snapshot = model_accounting::snapshot_for_test();
    assert_eq!(snapshot.codex_spawn_count, 0);
    assert!(
        snapshot.roles.is_empty(),
        "the external path must not reach a model transport: {:?}",
        snapshot.roles
    );
}

#[test]
fn an_accepted_submission_is_never_reported_as_a_completed_goal() {
    let f = fixture("not-success");
    let goal_id = start_external(&f.session, &f.store);
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = next["model_work"].clone();
    let view = serde_json::to_value(
        external_goal_api::goal_submit(
            &submit_args(
                &f.session,
                &goal_id,
                &work,
                &Uuid::new_v4().to_string(),
                valid_proposal(&work),
            ),
            &f.session,
            &f.store,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(view["outcome"], "ACCEPTED_FOR_HOST_PROCESSING");
    assert_eq!(view["goal_state"]["result_state"], "NOT_TERMINAL");
    assert_eq!(view["goal_state"]["result_kind"], "NONTERMINAL");
    assert_ne!(view["goal_state"]["status"], "COMPLETED");
    assert!(view["receipt"]["committed_revision"].is_null());

    let result = serde_json::to_value(
        goal_api::goal_result(
            &json!({"session_id": f.session.id, "goal_id": goal_id.as_str()}),
            &f.session,
            &f.store,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(result["result_state"], "NOT_TERMINAL");
    assert_eq!(result["result_kind"], "NONTERMINAL");
    assert_eq!(result["execution_wait"], "WAITING_FOR_HOST_APPLICATION");
}

#[test]
fn receipt_decisions_are_bounded_and_never_carry_a_committed_revision_early() {
    let f = fixture("receipt-shape");
    let goal_id = start_external(&f.session, &f.store);
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = next["model_work"].clone();
    const MARKER: &str = "SECRET_MODEL_PROSE_MUST_NOT_PERSIST";
    let outcome = submit_outcome(
        &f.session,
        &f.store,
        &submit_args(
            &f.session,
            &goal_id,
            &work,
            &Uuid::new_v4().to_string(),
            json!({"summary": MARKER, "tasks": MARKER}),
        ),
    )
    .unwrap();
    assert_eq!(outcome, "REJECTED_SCHEMA");

    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    let receipt = &external_state(&goal).unwrap().receipts()[0];
    assert_eq!(receipt.decision(), SubmissionDecision::RejectedSchema);
    assert_eq!(receipt.committed_revision(), None);
    let detail = receipt.redacted_detail().unwrap();
    assert!(detail.len() <= 512);
    assert!(!detail.contains('\u{0}'));
    // The raw response is never persisted anywhere in the durable document.
    let document = serde_json::to_string(&goal).unwrap();
    assert!(!document.contains(MARKER));
    assert!(!document.contains("SECRET"));
}

#[test]
fn goal_submit_refuses_malformed_request_boundaries() {
    let f = fixture("malformed-request");
    let goal_id = start_external(&f.session, &f.store);
    let next = goal_next(&f.session, &f.store, &goal_id);
    let work = next["model_work"].clone();

    // A response that is not JSON at all arrives as a JSON *string*, so it is
    // refused by the bounded object check instead of being parsed as a document.
    let args = submit_args(
        &f.session,
        &goal_id,
        &work,
        &Uuid::new_v4().to_string(),
        json!("{\"goal_id\": not-json"),
    );
    assert_eq!(
        submit_outcome(&f.session, &f.store, &args).unwrap(),
        "REJECTED_SCHEMA"
    );

    // A missing response field is a malformed request and is refused before any
    // durable read, so it consumes no budget and writes nothing.
    let mut missing = args.clone();
    missing.as_object_mut().unwrap().remove("response");
    assert_eq!(
        submit_outcome(&f.session, &f.store, &missing).unwrap_err(),
        "INVALID_ARGUMENT"
    );

    // An unknown request field is refused rather than ignored, so a caller can
    // not smuggle authority-bearing keys past `deny_unknown_fields`.
    let mut unknown = submit_args(
        &f.session,
        &goal_id,
        &work,
        &Uuid::new_v4().to_string(),
        valid_proposal(&work),
    );
    unknown["role"] = json!("PLANNER");
    assert_eq!(
        submit_outcome(&f.session, &f.store, &unknown).unwrap_err(),
        "INVALID_ARGUMENT"
    );

    // Only the bounded response refusal was recorded; neither malformed request
    // reached durable state at all.
    let goal = f.store.load_goal(&f.session.id, &goal_id).unwrap();
    let state = external_state(&goal).unwrap();
    assert_eq!(
        state.receipts().len(),
        1,
        "only the bounded response refusal is recorded"
    );
    assert_eq!(
        state.receipts()[0].decision(),
        SubmissionDecision::RejectedSchema
    );
    assert!(state.active_work().is_none());
}
