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
use crate::external_model_store::{
    PlannerSubmissionOutcome, commit_planner_submission, commit_submission, external_state,
};
use crate::external_model_work::{
    MAX_HOST_STEPS_PER_CALL, ModelWorkItem, SubmissionDecision, SubmissionId, SubmissionReceipt,
    WorkId, WorkItemState, WorkRole, check_response_bounds, payload_digest,
};
use crate::external_publish::{self, ReviewerOutcome};
use crate::external_writer::{self, WriterStageOutcome};
use crate::goal::{Goal, GoalId, GoalStatus};
use crate::goal_api::{self, GoalApiError};
use crate::readonly_worker::{self, ReadonlyApplyOutcome};
use crate::task::TaskId;
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

/// The minimum top-level fields a V1 Writer submission must carry.
pub(crate) const WRITER_OUTPUT_REQUIRED_FIELDS: &[&str] = &[
    "goal_id",
    "task_id",
    "attempt_id",
    "goal_revision",
    "plan_revision",
    "status",
    "summary",
    "evidence",
    "proposed_operations",
];

/// The minimum top-level fields a V1 Readonly submission must carry.
///
/// The identity fields are host-issued values the client must echo back, which
/// is how the host refuses an answer aimed at a superseded Attempt.
pub(crate) const REVIEWER_OUTPUT_REQUIRED_FIELDS: &[&str] = &[
    "goal_id",
    "task_id",
    "attempt_id",
    "goal_revision",
    "plan_revision",
    "summary",
    "blocking_findings",
    "evidence",
];

