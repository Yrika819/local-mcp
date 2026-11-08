//! E5-A — external Writer integration, phases W0 (issue) and W1 (validate +
//! private durable stage).
//!
//! The Writer is the only role whose output can change the workspace, so it is
//! deliberately **not** applied in one commit like Planner/Readonly. This module
//! implements the two non-mutating phases and leaves host publication to a
//! separate, reviewed step:
//!
//! * **W0 (issue)** — one commit starts the Writer Attempt (`READY → RUNNING`,
//!   which is also what records the workspace mutation lease) and registers the
//!   host-authored `ModelWorkItem`. No filesystem write happens.
//! * **W1 (validate + stage)** — the submitted proposal is validated by the
//!   existing writer validator (`parse_and_validate_writer_result` +
//!   `materialize_operations`, the same functions the Codex path uses), its
//!   typed `WRITE_UTF8` content is sealed into a private content-addressed stage,
//!   and one commit records the prepared `MutationIntent`, the accepted receipt,
//!   and the WorkItem's `SUBMITTED` transition. Still no workspace write.
//!
//! Publication (W2) lives in `external_publish`: this module only validates,
//! seals, and durably prepares. Keeping the two apart is what makes each phase's
//! authority claim exactly as strong as its evidence.
//!
//! Authority rules pinned here:
//!
//! 1. The model never supplies a path, a preimage, an operation id, a request id,
//!    or a revision that the host uses. Only `WRITE_UTF8` content originates in
//!    the proposal; every other field is host-derived during materialization.
//! 2. The stage holds content only. Paths and preimages live in the durable
//!    `MutationIntent`, so a tampered stage cannot redirect a write.
//! 3. A non-`candidate_complete` proposal never stages, never mutates, and never
//!    leaves the lease held: it records the permitted Task transition and a
//!    receipt in one commit.
//! 4. Replaying the same submission returns the stored receipt without a second
//!    stage; a conflicting payload is refused.

use serde::Serialize;

use crate::config;
use crate::external_model_store::{external_state, external_state_mut};
use crate::external_model_work::{
    IssueBlock, MAX_REQUEST_PAYLOAD_BYTES, ModelWorkItem, OUTPUT_CONTRACT_WRITER_V1, SubmissionId,
    SubmissionReceipt, WorkId, WorkItemState, WorkRole, sha256_hex,
};
use crate::goal::{Goal, GoalId};
use crate::mutation::{MutationIntent, MutationOperationIntent};
use crate::orchestrator_error::OrchestratorError;
use crate::task::{TaskId, TaskStatus, TaskTransitionContext};
use crate::task_store::TaskStore;
use crate::writer::{self, WriterError, WriterRequest, WriterStatus};
use crate::writer_stage::{self, StageError, StageIdentity, WriteStage};

/// Progress code: a validated Writer proposal is durably prepared.
pub(crate) const WRITE_INTENT_PREPARED: &str = "WRITE_INTENT_PREPARED";

/// Bounded, model-free refusal for a Writer submission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WriterValidationRefusal {
    code: &'static str,
    detail: &'static str,
}

impl WriterValidationRefusal {
    pub(crate) fn code(self) -> &'static str {
        self.code
    }

    pub(crate) fn detail(self) -> &'static str {
        self.detail
    }
}

/// Result of issuing one external Writer WorkItem.
#[expect(
    clippy::large_enum_variant,
    reason = "The issuance result is short-lived; handing back the durable WorkItem is the point."
)]
pub(crate) enum WriterIssueOutcome {
    Issued { item: ModelWorkItem },
    Refused(IssueBlock),
}

/// Result of accepting one external Writer submission.
pub(crate) enum WriterStageOutcome {
    /// The proposal is durably prepared and awaits host publication.
    Prepared {
        goal: Goal,
        receipt: SubmissionReceipt,
        operation_count: usize,
    },
    /// A non-mutating status was recorded (blocked / needs_replan / failed).
    RecordedNonMutating {
        goal: Goal,
        receipt: SubmissionReceipt,
    },
    /// Nothing matched the durable binding; nothing was written.
    Refused { code: &'static str },
}

/// Host-authored request handed to the external Writer client.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ExternalWriterWorkRequest {
    pub(crate) goal_id: String,
    pub(crate) task_id: String,
    pub(crate) attempt_id: String,
    pub(crate) goal_revision: u64,
    pub(crate) plan_revision: u32,
    pub(crate) goal_cwd: std::path::PathBuf,
    pub(crate) task_title: String,
    pub(crate) task_objective: String,
    pub(crate) allowed_paths: Vec<std::path::PathBuf>,
    pub(crate) forbidden_paths: Vec<std::path::PathBuf>,
    /// The only operation kinds V1 accepts from an external Writer.
    pub(crate) allowed_operations: Vec<&'static str>,
    pub(crate) output_contract: &'static str,
}

