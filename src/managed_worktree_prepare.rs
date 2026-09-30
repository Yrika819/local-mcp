//! Managed Worktrees V1 — host PrepareWorkspace and creation recovery (Phase 3).
//!
//! Frozen contract: `docs/MANAGED_WORKTREES_V1_DESIGN.md` sections 5, 6, 7, 8,
//! 10, 11, 20, and 21.
//!
//! This module is the host-owned decision layer. It owns three things, in this
//! order, and refuses to continue past any of them:
//!
//! 1. **Session path authority** over the exact derived target. Durable Goal
//!    state is not authority, so an uncovered target stops preparation before
//!    any durable or Git side effect. `session.permitted_directories` is never
//!    appended to, and a previously granted approval is never recreated from
//!    durable state.
//! 2. **Eligibility**, reusing the Phase 2 `classify_eligibility` over the
//!    existing read-only observation seam. No discovery logic is duplicated.
//! 3. **Durable `PREPARED` intent before Git**, then the single host creation
//!    command, then mechanical read-only reconciliation.
//!
//! Ordering note: the eligibility gate is enforced on the *fresh* `REQUESTED ->
//! PREPARED` transition only. Recovery of an already-`PREPARED` intent
//! reconciles against the intent's frozen base commit instead of re-deciding
//! the base, because design section 11 makes a crash after `PREPARED` durable
//! state recoverable and a later primary commit is not a reason to strand it.
//!
//! Recorded deviation: design section 6 also lists "the primary worktree has no
//! tracked, staged, or untracked changes" and "no merge/rebase/... operation is
//! in progress" as preconditions for creation, and those two are likewise not
//! re-evaluated when recovering a `PREPARED` intent. This is safe because
//! creation passes an exact base **commit**: Git checks out that revision, so no
//! primary working-tree state is ever copied or hidden, which is the hazard
//! section 6 and design invariant 13 exist to prevent. Re-running the full
//! eligibility gate on recovery would instead make a crashed, side-effect-free
//! creation permanently unrecoverable, which section 11 explicitly forbids.
//!
//! Phase boundary: nothing here routes Planner, writer, verifier, or readonly
//! workers to the managed root. A managed Goal reaching `ACTIVE` in this phase
//! still cannot be planned; that is Phase 4.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use crate::config;
use crate::goal::{Goal, GoalId};
use crate::managed_worktree::{
    MAX_LIFETIME_CREATION_ATTEMPTS, ManagedWorktreeCreationIntent, ManagedWorktreeLifecycle,
    ManagedWorktreeRecord, WorktreeId, WorktreeOperationId, managed_branch_ref,
};
use crate::managed_worktree_create::{
    ManagedWorktreeCreation, ManagedWorktreeCreationError, ManagedWorktreeCreator,
};
use crate::managed_worktree_discovery::{
    Eligibility, ExpectedWorktreeTarget, Reconciliation, RepositoryObservation,
    classify_eligibility, classify_reconciliation, same_path_identity,
};
use crate::managed_worktree_observe::{ReadOnlyGit, observe_repository};
use crate::orchestrator_error::OrchestratorError;
use crate::task_store::TaskStore;

/// Why managed workspace preparation could not be evaluated at all.
///
/// Every *decision* is expressed as [`ManagedWorkspacePreparation::Blocked`], so
/// this type is reserved for "the host could not even classify the Goal".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ManagedWorkspaceError {
    /// Host-owned root configuration or path derivation failed.
    HostConfiguration(String),
    /// Read-only observation could not be produced.
    Observation(String),
    /// Durable Goal state does not permit managed preparation.
    GoalState(String),
    /// The host approval authority could not be reached. Treated exactly like a
    /// refusal: no Git invocation, and no attempt consumed.
    ApprovalUnavailable(String),
}

impl std::fmt::Display for ManagedWorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostConfiguration(detail) => {
                write!(f, "managed workspace host configuration: {detail}")
            }
            Self::Observation(detail) => write!(f, "managed workspace observation: {detail}"),
            Self::GoalState(detail) => write!(f, "managed workspace goal state: {detail}"),
            Self::ApprovalUnavailable(detail) => {
                write!(f, "managed workspace approval unavailable: {detail}")
            }
        }
    }
}

