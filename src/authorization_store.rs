#![allow(
    dead_code,
    reason = "A1 lands the durable boundary before A2 wires every helper into the gate."
)]

//! Durable I/O boundary for Authorization B.
//!
//! The authoritative grant ledger is one small JSON document per Goal:
//!
//! ```text
//! <state>/authorization/<session_id>/<goal_id>.json
//! <state>/authorization/<session_id>/<goal_id>.lock
//! ```
//!
//! Two processes touch it:
//!
//! * the Owner Control Surface (`local-mcp start`) issues, denies and revokes
//!   grants; and
//! * the MCP server charges one operation id immediately before a filesystem
//!   write.
//!
//! Both take the per-Goal `.lock` file. The lock is the **linearization point**
//! between an Owner revocation and an in-flight publication: a revocation can
//! only be finalized after the current operation's critical section ends, and
//! once it is finalized no later operation can pass the state check.
//!
//! The lock order is always ledger → Goal session lock. Nothing in this module
//! takes the Goal lock first and then waits for the ledger lock.
//!
//! Every read fails closed: a missing ledger means "no grant", a malformed
//! ledger means "refuse", and never "allowed".

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::authorization::{
    AUTHORIZATION_FENCE_TIMEOUT, AUTHORIZATION_SCHEMA_VERSION, AuthorizationAnchor,
    AuthorizationLedgerV1, AuthorizationView, BACKEND_IDENTITY_EXTERNAL_WRITER_V1,
    GoalWriteGrantV1, GrantBinding, GrantError, GrantOperation, GrantPathRule, GrantRefusal,
    GrantRevocationState, OwnerDecisionKind, PENDING_GRANT_TTL_SECONDS, PendingGrantRequestV1,
    RequestedWriteScopeV1, WriteBudgetV1,
};
use crate::config;
use crate::external_model_store::external_state_mut;
use crate::external_model_work::ExternalModelState;
use crate::goal::{Goal, GoalId};
use crate::orchestrator_error::OrchestratorError;
use crate::secure_fs;
use crate::task_store::TaskStore;
use crate::workspace_snapshot::NormalizedWorkspacePath;

/// A gate failure, separated so callers can report a typed protocol outcome
/// instead of turning every refusal into a persistence error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthorizationGateError {
    Refusal(GrantRefusal),
    State(GrantError),
}

impl AuthorizationGateError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Refusal(refusal) => refusal.code(),
            Self::State(error) => error.code(),
        }
    }

    pub(crate) fn detail(self) -> &'static str {
        match self {
            Self::Refusal(refusal) => refusal.detail(),
            Self::State(error) => match error {
                GrantError::InvalidRequest(detail)
                | GrantError::Corrupt(detail)
                | GrantError::WorkspaceUnavailable(detail)
                | GrantError::StateMismatch(detail) => detail,
                other => other.code(),
            },
        }
    }
}

impl From<GrantRefusal> for AuthorizationGateError {
    fn from(value: GrantRefusal) -> Self {
        Self::Refusal(value)
    }
}

impl From<GrantError> for AuthorizationGateError {
    fn from(value: GrantError) -> Self {
        Self::State(value)
    }
}

fn map_io(_error: std::io::Error) -> GrantError {
    GrantError::Io
}

fn authorization_root(store: &TaskStore) -> PathBuf {
    store.state_root().join("authorization")
}

/// Ledger path for one Goal, validated so a caller cannot traverse out of the
/// authorization root through a crafted id.
pub(crate) fn ledger_path(
    store: &TaskStore,
    session_id: &str,
    goal_id: &str,
) -> Result<PathBuf, GrantError> {
    config::validate_session_id(session_id)
        .map_err(|_| GrantError::Corrupt("authorization session id is unsafe"))?;
    uuid::Uuid::parse_str(goal_id)
        .map_err(|_| GrantError::Corrupt("authorization goal id is unsafe"))?;
    Ok(authorization_root(store)
        .join(session_id)
        .join(format!("{goal_id}.json")))
}

fn ensure_directory(store: &TaskStore, path: &Path) -> Result<(), GrantError> {
    let root = authorization_root(store);
    secure_fs::ensure_private_directory(&root, Some(store.state_root())).map_err(map_io)?;
    secure_fs::ensure_private_directory(path, Some(&root)).map_err(map_io)
}

