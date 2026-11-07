//! E3-B — the `goal_next` / `goal_submit` MCP contract.
//!
//! Two shapes, one rule: the host owns every decision.
//!
//! * `goal_next` advances host-only state by at most `max_host_steps` and then
//!   either hands back one bounded `ModelWorkItem` or a typed stop reason.
//! * `goal_submit` validates one untrusted model response against the same host
//!   validators the Codex path uses, records exactly one durable verdict
//!   (receipt plus WorkItem transition), and never claims success.
//!
//! `goal_submit` never applies a plan, writes a file, requests an approval, or
//! completes a Goal. Acceptance means "the host recorded this response"; the
//! receipt's `committed_revision` stays `None` until a later host phase (E4/E5)
//! applies and verifies it.
//!
//! Contract violations are returned as structured results, not transport
//! errors, so a client can tell "you violated the contract" from "the host
//! failed". The only transport errors here are malformed arguments, a Goal on
//! the wrong backend, and durable-state failures.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config;
use crate::external_goal_driver as driver;
use crate::external_goal_driver::{DriverError, DriverOutcome, DriverRefusal, DriverReport};
use crate::external_model_store::{commit_submission, external_state};
use crate::external_model_work::{
    MAX_HOST_STEPS_PER_CALL, ModelWorkItem, SubmissionDecision, SubmissionId, SubmissionReceipt,
    WorkId, WorkItemState, WorkRole, check_response_bounds, payload_digest,
};
use crate::goal::{Goal, GoalStatus};
use crate::goal_api::{self, GoalApiError};
use crate::planner;
use crate::task_store::{TaskStore, utc_now_rfc3339};

/// The minimum top-level fields a V1 Planner submission must carry.
///
/// The full role contract travels in the host-built `request`; this list is the
/// schema summary a client needs in order to answer at all, and it is pinned
/// here so `MODEL_REQUIRED` is actionable without reading Rust source.
pub(crate) const PLANNER_OUTPUT_REQUIRED_FIELDS: &[&str] = &[
    "goal_id",
    "goal_revision",
    "summary",
    "tasks",
    "criterion_bindings",
];

// ---------------------------------------------------------------------------
// Request shapes.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalNextRequest {
    session_id: String,
    #[serde(default)]
    goal_id: Option<String>,
    #[serde(default)]
    max_host_steps: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GoalSubmitRequest {
    session_id: String,
    goal_id: String,
    work_id: String,
    submission_id: String,
    expected_goal_revision: u64,
    request_digest: String,
    /// The untrusted model response. Never a permission grant (ADR-07).
    response: Value,
}