pub(crate) const READONLY_OUTPUT_REQUIRED_FIELDS: &[&str] = &[
    "goal_id",
    "task_id",
    "attempt_id",
    "goal_revision",
    "plan_revision",
    "status",
    "summary",
    "evidence",
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
    /// verdicts, `COMMITTED`, and `BUDGET_EXHAUSTED`.
    outcome: &'static str,
    /// Borrowed from a durable receipt when one exists, so it is owned here.
    code: String,
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

/// Advance an EXTERNAL Goal by at most `max_host_steps` host steps (E4-B).
///
/// One call performs as many *host-owned* steps as the caller's budget allows
/// and stops the moment a step needs external inference, refuses, or reports a
/// typed no-action. The loop is bounded twice: by `max_host_steps` and by the
/// observation that every ``Progress`` step must commit a new Goal revision, so
/// a step that reports progress without advancing is never repeated.
pub(crate) async fn goal_next(
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

    let mut current = goal;
    let mut steps = 0u32;
    let report = loop {
        let report = driver::advance_external_step(store, session, &current)
            .await
            .map_err(driver_error)?;
        steps = steps.saturating_add(report.host_steps_executed);
        if !matches!(report.outcome, DriverOutcome::Progress { .. }) {
            break report;
        }
        // A progress step must have committed a new revision. Re-read so the
        // reported revision, budget, and any terminal status describe the
        // committed state, and refuse to loop if nothing advanced.
        let reloaded = store
            .load_goal(&session.id, current.id())
            .map_err(GoalApiError::from_orchestrator)?;
        if reloaded.revision() == current.revision() {
            break DriverReport {
                outcome: DriverOutcome::Progress {
                    code: "HOST_STEP_DEFERRED",
                    detail: "the host step made no durable progress; call goal_next again",
                },
                host_steps_executed: 0,
            };
        }
        let terminal = reloaded.status().is_terminal();
        current = reloaded;
        if terminal || steps >= max_host_steps {
            break report;
        }
    };
    // Always report the persisted snapshot: an issuance commits a new revision,
    // so the pre-step snapshot would understate the Goal's revision and hide the
    // active WorkItem (`execution_wait`) the client must answer.
    let final_goal = store
        .load_goal(&session.id, current.id())
        .map_err(GoalApiError::from_orchestrator)?;
    Ok(driver_view(&final_goal, report, steps, max_host_steps))
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

fn driver_view(
    goal: &Goal,
    report: DriverReport,
    steps: u32,
    max_host_steps: u32,
) -> ExternalGoalNextView {
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
            builder.next_action = Some(if goal.status().is_terminal() {
                "OBSERVE_TERMINAL_STATE"
            } else {
                "CALL_GOAL_NEXT"
            });
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
        DriverOutcome::Refused(DriverRefusal::UnrecoverableSubmitted { work_id }) => {
            let mut builder = base_next_view(goal, "BLOCKED", steps, max_host_steps);
            builder.pending_work_id = Some(work_id.as_str().to_owned());
            builder.blockers = Some(vec![ExternalBlockerView {
                code: "EXTERNAL_SUBMITTED_RECOVERY_UNSAFE".to_owned(),
                detail: "an accepted pre-E4 submission has no durable model artifact, so the host cannot apply it; the stored receipts are preserved and the Goal needs owner recovery".to_owned(),
                mandatory: true,
            }]);
            builder.build()
        }
        DriverOutcome::Refused(DriverRefusal::HostStepRefused { code, detail }) => {
            let mut builder = base_next_view(goal, "BLOCKED", steps, max_host_steps);
            builder.blockers = Some(vec![ExternalBlockerView {
                code: code.to_owned(),
                detail: detail.to_owned(),
                mandatory: true,
            }]);
            builder.build()
        }
        DriverOutcome::Refused(DriverRefusal::NoAction { code }) => {
            let mut builder = base_next_view(goal, "NO_ACTION_WITH_REASON", steps, max_host_steps);
            builder.no_action_reason = Some(code);
            builder.build()
        }
        DriverOutcome::Refused(DriverRefusal::DependencyWaiting { code, detail }) => {
            let mut builder = base_next_view(goal, "NO_ACTION_WITH_REASON", steps, max_host_steps);
            builder.no_action_reason = Some(code);
            builder.blockers = Some(vec![ExternalBlockerView {
                code: code.to_owned(),
                detail,
                mandatory: false,
            }]);
            builder.build()
        }
    }
}

fn model_work_view(item: ModelWorkItem) -> ModelWorkView {
    let required_fields = match item.role() {
        WorkRole::Readonly => READONLY_OUTPUT_REQUIRED_FIELDS,
        WorkRole::Writer => WRITER_OUTPUT_REQUIRED_FIELDS,
        WorkRole::Reviewer => REVIEWER_OUTPUT_REQUIRED_FIELDS,
        _ => PLANNER_OUTPUT_REQUIRED_FIELDS,
    };
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
        response_required_fields: required_fields.to_vec(),
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
    //
    // The lookup key is `(work_id, submission_id, payload_sha256)`, not
    // `submission_id` alone. A receipt only ever describes the WorkItem it was
    // recorded against, so a submission_id that was already used by a different
    // WorkItem of the same Goal must not be accepted as a replay: returning the
    // old receipt there would let a client claim a verdict for work the host
    // never received. The refusal is a fixed code with no detail, so it cannot
    // become an oracle for another Goal or session.
    if let Some(stored) = state.find_receipt(&submission_id) {
        if stored.work_id() != &work_id {
            return Ok(rejected_view(
                &goal,
                &request,
                "REJECTED_AUTHORITY",
                "SUBMISSION_WORK_ID_MISMATCH",
                &payload_sha256,
                "STOP_AND_RECONCILE",
            ));
        }
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

    if let Err(violation) = check_response_bounds(&request.response) {
        return submit_bounded_refusal(
            store,
            &goal,
            &goal_id,
            &request,
            &work_id,
            &submission_id,
            &payload_sha256,
            violation.code(),
            violation.detail(),
        );
    }

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

    // The Planner is the first role whose response has a host effect (H1-2 /
    // E4-A): validation, Task DAG application, plan revision, receipt, and the
    // WorkItem's COMMITTED transition are one durable commit.
    if active.role() == WorkRole::Planner {
        return submit_planner_response(
            store,
            session,
            &goal,
            &goal_id,
            &request,
            &work_id,
            &submission_id,
            &payload_sha256,
        );
    }

    // The Writer role (E5-A) validates the untrusted proposal with the existing
    // writer validator, seals its typed WRITE_UTF8 content into a private stage,
    // and records the prepared MutationIntent in one commit. It does NOT mutate
    // the workspace here: host publication is a separate, reviewed step.
    if active.role() == WorkRole::Writer {
        return submit_writer_response(
            store,
            session,
            &goal,
            &goal_id,
            &request,
            &work_id,
            &submission_id,
            &payload_sha256,
            active,
        );
    }

    // The Reviewer role (E5-B) validates the untrusted verdict against the durable
    // postimage and lets the Task's own REVIEW_GATE decide. A pass only reaches
    // VERIFYING; the host Task Verifier still owns COMPLETED.
    if active.role() == WorkRole::Reviewer {
        return submit_reviewer_response(
            store,
            session,
            &goal,
            &goal_id,
            &request,
            &work_id,
            &submission_id,
            &payload_sha256,
        );
    }

    // The Readonly role (E4-B) applies the answer through the existing readonly
    // parser and the existing Task state table, in one commit with its receipt.
    // The final Task verdict is still the host Task Verifier's, not the model's.
    if active.role() == WorkRole::Readonly {
        return submit_readonly_response(
            store,
            session,
            &goal,
            &goal_id,
            &request,
            &work_id,
            &submission_id,
            &payload_sha256,
            active,
        );
    }

    let (decision, code, detail) = role_verdict(&goal, active, &request.response)?;
    let outcome = decision.as_str();

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

/// Record one schema refusal that needs no role validator (bounded response).
///
/// The receipt and the WorkItem's REJECTED transition are one commit, exactly
/// like a validator refusal, so a refused WorkItem never stays active.
#[expect(
    clippy::too_many_arguments,
    reason = "The durable refusal keeps goal, work identity, digest, and verdict channels explicit."
)]
fn submit_bounded_refusal(
    store: &TaskStore,
    goal: &Goal,
    goal_id: &GoalId,
    request: &GoalSubmitRequest,
    work_id: &WorkId,
    submission_id: &SubmissionId,
    payload_sha256: &str,
    code: &'static str,
    detail: String,
) -> Result<ExternalGoalSubmitView, GoalApiError> {
    let receipt = SubmissionReceipt::new(
        work_id.clone(),
        submission_id.clone(),
        payload_sha256.to_owned(),
        SubmissionDecision::RejectedSchema,
        None,
        code,
        Some(detail),
        utc_now_rfc3339(),
    );
    let committed = commit_submission(store, goal.session_id(), goal_id, goal.revision(), receipt)
        .map_err(GoalApiError::from_orchestrator)?;
    let stored = external_state(&committed)
        .map_err(GoalApiError::from_orchestrator)?
        .find_receipt(submission_id)
        .map(receipt_view);
    Ok(submit_view(
        &committed,
        request,
        "REJECTED_SCHEMA",
        code,
        stored,
        None,
        payload_sha256,
        next_action_for_outcome("REJECTED_SCHEMA"),
    ))
}

/// Validate and apply one external Planner submission atomically (E4-A).
///
/// The whole Planner effect — proposal validation, Task DAG materialization,
/// criterion binding, plan revision, receipt, and the WorkItem transition — is
/// a single `TaskStore` commit, so the failure modes H1-2 names are
/// unrepresentable: there is no durable state in which the plan is applied
/// without its receipt, or its receipt without the plan.
#[expect(
    clippy::too_many_arguments,
    reason = "Goal, work identity, digest, and the untrusted payload are independent authority channels."
)]
fn submit_planner_response(
    store: &TaskStore,
    session: &config::Session,
    goal: &Goal,
    goal_id: &GoalId,
    request: &GoalSubmitRequest,
    work_id: &WorkId,
    submission_id: &SubmissionId,
    payload_sha256: &str,
) -> Result<ExternalGoalSubmitView, GoalApiError> {
    let bytes = serde_json::to_vec(&request.response).map_err(|_| {
        GoalApiError::external(
            "INVALID_ARGUMENT",
            "the submitted response could not be serialized for host validation",
        )
    })?;
    let outcome = commit_planner_submission(
        store,
        session,
        goal_id,
        goal.revision(),
        work_id,
        submission_id,
        payload_sha256,
        &bytes,
    )
    .map_err(GoalApiError::from_orchestrator)?;

    match outcome {
        PlannerSubmissionOutcome::Committed { goal, receipt } => Ok(submit_view(
            &goal,
            request,
            "COMMITTED",
            "PLAN_APPLIED",
            Some(receipt_view(&receipt)),
            None,
            payload_sha256,
            next_action_for_outcome("COMMITTED"),
        )),
        PlannerSubmissionOutcome::Rejected { goal, receipt } => Ok(submit_view(
            &goal,
            request,
            "REJECTED_SCHEMA",
            receipt.code(),
            Some(receipt_view(&receipt)),
            receipt.redacted_detail().map(str::to_owned),
            payload_sha256,
            next_action_for_outcome("REJECTED_SCHEMA"),
        )),
        PlannerSubmissionOutcome::Refused { code } => Ok(rejected_view(
            goal,
            request,
            "REJECTED_STALE_REVISION",
            code,
            payload_sha256,
            "CALL_GOAL_NEXT",
        )),
    }
}