fn ledger_directory(store: &TaskStore, session_id: &str) -> Result<PathBuf, GrantError> {
    config::validate_session_id(session_id)
        .map_err(|_| GrantError::Corrupt("authorization session id is unsafe"))?;
    Ok(authorization_root(store).join(session_id))
}

/// Read one ledger. `Ok(None)` means "no grant was ever requested".
pub(crate) fn load_ledger(
    store: &TaskStore,
    session_id: &str,
    goal_id: &str,
) -> Result<Option<AuthorizationLedgerV1>, GrantError> {
    let path = ledger_path(store, session_id, goal_id)?;
    let mut file = match secure_fs::open_private_existing(&path, false) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(GrantError::Io),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(map_io)?;
    let ledger: AuthorizationLedgerV1 = serde_json::from_slice(&bytes)
        .map_err(|_| GrantError::Corrupt("authorization ledger is malformed"))?;
    ledger.validate()?;
    Ok(Some(ledger))
}

/// Atomically replace one ledger (private temp + fsync + replace + parent fsync).
pub(crate) fn save_ledger(
    store: &TaskStore,
    ledger: &AuthorizationLedgerV1,
) -> Result<(), GrantError> {
    ledger.validate()?;
    let path = ledger_path(store, ledger.session_id(), ledger.goal_id())?;
    let directory = ledger_directory(store, ledger.session_id())?;
    ensure_directory(store, &directory)?;
    match secure_fs::open_private_existing(&path, false) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(GrantError::Io),
    }
    let bytes = serde_json::to_vec_pretty(ledger)
        .map_err(|_| GrantError::Corrupt("authorization ledger cannot be serialized"))?;
    let (temporary, mut file) = secure_fs::create_unique_temp(&path).map_err(map_io)?;
    let write_result = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()
    })();
    drop(file);
    if write_result.is_err() {
        let _ = secure_fs::remove_private_temp(&temporary);
        return Err(GrantError::Io);
    }
    if secure_fs::atomic_replace(&temporary, &path).is_err() {
        let _ = secure_fs::remove_private_temp(&temporary);
        return Err(GrantError::Io);
    }
    if secure_fs::sync_parent_directory(&directory).is_err() {
        let _ = secure_fs::remove_private_temp(&temporary);
        return Err(GrantError::Io);
    }
    Ok(())
}

/// Holds the per-Goal ledger lock and releases it on drop.
#[derive(Debug)]
struct LedgerGuard {
    file: File,
}

impl Drop for LedgerGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn lock_path(store: &TaskStore, session_id: &str, goal_id: &str) -> Result<PathBuf, GrantError> {
    Ok(ledger_path(store, session_id, goal_id)?.with_extension("lock"))
}

fn acquire_ledger_lock(
    store: &TaskStore,
    session_id: &str,
    goal_id: &str,
) -> Result<LedgerGuard, GrantError> {
    let path = lock_path(store, session_id, goal_id)?;
    let directory = ledger_directory(store, session_id)?;
    ensure_directory(store, &directory)?;
    match secure_fs::open_private_existing(&path, false) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(GrantError::Io),
    }
    let file = secure_fs::create_or_open_lock(&path).map_err(map_io)?;
    let deadline = Instant::now() + AUTHORIZATION_FENCE_TIMEOUT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(LedgerGuard { file }),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(GrantError::FenceTimeout);
            }
            Err(_) => return Err(GrantError::Io),
        }
    }
}

fn load_with_lock(
    store: &TaskStore,
    session_id: &str,
    goal_id: &str,
    _guard: &LedgerGuard,
) -> Result<Option<AuthorizationLedgerV1>, GrantError> {
    load_ledger(store, session_id, goal_id)
}

fn load_or_create(
    store: &TaskStore,
    goal: &Goal,
    _guard: &LedgerGuard,
) -> Result<AuthorizationLedgerV1, GrantError> {
    match load_ledger(store, goal.session_id(), goal.id().as_str())? {
        Some(ledger) => {
            ledger.matches_goal(goal)?;
            Ok(ledger)
        }
        None => AuthorizationLedgerV1::new(goal),
    }
}