fn external_request(request: &WriterRequest) -> ExternalWriterWorkRequest {
    ExternalWriterWorkRequest {
        goal_id: request.goal_id().to_owned(),
        task_id: request.task_id().to_owned(),
        attempt_id: request.attempt_id().to_owned(),
        goal_revision: request.goal_revision(),
        plan_revision: request.plan_revision(),
        goal_cwd: request.goal_cwd().to_path_buf(),
        task_title: request.task_title().to_owned(),
        task_objective: request.task_objective().to_owned(),
        allowed_paths: request.allowed_paths().to_vec(),
        forbidden_paths: request.forbidden_paths().to_vec(),
        allowed_operations: vec!["WRITE_UTF8"],
        output_contract: OUTPUT_CONTRACT_WRITER_V1,
    }
}

/// W0 — start the Writer Attempt and register its WorkItem in one commit.
pub(crate) fn issue_external_writer_work_item(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    expected_revision: u64,
) -> Result<WriterIssueOutcome, OrchestratorError> {
    let mut slot: Option<Result<ModelWorkItem, IssueBlock>> = None;
    let _committed =
        store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
            if let Err(block) = external_state(goal)?.check_issue_budget(WorkRole::Writer) {
                slot = Some(Err(block));
                return Ok(());
            }
            writer::ensure_writer_task_ready(goal, task_id)?;
            let bound_revision = goal.revision().checked_add(1).ok_or_else(|| {
                OrchestratorError::CorruptGoal("goal revision overflow".to_owned())
            })?;
            let issue_counter = external_state(goal)?
                .issue_counter()
                .checked_add(1)
                .ok_or_else(|| {
                    OrchestratorError::CorruptGoal("external work item counter overflow".to_owned())
                })?;
            let plan_revision = goal.plan_revision();
            // The Attempt must exist before the request can name it. A failure
            // after this point aborts the whole commit, so no half-started
            // Attempt (or half-held lease) survives.
            goal.transition_task(
                task_id,
                TaskStatus::Running,
                TaskTransitionContext::default(),
                now,
            )?;
            let request =
                writer::build_writer_request_at_revision(goal, task_id, session, bound_revision)
                    .map_err(|_| {
                        OrchestratorError::InvalidDag(
                            "external writer request could not be built".to_owned(),
                        )
                    })?;
            let attempt_id = goal.tasks()[task_id]
                .latest_attempt()
                .ok_or_else(|| {
                    OrchestratorError::CorruptGoal(
                        "writer Attempt was not created for the external request".to_owned(),
                    )
                })?
                .id()
                .as_str()
                .to_owned();
            let payload = serde_json::to_string(&external_request(&request)).map_err(|_| {
                OrchestratorError::InvalidDag(
                    "external writer request could not be serialized".to_owned(),
                )
            })?;
            if payload.len() > MAX_REQUEST_PAYLOAD_BYTES {
                return Err(OrchestratorError::InvalidDag(
                    "external writer request exceeds the durable WorkItem bound".to_owned(),
                ));
            }
            let item = ModelWorkItem::pending(
                WorkId::new(),
                session.id.clone(),
                goal_id.as_str().to_owned(),
                WorkRole::Writer,
                Some(task_id.as_str().to_owned()),
                Some(attempt_id),
                bound_revision,
                plan_revision,
                sha256_hex(payload.as_bytes()),
                payload,
                OUTPUT_CONTRACT_WRITER_V1,
                issue_counter,
                now,
            )?;
            external_state_mut(goal)?.record_issued(item.clone())?;
            slot = Some(Ok(item));
            Ok(())
        })?;

    match slot {
        Some(Ok(item)) => Ok(WriterIssueOutcome::Issued { item }),
        Some(Err(block)) => Ok(WriterIssueOutcome::Refused(block)),
        None => Err(OrchestratorError::CorruptGoal(
            "external writer issuance committed without a WorkItem".to_owned(),
        )),
    }
}