impl std::error::Error for ManagedWorkspaceError {}

impl From<OrchestratorError> for ManagedWorkspaceError {
    fn from(error: OrchestratorError) -> Self {
        Self::GoalState(error.to_string())
    }
}

impl From<ManagedWorktreeCreationError> for ManagedWorkspaceError {
    fn from(error: ManagedWorktreeCreationError) -> Self {
        // A failure to start Git is not proof that no side effect exists, so it
        // is surfaced as an evaluation error and never as a retry permission.
        Self::Observation(error.to_string())
    }
}

/// Durable evidence explaining why preparation stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedWorkspaceBlock {
    pub(crate) code: &'static str,
    pub(crate) detail: String,
}

/// The result of a host preparation step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ManagedWorkspacePreparation {
    /// The Goal is `PRIMARY`. The caller must continue through the existing
    /// runner completely unchanged.
    NotManaged,
    /// The managed workspace is durably `ACTIVE` and exactly reconciled.
    Active { head: String },
    /// Preparation stopped. No side effect exists beyond what is durably
    /// recorded, and the Goal is blocked awaiting explicit host recovery.
    Blocked(ManagedWorkspaceBlock),
}

impl ManagedWorkspacePreparation {
    fn block(code: &'static str, detail: impl Into<String>) -> Self {
        Self::Blocked(ManagedWorkspaceBlock {
            code,
            detail: detail.into(),
        })
    }

    pub(crate) fn block_detail(&self) -> Option<&ManagedWorkspaceBlock> {
        match self {
            Self::Blocked(block) => Some(block),
            _ => None,
        }
    }
}

/// The host-derived managed root for one Goal: `<managed-root>/<session>/<goal>`.
///
/// `managed_root` is host configuration/state, and the session and Goal
/// segments come only from validated host identities. No MCP field, no model
/// prose, and no existing arbitrary worktree participates.
///
/// This performs path canonicalization only. It grants no authority: the Session
/// gate is applied separately by [`prepare_managed_workspace`].
pub(crate) fn canonical_managed_target(
    managed_root: &Path,
    session: &config::Session,
    goal_id: &GoalId,
) -> Result<PathBuf, ManagedWorkspaceError> {
    let requested = managed_root
        .join(session.id.as_str())
        .join(goal_id.as_str());
    config::canonical_future_path(&requested).map_err(|error| {
        ManagedWorkspaceError::HostConfiguration(format!(
            "cannot derive the managed target for Goal {}: {error}",
            goal_id.as_str()
        ))
    })
}

/// Build the durable `REQUESTED` record for a newly managed-aware Goal.
///
/// The repository identity, base commit, and source ref are mechanically
/// observed through the existing read-only seam. Nothing here mutates Git or
/// creates a path, and nothing here is eligible-checked: design section 6 gates
/// *creation*, which happens later in `prepare_managed_workspace`.
pub(crate) fn request_managed_workspace(
    managed_root: &Path,
    session: &config::Session,
    goal_id: &GoalId,
    git: &dyn ReadOnlyGit,
) -> Result<ManagedWorktreeRecord, ManagedWorkspaceError> {
    // Verbatim on purpose: the durable `primary_root` must stay byte-equal to
    // the session's `cwd`. Only the child-process path is de-verbatim.
    let primary_root = std::fs::canonicalize(&session.cwd).map_err(|error| {
        ManagedWorkspaceError::Observation(format!("session cwd cannot be canonicalized: {error}"))
    })?;
    let target = canonical_managed_target(managed_root, session, goal_id)?;
    let observation = observe_repository(git, &primary_root, &[], &[target.as_path()])
        .map_err(|error| ManagedWorkspaceError::Observation(error.to_string()))?;
    if let Err(reason) = observation.is_trustworthy() {
        return Err(ManagedWorkspaceError::Observation(reason));
    }
    if !same_path_identity(observation.top_level(), &primary_root) {
        return Err(ManagedWorkspaceError::Observation(format!(
            "session cwd is not the canonical Git top level (top level is {})",
            observation.top_level().display()
        )));
    }
    let base_commit = observation.head().ok_or_else(|| {
        ManagedWorkspaceError::Observation("primary HEAD does not resolve to a commit".to_owned())
    })?;
    let common_dir = observation.common_dir().to_path_buf();

    ManagedWorktreeRecord::requested(
        goal_id,
        WorktreeId::new(),
        primary_root,
        common_dir,
        target,
        base_commit.to_owned(),
        observation.head_ref().map(str::to_owned),
        1,
        0,
    )
    .map_err(ManagedWorkspaceError::from)
}