/// The durable anchor of one EXTERNAL Goal (zero for any older record).
pub(crate) fn anchor_of(goal: &Goal) -> Result<AuthorizationAnchor, GrantError> {
    let state = crate::external_model_store::external_state(goal)
        .map_err(|_| GrantError::BackendNotExternal)?;
    Ok(state.authorization_anchor().clone())
}

/// Update the Goal-side anchor under Goal CAS.
///
/// Retries a bounded number of times on a concurrent revision. Any other error
/// is returned: an unanchored grant is unusable, so a silent failure would be
/// worse than a visible one.
fn update_anchor(
    store: &TaskStore,
    session_id: &str,
    goal_id: &GoalId,
    update: impl Fn(&mut AuthorizationAnchor),
) -> Result<Goal, OrchestratorError> {
    let mut last_error = None;
    for _ in 0..3 {
        let goal = store.load_goal(session_id, goal_id)?;
        let revision = goal.revision();
        let outcome = store.mutate_goal_snapshot(session_id, goal_id, revision, |goal, _now| {
            let state = external_state_mut(goal)?;
            update(state.authorization_anchor_mut());
            Ok(())
        });
        match outcome {
            Ok(goal) => return Ok(goal),
            Err(error @ OrchestratorError::RevisionConflict { .. }) => last_error = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        OrchestratorError::PersistenceIo(std::io::Error::other(
            "authorization anchor could not be updated",
        ))
    }))
}

/// Record an issued grant in the Goal anchor.
pub(crate) fn record_anchor_grant(
    store: &TaskStore,
    session_id: &str,
    goal_id: &GoalId,
    grant: &GoalWriteGrantV1,
) -> Result<Goal, OrchestratorError> {
    let grant_id = grant.grant_id().to_owned();
    let epoch = grant.authorization_epoch();
    update_anchor(store, session_id, goal_id, move |anchor| {
        // Only a *new generation* starts a fresh budget. Re-anchoring the same
        // grant (for example when a second Writer WorkItem is issued under it)
        // must preserve the usage floor, or the budget could be spent twice.
        let new_generation =
            anchor.last_grant_id() != Some(grant_id.as_str()) || anchor.highest_epoch() < epoch;
        if new_generation {
            anchor.reset_usage_floor();
        }
        anchor.record_grant(&grant_id, epoch);
    })
}

/// Create a durable pending request for the Owner.
///
/// The request is data the OCS validated; it does not authorize anything.
pub(crate) fn begin_pending_request(
    store: &TaskStore,
    goal: &Goal,
    scope: &RequestedWriteScopeV1,
    request_id: String,
    nonce_sha256: String,
    requested_at: String,
    expires_at: String,
) -> Result<PendingGrantRequestV1, AuthorizationGateError> {
    let grant_lifetime_seconds = scope.expires_in_seconds();
    if !goal.model_backend().is_external() {
        return Err(GrantError::BackendNotExternal.into());
    }
    let workspace = crate::authorization::GrantWorkspaceIdentity::from_goal(goal)?;
    // Resolve every rule against the exact workspace root now, so the Owner sees
    // a scope that cannot escape through a pre-existing symlink/junction. The
    // publish gate repeats this with the operation's canonical path.
    let resolved_root = config::canonical_future_path(workspace.root())
        .map_err(|_| GrantError::InvalidRequest("the workspace root cannot be resolved"))?;
    for rule in scope.path_rules()? {
        let candidate = workspace.root().join(rule.path());
        let resolved = config::canonical_future_path(&candidate).map_err(|_| {
            GrantError::InvalidRequest("a write-scope rule cannot be resolved inside the workspace")
        })?;
        if !resolved.starts_with(&resolved_root) {
            return Err(GrantError::InvalidRequest(
                "a write-scope rule escapes the workspace root",
            )
            .into());
        }
    }
    let request = PendingGrantRequestV1::new(
        request_id.clone(),
        goal.session_id().to_owned(),
        goal.id().as_str().to_owned(),
        BACKEND_IDENTITY_EXTERNAL_WRITER_V1.to_owned(),
        crate::authorization::goal_identity_digest(goal)?,
        workspace,
        scope.path_rules()?,
        scope.allowed_operations().to_vec(),
        scope.budget()?,
        requested_at,
        expires_at,
        grant_lifetime_seconds,
        nonce_sha256,
    )?;
    let guard = acquire_ledger_lock(store, goal.session_id(), goal.id().as_str())?;
    let mut ledger = load_or_create(store, goal, &guard)?;
    let now = crate::authorization::now_unix_seconds();
    let now_text = crate::authorization::format_utc_rfc3339(now);
    // An earlier pending request can never be approved after an OCS restart
    // (its live nonce is gone), and the OCS only calls this after checking its
    // in-memory queue. Replacing it is therefore both safe and necessary.
    if ledger.pending().is_some() {
        ledger.replace_pending(request.clone(), &now_text)?;
    } else {
        ledger.record_pending(request.clone(), &now_text)?;
    }
    save_ledger(store, &ledger)?;
    Ok(request)
}