/// A validated proposal, kept opaque so only the host can act on it.
pub(crate) struct ValidatedWriterOutput {
    request: WriterRequest,
    status: WriterStatus,
    summary: String,
    report_digest: String,
    /// `(index, content)` for a candidate-complete proposal; empty otherwise.
    staged_content: Vec<(u32, String)>,
    /// Host-derived operation metadata, used to build the durable intent.
    plan: Vec<HostPlannedWrite>,
}

/// One host-derived write plan entry. Path and preimage come from
/// materialization, never from the model.
pub(crate) struct HostPlannedWrite {
    pub(crate) index: u32,
    pub(crate) path: std::path::PathBuf,
    pub(crate) expected_preimage: crate::mutation::MutationPreimage,
    pub(crate) observed_before: crate::mutation::FileObservation,
    pub(crate) intended_after_sha256: String,
    pub(crate) request_id: String,
}

/// W1a — validate one untrusted external Writer proposal against the durable
/// Attempt. No durable write happens here.
pub(crate) fn validate_external_writer_output(
    goal: &Goal,
    session: &config::Session,
    task_id: &TaskId,
    attempt_id: &str,
    raw: &[u8],
) -> Result<ValidatedWriterOutput, WriterValidationRefusal> {
    let request = writer::build_writer_request_at_revision(goal, task_id, session, goal.revision())
        .map_err(|_| WriterValidationRefusal {
            code: "WRITER_REQUEST_UNAVAILABLE",
            detail: "the host could not rebuild the writer request for this Task",
        })?;
    if request.attempt_id() != attempt_id {
        return Err(WriterValidationRefusal {
            code: "WRITER_ATTEMPT_STALE",
            detail: "the Writer Attempt this WorkItem was bound to is no longer current",
        });
    }
    let result =
        writer::parse_and_validate_writer_result(raw, &request).map_err(|error| match error {
            WriterError::AuthorityViolation(_) => WriterValidationRefusal {
                code: "WRITER_IDENTITY_MISMATCH",
                detail: "the writer result identity or revision does not match the host request",
            },
            WriterError::OutputInvalid(_) => WriterValidationRefusal {
                code: "WRITER_OUTPUT_INVALID",
                detail: "the writer result failed the host schema checks",
            },
            _ => WriterValidationRefusal {
                code: "WRITER_OUTPUT_REJECTED",
                detail: "the host rejected the writer proposal",
            },
        })?;
    let report_digest = sha256_hex(raw);

    if result.status() != WriterStatus::CandidateComplete {
        return Ok(ValidatedWriterOutput {
            request,
            status: result.status(),
            summary: result.summary().to_owned(),
            report_digest,
            staged_content: Vec::new(),
            plan: Vec::new(),
        });
    }

    // Host path/preimage authority: the same materializer the Codex path uses.
    let operations =
        writer::materialize_operations(goal, session, task_id, result.operations_ref()).map_err(
            |error| match error {
                WriterError::PreimageMismatch(_) => WriterValidationRefusal {
                    code: "WRITER_PREIMAGE_MISMATCH",
                    detail: "a writer target no longer matches its expected preimage",
                },
                WriterError::AuthorityViolation(_) => WriterValidationRefusal {
                    code: "WRITER_SCOPE_VIOLATION",
                    detail: "the writer proposal targets a path outside the Task scope",
                },
                _ => WriterValidationRefusal {
                    code: "WRITER_OPERATION_REJECTED",
                    detail: "the host rejected a writer operation",
                },
            },
        )?;

    let request_ids = operations
        .iter()
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect::<Vec<_>>();
    let mut staged_content = Vec::with_capacity(operations.len());
    let mut plan = Vec::with_capacity(operations.len());
    for (index, operation) in operations.iter().enumerate() {
        let index = u32::try_from(index).map_err(|_| WriterValidationRefusal {
            code: "WRITER_OPERATION_REJECTED",
            detail: "the writer proposal has too many operations",
        })?;
        staged_content.push((index, operation.content().to_owned()));
        plan.push(HostPlannedWrite {
            index,
            path: operation.path().to_path_buf(),
            expected_preimage: writer::mutation_preimage(operation.expected_preimage()),
            observed_before: writer::mutation_observation(operation.before()),
            intended_after_sha256: sha256_hex(operation.content().as_bytes()),
            request_id: request_ids[index as usize].clone(),
        });
    }

    Ok(ValidatedWriterOutput {
        request,
        status: result.status(),
        summary: result.summary().to_owned(),
        report_digest,
        staged_content,
        plan,
    })
}