/// The approval operation name shown to the operator.
const MANAGED_CREATION_APPROVAL_OPERATION: &str = "managed_worktree_create";

/// Whether the host-native managed-creation Git mutation is approval-gated.
///
/// Design section 23 is explicit: on Windows "host-native mutation remains
/// approval-gated under current Windows policy". Linux and macOS already place
/// managed creation inside the existing Session-authority and frozen Unix model,
/// and this phase must not add an interactive requirement there, so the gate is
/// platform policy rather than a universal one.
pub(crate) const fn managed_creation_requires_approval() -> bool {
    cfg!(windows)
}

/// The exact host-owned operation an approval may authorize.
///
/// Every field is derived from the durable `PREPARED` intent, so a caller or a
/// model cannot alter what is being approved. The request is a description, not
/// an authority source: after approval the caller re-reads the durable intent and
/// refuses to proceed if it no longer matches this description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManagedCreationApproval {
    pub(crate) goal_id: String,
    pub(crate) primary_root: PathBuf,
    pub(crate) worktree_root: PathBuf,
    pub(crate) branch_ref: String,
    pub(crate) base_commit: String,
}

impl ManagedCreationApproval {
    fn from_intent(intent: &ManagedWorktreeCreationIntent, record: &ManagedWorktreeRecord) -> Self {
        Self {
            goal_id: intent.goal_id().as_str().to_owned(),
            primary_root: record.primary_root().to_path_buf(),
            worktree_root: record.worktree_root().to_path_buf(),
            branch_ref: record.branch_ref().to_owned(),
            base_commit: record.base_commit().to_owned(),
        }
    }

    /// Whether this description still describes `intent` exactly.
    ///
    /// A mismatch means durable state changed across the approval window, so the
    /// approval no longer authorizes the operation that is about to run.
    pub(crate) fn matches(&self, intent: &ManagedWorktreeCreationIntent) -> bool {
        self.goal_id == intent.goal_id().as_str()
            && self.worktree_root == intent.worktree_root()
            && self.branch_ref == intent.branch_ref()
            && self.base_commit == intent.base_commit()
    }

    fn operation(&self) -> &'static str {
        MANAGED_CREATION_APPROVAL_OPERATION
    }

    fn detail(&self) -> String {
        format!(
            "operation=managed_worktree_create goal={} primary_root={} worktree_root={} \
             branch={} base_commit={}",
            self.goal_id,
            self.primary_root.display(),
            self.worktree_root.display(),
            self.branch_ref,
            self.base_commit,
        )
    }
}

/// The one question the managed creation path may ask the host approval
/// authority.
///
/// This is a narrow seam so the preparation logic stays free of UI and IPC
/// details, and so tests can inject a deterministic double instead of requiring
/// an interactive approval UI.
pub(crate) type ManagedApprovalFuture<'a> =
    Pin<Box<dyn Future<Output = Result<bool, ManagedWorkspaceError>> + Send + 'a>>;

pub(crate) trait ManagedCreationApprover {
    /// Approve exactly this operation, or refuse it.
    ///
    /// Returning `Err` is a failure to obtain approval and is treated exactly
    /// like a refusal: no Git invocation.
    fn approve(&self, request: &ManagedCreationApproval) -> ManagedApprovalFuture<'_>;
}

/// The production approver, reusing the existing local approval system.
///
/// It is the same `approvals::request` path the `without_sandbox` and staging
/// mutations use, so the existing yolo semantics are preserved unchanged: yolo
/// auto-allows each request, and a missing session socket or a malformed reply
/// already fails closed.
pub(crate) struct SessionManagedCreationApprover {
    session_id: String,
}

impl SessionManagedCreationApprover {
    pub(crate) fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
        }
    }
}