/// Approve one pending request. Called only by the OCS.
pub(crate) fn approve_pending(
    store: &TaskStore,
    session_id: &str,
    goal_id: &str,
    request_id: &str,
    nonce: &str,
    decision_id: String,
) -> Result<GoalWriteGrantV1, AuthorizationGateError> {
    let parsed_goal = GoalId::parse(goal_id)
        .map_err(|_| GrantError::Corrupt("authorization goal id is malformed"))?;
    let goal = store
        .load_goal(session_id, &parsed_goal)
        .map_err(|_| GrantError::Io)?;
    let nonce_sha256 = crate::authorization::nonce_sha256(nonce);
    let now = crate::authorization::format_utc_rfc3339(crate::authorization::now_unix_seconds());
    let guard = acquire_ledger_lock(store, session_id, goal_id)?;
    let mut ledger =
        load_with_lock(store, session_id, goal_id, &guard)?.ok_or(GrantError::NoPendingRequest)?;
    ledger.matches_goal(&goal)?;
    let grant = ledger.build_grant(request_id, &nonce_sha256, decision_id.clone(), &now)?;
    // Anchor first: a crash here leaves an anchor ahead of the ledger, which
    // fails closed. A crash after the ledger write but before the anchor would
    // instead leave a ledger record the Goal never observed, which is exactly
    // what the gate must not accept.
    record_anchor_grant(store, session_id, goal.id(), &grant).map_err(|_| GrantError::Io)?;
    ledger.record_approved(grant.clone(), request_id, decision_id, &nonce_sha256, &now)?;
    save_ledger(store, &ledger)?;
    Ok(grant)
}

/// Deny one pending request. Called only by the OCS.
pub(crate) fn deny_pending(
    store: &TaskStore,
    session_id: &str,
    goal_id: &str,
    request_id: &str,
    decision_id: String,
    reason: Option<String>,
) -> Result<(), AuthorizationGateError> {
    let now = crate::authorization::format_utc_rfc3339(crate::authorization::now_unix_seconds());
    let guard = acquire_ledger_lock(store, session_id, goal_id)?;
    let mut ledger =
        load_with_lock(store, session_id, goal_id, &guard)?.ok_or(GrantError::NoPendingRequest)?;
    ledger.deny(request_id, decision_id, reason, &now)?;
    save_ledger(store, &ledger)?;
    Ok(())
}

