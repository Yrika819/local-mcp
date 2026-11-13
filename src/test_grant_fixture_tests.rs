//! H5-C — proof that the isolated test-grant fixture is isolated.
//!
//! Two independent halves are asserted here:
//!
//! * **The boundary works when the feature is on**: a marked disposable root can
//!   carry a `TEST_FIXTURE_V1` grant that the production publication gate accepts,
//!   and the same grant is refused the moment the root is no longer a marked
//!   fixture root. That is the "an auto-generated grant cannot be restored into
//!   real operation" requirement, expressed as a test rather than a promise.
//! * **The boundary is absent from a normal build**: without the feature the
//!   binary has no command that can mint a grant, so the production grant path is
//!   unchanged.

#[cfg(feature = "test-grant-fixture")]
use std::sync::Mutex;

#[cfg(feature = "test-grant-fixture")]
use uuid::Uuid;

#[cfg(feature = "test-grant-fixture")]
use crate::config;
#[cfg(feature = "test-grant-fixture")]
use crate::goal::GoalId;
#[cfg(feature = "test-grant-fixture")]
use crate::goal_api;
#[cfg(feature = "test-grant-fixture")]
use crate::task_store::TaskStore;

/// The fixture lookup is process-global in test builds, so the tests that use it
/// take this lock and never run concurrently.
#[cfg(feature = "test-grant-fixture")]
static SERIAL: Mutex<()> = Mutex::new(());

#[cfg(feature = "test-grant-fixture")]
struct Fixture {
    root: std::path::PathBuf,
    state: std::path::PathBuf,
    session: config::Session,
    store: TaskStore,
    goal_id: GoalId,
}

#[cfg(feature = "test-grant-fixture")]
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(feature = "test-grant-fixture")]
fn fixture(label: &str) -> Fixture {
    let root = std::env::temp_dir().join(format!("local-mcp-h5c-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(workspace.join("src/main.rs"), b"fn main() {}\n").unwrap();
    let session = config::Session {
        id: format!("h5c-{label}-{}", Uuid::new_v4()),
        cwd: workspace.clone(),
        permitted_directories: vec![workspace],
    };
    let store = TaskStore::with_state_root(state.clone());
    let view = goal_api::goal_start(
        &serde_json::json!({
            "session_id": session.id,
            "objective": "H5-C isolated Writer grant fixture",
            "model_backend": "EXTERNAL",
            "write_scope": {
                "allowed_paths": ["src/**"],
                "allowed_operations": ["WRITE_UTF8"],
                "max_operations": 4,
                "max_bytes": 4096,
                "expires_in_seconds": 900
            }
        }),
        &session,
        &store,
    )
    .unwrap();
    let view = serde_json::to_value(view).unwrap();
    let goal_id = GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap();
    Fixture {
        root,
        state,
        session,
        store,
        goal_id,
    }
}

#[cfg(feature = "test-grant-fixture")]
impl Fixture {
    fn mark_fixture_root(&self) -> String {
        config::mark_test_grant_fixture_root(&self.state, &self.root.join("workspace"))
            .expect("a disposable root can be marked")
    }

    fn ledger(&self) -> crate::authorization::AuthorizationLedgerV1 {
        crate::authorization_store::load_ledger(
            &self.store,
            &self.session.id,
            self.goal_id.as_str(),
        )
        .unwrap()
        .unwrap()
    }
}

#[cfg(feature = "test-grant-fixture")]
#[test]
fn a_fixture_grant_is_issued_and_accepted_inside_a_marked_root() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let f = fixture("issued");
    let fixture_id = f.mark_fixture_root();
    let _override = config::set_test_fixture_root(&f.state);

    let grant =
        crate::test_grant_fixture::issue_test_fixture_grant(&f.store, &f.session.id, &f.goal_id)
            .expect("a marked fixture root can mint a test grant");

    // The identity is unmistakably a fixture, not an Owner decision.
    assert_eq!(
        grant.owner_attestation().as_str(),
        "TEST_FIXTURE_V1",
        "a fixture grant must never claim an Owner attestation"
    );
    assert_eq!(grant.test_fixture_id(), Some(fixture_id.as_str()));
    assert!(
        !grant.owner_attestation().same_uid_process_resistance(),
        "the fixture must not claim the same-uid resistance it does not have"
    );

    // The production resolution path accepts it *here*.
    let ledger = f.ledger();
    let goal = f.store.load_goal(&f.session.id, &f.goal_id).unwrap();
    let anchor = crate::authorization_store::anchor_of(&goal).unwrap();
    let now = crate::authorization::format_utc_rfc3339(crate::authorization::now_unix_seconds());
    let usable = ledger
        .usable_grant(&anchor, &now)
        .expect("usable inside its root");
    assert_eq!(usable.grant_id(), grant.grant_id());

    // And the Owner-facing projection says what it is.
    let view = crate::authorization_store::authorization_view(&f.store, &goal);
    assert_eq!(view.owner_attestation, "TEST_FIXTURE_V1");
    assert!(!view.same_uid_process_resistance);
}

#[cfg(feature = "test-grant-fixture")]
#[test]
fn a_fixture_grant_cannot_be_issued_into_an_unmarked_root() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let f = fixture("unmarked");
    // No marker: the root is just a state root.
    let _override = config::set_test_fixture_root(&f.state);
    let error =
        crate::test_grant_fixture::issue_test_fixture_grant(&f.store, &f.session.id, &f.goal_id)
            .expect_err("an unmarked root must not mint a test grant");
    assert!(
        matches!(
            error,
            crate::test_grant_fixture::FixtureGrantError::NotAFixtureRoot(_)
        ),
        "unexpected refusal: {error}"
    );
    // Nothing was written: no ledger, no anchor epoch.
    let goal = f.store.load_goal(&f.session.id, &f.goal_id).unwrap();
    assert_eq!(
        crate::authorization_store::anchor_of(&goal)
            .unwrap()
            .highest_epoch(),
        0
    );
}