/// W1b — seal the stage and record the prepared intent, the receipt, and the
/// WorkItem transition in one durable commit.
///
/// The stage files are written first (outside the lock) and are only *linked* to
/// the Goal by this commit. A crash before the commit leaves an orphan stage and
/// no journal entry, which recovery treats as never-begun; a crash after it
/// leaves a fully linked, verifiable preparation.
#[expect(
    clippy::too_many_arguments,
    reason = "Goal, work identity, digest, and the validated proposal are independent authority channels."
)]
pub(crate) fn commit_external_writer_preparation(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    expected_revision: u64,
    work_id: &WorkId,
    submission_id: &SubmissionId,
    payload_sha256: &str,
    output: ValidatedWriterOutput,
) -> Result<WriterStageOutcome, OrchestratorError> {
    // A non-candidate result never stages and never mutates.
    if output.status != WriterStatus::CandidateComplete {
        return commit_non_mutating_writer_result(
            store,
            session,
            goal_id,
            expected_revision,
            work_id,
            submission_id,
            payload_sha256,
            output,
        );
    }

    let task_id = TaskId::parse(output.request.task_id()).map_err(|_| {
        OrchestratorError::CorruptGoal("writer request carries an invalid Task id".to_owned())
    })?;
    // E5-H2: the stage id must be unique per *submission candidate*, not per
    // Attempt. Two clients can hold the same Pending WorkItem and stage different
    // content concurrently; with an Attempt-derived id they would share (and
    // overwrite) one directory, and the loser's cleanup would delete the winner's
    // linked stage. A fresh nonce per candidate makes every stage immutable and
    // unambiguously owned.
    let operation_id = format!(
        "e5-writer:{}:{}",
        output.request.attempt_id(),
        uuid::Uuid::new_v4()
    );
    let scope_identity = crate::workspace_publish::scope_identity(
        &output
            .plan
            .iter()
            .map(|planned| (planned.path.clone(), planned.intended_after_sha256.clone()))
            .collect::<Vec<_>>(),
    );
    let identity = StageIdentity {
        session_id: session.id.clone(),
        goal_id: goal_id.as_str().to_owned(),
        task_id: task_id.as_str().to_owned(),
        attempt_id: output.request.attempt_id().to_owned(),
        plan_revision: output.request.plan_revision(),
        operation_id: operation_id.clone(),
        scope_identity: scope_identity.clone(),
    };

    // Seal the private stage before linking it to the Goal.
    #[cfg(test)]
    test_hooks::before_stage(payload_sha256, &operation_id);
    let stage =
        match writer_stage::write_stage(store.state_root(), &identity, &output.staged_content) {
            Ok(stage) => stage,
            Err(error) => return Err(stage_error_to_orchestrator(error)),
        };
    #[cfg(test)]
    test_hooks::after_stage(payload_sha256, &operation_id);
    if stage.operation_count() != output.plan.len() {
        return Err(OrchestratorError::CorruptGoal(
            "writer stage does not match its host-derived plan".to_owned(),
        ));
    }

    let intent = MutationIntent::new(
        operation_id.clone(),
        scope_identity,
        output
            .plan
            .iter()
            .map(|planned| {
                MutationOperationIntent::new(
                    planned.index,
                    planned.path.clone(),
                    planned.expected_preimage.clone(),
                    planned.observed_before.clone(),
                    planned.intended_after_sha256.clone(),
                    planned.request_id.clone(),
                )
            })
            .collect::<Vec<_>>(),
    )
    .map_err(|error| OrchestratorError::InvalidDag(error.to_string()))?;
    let request_ids = output
        .plan
        .iter()
        .map(|planned| planned.request_id.clone())
        .collect::<Vec<_>>();
    let summary = output.summary.clone();
    let report_digest = output.report_digest.clone();
    let attempt_id = output.request.attempt_id().to_owned();
    let operation_id_str = operation_id.clone();

    let mut outcome: Option<SubmissionReceipt> = None;
    let mut refused: Option<&'static str> = None;
    let committed =
        store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
            let binding_ok = matches!(
                external_state(goal)?.active_work(),
                Some(active)
                    if active.work_id() == work_id
                        && active.state() == WorkItemState::Pending
                        && active.goal_revision() == goal.revision()
                        && active.plan_revision() == goal.plan_revision()
                        && active.task_id() == Some(task_id.as_str())
                        && active.attempt_id() == Some(attempt_id.as_str())
            );
            if !binding_ok {
                refused = Some("WORK_ITEM_NOT_PENDING");
                return Ok(());
            }
            writer::ensure_writer_task_ready_for_attempt(goal, &task_id, &attempt_id)?;
            goal.task_prepare_latest_mutation_intent(&task_id, intent.clone())?;
            for request_id in &request_ids {
                goal.task_bind_latest_attempt_execution(
                    &task_id,
                    None,
                    None,
                    Some(request_id.clone()),
                    None,
                    None,
                    None,
                    None,
                )?;
            }
            goal.task_record_latest_worker_report(
                &task_id,
                crate::task::WorkerReport::new(summary.clone(), Vec::new()),
            )?;
            let attempt = goal.tasks()[&task_id]
                .latest_attempt()
                .ok_or_else(|| {
                    OrchestratorError::CorruptGoal("writer Task lacks an Attempt".to_owned())
                })?
                .id()
                .clone();
            goal.task_add_evidence(
                &task_id,
                crate::task::TaskEvidence::WorkerReport {
                    attempt_id: attempt,
                    report_digest: report_digest.clone(),
                },
            )?;
            let receipt = SubmissionReceipt::new(
                work_id.clone(),
                submission_id.clone(),
                payload_sha256.to_owned(),
                crate::external_model_work::SubmissionDecision::AcceptedForHostProcessing,
                None,
                "WRITE_INTENT_PREPARED",
                None,
                now,
            );
            external_state_mut(goal)?.record_submission(receipt.clone(), now)?;
            outcome = Some(receipt);
            Ok(())
        });

    match committed {
        Ok(goal) => {
            if let Some(code) = refused {
                // Our commit did not happen. Remove *our own* stage only, and only
                // after proving the durable state does not reference this id.
                remove_own_stage_if_unlinked(store, session, goal_id, &operation_id);
                return Ok(WriterStageOutcome::Refused { code });
            }
            let receipt = outcome.ok_or_else(|| {
                OrchestratorError::CorruptGoal(
                    "writer preparation committed without a receipt".to_owned(),
                )
            })?;
            debug_assert_eq!(operation_id.as_str(), operation_id_str);
            Ok(WriterStageOutcome::Prepared {
                goal,
                receipt,
                operation_count: output.plan.len(),
            })
        }
        Err(OrchestratorError::RevisionConflict { .. }) => {
            // Another client committed first. Its stage is sealed under its own
            // id; ours is unlinked, so only ours is removed.
            remove_own_stage_if_unlinked(store, session, goal_id, &operation_id);
            Ok(WriterStageOutcome::Refused {
                code: "GOAL_REVISION_STALE",
            })
        }
        Err(error) => {
            remove_own_stage_if_unlinked(store, session, goal_id, &operation_id);
            Err(error)
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Goal, work identity, digest, and the validated proposal are independent authority channels."
)]
fn commit_non_mutating_writer_result(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    expected_revision: u64,
    work_id: &WorkId,
    submission_id: &SubmissionId,
    payload_sha256: &str,
    output: ValidatedWriterOutput,
) -> Result<WriterStageOutcome, OrchestratorError> {
    let task_id = TaskId::parse(output.request.task_id()).map_err(|_| {
        OrchestratorError::CorruptGoal("writer request carries an invalid Task id".to_owned())
    })?;
    let attempt_id = output.request.attempt_id().to_owned();
    let next = match output.status {
        WriterStatus::Blocked => TaskStatus::Blocked,
        WriterStatus::NeedsReplan => TaskStatus::NeedsReplan,
        WriterStatus::Failed => TaskStatus::Failed,
        WriterStatus::CandidateComplete => {
            return Err(OrchestratorError::CorruptGoal(
                "candidate_complete reached the non-mutating writer path".to_owned(),
            ));
        }
    };
    let summary = output.summary.clone();
    let report_digest = output.report_digest.clone();
    let code = match output.status {
        WriterStatus::Blocked => "WRITER_BLOCKED",
        WriterStatus::NeedsReplan => "WRITER_NEEDS_REPLAN",
        WriterStatus::Failed => "WRITER_FAILED",
        WriterStatus::CandidateComplete => unreachable!("checked above"),
    };

    let mut outcome: Option<SubmissionReceipt> = None;
    let mut refused: Option<&'static str> = None;
    let committed =
        store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
            let binding_ok = matches!(
                external_state(goal)?.active_work(),
                Some(active)
                    if active.work_id() == work_id
                        && active.state() == WorkItemState::Pending
                        && active.goal_revision() == goal.revision()
                        && active.plan_revision() == goal.plan_revision()
                        && active.task_id() == Some(task_id.as_str())
                        && active.attempt_id() == Some(attempt_id.as_str())
            );
            if !binding_ok {
                refused = Some("WORK_ITEM_NOT_PENDING");
                return Ok(());
            }
            writer::ensure_writer_task_ready_for_attempt(goal, &task_id, &attempt_id)?;
            goal.task_record_latest_worker_report(
                &task_id,
                crate::task::WorkerReport::new(summary.clone(), Vec::new()),
            )?;
            let attempt = goal.tasks()[&task_id]
                .latest_attempt()
                .ok_or_else(|| {
                    OrchestratorError::CorruptGoal("writer Task lacks an Attempt".to_owned())
                })?
                .id()
                .clone();
            goal.task_add_evidence(
                &task_id,
                crate::task::TaskEvidence::WorkerReport {
                    attempt_id: attempt,
                    report_digest: report_digest.clone(),
                },
            )?;
            if next == TaskStatus::Blocked {
                goal.task_add_blocker(
                    &task_id,
                    crate::task::TaskBlocker::new("WRITER_BLOCKED", summary.clone(), true),
                )?;
            }
            goal.transition_task(&task_id, next, TaskTransitionContext::default(), now)?;
            let receipt = SubmissionReceipt::new(
                work_id.clone(),
                submission_id.clone(),
                payload_sha256.to_owned(),
                crate::external_model_work::SubmissionDecision::Committed,
                Some(goal.revision().checked_add(1).ok_or_else(|| {
                    OrchestratorError::CorruptGoal("goal revision overflow".to_owned())
                })?),
                code,
                None,
                now,
            );
            external_state_mut(goal)?.record_submission(receipt.clone(), now)?;
            outcome = Some(receipt);
            Ok(())
        });

    match committed {
        Ok(goal) => {
            if let Some(code) = refused {
                return Ok(WriterStageOutcome::Refused { code });
            }
            let receipt = outcome.ok_or_else(|| {
                OrchestratorError::CorruptGoal(
                    "writer non-mutating commit produced no receipt".to_owned(),
                )
            })?;
            Ok(WriterStageOutcome::RecordedNonMutating { goal, receipt })
        }
        Err(OrchestratorError::RevisionConflict { .. }) => Ok(WriterStageOutcome::Refused {
            code: "GOAL_REVISION_STALE",
        }),
        Err(error) => Err(error),
    }
}