/// Record `REVOKE_REQUESTED`, then finalize `REVOKED`, then anchor the epoch.
///
/// The two writes are deliberate: if the process dies between them the durable
/// state says `REVOKE_REQUESTED`, which already refuses every new operation.
/// Finalization acquires the same fence, so it can never interleave with an
/// operation that has already begun.
pub(crate) fn revoke_active(
    store: &TaskStore,
    session_id: &str,
    goal_id: &str,
    now: Option<String>,
) -> Result<(String, u64), AuthorizationGateError> {
    let now = now.unwrap_or_else(|| {
        crate::authorization::format_utc_rfc3339(crate::authorization::now_unix_seconds())
    });
    let parsed_goal = GoalId::parse(goal_id)
        .map_err(|_| GrantError::Corrupt("authorization goal id is malformed"))?;
    let guard = acquire_ledger_lock(store, session_id, goal_id)?;
    let mut ledger =
        load_with_lock(store, session_id, goal_id, &guard)?.ok_or(GrantError::UnknownGrant)?;
    let grant_id = ledger
        .latest_grant()
        .map(|grant| grant.grant_id().to_owned())
        .ok_or(GrantError::UnknownGrant)?;
    let grant_epoch = ledger
        .latest_grant()
        .map(|grant| grant.authorization_epoch())
        .unwrap_or(0);
    if ledger
        .latest_grant()
        .is_some_and(|grant| grant.revocation().state() == GrantRevocationState::Revoked)
    {
        // Idempotent retry must also repair a crash window in which the ledger
        // said REVOKED but the Goal anchor was never written. Without this, a
        // restored ACTIVE ledger would satisfy the stale anchor.
        let anchor = anchor_of(
            &store
                .load_goal(session_id, &parsed_goal)
                .map_err(|_| GrantError::Io)?,
        )?;
        if anchor.revoked_at_epoch() != Some(grant_epoch) {
            update_anchor(store, session_id, &parsed_goal, |anchor| {
                anchor.record_revocation(grant_epoch);
            })
            .map_err(|_| GrantError::Io)?;
        }
        return Ok((grant_id, grant_epoch));
    }
    // The anchor is written *before* REVOKED is finalized, so a crash can only
    // leave an anchor that already denies the grant (fail closed), never a
    // REVOKED ledger behind a stale anchor. The anchor names the *revoked*
    // generation; the ledger's next epoch is greater, so a fresh grant is legal.
    update_anchor(store, session_id, &parsed_goal, |anchor| {
        anchor.record_revocation(grant_epoch);
    })
    .map_err(|_| GrantError::Io)?;
    let request_decision = uuid::Uuid::new_v4().to_string();
    ledger.request_revocation(&grant_id, request_decision, &now)?;
    save_ledger(store, &ledger)?;
    let final_decision = uuid::Uuid::new_v4().to_string();
    let _next_epoch = ledger.finalize_revocation(&grant_id, final_decision, &now)?;
    save_ledger(store, &ledger)?;
    Ok((grant_id, grant_epoch))
}

/// Read the grant that may authorize work right now (no lock, snapshot).
pub(crate) fn usable_grant(
    store: &TaskStore,
    goal: &Goal,
) -> Result<GoalWriteGrantV1, AuthorizationGateError> {
    if !goal.model_backend().is_external() {
        return Err(GrantError::BackendNotExternal.into());
    }
    let ledger =
        load_ledger(store, goal.session_id(), goal.id().as_str())?.ok_or(GrantRefusal::Required)?;
    ledger.matches_goal(goal)?;
    let workspace = crate::authorization::GrantWorkspaceIdentity::from_goal(goal)?;
    let anchor = anchor_of(goal)?;
    let now = crate::authorization::format_utc_rfc3339(crate::authorization::now_unix_seconds());
    let grant = ledger.usable_grant(&anchor, &now)?.clone();
    if grant.workspace().digest() != workspace.digest() {
        return Err(GrantRefusal::ScopeMismatch.into());
    }
    Ok(grant)
}

/// Validate and charge one operation under the per-Goal fence.
///
/// `check` runs while the fence is held and after the grant state, identity,
/// workspace and binding have been verified, so a path check cannot race a
/// revocation or a grant replacement. The charge is written before the caller
/// performs the side effect and is idempotent per operation id.
pub(crate) fn authorize_operation<P, B>(
    store: &TaskStore,
    goal: &Goal,
    binding: &GrantBinding,
    operation_id: &str,
    bytes: u64,
    preflight: P,
    begin: B,
) -> Result<(), AuthorizationGateError>
where
    P: FnOnce(&GoalWriteGrantV1) -> Result<(), GrantRefusal>,
    B: FnOnce(&GoalWriteGrantV1) -> Result<(), GrantRefusal>,
{
    let fence = prepare_charge(store, goal, binding, operation_id, bytes, preflight)?;
    begin(fence.grant()).map_err(AuthorizationGateError::from)
}

/// A charged operation whose per-Goal ledger fence is still held.
///
/// Publication keeps this value alive across the filesystem write and the
/// `CompleteOperation` commit. A revocation therefore cannot finalize while an
/// operation is executing: it either runs before the operation begins (and the
/// operation is refused) or after the operation is durably journaled. The lock
/// is per Goal, never the session lock, and every wait is bounded.
#[derive(Debug)]
pub(crate) struct PublicationFence {
    _guard: LedgerGuard,
    grant: GoalWriteGrantV1,
    goal_revision: u64,
}

impl PublicationFence {
    pub(crate) fn grant(&self) -> &GoalWriteGrantV1 {
        &self.grant
    }

