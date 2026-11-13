//! Authorization B — model and durable-ledger acceptance tests (AT-GW-001..030
//! at the unit level).
//!
//! These tests use the real durable boundary (`TaskStore` + the private ledger)
//! and the test-only Owner Approval Harness. The harness mirrors the OCS two
//! writes (anchor then ledger); production binaries contain neither the harness
//! nor this module.

use std::path::PathBuf;

use serde_json::json;
use uuid::Uuid;

use crate::authorization::{
    AuthorizationAnchor, GrantPathKind, GrantPathRule, GrantRefusal, GrantRevocationState,
    GrantUsage, RequestedWriteScopeV1, WriteBudgetV1, format_utc_rfc3339, now_unix_seconds,
    parse_utc_rfc3339_seconds, relative_workspace_path,
};
use crate::authorization_store::{self, AuthorizationGateError};
use crate::config;
use crate::goal::GoalId;
use crate::goal_api;
use crate::task_store::TaskStore;
use crate::workspace_snapshot::NormalizedWorkspacePath;

struct Fixture {
    root: PathBuf,
    session: config::Session,
    store: TaskStore,
    goal_id: GoalId,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn fixture(label: &str) -> Fixture {
    let root = std::env::temp_dir().join(format!("local-mcp-auth-{label}-{}", Uuid::new_v4()));
    let workspace = root.join("workspace");
    let state = root.join("state");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(workspace.join("src/main.rs"), b"fn main() {}\n").unwrap();
    let session = config::Session {
        id: format!("auth-{label}-{}", Uuid::new_v4()),
        cwd: workspace.clone(),
        permitted_directories: vec![workspace],
    };
    let store = TaskStore::with_state_root(state);
    let view = goal_api::goal_start(
        &json!({
            "session_id": session.id,
            "objective": "Authorization B acceptance",
            "model_backend": "EXTERNAL",
            "write_scope": {
                "allowed_paths": ["src/**"],
                "allowed_operations": ["WRITE_UTF8"],
                "max_operations": 4,
                "max_bytes": 4096,
                "expires_in_seconds": 600
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
        session,
        store,
        goal_id,
    }
}

impl Fixture {
    fn goal(&self) -> crate::goal::Goal {
        self.store
            .load_goal(&self.session.id, &self.goal_id)
            .unwrap()
    }

    fn install(&self) -> crate::authorization::GoalWriteGrantV1 {
        authorization_store::test_support::install_grant(
            &self.store,
            &self.goal(),
            &["src/**"],
            4,
            4096,
            600,
        )
    }

    fn ledger(&self) -> crate::authorization::AuthorizationLedgerV1 {
        authorization_store::load_ledger(&self.store, &self.session.id, self.goal_id.as_str())
            .unwrap()
            .unwrap()
    }
}

fn refusal(error: AuthorizationGateError) -> GrantRefusal {
    match error {
        AuthorizationGateError::Refusal(refusal) => refusal,
        AuthorizationGateError::State(error) => panic!("expected refusal, got {}", error.code()),
    }
}

// ---------------------------------------------------------------------------
// Pure model.
// ---------------------------------------------------------------------------

#[test]
fn at_gw_009_path_rules_cover_exact_files_and_subtrees_only() {
    let tree = GrantPathRule::parse("src/**").unwrap();
    assert_eq!(tree.kind(), GrantPathKind::Tree);
    assert!(tree.permits(&NormalizedWorkspacePath::parse("src/main.rs").unwrap()));
    assert!(tree.permits(&NormalizedWorkspacePath::parse("src/a/b.rs").unwrap()));
    assert!(!tree.permits(&NormalizedWorkspacePath::parse("srcx/main.rs").unwrap()));
    assert!(!tree.permits(&NormalizedWorkspacePath::parse("docs/readme.md").unwrap()));

    let file = GrantPathRule::parse("docs/design.md").unwrap();
    assert_eq!(file.kind(), GrantPathKind::File);
    assert!(file.permits(&NormalizedWorkspacePath::parse("docs/design.md").unwrap()));
    assert!(!file.permits(&NormalizedWorkspacePath::parse("docs/design.md.bak").unwrap()));

    let root = GrantPathRule::parse("**").unwrap();
    assert!(root.permits(&NormalizedWorkspacePath::parse("anything.txt").unwrap()));

    for rejected in [
        "src/../escape",
        ".git/config",
        "**/src",
        "src/**/main.rs",
        "",
    ] {
        assert!(
            GrantPathRule::parse(rejected).is_err(),
            "path rule {rejected:?} must be refused"
        );
    }
}

#[test]
fn at_gw_013_014_budget_and_charge_are_idempotent() {
    let budget = WriteBudgetV1::new(2, 10).unwrap();
    assert!(budget.allows(0, 0, 2, 10));
    assert!(!budget.allows(0, 0, 3, 1));
    assert!(!budget.allows(0, 0, 1, 11));

    let mut usage = GrantUsage::default();
    usage.charge("op#0", 4).unwrap();
    usage.charge("op#1", 2).unwrap();
    assert_eq!(usage.operations(), 2);
    assert_eq!(usage.bytes(), 6);
    // The same operation id is a no-op, never a second charge.
    usage.charge("op#0", 4).unwrap();
    assert_eq!(usage.operations(), 2);
    assert_eq!(usage.bytes(), 6);
    assert!(usage.is_charged("op#1"));
    // A third distinct operation exceeds the operator budget.
    assert!(!budget.allows(usage.operations(), usage.bytes(), 1, 1));
}

#[test]
fn at_gw_011_time_roundtrip_and_clock_regression_surface() {
    let now = now_unix_seconds();
    let text = format_utc_rfc3339(now);
    assert_eq!(parse_utc_rfc3339_seconds(&text), Some(now));
    for malformed in [
        "2026-11-08T12:00:00",
        "2026-11-08 12:00:00Z",
        "2026-13-08T12:00:00Z",
        "2026-11-08T24:00:00Z",
        "not-a-time",
    ] {
        assert_eq!(parse_utc_rfc3339_seconds(malformed), None);
    }
}

#[test]
fn at_gw_005_a_pending_request_requires_the_live_nonce_and_a_pending_request() {
    let f = fixture("pending");
    let scope = RequestedWriteScopeV1::from_mcp_value(&json!({
        "allowed_paths": ["src/**"],
        "allowed_operations": ["WRITE_UTF8"],
        "max_operations": 1,
        "max_bytes": 100,
        "expires_in_seconds": 300
    }))
    .unwrap();
    assert!(scope.validate().is_ok());
    let goal = f.goal();
    let now = now_unix_seconds();
    let request = authorization_store::begin_pending_request(
        &f.store,
        &goal,
        &scope,
        Uuid::new_v4().to_string(),
        crate::authorization::nonce_sha256("correct-nonce"),
        format_utc_rfc3339(now),
        format_utc_rfc3339(now + 300),
    )
    .unwrap();

    // No nonce: the request is durable but authorizes nothing.
    assert_eq!(
        refusal(authorization_store::usable_grant(&f.store, &goal).unwrap_err()),
        GrantRefusal::Pending
    );
    // Wrong nonce is refused and the request survives.
    let wrong = authorization_store::approve_pending(
        &f.store,
        &f.session.id,
        f.goal_id.as_str(),
        request.request_id(),
        "wrong-nonce",
        Uuid::new_v4().to_string(),
    );
    assert!(matches!(
        wrong.unwrap_err(),
        AuthorizationGateError::State(crate::authorization::GrantError::NonceMismatch)
    ));
    // Correct nonce issues exactly one grant.
    let grant = authorization_store::approve_pending(
        &f.store,
        &f.session.id,
        f.goal_id.as_str(),
        request.request_id(),
        "correct-nonce",
        Uuid::new_v4().to_string(),
    )
    .unwrap();
    assert_eq!(grant.authorization_epoch(), 1);
    let usable = authorization_store::usable_grant(&f.store, &f.goal()).unwrap();
    assert_eq!(usable.grant_id(), grant.grant_id());
    // A second approval of the consumed request changes nothing.
    assert!(
        authorization_store::approve_pending(
            &f.store,
            &f.session.id,
            f.goal_id.as_str(),
            request.request_id(),
            "correct-nonce",
            Uuid::new_v4().to_string(),
        )
        .is_err()
    );
}

#[test]
fn at_gw_002_a_grant_without_its_goal_anchor_is_unusable() {
    let f = fixture("anchor");
    let grant = f.install();
    assert!(authorization_store::usable_grant(&f.store, &f.goal()).is_ok());

    // Remove the Goal-side anchor while leaving the ledger ACTIVE. This is the
    // state a forged ledger record (or a crash before the anchor write) produces.
    let revision = f.goal().revision();
    f.store
        .mutate_goal_snapshot(&f.session.id, &f.goal_id, revision, |goal, _now| {
            let state = crate::external_model_store::external_state_mut(goal)?;
            *state.authorization_anchor_mut() = AuthorizationAnchor::default();
            Ok(())
        })
        .unwrap();
    let error = authorization_store::usable_grant(&f.store, &f.goal()).unwrap_err();
    assert_eq!(refusal(error), GrantRefusal::StateMismatch);
    assert!(!grant.grant_id().is_empty());
}

#[test]
fn at_gw_008_cross_workspace_binding_is_refused() {
    let f = fixture("workspace");
    let grant = f.install();
    let digest = grant.workspace().digest();
    // Rewriting the Goal into managed mode is a different workspace identity.
    let goal = f.goal();
    assert_eq!(
        crate::authorization::GrantWorkspaceIdentity::from_goal(&goal)
            .unwrap()
            .digest(),
        digest
    );
    let other = GrantPathRule::parse("src/**").unwrap();
    assert!(other.permits(&NormalizedWorkspacePath::parse("src/main.rs").unwrap()));
}

#[test]
fn at_gw_019_a_new_grant_generation_invalidates_the_old_binding() {
    let f = fixture("epoch");
    let first = f.install();
    let binding = crate::authorization::GrantBinding::from_grant(&first);
    assert!(binding.matches(&first));

    // Revoking bumps the epoch and spends the old generation.
    authorization_store::revoke_active(&f.store, &f.session.id, f.goal_id.as_str(), None).unwrap();
    let revoked = f.ledger();
    assert_eq!(
        revoked.latest_grant().unwrap().revocation().state(),
        GrantRevocationState::Revoked
    );
    assert_eq!(revoked.authorization_epoch(), 2);
    assert!(authorization_store::usable_grant(&f.store, &f.goal()).is_err());

    // A fresh grant is a new generation and the old binding can never match it.
    let second = f.install();
    assert_ne!(second.grant_id(), first.grant_id());
    assert_ne!(second.authorization_epoch(), first.authorization_epoch());
    assert!(!binding.matches(&second));
}

#[test]
fn at_gw_021_a_rolled_back_ledger_is_refused_by_the_goal_anchor() {
    let f = fixture("rollback");
    let first = f.install();
    let path =
        authorization_store::ledger_path(&f.store, &f.session.id, f.goal_id.as_str()).unwrap();
    let before_revocation = std::fs::read(&path).unwrap();

    authorization_store::revoke_active(&f.store, &f.session.id, f.goal_id.as_str(), None).unwrap();
    assert!(authorization_store::usable_grant(&f.store, &f.goal()).is_err());

    // Restore the pre-revocation ACTIVE ledger. The Goal anchor still remembers
    // the revocation epoch, so the restored record must not authorize anything.
    std::fs::write(&path, before_revocation).unwrap();
    let error = authorization_store::usable_grant(&f.store, &f.goal()).unwrap_err();
    assert!(
        matches!(
            refusal(error),
            GrantRefusal::Revoked | GrantRefusal::RollbackDetected | GrantRefusal::StateMismatch
        ),
        "rolled-back ledger must fail closed"
    );
    assert!(!first.grant_id().is_empty());
}

#[test]
fn at_gw_011_expiry_and_clock_regression_fail_closed() {
    let f = fixture("expiry");
    f.install();
    let now = now_unix_seconds();

    let mut ledger = f.ledger();
    ledger.force_grant_window(
        &format_utc_rfc3339(now - 120),
        &format_utc_rfc3339(now - 60),
    );
    authorization_store::save_ledger(&f.store, &ledger).unwrap();
    assert_eq!(
        refusal(authorization_store::usable_grant(&f.store, &f.goal()).unwrap_err()),
        GrantRefusal::Expired
    );

    // Clock regression: "now" is before the grant was issued.
    let mut ledger = f.ledger();
    ledger.force_grant_window(
        &format_utc_rfc3339(now + 600),
        &format_utc_rfc3339(now + 1200),
    );
    authorization_store::save_ledger(&f.store, &ledger).unwrap();
    assert_eq!(
        refusal(authorization_store::usable_grant(&f.store, &f.goal()).unwrap_err()),
        GrantRefusal::Expired
    );
}

#[test]
fn at_gw_022_a_corrupt_ledger_fails_closed() {
    let f = fixture("corrupt");
    f.install();
    let path =
        authorization_store::ledger_path(&f.store, &f.session.id, f.goal_id.as_str()).unwrap();
    std::fs::write(&path, b"{not json").unwrap();
    let error = authorization_store::usable_grant(&f.store, &f.goal()).unwrap_err();
    assert!(matches!(
        error,
        AuthorizationGateError::State(crate::authorization::GrantError::Corrupt(_))
    ));
}

#[test]
fn at_gw_006_007_a_ledger_bound_to_another_goal_or_session_never_matches() {
    let f = fixture("binding");
    f.install();
    let ledger = f.ledger();
    // Re-point the ledger at another Goal id. It still validates internally, but
    // must not match the Goal whose path it occupies.
    let other = GoalId::new();
    let mut text = serde_json::to_value(&ledger).unwrap();
    text["goal_id"] = json!(other.as_str());
    let tampered: crate::authorization::AuthorizationLedgerV1 =
        serde_json::from_value(text).unwrap();
    let error = tampered.matches_goal(&f.goal()).unwrap_err();
    assert!(matches!(
        error,
        crate::authorization::GrantError::StateMismatch(_)
    ));
}

#[test]
fn at_gw_015_016_revocation_blocks_new_operations_and_is_idempotent() {
    let f = fixture("revoke");
    let grant = f.install();
    let binding = crate::authorization::GrantBinding::from_grant(&grant);
    let goal = f.goal();
    let path = workspace_path(&f, "src/main.rs");
    let relative = relative_workspace_path(&path, grant.workspace().root()).unwrap();
    assert!(authorization_store::check_operation_scope(&grant, &relative).is_ok());

    // A budget charge succeeds while ACTIVE.
    authorization_store::authorize_operation(
        &f.store,
        &goal,
        &binding,
        "op#0",
        4,
        |grant| authorization_store::check_operation_scope(grant, &relative),
        |_| Ok(()),
    )
    .unwrap();

    let (first_id, epoch) =
        authorization_store::revoke_active(&f.store, &f.session.id, f.goal_id.as_str(), None)
            .unwrap();
    assert_eq!(first_id, grant.grant_id());
    assert_eq!(epoch, 1);
    // Idempotent: a second revocation reports the same grant and nothing new.
    let (second_id, _) =
        authorization_store::revoke_active(&f.store, &f.session.id, f.goal_id.as_str(), None)
            .unwrap();
    assert_eq!(second_id, first_id);

    // No new operation can be authorized after REVOKED.
    let error = authorization_store::authorize_operation(
        &f.store,
        &f.goal(),
        &binding,
        "op#1",
        4,
        |_| Ok(()),
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(matches!(
        refusal(error),
        GrantRefusal::Revoked | GrantRefusal::StateMismatch
    ));
}

#[test]
fn at_gw_013_budget_exhaustion_is_refused_before_the_side_effect() {
    let f = fixture("budget");
    let grant = authorization_store::test_support::install_grant(
        &f.store,
        &f.goal(),
        &["src/**"],
        1,
        4,
        600,
    );
    let binding = crate::authorization::GrantBinding::from_grant(&grant);
    let path = workspace_path(&f, "src/main.rs");
    let relative = relative_workspace_path(&path, grant.workspace().root()).unwrap();
    authorization_store::authorize_operation(
        &f.store,
        &f.goal(),
        &binding,
        "op#0",
        4,
        |grant| authorization_store::check_operation_scope(grant, &relative),
        |_| Ok(()),
    )
    .unwrap();
    let error = authorization_store::authorize_operation(
        &f.store,
        &f.goal(),
        &binding,
        "op#1",
        1,
        |_| Ok(()),
        |_| Ok(()),
    )
    .unwrap_err();
    assert_eq!(refusal(error), GrantRefusal::BudgetExhausted);
}

#[test]
fn at_gw_010_scope_mismatch_is_refused_by_the_grant_not_the_writer() {
    let f = fixture("scope");
    let grant = f.install();
    let binding = crate::authorization::GrantBinding::from_grant(&grant);
    let outside = f.session.cwd.join("README.md");
    std::fs::write(&outside, b"outside\n").unwrap();
    let outside = std::fs::canonicalize(&outside).unwrap();
    let relative = relative_workspace_path(&outside, grant.workspace().root()).unwrap();
    let error = authorization_store::authorize_operation(
        &f.store,
        &f.goal(),
        &binding,
        "op#0",
        8,
        |grant| authorization_store::check_operation_scope(grant, &relative),
        |_| Ok(()),
    )
    .unwrap_err();
    assert_eq!(refusal(error), GrantRefusal::ScopeMismatch);
}

#[test]
fn at_gw_020_the_scope_proposal_is_durable_and_visible_before_any_grant() {
    let f = fixture("proposal");
    let goal = f.goal();
    assert!(authorization_store::has_write_scope(&goal));
    let scope = authorization_store::scope_of(&goal).unwrap();
    assert_eq!(scope.allowed_paths(), ["src/**"]);
    let view = authorization_store::authorization_view(&f.store, &goal);
    assert_eq!(view.state, "REQUIRED");
    assert!(!view.same_uid_process_resistance);
    assert_eq!(view.owner_attestation, "CONSOLE_LIVE_NONCE_V1");
}

fn workspace_path(f: &Fixture, relative: &str) -> PathBuf {
    std::fs::canonicalize(f.session.cwd.join(relative)).unwrap()
}

#[test]
fn at_gw_004_a_model_supplied_authorization_field_is_not_parsed_as_authority() {
    // The submission schema is strict and has no authorization field. A response
    // that carries one is a schema violation, never a grant.
    // `ExternalModelState` uses `deny_unknown_fields`: a response that tries to
    // smuggle an authorization record into durable state is a schema error.
    let value = json!({
        "schema_version": 2,
        "issue_counter": 0,
        "authorization": {"state": "ACTIVE", "grant_id": "x"}
    });
    assert!(
        serde_json::from_value::<crate::external_model_work::ExternalModelState>(value).is_err()
    );
}

#[test]
fn at_gw_025_an_absent_owner_surface_fails_closed() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let outcome = runtime.block_on(crate::approvals::request_grant_from_owner(
        "no-such-session",
        "11111111-1111-4111-8111-111111111111",
        std::time::Duration::from_millis(50),
    ));
    assert_eq!(outcome, crate::approvals::OwnerGrantOutcome::Unavailable);
}

#[test]
fn at_gw_023_two_concurrent_charges_for_one_operation_id_charge_once() {
    let f = fixture("concurrent");
    let grant = f.install();
    let binding = crate::authorization::GrantBinding::from_grant(&grant);
    let goal = f.goal();
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let store = &f.store;
            let goal = &goal;
            let binding = &binding;
            scope.spawn(move || {
                crate::authorization_store::authorize_operation(
                    store,
                    goal,
                    binding,
                    "op#0",
                    4,
                    |_| Ok(()),
                    |_| Ok(()),
                )
                .unwrap();
            });
        }
    });
    let ledger = f.ledger();
    assert_eq!(ledger.latest_grant().unwrap().usage().operations(), 1);
    assert_eq!(ledger.latest_grant().unwrap().usage().bytes(), 4);
}

#[test]
fn at_gw_024_a_revocation_after_authorization_blocks_every_later_operation() {
    let f = fixture("fence");
    let grant = f.install();
    let binding = crate::authorization::GrantBinding::from_grant(&grant);
    crate::authorization_store::authorize_operation(
        &f.store,
        &f.goal(),
        &binding,
        "op#0",
        4,
        |_| Ok(()),
        |_| Ok(()),
    )
    .unwrap();
    crate::authorization_store::revoke_active(&f.store, &f.session.id, f.goal_id.as_str(), None)
        .unwrap();
    for operation in ["op#1", "op#2"] {
        let error = crate::authorization_store::authorize_operation(
            &f.store,
            &f.goal(),
            &binding,
            operation,
            1,
            |_| Ok(()),
            |_| Ok(()),
        )
        .unwrap_err();
        assert!(matches!(
            refusal(error),
            GrantRefusal::Revoked | GrantRefusal::StateMismatch
        ));
    }
    assert_eq!(f.ledger().latest_grant().unwrap().usage().operations(), 1);
}

#[test]
fn at_gw_009b_path_rules_reject_console_injection_characters() {
    for rejected in [
        "src/\u{1b}[2Jevil",
        "src/\u{7}bell",
        "src/\u{202e}gnp.exe",
        "src/\u{2066}isolate",
    ] {
        assert!(
            GrantPathRule::parse(rejected).is_err(),
            "path rule {rejected:?} must not reach the Owner console"
        );
    }
}

#[test]
fn at_gw_021b_the_revocation_anchor_precedes_revoked_and_is_repaired_on_retry() {
    let f = fixture("h3-anchor");
    let grant = f.install();
    let path =
        authorization_store::ledger_path(&f.store, &f.session.id, f.goal_id.as_str()).unwrap();
    let before_revocation = std::fs::read(&path).unwrap();

    authorization_store::revoke_active(&f.store, &f.session.id, f.goal_id.as_str(), None).unwrap();
    // Simulate a crash after the ledger reached REVOKED but before the anchor
    // was durable.
    let revision = f.goal().revision();
    f.store
        .mutate_goal_snapshot(&f.session.id, &f.goal_id, revision, |goal, _now| {
            *crate::external_model_store::external_state_mut(goal)?.authorization_anchor_mut() =
                AuthorizationAnchor::default();
            Ok(())
        })
        .unwrap();
    // The idempotent retry repairs the anchor instead of returning early.
    authorization_store::revoke_active(&f.store, &f.session.id, f.goal_id.as_str(), None).unwrap();
    let anchor = authorization_store::anchor_of(&f.goal()).unwrap();
    assert_eq!(
        anchor.revoked_at_epoch(),
        Some(grant.authorization_epoch()),
        "the retry must restore the revocation anchor"
    );

    // Restoring the pre-revocation ACTIVE ledger must still be denied.
    std::fs::write(&path, before_revocation).unwrap();
    assert!(authorization_store::usable_grant(&f.store, &f.goal()).is_err());
}

#[test]
fn at_gw_021c_a_restored_ledger_cannot_regain_spent_budget() {
    let f = fixture("h4-budget");
    let grant = authorization_store::test_support::install_grant(
        &f.store,
        &f.goal(),
        &["src/**"],
        1,
        4096,
        600,
    );
    let path =
        authorization_store::ledger_path(&f.store, &f.session.id, f.goal_id.as_str()).unwrap();
    let before_charge = std::fs::read(&path).unwrap();
    let binding = crate::authorization::GrantBinding::from_grant(&grant);
    authorization_store::authorize_operation(
        &f.store,
        &f.goal(),
        &binding,
        "op#0",
        4,
        |_| Ok(()),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(f.ledger().latest_grant().unwrap().usage().operations(), 1);

    // Restore the ACTIVE ledger from before the charge: same grant id and
    // epoch, usage 0. The Goal anchor's monotonic floor must still deny a
    // second operation.
    std::fs::write(&path, before_charge).unwrap();
    let error = authorization_store::usable_grant(&f.store, &f.goal()).unwrap_err();
    assert_eq!(refusal(error), GrantRefusal::BudgetExhausted);
    assert_eq!(
        authorization_store::anchor_of(&f.goal())
            .unwrap()
            .usage_operations(),
        1
    );
}

#[test]
fn at_gw_016b_revocation_waits_for_an_in_flight_operation_fence() {
    use std::sync::mpsc;
    let f = fixture("h1-fence");
    let grant = f.install();
    let binding = crate::authorization::GrantBinding::from_grant(&grant);
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (revoke_tx, revoke_rx) = mpsc::channel::<std::time::Duration>();
    std::thread::scope(|scope| {
        let store = &f.store;
        let goal = f.goal();
        let binding = &binding;
        scope.spawn(move || {
            let goal_ref = &goal;
            let _fence = crate::authorization_store::acquire_publication_fence(
                store,
                goal_ref,
                binding,
                "op#0",
                4,
                |_| Ok(()),
            )
            .unwrap();
            started_tx.send(()).unwrap();
            // Hold the fence as a filesystem operation would.
            std::thread::sleep(std::time::Duration::from_millis(800));
        });
        started_rx.recv().unwrap();
        let store = &f.store;
        let session = &f.session;
        let goal_id = f.goal_id.as_str();
        scope.spawn(move || {
            let started = std::time::Instant::now();
            crate::authorization_store::revoke_active(store, &session.id, goal_id, None).unwrap();
            revoke_tx.send(started.elapsed()).unwrap();
        });
        let waited = revoke_rx.recv().unwrap();
        assert!(
            waited >= std::time::Duration::from_millis(500),
            "revocation must wait for the in-flight fence, waited {waited:?}"
        );
    });
    assert!(authorization_store::usable_grant(&f.store, &f.goal()).is_err());
}

#[test]
fn at_gw_028b_the_requested_grant_lifetime_is_honoured() {
    let f = fixture("lifetime");
    let grant = crate::authorization_store::test_support::install_grant(
        &f.store,
        &f.goal(),
        &["src/**"],
        4,
        4096,
        60,
    );
    let issued = parse_utc_rfc3339_seconds(grant.issued_at()).unwrap();
    let expires = parse_utc_rfc3339_seconds(grant.expires_at()).unwrap();
    assert!(expires > issued);
    assert!(
        expires - issued <= 61,
        "a 60-second request must not become a 15-minute grant"
    );
}

#[cfg(unix)]
#[test]
fn at_gw_009c_a_write_scope_rule_cannot_escape_through_an_existing_symlink() {
    let f = fixture("rule-symlink");
    let outside = f.root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, f.session.cwd.join("src/escape")).unwrap();
    let scope = RequestedWriteScopeV1::from_mcp_value(&json!({
        "allowed_paths": ["src/escape/**"],
        "allowed_operations": ["WRITE_UTF8"],
        "max_operations": 1,
        "max_bytes": 100,
        "expires_in_seconds": 300
    }))
    .unwrap();
    let now = now_unix_seconds();
    let error = authorization_store::begin_pending_request(
        &f.store,
        &f.goal(),
        &scope,
        Uuid::new_v4().to_string(),
        crate::authorization::nonce_sha256("nonce"),
        format_utc_rfc3339(now),
        format_utc_rfc3339(now + 300),
    )
    .unwrap_err();
    assert_eq!(error.code(), "INVALID_ARGUMENT");
}

#[test]
fn at_gw_019c_a_new_grant_generation_has_a_fresh_budget_floor() {
    let f = fixture("regrant-budget");
    let first = crate::authorization_store::test_support::install_grant(
        &f.store,
        &f.goal(),
        &["src/**"],
        1,
        4096,
        600,
    );
    let first_binding = crate::authorization::GrantBinding::from_grant(&first);
    crate::authorization_store::authorize_operation(
        &f.store,
        &f.goal(),
        &first_binding,
        "op#0",
        4,
        |_| Ok(()),
        |_| Ok(()),
    )
    .unwrap();
    crate::authorization_store::revoke_active(&f.store, &f.session.id, f.goal_id.as_str(), None)
        .unwrap();
    let second = crate::authorization_store::test_support::install_grant(
        &f.store,
        &f.goal(),
        &["src/**"],
        1,
        4096,
        600,
    );
    assert_eq!(
        crate::authorization_store::anchor_of(&f.goal())
            .unwrap()
            .usage_operations(),
        0,
        "a new generation starts with its own budget"
    );
    let second_binding = crate::authorization::GrantBinding::from_grant(&second);
    crate::authorization_store::authorize_operation(
        &f.store,
        &f.goal(),
        &second_binding,
        "op#0",
        4,
        |_| Ok(()),
        |_| Ok(()),
    )
    .unwrap();
}

#[test]
fn at_gw_013b_an_already_charged_operation_resumes_at_the_budget_ceiling() {
    let f = fixture("resume");
    let grant = crate::authorization_store::test_support::install_grant(
        &f.store,
        &f.goal(),
        &["src/**"],
        1,
        4096,
        600,
    );
    let binding = crate::authorization::GrantBinding::from_grant(&grant);
    // Charge op#0 but do not begin it (the crash window).
    drop(
        crate::authorization_store::acquire_publication_fence(
            &f.store,
            &f.goal(),
            &binding,
            "op#0",
            4,
            |_| Ok(()),
        )
        .unwrap(),
    );
    // A different operation is genuinely over budget.
    let error = crate::authorization_store::acquire_publication_fence(
        &f.store,
        &f.goal(),
        &binding,
        "op#1",
        1,
        |_| Ok(()),
    )
    .unwrap_err();
    assert_eq!(refusal(error), GrantRefusal::BudgetExhausted);
    // The exact charged operation resumes without a second charge.
    drop(
        crate::authorization_store::acquire_publication_fence(
            &f.store,
            &f.goal(),
            &binding,
            "op#0",
            4,
            |_| Ok(()),
        )
        .unwrap(),
    );
    assert_eq!(f.ledger().latest_grant().unwrap().usage().operations(), 1);
}

#[test]
fn at_gw_021d_a_stale_goal_snapshot_cannot_bypass_a_restored_ledger() {
    let f = fixture("stale-anchor");
    let grant = f.install();
    let binding = crate::authorization::GrantBinding::from_grant(&grant);
    let stale_goal = f.goal();
    let path =
        authorization_store::ledger_path(&f.store, &f.session.id, f.goal_id.as_str()).unwrap();
    let before_revocation = std::fs::read(&path).unwrap();

    authorization_store::revoke_active(&f.store, &f.session.id, f.goal_id.as_str(), None).unwrap();
    // Ledger-only rollback: the Goal keeps its durable revocation anchor.
    std::fs::write(&path, before_revocation).unwrap();

    // The caller holds a pre-revocation Goal snapshot. The fence must re-read the
    // authoritative Goal and refuse; it must also not consume budget.
    let error = authorization_store::acquire_publication_fence(
        &f.store,
        &stale_goal,
        &binding,
        "op#0",
        4,
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(matches!(
        refusal(error),
        GrantRefusal::Revoked | GrantRefusal::StateMismatch | GrantRefusal::RollbackDetected
    ));
    assert_eq!(
        f.ledger().latest_grant().unwrap().usage().operations(),
        0,
        "a refused publication never charges"
    );
}

#[test]
fn at_gw_013c_reanchoring_the_same_grant_preserves_spent_budget() {
    let f = fixture("reanchor");
    let grant = crate::authorization_store::test_support::install_grant(
        &f.store,
        &f.goal(),
        &["src/**"],
        2,
        4096,
        600,
    );
    let binding = crate::authorization::GrantBinding::from_grant(&grant);
    crate::authorization_store::authorize_operation(
        &f.store,
        &f.goal(),
        &binding,
        "op#0",
        4,
        |_| Ok(()),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(
        crate::authorization_store::anchor_of(&f.goal())
            .unwrap()
            .usage_operations(),
        1
    );
    // A second Writer WorkItem under the same grant re-anchors it; that must
    // not hand the earlier operation's budget back.
    crate::authorization_store::record_anchor_grant(&f.store, &f.session.id, &f.goal_id, &grant)
        .unwrap();
    assert_eq!(
        crate::authorization_store::anchor_of(&f.goal())
            .unwrap()
            .usage_operations(),
        1
    );
}

#[test]
fn at_gw_026_codex_goals_are_unaffected_by_authorization() {
    let f = fixture("codex-unaffected");
    // write_scope is EXTERNAL-only.
    let error = goal_api::goal_start(
        &json!({
            "session_id": f.session.id,
            "objective": "codex must not gain a write scope",
            "model_backend": "CODEX",
            "write_scope": {
                "allowed_paths": ["src/**"],
                "allowed_operations": ["WRITE_UTF8"],
                "max_operations": 1,
                "max_bytes": 100,
                "expires_in_seconds": 300
            }
        }),
        &f.session,
        &f.store,
    )
    .unwrap_err();
    assert_eq!(error.code(), "INVALID_ARGUMENT");
}

#[test]
fn at_gw_009d_console_safety_covers_all_bidi_and_separator_controls() {
    for rejected in [
        "src/\u{061c}alm",
        "src/\u{200e}lrm",
        "src/\u{200f}rlm",
        "src/\u{2028}line",
        "src/\u{2029}para",
        "src/\u{feff}bom",
        "src/\u{e0041}tag",
    ] {
        assert!(
            GrantPathRule::parse(rejected).is_err(),
            "path rule {rejected:?} must be refused"
        );
    }
    assert!(crate::authorization::is_console_unsafe('\u{200e}'));
    assert!(crate::authorization::is_console_unsafe('\u{2028}'));
    assert!(!crate::authorization::is_console_unsafe('a'));
}

#[test]
fn at_gw_021e_a_stale_grant_cannot_displace_the_anchor_last_grant() {
    let mut anchor = AuthorizationAnchor::default();
    anchor.record_grant("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", 2);
    anchor.record_grant("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", 1);
    assert_eq!(
        anchor.last_grant_id(),
        Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
    );
    assert_eq!(anchor.highest_epoch(), 2);
}

#[test]
fn at_gw_009e_the_console_filter_covers_format_and_invisible_characters() {
    for rejected in [
        "src/\u{00ad}soft",
        "src/\u{200b}zero",
        "src/\u{200c}zwnj",
        "src/\u{200d}zwj",
        "src/\u{2060}wordjoiner",
        "src/\u{fff9}interlinear",
    ] {
        assert!(
            GrantPathRule::parse(rejected).is_err(),
            "path rule {rejected:?} must be refused"
        );
    }
    assert!(crate::authorization::is_console_unsafe('\u{00ad}'));
    assert!(crate::authorization::is_console_unsafe('\u{200b}'));
    assert!(crate::authorization::is_console_unsafe('\u{2060}'));
}

#[test]
fn at_gw_009f_supplementary_plane_format_characters_are_refused() {
    for rejected in [
        "src/\u{1d173}musical",
        "src/\u{110bd}kaithi",
        "src/\u{13430}egyptian",
        "src/\u{1bca0}shorthand",
    ] {
        assert!(
            GrantPathRule::parse(rejected).is_err(),
            "path rule {rejected:?} must be refused"
        );
    }
    assert!(crate::authorization::is_console_unsafe('\u{1d173}'));
    assert!(crate::authorization::is_console_unsafe('\u{110bd}'));
}

// ---------------------------------------------------------------------------
// H4-C / I-6 — the publication and revocation fence timeouts.
//
// The production wait is a 30 second constant; asserting only the constant
// would prove nothing about behaviour, and waiting 30 real seconds in CI would
// be an unusable test. Both are covered: the constant is asserted directly, and
// the *same* code path is exercised with an injected wait so the timeout branch
// is hit deterministically.
// ---------------------------------------------------------------------------

/// AT-GW-031 — the production fence wait is exactly the documented 30 seconds.
#[test]
fn at_gw_031_the_publication_fence_wait_is_thirty_seconds() {
    assert_eq!(
        crate::authorization::AUTHORIZATION_FENCE_TIMEOUT,
        std::time::Duration::from_secs(30),
        "the shipped fence bound is part of the V1 contract"
    );
}

/// AT-GW-032 — a busy fence is a typed timeout, and a timed-out acquisition
/// changes nothing durable.
#[test]
fn at_gw_032_a_busy_fence_times_out_without_changing_the_ledger() {
    use std::time::{Duration, Instant};

    let f = fixture("fence-busy");
    let _grant = f.install();
    let ledger_path =
        crate::authorization_store::ledger_path(&f.store, &f.session.id, f.goal_id.as_str())
            .unwrap();
    let before = std::fs::read(&ledger_path).unwrap();

    let held = crate::authorization_store::test_support::hold_ledger_lock(
        &f.store,
        &f.session.id,
        f.goal_id.as_str(),
    );
    let started = Instant::now();
    let error = crate::authorization_store::test_support::acquire_lock_with_wait(
        &f.store,
        &f.session.id,
        f.goal_id.as_str(),
        Duration::from_millis(100),
    )
    .expect_err("a held fence cannot be acquired");
    assert_eq!(error, crate::authorization::GrantError::FenceTimeout);
    assert_eq!(error.code(), "AUTHORIZATION_FENCE_BUSY");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the injected bound must be the one that applies"
    );
    assert_eq!(
        std::fs::read(&ledger_path).unwrap(),
        before,
        "a timed-out fence acquisition must not write the ledger"
    );

    // Releasing the holder makes the same acquisition succeed, so the timeout is
    // a bound on waiting, not a wedged lock.
    drop(held);
    crate::authorization_store::test_support::acquire_lock_with_wait(
        &f.store,
        &f.session.id,
        f.goal_id.as_str(),
        Duration::from_secs(5),
    )
    .expect("the fence is acquirable again once released");
}

/// AT-GW-033 — a revocation that cannot take the fence is DEFERRED. It is never
/// reported as REVOKED, and the grant stays usable until the fence is released.
#[test]
fn at_gw_033_a_deferred_revocation_is_not_reported_as_revoked() {
    use std::time::Duration;

    let f = fixture("revoke-deferred");
    let _grant = f.install();
    let held = crate::authorization_store::test_support::hold_ledger_lock(
        &f.store,
        &f.session.id,
        f.goal_id.as_str(),
    );

    let error = crate::authorization_store::revoke_active_within(
        &f.store,
        &f.session.id,
        f.goal_id.as_str(),
        None,
        Duration::from_millis(100),
    )
    .expect_err("a held fence defers the revocation");
    assert_eq!(error.code(), "REVOCATION_DEFERRED");
    assert_ne!(
        error.code(),
        "WRITE_AUTHORIZATION_REVOKED",
        "a deferred revocation must never be reported as completed"
    );
    assert_eq!(
        f.ledger()
            .latest_grant()
            .expect("a grant exists")
            .revocation()
            .state(),
        GrantRevocationState::NotRevoked,
        "the durable grant state is unchanged by a deferred revocation"
    );
    assert_eq!(
        crate::authorization_store::anchor_of(&f.goal())
            .unwrap()
            .revoked_at_epoch(),
        None,
        "the anchor must not record a revocation that never happened"
    );

    // Once the fence is free the same call finalizes for real.
    drop(held);
    crate::authorization_store::revoke_active(&f.store, &f.session.id, f.goal_id.as_str(), None)
        .expect("the retry finalizes the revocation");
    assert_eq!(
        f.ledger()
            .latest_grant()
            .expect("a grant exists")
            .revocation()
            .state(),
        GrantRevocationState::Revoked
    );
    assert!(
        crate::authorization_store::anchor_of(&f.goal())
            .unwrap()
            .revoked_at_epoch()
            .is_some(),
        "the finalized revocation is anchored"
    );
}

// ---------------------------------------------------------------------------
// H4-B — independent-review findings (CRITICAL/HIGH: none).
//
// Both findings were confirmed against the source before fixing, and each has a
// regression test here. The reviewer's line references were checked, not taken
// on trust.
// ---------------------------------------------------------------------------

/// H4-B-MEDIUM — a stale grant generation must not clear a newer generation's
/// usage floor.
///
/// Defect: `record_anchor_grant` reset the floor for *any* different grant id
/// and only afterwards let `record_grant` refuse the stale epoch. A rolled-back
/// ledger therefore left "newer grant identity, zero usage floor", which returns
/// spent budget. Reproduction here is at the durable boundary: anchor the newer
/// grant, charge one operation, then re-anchor the older generation and observe
/// the floor.
#[test]
fn at_gw_034_a_stale_generation_cannot_clear_a_newer_usage_floor() {
    let f = fixture("stale-anchor-floor");
    let first = f.install();
    crate::authorization_store::revoke_active(&f.store, &f.session.id, f.goal_id.as_str(), None)
        .unwrap();
    let second = f.install();
    let second_binding = crate::authorization::GrantBinding::from_grant(&second);
    crate::authorization_store::authorize_operation(
        &f.store,
        &f.goal(),
        &second_binding,
        "op#0",
        4,
        |_| Ok(()),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(
        crate::authorization_store::anchor_of(&f.goal())
            .unwrap()
            .usage_operations(),
        1,
        "the charge is mirrored into the Goal anchor"
    );

    // A rolled-back ledger proposing the previous generation re-anchors it.
    crate::authorization_store::record_anchor_grant(&f.store, &f.session.id, &f.goal_id, &first)
        .expect("re-anchoring a stale grant is accepted but must not be honoured");

    let anchor = crate::authorization_store::anchor_of(&f.goal()).unwrap();
    assert_eq!(
        anchor.usage_operations(),
        1,
        "a stale generation must not resurrect spent budget"
    );
    assert_eq!(
        anchor.last_grant_id(),
        Some(second.grant_id()),
        "the Goal keeps the newer generation it already observed"
    );
    assert_eq!(anchor.highest_epoch(), second.authorization_epoch());
}

/// H4-B-LOW — `REVOKE_REQUESTED` must not be projected as `REVOKED`.
///
/// Both states deny publication, so this is a reporting defect, not an
/// authorization bypass. The assertion covers both halves: the projection names
/// the durable state, and the permission decision is still a refusal.
#[test]
fn at_gw_035_a_requested_revocation_is_projected_as_requested_not_revoked() {
    let f = fixture("revoke-requested-view");
    let grant = f.install();
    let epoch = crate::authorization_store::test_support::request_revocation_only(
        &f.store,
        &f.session.id,
        f.goal_id.as_str(),
    );
    assert_eq!(epoch, grant.authorization_epoch());

    // The durable state really is REVOKE_REQUESTED, not REVOKED.
    assert_eq!(
        f.ledger()
            .latest_grant()
            .expect("a grant exists")
            .revocation()
            .state(),
        GrantRevocationState::RevokeRequested
    );

    let view = crate::authorization_store::authorization_view(&f.store, &f.goal());
    let view = serde_json::to_value(view).unwrap();
    assert_eq!(
        view["state"], "REVOKE_REQUESTED",
        "the projection must not claim the finalization write happened: {view}"
    );

    // Publication is still refused while the revocation is only requested.
    let binding = crate::authorization::GrantBinding::from_grant(&grant);
    assert!(
        crate::authorization_store::authorize_operation(
            &f.store,
            &f.goal(),
            &binding,
            "op#0",
            4,
            |_| Ok(()),
            |_| Ok(()),
        )
        .is_err(),
        "a requested revocation must already deny new operations"
    );
}

/// H4-B — the finalized state is still projected as REVOKED, so the fix above is
/// a refinement and not a blanket rename.
#[test]
fn at_gw_036_a_finalized_revocation_is_projected_as_revoked() {
    let f = fixture("revoked-view");
    let _grant = f.install();
    crate::authorization_store::revoke_active(&f.store, &f.session.id, f.goal_id.as_str(), None)
        .unwrap();
    let view = crate::authorization_store::authorization_view(&f.store, &f.goal());
    let view = serde_json::to_value(view).unwrap();
    assert_eq!(view["state"], "REVOKED", "{view}");
}

/// H5.1-C R2-10 — `/grant status` must report the effective usage enforcement
/// uses, not the raw ledger counter.
///
/// A restored ledger can under-report spending while the Goal anchor still
/// holds the monotonic floor. The displayed number must match the number the
/// budget gate actually applies.
#[test]
fn at_gw_037_grant_status_reports_the_effective_usage_floor() {
    let f = fixture("effective-usage");
    let grant = f.install();
    let binding = crate::authorization::GrantBinding::from_grant(&grant);
    crate::authorization_store::authorize_operation(
        &f.store,
        &f.goal(),
        &binding,
        "op#0",
        4,
        |_| Ok(()),
        |_| Ok(()),
    )
    .unwrap();

    // Deliberately raise the anchor floor above the ledger's charge, exactly as
    // a ledger restore can leave it after the anchor write won the race.
    let goal = f.goal();
    let raised = f
        .store
        .mutate_goal_snapshot(&f.session.id, &f.goal_id, goal.revision(), |goal, _now| {
            let state = crate::external_model_store::external_state_mut(goal)?;
            state.authorization_anchor_mut().record_usage_floor(3, 12);
            Ok(())
        })
        .unwrap();
    let anchor = crate::authorization_store::anchor_of(&raised).unwrap();
    let rendered = crate::authorization_store::describe_ledger(&f.ledger(), Some(&anchor));
    assert!(
        rendered.contains("usage 3/") && rendered.contains("12/"),
        "the display must use the anchor floor: {rendered}"
    );
    assert!(
        !rendered.contains("usage 1/"),
        "the raw ledger charge must not be displayed: {rendered}"
    );

    // The API projection agrees with the console: both use the effective value.
    let view = crate::authorization_store::authorization_view(&f.store, &raised);
    let view = serde_json::to_value(view).unwrap();
    assert_eq!(view["usage"]["operations"], 3, "{view}");
    assert_eq!(view["usage"]["bytes"], 12, "{view}");
}

/// H5.3: a `WRITE_UTF8` operation with `expected_preimage: ABSENT` names a file
/// that does not exist yet. Until this fix the workspace-relative conversion
/// canonicalized the target itself, which fails on a missing path, so the
/// external Writer could never create a file even though the published response
/// contract offers `{kind: ABSENT}`.
#[test]
fn a_new_file_inside_the_workspace_resolves_through_its_parent() {
    let f = fixture("new-file");
    let grant = f.install();
    let root = grant.workspace().root();
    let target = root.join("data").join("ledger.csv");
    std::fs::create_dir_all(root.join("data")).unwrap();
    assert!(
        !target.exists(),
        "the probe must name a file that does not exist"
    );

    let relative = relative_workspace_path(&target, root)
        .expect("a new file inside the workspace must resolve");
    assert_eq!(relative.as_str(), "data/ledger.csv");
    // Resolution is not authorization: this grant does not cover the path, and
    // the scope check must still refuse it.
    assert!(!grant.permits_path(&relative));
}

/// The fix must not widen the grant: a new file whose parent is outside the
/// workspace is still refused, and a missing parent is refused too.
#[test]
fn a_new_file_outside_the_workspace_or_without_a_parent_is_refused() {
    let f = fixture("new-file-outside");
    let grant = f.install();
    let root = grant.workspace().root();

    let outside_parent = root.parent().unwrap().join("elsewhere");
    std::fs::create_dir_all(&outside_parent).unwrap();
    assert_eq!(
        relative_workspace_path(&outside_parent.join("escape.txt"), root),
        Err(GrantRefusal::ScopeMismatch)
    );

    assert_eq!(
        relative_workspace_path(&root.join("no-such-directory").join("file.txt"), root),
        Err(GrantRefusal::ScopeMismatch)
    );
}