/// How a refused pre-mutation Writer proposal must leave the Task.
///
/// Mirrors the Codex writer path: a stale preimage is a `NEEDS_REPLAN` (the
/// proposal shape, not the Task, was wrong), while every other pre-mutation
/// refusal is a mandatory `BLOCKED` with the existing refusal code. Nothing here
/// mutates the workspace, and no stage exists yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriterRefusalClass {
    NeedsReplan,
    Blocked,
}

impl WriterRefusalClass {
    pub(crate) fn for_code(code: &str) -> Self {
        if code == "WRITER_PREIMAGE_MISMATCH" {
            Self::NeedsReplan
        } else {
            Self::Blocked
        }
    }
}

/// Record one refused Writer submission: receipt, worker report, and the
/// pre-mutation Task transition in a single commit.
///
/// This is the Writer equivalent of the readonly refusal path. It exists so a
/// rejected proposal can neither wedge the Task in `RUNNING` (which would stop
/// the Goal forever) nor be retried implicitly against the same Attempt.
#[expect(
    clippy::too_many_arguments,
    reason = "Goal, work identity, digest, verdict, and refusal class are independent authority channels."
)]
pub(crate) fn commit_external_writer_refusal(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    expected_revision: u64,
    work_id: &WorkId,
    submission_id: &SubmissionId,
    payload_sha256: &str,
    code: &'static str,
    detail: &'static str,
    class: WriterRefusalClass,
) -> Result<WriterStageOutcome, OrchestratorError> {
    let mut applied: Option<SubmissionReceipt> = None;
    let mut refused: Option<&'static str> = None;
    let committed =
        store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
            let binding = {
                let state = external_state(goal)?;
                match state.active_work() {
                    Some(active)
                        if active.work_id() == work_id
                            && active.state() == WorkItemState::Pending
                            && active.goal_revision() == goal.revision() =>
                    {
                        Some(active.clone())
                    }
                    _ => None,
                }
            };
            let Some(active) = binding else {
                refused = Some("WORK_ITEM_NOT_PENDING");
                return Ok(());
            };
            let Some(task_id) = active.task_id().and_then(|value| TaskId::parse(value).ok()) else {
                refused = Some("WORK_ITEM_TASK_INVALID");
                return Ok(());
            };
            let Some(attempt_id) = active.attempt_id() else {
                refused = Some("WORK_ITEM_ATTEMPT_INVALID");
                return Ok(());
            };
            writer::ensure_writer_task_ready_for_attempt(goal, &task_id, attempt_id)?;
            match class {
                WriterRefusalClass::Blocked => {
                    goal.task_add_blocker(
                        &task_id,
                        crate::task::TaskBlocker::new(code, detail, true),
                    )?;
                    goal.transition_task(
                        &task_id,
                        TaskStatus::Blocked,
                        TaskTransitionContext::default(),
                        now,
                    )?;
                }
                WriterRefusalClass::NeedsReplan => goal.transition_task(
                    &task_id,
                    TaskStatus::NeedsReplan,
                    TaskTransitionContext::default(),
                    now,
                )?,
            }
            let receipt = SubmissionReceipt::new(
                work_id.clone(),
                submission_id.clone(),
                payload_sha256.to_owned(),
                crate::external_model_work::SubmissionDecision::RejectedSchema,
                None,
                code,
                Some(detail.to_owned()),
                now,
            );
            external_state_mut(goal)?.record_submission(receipt.clone(), now)?;
            applied = Some(receipt);
            Ok(())
        });

    match committed {
        Ok(goal) => {
            if let Some(code) = refused {
                return Ok(WriterStageOutcome::Refused { code });
            }
            let receipt = applied.ok_or_else(|| {
                OrchestratorError::CorruptGoal(
                    "writer refusal committed without a receipt".to_owned(),
                )
            })?;
            Ok(WriterStageOutcome::RecordedNonMutating { goal, receipt })
        }
        Err(OrchestratorError::RevisionConflict { .. }) => Ok(WriterStageOutcome::Refused {
            code: "GOAL_REVISION_STALE",
        }),
        Err(error) => Err(error),
    }
}