impl ManagedCreationApprover for SessionManagedCreationApprover {
    fn approve(&self, request: &ManagedCreationApproval) -> ManagedApprovalFuture<'_> {
        let session_id = self.session_id.clone();
        let operation = request.operation();
        let detail = request.detail();
        let primary_root = request.primary_root.clone();
        Box::pin(async move {
            crate::approvals::request(&session_id, operation, detail, primary_root)
                .await
                .map_err(|error| ManagedWorkspaceError::ApprovalUnavailable(format!("{error:#}")))
        })
    }
}

/// Run the host PrepareWorkspace step, or reconcile an in-flight one.
///
/// `PRIMARY` Goals return [`ManagedWorkspacePreparation::NotManaged`] and cause
/// no filesystem or Git access at all.
///
/// The platform approval gate comes from [`managed_creation_requires_approval`].
/// Tests that must exercise the gate on a non-Windows host call
/// [`prepare_managed_workspace_with_policy`] directly.
pub(crate) async fn prepare_managed_workspace(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    git: &dyn ReadOnlyGit,
    creator: &dyn ManagedWorktreeCreator,
    approver: &dyn ManagedCreationApprover,
) -> Result<ManagedWorkspacePreparation, ManagedWorkspaceError> {
    prepare_managed_workspace_with_policy(
        store,
        session,
        goal_id,
        git,
        creator,
        approver,
        managed_creation_requires_approval(),
    )
    .await
}