    /// The Goal revision after the anchor usage floor was committed, if it was.
    /// The caller must use this revision for `BeginOperation`.
    pub(crate) fn goal_revision(&self) -> u64 {
        self.goal_revision
    }
}

/// Acquire the publication fence for one operation.
///
/// The grant state, epoch, anchor, expiry, workspace, binding and path/operation
/// scope are verified under the fence; the monotonic usage floor is written to
/// the Goal anchor and the operation id is charged before the caller may perform
/// any side effect. The returned fence must be dropped only after the operation
/// is complete or journaled UNKNOWN.
pub(crate) fn acquire_publication_fence<P>(
    store: &TaskStore,
    goal: &Goal,
    binding: &GrantBinding,
    operation_id: &str,
    bytes: u64,
    preflight: P,
) -> Result<PublicationFence, AuthorizationGateError>
where
    P: FnOnce(&GoalWriteGrantV1) -> Result<(), GrantRefusal>,
{
    prepare_charge(store, goal, binding, operation_id, bytes, preflight)
}

fn prepare_charge<P>(
    store: &TaskStore,
    goal: &Goal,
    binding: &GrantBinding,
    operation_id: &str,
    bytes: u64,
    preflight: P,
) -> Result<PublicationFence, AuthorizationGateError>
where
    P: FnOnce(&GoalWriteGrantV1) -> Result<(), GrantRefusal>,
{
    if !goal.model_backend().is_external() {
        return Err(GrantError::BackendNotExternal.into());
    }
    let guard = acquire_ledger_lock(store, goal.session_id(), goal.id().as_str())?;
    // The caller's Goal snapshot may predate the fence. Re-read the
    // authoritative Goal *while holding it*: the revocation anchor is written
    // under this same lock, so a stale anchor can never authorize a write.
    let goal = store
        .load_goal(goal.session_id(), goal.id())
        .map_err(|_| GrantError::Io)?;
    let mut ledger = load_with_lock(store, goal.session_id(), goal.id().as_str(), &guard)?
        .ok_or(GrantRefusal::Required)?;
    ledger.matches_goal(&goal)?;
    let workspace = crate::authorization::GrantWorkspaceIdentity::from_goal(&goal)?;
    let anchor = anchor_of(&goal)?;
    let now = crate::authorization::format_utc_rfc3339(crate::authorization::now_unix_seconds());
    let grant = ledger
        .usable_grant_for_operation(&anchor, &now, operation_id)?
        .clone();
    if grant.workspace().digest() != workspace.digest() {
        return Err(GrantRefusal::ScopeMismatch.into());
    }
    if !binding.matches(&grant) {
        return Err(GrantRefusal::ScopeMismatch.into());
    }
    // Preflight is pure: a path/operation mismatch never consumes budget.
    preflight(&grant)?;

    let mut goal_revision = goal.revision();
    if !grant.usage().is_charged(operation_id) {
        let floor_operations = grant.usage().operations().max(anchor.usage_operations());
        let floor_bytes = grant.usage().bytes().max(anchor.usage_bytes());
        if !grant
            .budget()
            .allows(floor_operations, floor_bytes, 1, bytes)
        {
            return Err(GrantRefusal::BudgetExhausted.into());
        }
        // Anchor first: the Goal-side monotonic floor is what makes a restored
        // ledger unable to regain spent budget. A crash between the two writes
        // leaves a floor ahead of the ledger, which is conservative (never a
        // refund) and is reconciled by the next charge.
        let next_operations = floor_operations.saturating_add(1);
        let next_bytes = floor_bytes.saturating_add(bytes);
        let updated = update_anchor(store, goal.session_id(), goal.id(), |anchor| {
            anchor.record_usage_floor(next_operations, next_bytes);
        })
        .map_err(|_| GrantError::Io)?;
        goal_revision = updated.revision();
        // The charge is durable before the operation may begin. If the caller's
        // operation then fails, the charge is deliberately not refunded: no
        // proof of non-performance exists yet, and the design forbids refunding
        // without evidence.
        let grant_id = grant.grant_id().to_owned();
        ledger.charge_operation_with_floor(
            &grant_id,
            operation_id,
            bytes,
            floor_operations,
            floor_bytes,
            &now,
        )?;
        save_ledger(store, &ledger)?;
    }
    Ok(PublicationFence {
        _guard: guard,
        grant,
        goal_revision,
    })
}