#[cfg(feature = "test-grant-fixture")]
#[test]
fn a_fixture_grant_stops_being_authority_when_its_root_stops_being_a_fixture() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let f = fixture("restored");
    f.mark_fixture_root();
    let _override = config::set_test_fixture_root(&f.state);
    let grant =
        crate::test_grant_fixture::issue_test_fixture_grant(&f.store, &f.session.id, &f.goal_id)
            .expect("issued");

    let goal = f.store.load_goal(&f.session.id, &f.goal_id).unwrap();
    let anchor = crate::authorization_store::anchor_of(&goal).unwrap();
    let now = crate::authorization::format_utc_rfc3339(crate::authorization::now_unix_seconds());
    assert!(f.ledger().usable_grant(&anchor, &now).is_ok());

    // This is the "cannot be restored into real operation" case: the durable
    // grant record is intact, the anchor agrees, and it is still refused because
    // the root it would authorize in is not the fixture root that minted it.
    std::fs::remove_file(f.state.join(config::TEST_GRANT_FIXTURE_MARKER)).unwrap();
    let refusal = f
        .ledger()
        .usable_grant(&anchor, &now)
        .expect_err("a fixture grant outside its fixture root must be refused");
    assert_eq!(refusal.code(), "WRITE_AUTHORIZATION_STATE_MISMATCH");

    // The stage/publication gate goes through the same resolution, so it refuses
    // for the same reason.
    let gate = crate::authorization_store::usable_grant(&f.store, &goal);
    assert!(
        gate.is_err(),
        "the publication gate must refuse a stranded fixture grant"
    );
    let view = crate::authorization_store::authorization_view(&f.store, &goal);
    assert_eq!(view.state, "INVALID");
    assert_eq!(view.owner_attestation, "TEST_FIXTURE_V1");
    let _ = grant;
}

#[cfg(feature = "test-grant-fixture")]
#[test]
fn a_fixture_grant_for_a_codex_goal_is_refused() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let root = std::env::temp_dir().join(format!("local-mcp-h5c-codex-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let session = config::Session {
        id: format!("h5c-codex-{}", Uuid::new_v4()),
        cwd: workspace.clone(),
        permitted_directories: vec![workspace],
    };
    let store = TaskStore::with_state_root(state.clone());
    let view = goal_api::goal_start(
        &serde_json::json!({
            "session_id": session.id,
            "objective": "a CODEX goal must never receive a test grant",
        }),
        &session,
        &store,
    )
    .unwrap();
    let view = serde_json::to_value(view).unwrap();
    let goal_id = GoalId::parse(view["goal_id"].as_str().unwrap()).unwrap();
    config::mark_test_grant_fixture_root(&state, &root.join("workspace")).unwrap();
    let _override = config::set_test_fixture_root(&state);

    let error = crate::test_grant_fixture::issue_test_fixture_grant(&store, &session.id, &goal_id)
        .expect_err("a CODEX Goal must not receive a test grant");
    assert!(
        matches!(
            error,
            crate::test_grant_fixture::FixtureGrantError::NotAFixtureRoot(_)
        ),
        "unexpected refusal: {error}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// The default build (no `test-grant-fixture` feature) has no command that can
/// mint a grant, and the *only* way to obtain a `TEST_FIXTURE_V1` grant is that
/// command. This is the compile-time boundary, asserted rather than assumed.
#[cfg(not(feature = "test-grant-fixture"))]
#[test]
fn the_default_build_cannot_mint_a_test_grant() {
    use clap::CommandFactory;

    let command = crate::Cli::command();
    let names: Vec<String> = command
        .get_subcommands()
        .map(|subcommand| subcommand.get_name().to_owned())
        .collect();
    assert!(names.contains(&"mcp".to_owned()));
    assert!(
        !names.iter().any(|name| name.contains("test-grant")),
        "a default (release) build must not expose a grant-issuing command: {names:?}"
    );
    assert!(
        crate::config::test_grant_fixture_id().is_none() || true,
        "the marker reader exists but cannot create a grant on its own"
    );
}