/// The same preparation, with the approval gate decided by the caller.
///
/// Splitting the policy out is what makes the Windows gate testable on Unix CI
/// without weakening the real platform policy: production always passes
/// [`managed_creation_requires_approval`], and the `#[cfg(windows)]` test proves
/// that value is what production selects.
pub(crate) async fn prepare_managed_workspace_with_policy(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    git: &dyn ReadOnlyGit,
    creator: &dyn ManagedWorktreeCreator,
    approver: &dyn ManagedCreationApprover,
    requires_approval: bool,
) -> Result<ManagedWorkspacePreparation, ManagedWorkspaceError> {
    let goal = store.load_goal(&session.id, goal_id)?;
    if goal.workspace_mode().is_primary() {
        return Ok(ManagedWorkspacePreparation::NotManaged);
    }
    if goal.status().is_terminal() {
        return Ok(ManagedWorkspacePreparation::block(
            "MANAGED_GOAL_TERMINAL",
            format!(
                "a terminal {:?} Goal must not prepare a managed workspace",
                goal.status()
            ),
        ));
    }
    let record = goal.managed_worktree().ok_or_else(|| {
        ManagedWorkspaceError::GoalState(
            "MANAGED_WORKTREE workspace requires a durable managed-worktree record".to_owned(),
        )
    })?;

    // Session path authority gate, before any durable or Git side effect.
    //
    // This runs for every managed lifecycle, including `ACTIVE`, so a revoked
    // authorization stops preparation and recovery before any use. Durable Goal
    // state is not authority: an uncovered target is refused even though the
    // Goal durably names it.
    match config::resolve_path_covered_by_session_authority(session, record.worktree_root()) {
        Ok(live) if same_path_identity(&live, record.worktree_root()) => {}
        Ok(live) => {
            return refuse_managed_workspace(
                store,
                session,
                goal_id,
                ManagedWorkspaceBlock {
                    code: "MANAGED_SESSION_AUTHORITY",
                    detail: format!(
                        "durable managed target {} no longer canonicalizes to the authorized path {}",
                        record.worktree_root().display(),
                        live.display()
                    ),
                },
            );
        }
        Err(error) => {
            return refuse_managed_workspace(
                store,
                session,
                goal_id,
                ManagedWorkspaceBlock {
                    code: "MANAGED_SESSION_AUTHORITY",
                    detail: format!(
                        "managed target {} is not covered by current session authority: {error}",
                        record.worktree_root().display()
                    ),
                },
            );
        }
    }

    match record.lifecycle() {
        // Design section 20: reconcile durable state with Git's machine-readable
        // inventory before any managed operation, including on resume.
        ManagedWorktreeLifecycle::Active => {
            let expected = ExpectedWorktreeTarget::from_record(record);
            let observation = match observe(git, record) {
                Ok(observation) => observation,
                Err(block) => return refuse_managed_workspace(store, session, goal_id, block),
            };
            return match classify_reconciliation(&expected, &observation) {
                Reconciliation::ActiveExact { head } => {
                    Ok(ManagedWorkspacePreparation::Active { head })
                }
                other => block_lifecycle(
                    store,
                    session,
                    goal_id,
                    "MANAGED_RECOVERY_REQUIRED",
                    format!("active managed workspace no longer reconciles exactly: {other:?}"),
                ),
            };
        }
        ManagedWorktreeLifecycle::CleanupEligible
        | ManagedWorktreeLifecycle::Blocked
        | ManagedWorktreeLifecycle::Removed => {
            return Ok(ManagedWorkspacePreparation::block(
                "MANAGED_RECOVERY_REQUIRED",
                format!(
                    "managed lifecycle {:?} requires explicit host recovery",
                    record.lifecycle()
                ),
            ));
        }
        ManagedWorktreeLifecycle::Requested | ManagedWorktreeLifecycle::Prepared => {}
    }

    // The remaining control states (paused, pausing, cancelling, blocked) run
    // the foreground loop with zero steps, so preparing would be a host Git
    // mutation the runner contract forbids. This is checked after the lifecycle
    // classification so a non-creatable workspace still reports the more
    // specific explicit-recovery code, and before the authority gate so no
    // durable evidence is written for a Goal that will not proceed.
    if goal.blocks_foreground_run() {
        return Ok(ManagedWorkspacePreparation::block(
            "MANAGED_GOAL_CONTROL_STATE",
            format!(
                "a {:?} Goal must not prepare a managed workspace; the foreground runner performs no work in this state",
                goal.status()
            ),
        ));
    }

    // The lifetime attempt budget is durable state, so this loop is bounded by
    // the record itself: every iteration either returns or consumes one attempt,
    // and `consume_creation_attempt` refuses once the lifetime bound is spent.
    // A process restart, a resume, or a repeated `goal_run` re-enters here with
    // the same spent budget rather than a fresh one.
    loop {
        let goal = store.load_goal(&session.id, goal_id)?;
        let Some(record) = goal.managed_worktree() else {
            return Err(ManagedWorkspaceError::GoalState(
                "MANAGED_WORKTREE workspace requires a durable managed-worktree record".to_owned(),
            ));
        };

        // Eligibility is a creation-time gate (design section 6). It runs on the
        // fresh REQUESTED -> PREPARED transition only.
        if record.lifecycle() == ManagedWorktreeLifecycle::Requested {
            let observation = match observe(git, record) {
                Ok(observation) => observation,
                Err(block) => return refuse_managed_workspace(store, session, goal_id, block),
            };
            let expected = ExpectedWorktreeTarget::from_record(record);
            if let Eligibility::Ineligible(reason) =
                classify_eligibility(record.primary_root(), &expected, &observation)
            {
                return block_lifecycle(
                    store,
                    session,
                    goal_id,
                    "MANAGED_ELIGIBILITY",
                    format!("{reason:?}"),
                );
            }
        }

        // Step 1: a durable PREPARED intent exists before anything else. It
        // records the exact target, not an attempt.
        let intent = build_intent(record)?;
        let prepared = if record.lifecycle() == ManagedWorktreeLifecycle::Requested {
            persist(store, session, goal_id, |goal| {
                goal.set_managed_creation_intent(intent.clone())
            })?
        } else {
            persist(store, session, goal_id, |goal| {
                goal.renew_managed_creation_intent(intent.clone())
            })?
        };
        let record = prepared.managed_worktree().ok_or_else(|| {
            ManagedWorkspaceError::GoalState("managed record vanished".to_owned())
        })?;
        let intent = prepared.managed_worktree_creation_intent().ok_or_else(|| {
            ManagedWorkspaceError::GoalState("managed intent vanished".to_owned())
        })?;
        let expected = ExpectedWorktreeTarget::from_record_and_intent(record, intent)?;

        // Step 2: reconcile before invoking anything. Adopting an exact side
        // effect costs no attempt, because no invocation happens.
        let pre = match observe(git, record) {
            Ok(observation) => observation,
            Err(block) => return refuse_managed_workspace(store, session, goal_id, block),
        };
        match classify_reconciliation(&expected, &pre) {
            Reconciliation::ActiveExact { head } => {
                // An attempt is consumed before every spawn, so a zero count
                // proves this host never invoked Git here. An exact-looking
                // worktree in that state was not created by this lifecycle and
                // must not be adopted as owned.
                if record.creation_attempts_consumed() == 0 {
                    return block_lifecycle(
                        store,
                        session,
                        goal_id,
                        "MANAGED_RECOVERY_REQUIRED",
                        "an exact managed worktree exists but no creation attempt was ever \
                         consumed, so it cannot be proven to be this Goal's side effect"
                            .to_owned(),
                    );
                }
                return activate(store, session, goal_id, &head);
            }
            Reconciliation::NoSideEffect { .. } => {}
            other => {
                return block_lifecycle(
                    store,
                    session,
                    goal_id,
                    "MANAGED_RECOVERY_REQUIRED",
                    format!("managed target does not reconcile before creation: {other:?}"),
                );
            }
        }

        // Step 3: platform approval for the host-native mutation. On Windows the
        // frozen design keeps this approval-gated; the workspace_mode opt-in and
        // the Session path authority are separate requirements and neither one
        // satisfies this gate.
        if requires_approval {
            let approval = ManagedCreationApproval::from_intent(intent, record);
            let approved = approver.approve(&approval).await;
            let approved = match approved {
                Ok(approved) => approved,
                Err(error) => {
                    return refuse_managed_workspace(
                        store,
                        session,
                        goal_id,
                        ManagedWorkspaceBlock {
                            code: "MANAGED_CREATION_APPROVAL_UNAVAILABLE",
                            detail: format!(
                                "host approval for managed worktree creation could not be \
                                 obtained: {error}"
                            ),
                        },
                    );
                }
            };
            if !approved {
                return refuse_managed_workspace(
                    store,
                    session,
                    goal_id,
                    ManagedWorkspaceBlock {
                        code: "MANAGED_CREATION_APPROVAL_DENIED",
                        detail: format!(
                            "the operator denied managed worktree creation for goal {}",
                            intent.goal_id().as_str()
                        ),
                    },
                );
            }
            // A stale approval must not authorize a different operation: re-read
            // the durable intent and require it still describes exactly what was
            // approved.
            let current = store.load_goal(&session.id, goal_id)?;
            let current_intent = current.managed_worktree_creation_intent().ok_or_else(|| {
                ManagedWorkspaceError::GoalState(
                    "the approved creation intent is no longer durable".to_owned(),
                )
            })?;
            if !approval.matches(current_intent) {
                return block_lifecycle(
                    store,
                    session,
                    goal_id,
                    "MANAGED_RECOVERY_REQUIRED",
                    "durable managed creation state changed while approval was pending, so the \
                     approval no longer authorizes this operation"
                        .to_owned(),
                );
            }
        }

        // Steps 4 and 5: consume one lifetime attempt and persist it. This is a
        // single durable mutation, and Git does not run unless it succeeds, so a
        // failed persist can never cost or grant an invocation.
        if !record.permits_creation_attempt() {
            return block_lifecycle(
                store,
                session,
                goal_id,
                "MANAGED_RETRY_EXHAUSTED",
                format!(
                    "the lifetime creation budget is spent ({consumed}/{MAX_LIFETIME_CREATION_ATTEMPTS}) \
                     and no further host Git creation invocation is authorized",
                    consumed = record.creation_attempts_consumed()
                ),
            );
        }
        let spent = persist(store, session, goal_id, |goal| {
            goal.consume_managed_creation_attempt()
        })?;
        let record = spent.managed_worktree().ok_or_else(|| {
            ManagedWorkspaceError::GoalState("managed record vanished".to_owned())
        })?;
        let intent = spent.managed_worktree_creation_intent().ok_or_else(|| {
            ManagedWorkspaceError::GoalState("managed intent vanished".to_owned())
        })?;
        let expected = ExpectedWorktreeTarget::from_record_and_intent(record, intent)?;

        // Step 6: the single authorized mutation.
        //
        // Neither the exit status nor a failure to start the process is treated
        // as a verdict. Design section 11 requires reconciliation after *every*
        // attempted creation, including failure, timeout, and interruption, so
        // the result is recorded as evidence and then decided by the read-only
        // observation below.
        let creation = ManagedWorktreeCreation::from_intent(intent)?;
        let attempt_outcome = creator.create(&creation, record.primary_root());
        let attempt_evidence = describe_creation_attempt(&creation, &attempt_outcome);

        // Step 7: read-only reconciliation decides the outcome.
        let post = match observe(git, record) {
            Ok(observation) => observation,
            Err(block) => return refuse_managed_workspace(store, session, goal_id, block),
        };
        match classify_reconciliation(&expected, &post) {
            Reconciliation::ActiveExact { head } => {
                return activate(store, session, goal_id, &head);
            }
            // Anything the frozen Phase 2 predicate does not certify as a
            // retryable no-side-effect result needs explicit host recovery, and
            // is never repaired with force.
            state if !state.permits_bounded_retry() => {
                return block_lifecycle(
                    store,
                    session,
                    goal_id,
                    "MANAGED_RECOVERY_REQUIRED",
                    format!(
                        "managed target does not reconcile after creation: {state:?}; {attempt_evidence}"
                    ),
                );
            }
            // Proven no side effect. Whether another attempt is authorized is
            // decided by the durable budget at the top of the next iteration,
            // never by a per-call loop counter.
            _ => continue,
        }
    }
}