/// Validate and apply one external Readonly submission (E4-B).
///
/// The response is untrusted data: it is parsed by the existing readonly parser,
/// checked for identity/revision/schema/evidence bounds, and then applied to the
/// durable Task through the same state table the Codex readonly path uses. A
/// model that claims success only reaches `VERIFYING`; the host Task Verifier
/// decides whether the Task passed.
#[expect(
    clippy::too_many_arguments,
    reason = "Goal, work identity, digest, and the untrusted payload are independent authority channels."
)]
fn submit_readonly_response(
    store: &TaskStore,
    session: &config::Session,
    goal: &Goal,
    goal_id: &GoalId,
    request: &GoalSubmitRequest,
    work_id: &WorkId,
    submission_id: &SubmissionId,
    payload_sha256: &str,
    active: &ModelWorkItem,
) -> Result<ExternalGoalSubmitView, GoalApiError> {
    let Some(task_id) = active.task_id().and_then(|value| TaskId::parse(value).ok()) else {
        return Ok(unknown_work_id_view(goal, request, payload_sha256));
    };
    let Some(attempt_id) = active.attempt_id() else {
        return Ok(unknown_work_id_view(goal, request, payload_sha256));
    };
    let raw = serde_json::to_vec(&request.response).map_err(|_| {
        GoalApiError::external(
            "INVALID_ARGUMENT",
            "the submitted response could not be serialized for host validation",
        )
    })?;

    let revision = goal.revision();
    let outcome = match readonly_worker::validate_external_readonly_output(
        goal, session, &task_id, attempt_id, &raw,
    ) {
        Ok(output) => readonly_worker::commit_external_readonly_result(
            store,
            session,
            goal_id,
            revision,
            work_id,
            submission_id,
            payload_sha256,
            output,
        )
        .map_err(GoalApiError::from_orchestrator)?,
        Err(refusal) => readonly_worker::commit_external_readonly_refusal(
            store,
            session,
            goal_id,
            revision,
            work_id,
            submission_id,
            payload_sha256,
            refusal.code(),
            refusal.detail(),
        )
        .map_err(GoalApiError::from_orchestrator)?,
    };

    match outcome {
        ReadonlyApplyOutcome::Committed { goal, receipt } => {
            let code = receipt.code().to_owned();
            Ok(submit_view(
                &goal,
                request,
                "COMMITTED",
                &code,
                Some(receipt_view(&receipt)),
                None,
                payload_sha256,
                next_action_for_outcome("COMMITTED"),
            ))
        }
        ReadonlyApplyOutcome::Rejected { goal, receipt } => Ok(submit_view(
            &goal,
            request,
            "REJECTED_SCHEMA",
            receipt.code(),
            Some(receipt_view(&receipt)),
            receipt.redacted_detail().map(str::to_owned),
            payload_sha256,
            next_action_for_outcome("REJECTED_SCHEMA"),
        )),
        ReadonlyApplyOutcome::Refused { code } => Ok(rejected_view(
            goal,
            request,
            "REJECTED_STALE_REVISION",
            code,
            payload_sha256,
            "CALL_GOAL_NEXT",
        )),
    }
}

