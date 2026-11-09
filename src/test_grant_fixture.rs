//! H5-C — isolated test-grant fixture for a real Writer E2E.
//!
//! # The problem this solves
//!
//! A Writer `ModelWorkItem` is only issued when an Owner grant exists, and a
//! grant only exists after a live Owner Control Surface approval. A campaign
//! cannot fabricate that approval — that is the exact thing the campaign is
//! supposed to verify. But without *some* grant the Writer leg of the external
//! backend cannot be exercised end to end by a real model.
//!
//! # The design, and what each part buys
//!
//! 1. **A separate grant identity.** A fixture grant carries
//!    `owner_attestation: TEST_FIXTURE_V1` plus the fixture root id, so it can
//!    never be mistaken for an Owner decision in a ledger, a journal, an API
//!    projection or an evidence file. The production attestation is untouched.
//! 2. **A compile-time boundary.** This module and its CLI command exist only
//!    under the non-default `test-grant-fixture` cargo feature, so a release or
//!    operator build cannot mint one at all — not disabled by configuration,
//!    absent from the binary.
//! 3. **A runtime boundary that does not depend on the build.** Even a
//!    feature-enabled binary refuses to issue into a root that is not a marked
//!    disposable fixture root, and the *publication* gate independently refuses a
//!    `TEST_FIXTURE_V1` grant whose fixture id does not match a marker in the
//!    state root the host is running against. A fixture grant copied into a real
//!    operator state root is therefore refused by `resolve_grant`, which every
//!    issuance, stage, publication and revocation check goes through.
//! 4. **Identical downstream code.** Publication, postimage verification, the
//!    journal, revocation, the Task Verifier, the Goal Verifier and the Finalizer
//!    are the production implementations. Only the *decision* is a fixture.
//!
//! # What this is not
//!
//! It is not an Owner approval, it is not a production path, and it is not a
//! substitute for an owner-driven acceptance run. It is a test double for the
//! decision, and an honest record of any run that used it must say
//! `TEST_FIXTURE_V1`.

use crate::authorization::{GoalWriteGrantV1, OwnerAttestation, now_unix_seconds};
use crate::authorization_store::{self, AuthorizationGateError};
use crate::config;
use crate::goal::GoalId;
use crate::task_store::TaskStore;

/// Why a fixture grant was refused.
#[derive(Debug)]
pub(crate) enum FixtureGrantError {
    /// The state root is not a marked disposable fixture root.
    NotAFixtureRoot(&'static str),
    /// The Goal cannot carry a Writer grant (wrong backend, no scope, ...).
    Goal(AuthorizationGateError),
    /// Durable state could not be read or written.
    Store(String),
}

impl std::fmt::Display for FixtureGrantError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAFixtureRoot(detail) => {
                write!(
                    formatter,
                    "not a disposable test-grant fixture root: {detail}"
                )
            }
            Self::Goal(error) => {
                write!(
                    formatter,
                    "the Goal cannot carry a grant: {}",
                    error.detail()
                )
            }
            Self::Store(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for FixtureGrantError {}

/// Issue a `TEST_FIXTURE_V1` grant for a durable EXTERNAL Goal's stored scope.
///
/// Every step is the production implementation: the same pending-request
/// validation, the same grant construction, the same Goal anchor write, the same
/// ledger commit. The single difference is the attestation, and that is what
/// makes the result unusable outside this fixture root.
pub(crate) fn issue_test_fixture_grant(
    store: &TaskStore,
    session_id: &str,
    goal_id: &GoalId,
) -> Result<GoalWriteGrantV1, FixtureGrantError> {
    let fixture_id = config::test_grant_fixture_id().ok_or(FixtureGrantError::NotAFixtureRoot(
        "the state root carries no test-grant fixture marker",
    ))?;

    let goal = store
        .load_goal(session_id, goal_id)
        .map_err(|error| FixtureGrantError::Store(error.to_string()))?;
    if !goal.model_backend().is_external() {
        return Err(FixtureGrantError::NotAFixtureRoot(
            "a test grant is only meaningful for an EXTERNAL Goal",
        ));
    }

    let request_id = uuid::Uuid::new_v4().to_string();
    // A fixture nonce: it never leaves this process and it is not an Owner proof.
    // The ledger stores only its digest, exactly as it does for the real console.
    let nonce = format!("test-fixture-nonce-{request_id}");
    let nonce_sha256 = crate::authorization::nonce_sha256(&nonce);
    authorization_store::begin_request_for_goal(
        store,
        &goal,
        request_id.clone(),
        nonce_sha256.clone(),
    )
    .map_err(FixtureGrantError::Goal)?;

    let now_text = crate::authorization::format_utc_rfc3339(now_unix_seconds());
    let decision_id = uuid::Uuid::new_v4().to_string();
    let mut ledger = authorization_store::load_ledger(store, session_id, goal_id.as_str())
        .map_err(|error| FixtureGrantError::Store(error.code().to_owned()))?
        .ok_or_else(|| {
            FixtureGrantError::Store("the pending request was not persisted".to_owned())
        })?;
    let grant = ledger
        .build_grant(&request_id, &nonce_sha256, decision_id.clone(), &now_text)
        .map_err(|error| FixtureGrantError::Store(error.code().to_owned()))?
        .stamp_test_fixture(fixture_id);
    grant
        .validate()
        .map_err(|error| FixtureGrantError::Store(error.code().to_owned()))?;

    // Same order as the Owner path: anchor first, then the ledger commit, so a
    // crash leaves an anchor ahead of the ledger (fail closed) rather than a
    // ledger record the Goal never observed.
    authorization_store::record_anchor_grant(store, session_id, goal.id(), &grant)
        .map_err(|error| FixtureGrantError::Store(error.to_string()))?;
    ledger
        .record_approved_with_attestation(
            grant.clone(),
            &request_id,
            decision_id,
            &nonce_sha256,
            &now_text,
            OwnerAttestation::TestFixtureV1,
        )
        .map_err(|error| FixtureGrantError::Store(error.code().to_owned()))?;
    authorization_store::save_ledger(store, &ledger)
        .map_err(|error| FixtureGrantError::Store(error.code().to_owned()))?;
    Ok(grant)
}

/// The feature-gated `test-grant` command body.
///
/// It refuses everything it cannot prove is disposable, and it prints the exact
/// durable identity it created so a later report can name it.
pub(crate) fn run(
    store: &TaskStore,
    session_id: &str,
    goal_id: &str,
) -> Result<String, FixtureGrantError> {
    let parsed = GoalId::parse(goal_id)
        .map_err(|_| FixtureGrantError::Store("the Goal id is malformed".to_owned()))?;
    let grant = issue_test_fixture_grant(store, session_id, &parsed)?;
    Ok(format!(
        "issued TEST_FIXTURE_V1 grant {} epoch={} goal={} fixture_root={} expires={}",
        grant.grant_id(),
        grant.authorization_epoch(),
        parsed.as_str(),
        grant.test_fixture_id().unwrap_or("<missing>"),
        grant.expires_at(),
    ))
}