/// Bound, human-readable evidence about one creation attempt.
///
/// Git's own diagnostic is host-owned and is the only place the real reason a
/// command failed appears, so it is recorded rather than dropped. It is bounded
/// because it ends up in a durable Goal blocker and in the `goal_run` result.
fn describe_creation_attempt(
    creation: &ManagedWorktreeCreation,
    attempt: &Result<
        crate::managed_worktree_create::ManagedWorktreeCreationOutcome,
        crate::managed_worktree_create::ManagedWorktreeCreationError,
    >,
) -> String {
    const MAX_EVIDENCE_CHARS: usize = 400;
    let text = match attempt {
        Ok(outcome) => {
            let status = match outcome.exit_code {
                Some(code) => format!("exit status {code}"),
                None => "terminated without an exit status".to_owned(),
            };
            let stderr: String = outcome
                .stderr
                .trim()
                .chars()
                .take(MAX_EVIDENCE_CHARS)
                .collect();
            if stderr.is_empty() {
                format!(
                    "git worktree add -b {} completed with {status} and no diagnostics",
                    creation.branch_name()
                )
            } else {
                format!(
                    "git worktree add -b {} completed with {status}: {stderr}",
                    creation.branch_name()
                )
            }
        }
        Err(error) => format!("git worktree add could not run: {error}"),
    };
    text.chars().take(MAX_EVIDENCE_CHARS * 2).collect()
}