/// Validate, stage, and durably prepare one external Writer submission (E5-A).
///
/// Acceptance means "the host validated and sealed these exact bytes and recorded
/// a prepared mutation intent". It is never proof of a filesystem change, never
/// proof of review, and never Goal completion.
#[expect(
    clippy::too_many_arguments,
    reason = "Goal, work identity, digest, and the untrusted payload are independent authority channels."
)]
fn submit_writer_response(
    store: &TaskStore,
    session: &config::Session,
    goal: &Goal,
    goal_id: &GoalId,
    request: &GoalSubmitRequest,
    work_id: &WorkId,
    submission_id: &SubmissionId,
    payload_sha256: &str,
    active: &ModelWorkItem,
) -> Result<ExternalGoalSubmitView, GoalApiError> {
    let Some(task_id) = active.task_id().and_then(|value| TaskId::parse(value).ok()) else {
        return Ok(unknown_work_id_view(goal, request, payload_sha256));
    };
    let Some(attempt_id) = active.attempt_id() else {
        return Ok(unknown_work_id_view(goal, request, payload_sha256));
    };
    let raw = serde_json::to_vec(&request.response).map_err(|_| {
        GoalApiError::external(
            "INVALID_ARGUMENT",
            "the submitted response could not be serialized for host validation",
        )
    })?;
    let revision = goal.revision();
    let outcome = match external_writer::validate_external_writer_output(
        goal, session, &task_id, attempt_id, &raw,
    ) {
        Ok(output) => external_writer::commit_external_writer_preparation(
            store,
            session,
            goal_id,
            revision,
            work_id,
            submission_id,
            payload_sha256,
            output,
        )
        .map_err(GoalApiError::from_orchestrator)?,
        Err(refusal) => {
            // A refused proposal is recorded like a Codex writer refusal: the
            // receipt, the worker report, and the pre-mutation Task transition
            // in one commit. Still no stage and no workspace write.
            let class = external_writer::WriterRefusalClass::for_code(refusal.code());
            let committed = external_writer::commit_external_writer_refusal(
                store,
                session,
                goal_id,
                revision,
                work_id,
                submission_id,
                payload_sha256,
                refusal.code(),
                refusal.detail(),
                class,
            )
            .map_err(GoalApiError::from_orchestrator)?;
            return match committed {
                WriterStageOutcome::RecordedNonMutating { goal, receipt } => {
                    let code = receipt.code().to_owned();
                    Ok(submit_view(
                        &goal,
                        request,
                        "REJECTED_SCHEMA",
                        &code,
                        Some(receipt_view(&receipt)),
                        Some(refusal.detail().to_owned()),
                        payload_sha256,
                        next_action_for_outcome("REJECTED_SCHEMA"),
                    ))
                }
                WriterStageOutcome::Refused { code } => Ok(rejected_view(
                    goal,
                    request,
                    "REJECTED_STALE_REVISION",
                    code,
                    payload_sha256,
                    "CALL_GOAL_NEXT",
                )),
                WriterStageOutcome::Prepared { .. } => Err(GoalApiError::external(
                    "INVALID_ARGUMENT",
                    "a refused writer proposal cannot be prepared",
                )),
            };
        }
    };

    match outcome {
        WriterStageOutcome::Prepared {
            goal,
            receipt,
            operation_count,
            ..
        } => Ok(submit_view(
            &goal,
            request,
            "ACCEPTED_FOR_HOST_PROCESSING",
            external_writer::WRITE_INTENT_PREPARED,
            Some(receipt_view(&receipt)),
            Some(format!(
                "the host validated and sealed {operation_count} writer operation(s); workspace publication is a separate host step"
            )),
            payload_sha256,
            next_action_for_outcome("ACCEPTED_FOR_HOST_PROCESSING"),
        )),
        WriterStageOutcome::RecordedNonMutating { goal, receipt } => Ok(submit_view(
            &goal,
            request,
            "COMMITTED",
            receipt.code(),
            Some(receipt_view(&receipt)),
            None,
            payload_sha256,
            next_action_for_outcome("COMMITTED"),
        )),
        WriterStageOutcome::Refused { code } => Ok(rejected_view(
            goal,
            request,
            "REJECTED_STALE_REVISION",
            code,
            payload_sha256,
            "CALL_GOAL_NEXT",
        )),
    }
}