/// W2 status for the driver: whether a prepared intent still awaits publication.
///
/// Returns the durable operation id when a prepared, never-begun intent exists.
/// Publication itself is intentionally not implemented in this module.
pub(crate) fn pending_prepared_intent(goal: &Goal, task_id: &TaskId) -> Option<String> {
    let task = goal.tasks().get(task_id)?;
    let intent = task.latest_attempt()?.mutation_intent()?;
    if !intent.operations().is_empty()
        && intent
            .operations()
            .iter()
            .all(|operation| operation.state() == crate::mutation::MutationOperationState::Prepared)
    {
        Some(intent.operation_id().to_owned())
    } else {
        None
    }
}

/// Verify that a prepared stage still matches its durable intent.
///
/// Used by the driver and by recovery before publication is attempted. A stage
/// that cannot be verified is a hard stop, never a reason to ask the model again.
pub(crate) fn verify_prepared_stage(
    store: &TaskStore,
    session: &config::Session,
    goal: &Goal,
    task_id: &TaskId,
    operation_id: &str,
) -> Result<WriteStage, StageError> {
    let task = goal
        .tasks()
        .get(task_id)
        .ok_or(StageError::IdentityMismatch)?;
    let attempt = task.latest_attempt().ok_or(StageError::IdentityMismatch)?;
    let intent = attempt
        .mutation_intent()
        .ok_or(StageError::IdentityMismatch)?;
    if intent.operation_id() != operation_id {
        return Err(StageError::IdentityMismatch);
    }
    let identity = StageIdentity {
        session_id: session.id.clone(),
        goal_id: goal.id().as_str().to_owned(),
        task_id: task_id.as_str().to_owned(),
        attempt_id: attempt.id().as_str().to_owned(),
        plan_revision: goal.plan_revision(),
        operation_id: operation_id.to_owned(),
        scope_identity: intent.scope_identity().to_owned(),
    };
    let stage = writer_stage::load_stage(store.state_root(), &identity)?;
    if stage.operation_id() != operation_id || stage.operation_count() != intent.operations().len()
    {
        return Err(StageError::IntegrityMismatch);
    }
    for operation in intent.operations() {
        stage.read_content(operation.index(), operation.intended_after_sha256())?;
    }
    Ok(stage)
}