// ---------------------------------------------------------------------------
// Response shapes.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ExternalBlockerView {
    code: String,
    detail: String,
    mandatory: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ExternalProgressView {
    code: &'static str,
    detail: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ModelWorkView {
    work_id: String,
    session_id: String,
    goal_id: String,
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt_id: Option<String>,
    goal_revision: u64,
    plan_revision: u32,
    request_digest: String,
    /// The host-authored request, parsed so the client sees the exact contract it
    /// must answer. Never client-supplied.
    request: Value,
    output_contract: String,
    /// Minimum required top-level fields of the response object.
    response_required_fields: Vec<&'static str>,
    state: &'static str,
    issue_counter: u32,
    created_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ExternalGoalNextView {
    outcome: &'static str,
    goal_id: String,
    session_id: String,
    status: &'static str,
    revision: u64,
    plan_revision: u32,
    result_state: &'static str,
    result_kind: &'static str,
    execution_wait: &'static str,
    host_steps_executed: u32,
    max_host_steps: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_work: Option<ModelWorkView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_action: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blockers: Option<Vec<ExternalBlockerView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_action_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress: Option<ExternalProgressView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_work_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget: Option<goal_api::ExternalBudgetView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_stop: Option<goal_api::ExternalStopView>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ReceiptView {
    work_id: String,
    submission_id: String,
    payload_sha256: String,
    decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    committed_revision: Option<u64>,
    code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    redacted_detail: Option<String>,
    received_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ExternalGoalStateView {
    status: &'static str,
    revision: u64,
    plan_revision: u32,
    result_state: &'static str,
    result_kind: &'static str,
    execution_wait: &'static str,
    next_action: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ExternalGoalSubmitView {
    /// The closed decision vocabulary from B-02, plus the derived replay
    /// verdicts and `BUDGET_EXHAUSTED`.
    outcome: &'static str,
    code: &'static str,
    goal_id: String,
    session_id: String,
    work_id: String,
    submission_id: String,
    payload_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt: Option<ReceiptView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    goal_state: ExternalGoalStateView,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget: Option<goal_api::ExternalBudgetView>,
}

/// The single decision point that keeps an EXTERNAL Goal off the Codex path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GoalRunBackendDecision {
    /// `goal_run` may drive the existing Codex orchestration.
    Codex,
    /// `goal_run` must return `EXTERNAL_MODEL_REQUIRED` and start nothing.
    ExternalModelRequired,
}

/// Whether the diagnose-only Codex fallback may run for a session.
///
/// `codex_fallback` takes no Goal id, so the gate is the session's active Goal:
/// a session driving an EXTERNAL Goal must never reach the Codex diagnostic path
/// (ADR-08 / invariant 1). A session with no Goal, or with a CODEX Goal, keeps
/// exactly the previous behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CodexFallbackDecision {
    Allowed,
    RefusedForExternalBackend,
}

pub(crate) fn codex_fallback_decision(active_goal: Option<&Goal>) -> CodexFallbackDecision {
    match active_goal {
        Some(goal) if goal.model_backend().is_external() => {
            CodexFallbackDecision::RefusedForExternalBackend
        }
        _ => CodexFallbackDecision::Allowed,
    }
}

pub(crate) fn goal_run_backend_decision(goal: &Goal) -> GoalRunBackendDecision {
    if goal.model_backend().is_external() {
        GoalRunBackendDecision::ExternalModelRequired
    } else {
        GoalRunBackendDecision::Codex
    }
}

/// The pinned `goal_run` result for an EXTERNAL Goal.
///
/// This is a plain JSON text payload in the same shape every other `goal_run`
/// result uses, so an existing client can read it without a new code path.
pub(crate) fn external_goal_run_outcome(goal: &Goal) -> String {
    let payload = serde_json::json!({
        "outcome": "EXTERNAL_MODEL_REQUIRED",
        "model_backend": "EXTERNAL",
        "goal_id": goal.id().as_str(),
        "goal_status": goal_api::goal_status_name(goal.status()),
        "goal_revision": goal.revision(),
        "execution_wait": goal_api::execution_wait_for(goal),
        "next_action": "CALL_GOAL_NEXT",
        "codex_invoked": false,
        "detail": "EXTERNAL goals never start a Codex CLI process; drive them with goal_next and goal_submit",
    });
    serde_json::to_string(&payload).unwrap_or_else(|_| {
        "{\"outcome\":\"EXTERNAL_MODEL_REQUIRED\",\"codex_invoked\":false}".to_owned()
    })
}

// ---------------------------------------------------------------------------
// goal_next.
// ---------------------------------------------------------------------------

pub(crate) fn goal_next(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<ExternalGoalNextView, GoalApiError> {
    let request: GoalNextRequest = serde_json::from_value(args.clone()).map_err(|error| {
        GoalApiError::external(
            "INVALID_ARGUMENT",
            format!("invalid goal_next arguments: {error}"),
        )
    })?;
    goal_api::validate_session_binding(&request.session_id, session)?;
    let max_host_steps = match request.max_host_steps {
        None => MAX_HOST_STEPS_PER_CALL,
        Some(0) => {
            return Err(GoalApiError::external(
                "INVALID_ARGUMENT",
                "max_host_steps must be at least 1",
            ));
        }
        Some(value) if value > MAX_HOST_STEPS_PER_CALL => {
            return Err(GoalApiError::external(
                "HOST_STEP_BUDGET_EXCEEDED",
                format!("max_host_steps exceeds the host limit of {MAX_HOST_STEPS_PER_CALL}"),
            ));
        }
        Some(value) => value,
    };
    let goal = goal_api::resolve_goal(store, &session.id, request.goal_id.as_deref())?;

    if goal.model_backend().is_codex() {
        return Ok(no_action_view(&goal, max_host_steps, "MODEL_BACKEND_CODEX"));
    }
    if goal.status().is_terminal() {
        return Ok(terminal_view(&goal, max_host_steps));
    }
    match goal.status() {
        GoalStatus::Paused | GoalStatus::Pausing | GoalStatus::Cancelling => {
            return Ok(no_action_view(
                &goal,
                max_host_steps,
                goal_api::goal_status_name(goal.status()),
            ));
        }
        GoalStatus::Blocked => {
            return Ok(blocked_view(
                &goal,
                max_host_steps,
                "GOAL_BLOCKED",
                "the Goal is in a mandatory blocked state; host recovery is required",
            ));
        }
        _ => {}
    }

    let report = driver::next_external_step(store, session, &goal).map_err(driver_error)?;
    // Re-read so the reported revision and budget describe the committed state.
    let current = store
        .load_goal(&session.id, goal.id())
        .map_err(GoalApiError::from_orchestrator)?;
    Ok(driver_view(&current, report, max_host_steps))
}

fn driver_error(error: DriverError) -> GoalApiError {
    GoalApiError::external(error.code(), error.message())
}

fn no_action_view(goal: &Goal, max_host_steps: u32, reason: &'static str) -> ExternalGoalNextView {
    let mut builder = base_next_view(goal, "NO_ACTION_WITH_REASON", 0, max_host_steps);
    builder.no_action_reason = Some(reason);
    builder.build()
}

fn terminal_view(goal: &Goal, max_host_steps: u32) -> ExternalGoalNextView {
    let mut builder = base_next_view(goal, "TERMINAL", 0, max_host_steps);
    builder.next_action = Some("OBSERVE_TERMINAL_STATE");
    builder.build()
}

fn blocked_view(
    goal: &Goal,
    max_host_steps: u32,
    code: &str,
    detail: &str,
) -> ExternalGoalNextView {
    let mut builder = base_next_view(goal, "BLOCKED", 0, max_host_steps);
    builder.blockers = Some(vec![ExternalBlockerView {
        code: code.to_owned(),
        detail: detail.to_owned(),
        mandatory: true,
    }]);
    builder.build()
}

fn driver_view(goal: &Goal, report: DriverReport, max_host_steps: u32) -> ExternalGoalNextView {
    let steps = report.host_steps_executed;
    match report.outcome {
        DriverOutcome::ModelRequired(item) => {
            let mut builder = base_next_view(goal, "MODEL_REQUIRED", steps, max_host_steps);
            builder.model_work = Some(model_work_view(item));
            builder.next_action = Some("RESPOND_WITH_GOAL_SUBMIT");
            builder.build()
        }
        DriverOutcome::Progress { code, detail } => {
            let mut builder = base_next_view(goal, "HOST_PROGRESS", steps, max_host_steps);
            builder.progress = Some(ExternalProgressView { code, detail });
            builder.next_action = Some("CALL_GOAL_NEXT");
            builder.build()
        }
        DriverOutcome::Refused(DriverRefusal::RoleNotWired { stage }) => {
            let mut builder = base_next_view(goal, "NO_ACTION_WITH_REASON", steps, max_host_steps);
            builder.no_action_reason = Some("EXTERNAL_ROLE_NOT_WIRED");
            builder.blockers = Some(vec![ExternalBlockerView {
                code: "EXTERNAL_ROLE_NOT_WIRED".to_owned(),
                detail: format!(
                    "external model application for stage {stage} is not implemented in V1 (E4/E5)"
                ),
                mandatory: true,
            }]);
            builder.build()
        }
        DriverOutcome::Refused(DriverRefusal::Budget(block)) => {
            let mut builder = base_next_view(goal, "BLOCKED", steps, max_host_steps);
            builder.blockers = Some(vec![ExternalBlockerView {
                code: block.code().to_owned(),
                detail: block.detail().to_owned(),
                mandatory: true,
            }]);
            builder.build()
        }
        DriverOutcome::Refused(DriverRefusal::AwaitingHostApplication { work_id, role }) => {
            let mut builder = base_next_view(goal, "NO_ACTION_WITH_REASON", steps, max_host_steps);
            builder.no_action_reason = Some("EXTERNAL_ROLE_APPLICATION_NOT_WIRED");
            builder.pending_work_id = Some(work_id.as_str().to_owned());
            builder.blockers = Some(vec![ExternalBlockerView {
                code: "EXTERNAL_ROLE_APPLICATION_NOT_WIRED".to_owned(),
                detail: format!(
                    "the accepted {} response awaits the host application phase, which V1 does not implement (E4/E5)",
                    role.as_str()
                ),
                mandatory: true,
            }]);
            builder.build()
        }
    }
}

fn model_work_view(item: ModelWorkItem) -> ModelWorkView {
    let request = serde_json::from_str(item.request_payload()).unwrap_or(Value::Null);
    ModelWorkView {
        work_id: item.work_id().as_str().to_owned(),
        session_id: item.session_id().to_owned(),
        goal_id: item.goal_id().to_owned(),
        role: item.role().as_str(),
        task_id: item.task_id().map(str::to_owned),
        attempt_id: item.attempt_id().map(str::to_owned),
        goal_revision: item.goal_revision(),
        plan_revision: item.plan_revision(),
        request_digest: item.request_digest().to_owned(),
        request,
        output_contract: item.output_contract().to_owned(),
        response_required_fields: PLANNER_OUTPUT_REQUIRED_FIELDS.to_vec(),
        state: item.state().as_str(),
        issue_counter: item.issue_counter(),
        created_at: item.created_at().to_owned(),
    }
}

/// Every `goal_next` branch fills the shared durable-state projection exactly
/// once, so no branch can omit `execution_wait`, `pending_work_id`, or budget.
struct NextViewBuilder {
    goal_id: String,
    session_id: String,
    status: &'static str,
    revision: u64,
    plan_revision: u32,
    result_state: &'static str,
    result_kind: &'static str,
    execution_wait: &'static str,
    outcome: &'static str,
    host_steps_executed: u32,
    max_host_steps: u32,
    model_work: Option<ModelWorkView>,
    next_action: Option<&'static str>,
    blockers: Option<Vec<ExternalBlockerView>>,
    no_action_reason: Option<&'static str>,
    progress: Option<ExternalProgressView>,
    pending_work_id: Option<String>,
    budget: Option<goal_api::ExternalBudgetView>,
    last_stop: Option<goal_api::ExternalStopView>,
}

fn base_next_view(
    goal: &Goal,
    outcome: &'static str,
    host_steps_executed: u32,
    max_host_steps: u32,
) -> NextViewBuilder {
    NextViewBuilder {
        goal_id: goal.id().as_str().to_owned(),
        session_id: goal.session_id().to_owned(),
        status: goal_api::goal_status_name(goal.status()),
        revision: goal.revision(),
        plan_revision: goal.plan_revision(),
        result_state: result_state_name(goal),
        result_kind: goal_api::result_kind_for_status(goal.status()),
        execution_wait: goal_api::execution_wait_for(goal),
        outcome,
        host_steps_executed,
        max_host_steps,
        model_work: None,
        next_action: None,
        blockers: None,
        no_action_reason: None,
        progress: None,
        pending_work_id: goal_api::pending_work_id_for(goal),
        budget: goal_api::budget_view_for(goal),
        last_stop: goal_api::last_stop_view_for(goal),
    }
}

impl NextViewBuilder {
    fn build(self) -> ExternalGoalNextView {
        ExternalGoalNextView {
            outcome: self.outcome,
            goal_id: self.goal_id,
            session_id: self.session_id,
            status: self.status,
            revision: self.revision,
            plan_revision: self.plan_revision,
            result_state: self.result_state,
            result_kind: self.result_kind,
            execution_wait: self.execution_wait,
            host_steps_executed: self.host_steps_executed,
            max_host_steps: self.max_host_steps,
            model_work: self.model_work,
            next_action: self.next_action,
            blockers: self.blockers,
            no_action_reason: self.no_action_reason,
            progress: self.progress,
            pending_work_id: self.pending_work_id,
            budget: self.budget,
            last_stop: self.last_stop,
        }
    }
}

// ---------------------------------------------------------------------------
// goal_submit.
// ---------------------------------------------------------------------------

pub(crate) fn goal_submit(
    args: &Value,
    session: &config::Session,
    store: &TaskStore,
) -> Result<ExternalGoalSubmitView, GoalApiError> {
    let request: GoalSubmitRequest = serde_json::from_value(args.clone()).map_err(|error| {
        GoalApiError::external(
            "INVALID_ARGUMENT",
            format!("invalid goal_submit arguments: {error}"),
        )
    })?;
    goal_api::validate_session_binding(&request.session_id, session)?;
    let goal_id = goal_api::parse_goal_id(&request.goal_id)?;
    let work_id = parse_work_id(&request.work_id)?;
    let submission_id = parse_submission_id(&request.submission_id)?;
    let goal = store
        .load_goal(&session.id, &goal_id)
        .map_err(GoalApiError::from_orchestrator)?;
    if goal.model_backend().is_codex() {
        return Err(GoalApiError::external(
            "MODEL_BACKEND_CODEX",
            "goal_submit requires an EXTERNAL backend Goal",
        ));
    }

    let payload_sha256 = payload_digest(&request.response);
    let state = external_state(&goal).map_err(GoalApiError::from_orchestrator)?;

    // Idempotency first: a replay must not re-validate, re-apply, or consume
    // budget, and a conflicting duplicate must never replace the first verdict.
    if let Some(stored) = state.find_receipt(&submission_id) {
        let outcome = if stored.payload_sha256() == payload_sha256 {
            "REPLAY_OF_ACCEPTED_SUBMISSION"
        } else {
            "CONFLICTING_DUPLICATE"
        };
        return Ok(submit_view(
            &goal,
            &request,
            outcome,
            outcome,
            Some(receipt_view(stored)),
            None,
            &payload_sha256,
            next_action_for_outcome(outcome),
        ));
    }

    // A cancelled or otherwise terminal Goal is reported as such before any
    // WorkItem lookup, so a delayed submission cannot revive it.
    if goal.status().is_terminal() || goal.status() == GoalStatus::Cancelling {
        return Ok(rejected_view(
            &goal,
            &request,
            "CANCELLED_STALE",
            "GOAL_CANCELLED",
            &payload_sha256,
            "STOP_AND_RECONCILE",
        ));
    }

    // An unknown or foreign work_id is refused without touching durable state
    // and without confirming anything about another Goal or session
    // (TM-02 / TM-06).
    let Some(active) = state.active_work() else {
        return Ok(unknown_work_id_view(&goal, &request, &payload_sha256));
    };
    if active.work_id() != &work_id || active.goal_id() != goal.id().as_str() {
        return Ok(unknown_work_id_view(&goal, &request, &payload_sha256));
    }
    if active.state() != WorkItemState::Pending {
        return Ok(rejected_view(
            &goal,
            &request,
            "REJECTED_AUTHORITY",
            "WORK_ITEM_NOT_PENDING",
            &payload_sha256,
            "STOP_AND_RECONCILE",
        ));
    }
    if request.expected_goal_revision != goal.revision() {
        return Ok(rejected_view(
            &goal,
            &request,
            "REJECTED_STALE_REVISION",
            "GOAL_REVISION_STALE",
            &payload_sha256,
            "CALL_GOAL_NEXT",
        ));
    }
    if active.goal_revision() != goal.revision() || active.plan_revision() != goal.plan_revision() {
        return Ok(rejected_view(
            &goal,
            &request,
            "REJECTED_STALE_REVISION",
            "WORK_ITEM_REVISION_STALE",
            &payload_sha256,
            "CALL_GOAL_NEXT",
        ));
    }
    if request.request_digest != active.request_digest() {
        return Ok(rejected_view(
            &goal,
            &request,
            "REJECTED_AUTHORITY",
            "REQUEST_DIGEST_MISMATCH",
            &payload_sha256,
            "STOP_AND_RECONCILE",
        ));
    }

    let (decision, code, detail) = role_verdict(session, &goal, active, &request.response)?;
    let outcome = decision.as_str();

    if !state.receipt_capacity_available() {
        return Ok(rejected_view(
            &goal,
            &request,
            "BUDGET_EXHAUSTED",
            "RECEIPT_STORE_FULL",
            &payload_sha256,
            "STOP_AND_RECONCILE",
        ));
    }

    let receipt = SubmissionReceipt::new(
        work_id,
        submission_id.clone(),
        payload_sha256.clone(),
        decision,
        None,
        code,
        detail,
        utc_now_rfc3339(),
    );
    let committed = commit_submission(store, &session.id, &goal_id, goal.revision(), receipt)
        .map_err(GoalApiError::from_orchestrator)?;
    let stored = external_state(&committed)
        .map_err(GoalApiError::from_orchestrator)?
        .find_receipt(&submission_id)
        .map(receipt_view);
    Ok(submit_view(
        &committed,
        &request,
        outcome,
        code,
        stored,
        None,
        &payload_sha256,
        next_action_for_outcome(outcome),
    ))
}

fn unknown_work_id_view(
    goal: &Goal,
    request: &GoalSubmitRequest,
    payload_sha256: &str,
) -> ExternalGoalSubmitView {
    rejected_view(
        goal,
        request,
        "REJECTED_AUTHORITY",
        "UNKNOWN_WORK_ID",
        payload_sha256,
        "STOP_AND_RECONCILE",
    )
}

/// Validate one untrusted response with the host's existing role validator.
///
/// A role that V1 does not wire is refused as `ROLE_NOT_WIRED_EXTERNAL_V1`
/// instead of being accepted, so an unimplemented role can never be reported as
/// a success.
fn role_verdict(
    session: &config::Session,
    goal: &Goal,
    item: &ModelWorkItem,
    response: &Value,
) -> Result<(SubmissionDecision, &'static str, Option<String>), GoalApiError> {
    if let Err(violation) = check_response_bounds(response) {
        return Ok((
            SubmissionDecision::RejectedSchema,
            violation.code(),
            Some(violation.detail().to_owned()),
        ));
    }
    match item.role() {
        WorkRole::Planner => {
            let bytes = serde_json::to_vec(response).map_err(|_| {
                GoalApiError::external(
                    "INVALID_ARGUMENT",
                    "the submitted response could not be serialized for host validation",
                )
            })?;
            let goal_root = planner::execution_root_for_goal(goal, session).map_err(|error| {
                GoalApiError::external(
                    planner::planner_failure_code(&error),
                    "the host could not derive the Goal execution root for validation",
                )
            })?;
            match planner::validate_initial_plan_proposal_output(&bytes, goal, &goal_root) {
                Ok(()) => Ok((SubmissionDecision::AcceptedForHostProcessing, "OK", None)),
                Err(refusal) => Ok((
                    SubmissionDecision::RejectedSchema,
                    refusal.code(),
                    Some(refusal.detail().to_owned()),
                )),
            }
        }
        other => Ok((
            SubmissionDecision::RejectedSchema,
            "ROLE_NOT_WIRED_EXTERNAL_V1",
            Some(format!(
                "external {} responses are not wired to the host validators in V1 (E4/E5)",
                other.as_str()
            )),
        )),
    }
}

fn parse_work_id(value: &str) -> Result<WorkId, GoalApiError> {
    WorkId::parse(value).map_err(|_| {
        GoalApiError::external(
            "INVALID_ARGUMENT",
            "work_id must be a canonical lowercase UUID",
        )
    })
}

fn parse_submission_id(value: &str) -> Result<SubmissionId, GoalApiError> {
    SubmissionId::parse(value).map_err(|_| {
        GoalApiError::external(
            "INVALID_ARGUMENT",
            "submission_id must be a canonical lowercase UUID",
        )
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "The response projection carries goal, request, verdict, and receipt channels explicitly."
)]
fn submit_view(
    goal: &Goal,
    request: &GoalSubmitRequest,
    outcome: &'static str,
    code: &'static str,
    receipt: Option<ReceiptView>,
    detail: Option<String>,
    payload_sha256: &str,
    next_action: &'static str,
) -> ExternalGoalSubmitView {
    ExternalGoalSubmitView {
        outcome,
        code,
        goal_id: goal.id().as_str().to_owned(),
        session_id: goal.session_id().to_owned(),
        work_id: request.work_id.clone(),
        submission_id: request.submission_id.clone(),
        payload_sha256: payload_sha256.to_owned(),
        receipt,
        detail,
        goal_state: goal_state_view(goal, next_action),
        budget: goal_api::budget_view_for(goal),
    }
}

fn rejected_view(
    goal: &Goal,
    request: &GoalSubmitRequest,
    outcome: &'static str,
    code: &'static str,
    payload_sha256: &str,
    next_action: &'static str,
) -> ExternalGoalSubmitView {
    submit_view(
        goal,
        request,
        outcome,
        code,
        None,
        None,
        payload_sha256,
        next_action,
    )
}

fn receipt_view(receipt: &SubmissionReceipt) -> ReceiptView {
    ReceiptView {
        work_id: receipt.work_id().as_str().to_owned(),
        submission_id: receipt.submission_id().as_str().to_owned(),
        payload_sha256: receipt.payload_sha256().to_owned(),
        decision: receipt.decision().as_str(),
        committed_revision: receipt.committed_revision(),
        code: receipt.code().to_owned(),
        redacted_detail: receipt.redacted_detail().map(str::to_owned),
        received_at: receipt.received_at().to_owned(),
    }
}

fn goal_state_view(goal: &Goal, next_action: &'static str) -> ExternalGoalStateView {
    ExternalGoalStateView {
        status: goal_api::goal_status_name(goal.status()),
        revision: goal.revision(),
        plan_revision: goal.plan_revision(),
        result_state: result_state_name(goal),
        result_kind: goal_api::result_kind_for_status(goal.status()),
        execution_wait: goal_api::execution_wait_for(goal),
        next_action,
    }
}

fn result_state_name(goal: &Goal) -> &'static str {
    if goal.is_terminal() {
        "TERMINAL"
    } else {
        "NOT_TERMINAL"
    }
}

fn next_action_for_outcome(outcome: &'static str) -> &'static str {
    match outcome {
        "ACCEPTED_FOR_HOST_PROCESSING"
        | "REJECTED_SCHEMA"
        | "REPLAY_OF_ACCEPTED_SUBMISSION"
        | "REJECTED_STALE_REVISION"
        | "BUDGET_EXHAUSTED" => "CALL_GOAL_NEXT",
        _ => "STOP_AND_RECONCILE",
    }
}