/// Validate and record one external Reviewer submission (E5-B).
#[expect(
    clippy::too_many_arguments,
    reason = "Goal, work identity, digest, and the untrusted payload are independent authority channels."
)]
fn submit_reviewer_response(
    store: &TaskStore,
    session: &config::Session,
    goal: &Goal,
    goal_id: &GoalId,
    request: &GoalSubmitRequest,
    work_id: &WorkId,
    submission_id: &SubmissionId,
    payload_sha256: &str,
) -> Result<ExternalGoalSubmitView, GoalApiError> {
    let raw = serde_json::to_vec(&request.response).map_err(|_| {
        GoalApiError::external(
            "INVALID_ARGUMENT",
            "the submitted response could not be serialized for host validation",
        )
    })?;
    let outcome = external_publish::commit_external_reviewer_result(
        store,
        session,
        goal_id,
        goal.revision(),
        work_id,
        submission_id,
        payload_sha256,
        &raw,
    )
    .map_err(GoalApiError::from_orchestrator)?;

    match outcome {
        ReviewerOutcome::Recorded {
            goal,
            receipt,
            blocked,
        } => {
            let code = receipt.code().to_owned();
            Ok(submit_view(
                &goal,
                request,
                if blocked {
                    "REJECTED_SCHEMA"
                } else {
                    "COMMITTED"
                },
                &code,
                Some(receipt_view(&receipt)),
                None,
                payload_sha256,
                next_action_for_outcome(if blocked {
                    "REJECTED_SCHEMA"
                } else {
                    "COMMITTED"
                }),
            ))
        }
        ReviewerOutcome::Refused { code } => Ok(rejected_view(
            goal,
            request,
            "REJECTED_STALE_REVISION",
            code,
            payload_sha256,
            "CALL_GOAL_NEXT",
        )),
    }
}