/// Convenience path check used by the publication gate.
pub(crate) fn check_operation_scope(
    grant: &GoalWriteGrantV1,
    path: &NormalizedWorkspacePath,
) -> Result<(), GrantRefusal> {
    if !grant.permits_operation(GrantOperation::WriteUtf8) {
        return Err(GrantRefusal::ScopeMismatch);
    }
    if !grant.permits_path(path) {
        return Err(GrantRefusal::ScopeMismatch);
    }
    Ok(())
}

/// Build the API projection without creating any state.
pub(crate) fn authorization_view(store: &TaskStore, goal: &Goal) -> AuthorizationView {
    let Ok(state) = crate::external_model_store::external_state(goal) else {
        return AuthorizationView::not_requested();
    };
    if state.write_scope().is_none() {
        return AuthorizationView::not_requested();
    }
    let anchor = state.authorization_anchor();
    let now = crate::authorization::format_utc_rfc3339(crate::authorization::now_unix_seconds());
    match load_ledger(store, goal.session_id(), goal.id().as_str()) {
        Ok(Some(ledger)) => {
            if ledger.matches_goal(goal).is_err() {
                return AuthorizationView {
                    state: "INVALID",
                    grant_id: ledger
                        .latest_grant()
                        .map(|grant| grant.grant_id().to_owned()),
                    request_id: None,
                    authorization_epoch: Some(ledger.authorization_epoch()),
                    expires_at: None,
                    allowed_paths: None,
                    allowed_operations: None,
                    budget: None,
                    usage: None,
                    owner_attestation: crate::authorization::OwnerAttestation::ConsoleLiveNonceV1
                        .as_str(),
                    same_uid_process_resistance: false,
                };
            }
            AuthorizationView::from_ledger(&ledger, anchor, &now)
        }
        Ok(None) => AuthorizationView {
            state: "REQUIRED",
            grant_id: None,
            request_id: None,
            authorization_epoch: None,
            expires_at: None,
            allowed_paths: None,
            allowed_operations: None,
            budget: None,
            usage: None,
            owner_attestation: crate::authorization::OwnerAttestation::ConsoleLiveNonceV1.as_str(),
            same_uid_process_resistance: false,
        },
        Err(_) => AuthorizationView {
            state: "INVALID",
            grant_id: None,
            request_id: None,
            authorization_epoch: None,
            expires_at: None,
            allowed_paths: None,
            allowed_operations: None,
            budget: None,
            usage: None,
            owner_attestation: crate::authorization::OwnerAttestation::ConsoleLiveNonceV1.as_str(),
            same_uid_process_resistance: false,
        },
    }
}

/// Whether the Goal carries a write-scope proposal at all.
pub(crate) fn has_write_scope(goal: &Goal) -> bool {
    goal.external_model()
        .is_some_and(|state| state.write_scope().is_some())
}

/// Create a pending request from a durable Goal's stored proposal.
///
/// Called by the OCS after it has verified the Goal. The returned request is what
/// the console shows; the live nonce is passed separately and only its digest is
/// persisted.
pub(crate) fn begin_request_for_goal(
    store: &TaskStore,
    goal: &Goal,
    request_id: String,
    nonce_sha256: String,
) -> Result<PendingGrantRequestV1, AuthorizationGateError> {
    let scope = goal
        .external_model()
        .and_then(|state| state.write_scope())
        .ok_or(GrantError::InvalidRequest(
            "the Goal has no write-scope proposal",
        ))?;
    let now = crate::authorization::now_unix_seconds();
    let requested_at = crate::authorization::format_utc_rfc3339(now);
    let expires_at =
        crate::authorization::format_utc_rfc3339(now + PENDING_GRANT_TTL_SECONDS as i64);
    begin_pending_request(
        store,
        goal,
        scope,
        request_id,
        nonce_sha256,
        requested_at,
        expires_at,
    )
}