/// Observe the repository for reconciliation, or explain why it could not be
/// observed.
///
/// An unobservable repository is a block, never an error and never a retry: the
/// host cannot classify side effects it cannot see. The managed lifecycle is
/// left intact so a later explicit host run can reconcile again.
fn observe(
    git: &dyn ReadOnlyGit,
    record: &ManagedWorktreeRecord,
) -> Result<RepositoryObservation, ManagedWorkspaceBlock> {
    let refs = vec![managed_branch_ref(record.goal_id())];
    observe_repository(git, record.primary_root(), &refs, &[record.worktree_root()]).map_err(
        |error| ManagedWorkspaceBlock {
            code: "MANAGED_OBSERVATION_UNAVAILABLE",
            detail: format!("the managed repository could not be observed: {error}"),
        },
    )
}

/// Build the exact durable intent for the current attempt.
///
/// A fresh operation ID is minted per attempt so two attempts never share an
/// identity, per the Phase 1 operation-identity contract.
fn build_intent(
    record: &ManagedWorktreeRecord,
) -> Result<ManagedWorktreeCreationIntent, ManagedWorkspaceError> {
    ManagedWorktreeCreationIntent::prepared(
        record.goal_id(),
        record.worktree_id().clone(),
        WorktreeOperationId::new(),
        record.repository_common_dir().to_path_buf(),
        record.worktree_root().to_path_buf(),
        record.base_commit().to_owned(),
    )
    .map_err(ManagedWorkspaceError::from)
}