/// Validate one untrusted response with the host's existing role validator.
///
/// A role that V1 does not wire is refused as `ROLE_NOT_WIRED_EXTERNAL_V1`
/// instead of being accepted, so an unimplemented role can never be reported as
/// a success.
fn role_verdict(
    goal: &Goal,
    item: &ModelWorkItem,
    response: &Value,
) -> Result<(SubmissionDecision, &'static str, Option<String>), GoalApiError> {
    let _ = (goal, response);
    Ok((
        SubmissionDecision::RejectedSchema,
        "ROLE_NOT_WIRED_EXTERNAL_V1",
        Some(format!(
            "external {} responses are not wired to the host validators in V1 (E4/E5)",
            item.role().as_str()
        )),
    ))
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
    code: &str,
    receipt: Option<ReceiptView>,
    detail: Option<String>,
    payload_sha256: &str,
    next_action: &'static str,
) -> ExternalGoalSubmitView {
    ExternalGoalSubmitView {
        outcome,
        code: code.to_owned(),
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
        | "COMMITTED"
        | "REJECTED_SCHEMA"
        | "REPLAY_OF_ACCEPTED_SUBMISSION"
        | "REJECTED_STALE_REVISION"
        | "BUDGET_EXHAUSTED" => "CALL_GOAL_NEXT",
        _ => "STOP_AND_RECONCILE",
    }
}