/// Helper for tests and for the console's `/grant status` path.
pub(crate) fn describe_ledger(ledger: &AuthorizationLedgerV1) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "ledger epoch {} (schema {})",
        ledger.authorization_epoch(),
        AUTHORIZATION_SCHEMA_VERSION
    ));
    if let Some(pending) = ledger.pending() {
        lines.push(format!(
            "PENDING request {} expires {}",
            pending.request_id(),
            pending.expires_at()
        ));
    }
    for grant in ledger.grants() {
        lines.push(format!(
            "grant {} epoch {} state {} expires {} usage {}/{} ops {}/{} bytes",
            grant.grant_id(),
            grant.authorization_epoch(),
            grant.revocation().state().as_str(),
            grant.expires_at(),
            grant.usage().operations(),
            grant.budget().max_operations(),
            grant.usage().bytes(),
            grant.budget().max_bytes(),
        ));
    }
    for decision in ledger.decisions() {
        if decision.kind() == OwnerDecisionKind::Denied {
            lines.push(format!("DENIED request {}", decision.request_id()));
        }
    }
    lines.join("\n")
}

/// Ensure the durable scope proposal can be persisted.
pub(crate) fn validate_scope_for_goal(
    _goal: &Goal,
    scope: &RequestedWriteScopeV1,
) -> Result<(), GrantError> {
    scope.validate()
}

/// Expose the budget constructor to the goal-start validator.
pub(crate) fn budget_from(operations: u32, bytes: u64) -> Result<WriteBudgetV1, GrantError> {
    WriteBudgetV1::new(operations, bytes)
}

/// Expose path rule parsing to the goal-start validator.
pub(crate) fn parse_path_rule(value: &str) -> Result<GrantPathRule, GrantError> {
    GrantPathRule::parse(value)
}

/// Where the OCS and the MCP server agree a ledger lives.
pub(crate) fn ledger_exists(
    store: &TaskStore,
    session_id: &str,
    goal_id: &str,
) -> Result<bool, GrantError> {
    Ok(load_ledger(store, session_id, goal_id)?.is_some())
}

/// Read the exact workspace identity digest for a Goal.
pub(crate) fn workspace_digest(goal: &Goal) -> Result<String, GrantError> {
    Ok(crate::authorization::GrantWorkspaceIdentity::from_goal(goal)?.digest())
}

/// Read a Goal's stored scope proposal for display.
pub(crate) fn scope_of(goal: &Goal) -> Option<&RequestedWriteScopeV1> {
    goal.external_model()
        .and_then(ExternalModelState::write_scope)
}

/// Test-only Owner Approval Harness.
///
/// These helpers are compiled only into this crate's unit-test build. They
/// mirror what the Owner Control Surface does (a pending request with a known
/// nonce, then an approval that writes the anchor and the ledger), so tests can
/// exercise the real gate without pretending a human approved anything in
/// production. Production binaries contain none of this code.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Install one ACTIVE grant directly, as the OCS would.
    pub(crate) fn install_grant(
        store: &TaskStore,
        goal: &Goal,
        rule_spellings: &[&str],
        max_operations: u32,
        max_bytes: u64,
        ttl_seconds: u64,
    ) -> GoalWriteGrantV1 {
        let scope = serde_json::json!({
            "allowed_paths": rule_spellings,
            "allowed_operations": ["WRITE_UTF8"],
            "max_operations": max_operations,
            "max_bytes": max_bytes,
            "expires_in_seconds": ttl_seconds,
        });
        let scope = RequestedWriteScopeV1::from_mcp_value(&scope).expect("test scope");
        let nonce = "0123456789ab";
        let now = crate::authorization::now_unix_seconds();
        let request = begin_pending_request(
            store,
            goal,
            &scope,
            uuid::Uuid::new_v4().to_string(),
            crate::authorization::nonce_sha256(nonce),
            crate::authorization::format_utc_rfc3339(now),
            crate::authorization::format_utc_rfc3339(now + PENDING_GRANT_TTL_SECONDS as i64),
        )
        .expect("test pending request");
        approve_pending(
            store,
            goal.session_id(),
            goal.id().as_str(),
            request.request_id(),
            nonce,
            uuid::Uuid::new_v4().to_string(),
        )
        .expect("test grant approval")
    }

    /// Default whole-workspace grant used by pre-Authorization-B regression
    /// fixtures so their Writer coverage keeps running unchanged.
    pub(crate) fn install_default_grant(store: &TaskStore, goal: &Goal) -> GoalWriteGrantV1 {
        install_grant(store, goal, &["**"], 64, 16 * 1024 * 1024, 3600)
    }
}