/// Remove one candidate's stage, but only when it is provably unlinked.
///
/// A stage created by this call is linked to the Goal only by the commit that
/// records the `MutationIntent`. If that commit failed, the id cannot be
/// referenced — unless a concurrent identical candidate somehow produced the same
/// id, which the per-candidate nonce makes impossible. The durable re-read is
/// still performed as defence in depth: if anything references the id, the stage
/// is left alone.
fn remove_own_stage_if_unlinked(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    operation_id: &str,
) {
    let referenced = match store.load_goal(&session.id, goal_id) {
        Ok(goal) => goal.tasks().values().any(|task| {
            task.latest_attempt()
                .and_then(|attempt| attempt.mutation_intent())
                .is_some_and(|intent| intent.operation_id() == operation_id)
        }),
        // If the state cannot be read, keep the stage: losing it is worse than
        // leaving an inert orphan.
        Err(_) => true,
    };
    if referenced {
        return;
    }
    let _ = writer_stage::remove_never_begun_stage(
        store.state_root(),
        &session.id,
        goal_id.as_str(),
        operation_id,
    );
}

fn stage_error_to_orchestrator(error: StageError) -> OrchestratorError {
    OrchestratorError::InvalidDag(format!("{}: writer stage is not usable", error.code()))
}

/// Deterministic checkpoints used by the E5-H2 race tests.
///
/// Production builds contain none of this: the call sites are `#[cfg(test)]`-gated
/// and the module disappears entirely, so no synchronization primitive can be
/// reached from a real server.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::collections::HashMap;
    use std::sync::{Arc, Condvar, Mutex, OnceLock};

    /// `(payload digest, phase) -> gate`.
    type Gates = HashMap<(String, Phase), Arc<(Mutex<bool>, Condvar)>>;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub(crate) enum Phase {
        BeforeStage,
        AfterStage,
    }

    static GATES: OnceLock<Mutex<Gates>> = OnceLock::new();

    fn gates() -> &'static Mutex<Gates> {
        GATES.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Install a gate that blocks the thread staging `payload_sha256` at `phase`.
    pub(crate) fn arm(payload_sha256: &str, phase: Phase) -> Arc<(Mutex<bool>, Condvar)> {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        gates()
            .lock()
            .expect("gate registry")
            .insert((payload_sha256.to_owned(), phase), gate.clone());
        gate
    }

    /// Wait at an armed gate. Returns immediately when none is armed.
    fn wait(payload_sha256: &str, phase: Phase) {
        let gate = gates()
            .lock()
            .expect("gate registry")
            .get(&(payload_sha256.to_owned(), phase))
            .cloned();
        if let Some(gate) = gate {
            let (lock, condvar) = &*gate;
            let mut open = lock.lock().expect("gate state");
            while !*open {
                open = condvar.wait(open).expect("gate wait");
            }
        }
    }

    /// Release one armed gate.
    pub(crate) fn release(payload_sha256: &str, phase: Phase) {
        let gate = gates()
            .lock()
            .expect("gate registry")
            .remove(&(payload_sha256.to_owned(), phase));
        if let Some(gate) = gate {
            let (lock, condvar) = &*gate;
            *lock.lock().expect("gate state") = true;
            condvar.notify_all();
        }
    }

    /// Remove every gate so one test cannot leak into another.
    pub(crate) fn clear() {
        gates().lock().expect("gate registry").clear();
    }

    pub(crate) fn before_stage(payload_sha256: &str, _operation_id: &str) {
        wait(payload_sha256, Phase::BeforeStage);
    }

    pub(crate) fn after_stage(payload_sha256: &str, _operation_id: &str) {
        wait(payload_sha256, Phase::AfterStage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prepared_progress_code_is_stable() {
        assert_eq!(WRITE_INTENT_PREPARED, "WRITE_INTENT_PREPARED");
    }
}