fn persist(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    mutate: impl FnOnce(&mut Goal) -> Result<(), OrchestratorError>,
) -> Result<Goal, ManagedWorkspaceError> {
    let expected_revision = store.load_goal(&session.id, goal_id)?.revision();
    store
        .mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, _now| {
            mutate(goal)
        })
        .map_err(ManagedWorkspaceError::from)
}

fn activate(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    head: &str,
) -> Result<ManagedWorkspacePreparation, ManagedWorkspaceError> {
    // Design section 21: a workspace lifecycle mutation advances `Goal.revision`
    // and never `plan_revision`. The store enforces that, so this asserts the
    // invariant after the fact rather than deciding anything.
    let durable = persist(store, session, goal_id, |goal| {
        goal.activate_managed_worktree(head, &now_rfc3339())
    })?;
    debug_assert_eq!(
        durable.plan_revision(),
        0,
        "managed workspace activation must not change plan_revision"
    );
    Ok(ManagedWorkspacePreparation::Active {
        head: head.to_owned(),
    })
}

/// Record a durable Goal blocker explaining why preparation stopped, without
/// advancing the managed lifecycle.
///
/// This is used for refusals a later explicit host run can resolve: a Session
/// path-authority denial, or a repository that could not be observed. The
/// managed lifecycle deliberately stays `REQUESTED`/`PREPARED` so a later
/// `goal_run` can still proceed once the operator authorizes the root or the
/// repository becomes observable; burning the lifecycle to `BLOCKED` would
/// permanently strand the Goal behind a recovery operation that Managed
/// Worktrees V1 Phase 3 does not have.
fn refuse_managed_workspace(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    block: ManagedWorkspaceBlock,
) -> Result<ManagedWorkspacePreparation, ManagedWorkspaceError> {
    record_blocker(store, session, goal_id, block.code, block.detail.clone())?;
    Ok(ManagedWorkspacePreparation::Blocked(block))
}

/// Record a durable Goal blocker and move the managed lifecycle to `BLOCKED`.
///
/// Used for conditions that are not resolved by granting filesystem authority:
/// ineligibility, and every reconciliation state that needs explicit recovery.
fn block_lifecycle(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    code: &'static str,
    detail: String,
) -> Result<ManagedWorkspacePreparation, ManagedWorkspaceError> {
    let blocker_detail = detail.clone();
    let durable = persist(store, session, goal_id, |goal| {
        goal.add_blocker(crate::goal::GoalBlocker::new(code, blocker_detail, true))?;
        goal.block_managed_worktree()
    })?;
    debug_assert_eq!(
        durable.plan_revision(),
        0,
        "managed workspace blocking must not change plan_revision"
    );
    Ok(ManagedWorkspacePreparation::block(code, detail))
}

/// Append a durable, deduplicated Goal blocker.
///
/// The first detail recorded for a code is kept deliberately. Deduping keeps
/// the mutation a no-op on repeated refusals, and `mutate_goal_snapshot` then
/// leaves `Goal.revision` untouched, so re-running `goal_run` against an
/// unchanged cause does not churn durable state. The *current* detail is always
/// returned to the caller in [`ManagedWorkspacePreparation::Blocked`], so the
/// live reason is never lost.
fn record_blocker(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    code: &'static str,
    detail: String,
) -> Result<(), ManagedWorkspaceError> {
    let blocker_detail = detail.clone();
    persist(store, session, goal_id, |goal| {
        if goal.blockers().iter().any(|blocker| blocker.code() == code) {
            return Ok(());
        }
        goal.add_blocker(crate::goal::GoalBlocker::new(code, blocker_detail, true))
    })?;
    Ok(())
}

fn now_rfc3339() -> String {
    // Reuse the Goal store's own timestamp source so durable evidence is
    // consistent with the rest of the orchestrator.
    crate::task_store::utc_now_rfc3339()
}
