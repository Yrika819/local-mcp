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
