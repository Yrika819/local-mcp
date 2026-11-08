//! E5-A2 / E5-B — host publication of a prepared external Writer intent and the
//! external Reviewer leg.
//!
//! Publication is the only place in the External backend that changes a file, and
//! it is deliberately a **separate** host step from submission:
//!
//! 1. the durable intent, the sealed stage, the Attempt, the Task, and the
//!    workspace mutation lease must all still agree (otherwise nothing happens);
//! 2. every operation is revalidated against current path authority and its
//!    preimage *before* the first write, so a stale proposal cannot half-apply;
//! 3. each operation is journaled `BeginOperation` → published through the
//!    existing `ExecutionWriteBoundary`/`workspace_publish` helper → postimage
//!    verified → journaled `CompleteOperation`;
//! 4. a failure that cannot prove the target state is left as-is: it is never
//!    treated as "not performed" and never blindly replayed.
//!
//! The Reviewer leg consumes the durable postimage only. A reviewer response can
//! never publish, approve, or complete anything: it supplies a finding count that
//! the Task's own `REVIEW_GATE` specs evaluate, and a pass only reaches
//! `VERIFYING`.
//!
//! Nothing here is a workspace mutation claim beyond what each operation's
//! journal record proves: multi-file publication stays per-operation journaled
//! and is never described as one atomic filesystem transaction.

use crate::authorization::GrantRefusal;
use crate::authorization_store;
use crate::config;
use crate::external_model_store::{external_state, external_state_mut};
use crate::external_model_work::{
    ModelWorkItem, OUTPUT_CONTRACT_REVIEWER_V1, SubmissionDecision, SubmissionId,
    SubmissionReceipt, WorkId, WorkItemState, WorkRole, sha256_hex,
};
use crate::goal::{Goal, GoalId};
use crate::mutation::{MutationIntentState, MutationIntentUpdate};
use crate::orchestrator_error::OrchestratorError;
use crate::task::{TaskId, TaskStatus, TaskTransitionContext};
use crate::task_store::TaskStore;
use crate::writer::{self, WriteBoundary, WriterError};
use crate::writer_stage::WriteStage;

/// Progress code: every prepared operation was published and journaled.
pub(crate) const WRITER_PUBLISHED: &str = "WRITER_PUBLISHED";
/// Progress code: the reviewer invocation was durably begun.
pub(crate) const REVIEWER_INVOCATION_STARTED: &str = "REVIEWER_INVOCATION_STARTED";
/// Progress code: a reviewer verdict was recorded.
pub(crate) const REVIEWER_RECORDED: &str = "REVIEWER_RECORDED";

/// Outcome of one publication attempt.
#[derive(Debug)]
pub(crate) enum PublishOutcome {
    Published { goal: Box<Goal> },
    Refused { code: &'static str },
}

/// Outcome of one reviewer submission.
#[expect(
    clippy::large_enum_variant,
    reason = "The outcome is short-lived; carrying the committed Goal snapshot is the point."
)]
#[derive(Debug)]
pub(crate) enum ReviewerOutcome {
    Recorded {
        goal: Goal,
        receipt: SubmissionReceipt,
        blocked: bool,
    },
    Refused {
        code: &'static str,
    },
}

/// Whether this Goal's writer Task was published and now needs its reviewer leg.
///
/// Returns `(task_id, operation_id, reviewer_already_invoked)`.
pub(crate) fn reviewer_pending(goal: &Goal) -> Option<(TaskId, String, bool)> {
    for (task_id, task) in goal.tasks() {
        if task.worker() != crate::task::WorkerKind::CodexWriter
            || task.status() != TaskStatus::Running
        {
            continue;
        }
        let Some(attempt) = task.latest_attempt() else {
            continue;
        };
        let Some(intent) = attempt.mutation_intent() else {
            continue;
        };
        // `ReconciledPerformed` is the state recovery reaches after reconciling a
        // writer mutation whose completion could not be observed directly; the
        // review leg must resume from it exactly like the normal path.
        if !matches!(
            intent.state(),
            MutationIntentState::Applied | MutationIntentState::ReconciledPerformed
        ) {
            continue;
        }
        let invoked = match intent.reviewer_state() {
            crate::mutation::ReviewerInvocationState::NotStarted => false,
            crate::mutation::ReviewerInvocationState::Invoking => true,
            // A persisted review needs the Task verifier, not another reviewer.
            crate::mutation::ReviewerInvocationState::Persisted => continue,
        };
        return Some((task_id.clone(), intent.operation_id().to_owned(), invoked));
    }
    None
}

/// Whether this active WorkItem's Writer intent is already fully published.
///
/// This is the recovery predicate for the window between the last publication
/// commit and the host's `commit_complete_active_work`: both are separate commits
/// on purpose (the WorkItem completion must never be implied by a filesystem
/// write), so a crash in between leaves an active `SUBMITTED` Writer WorkItem
/// whose journal already proves the whole intent was applied. Returning the
/// operation id lets the driver finish the bookkeeping instead of dead-ending the
/// Goal with `WRITER_INTENT_NOT_PREPARED`.
///
/// It never authorises a second write: the caller only completes the WorkItem.
pub(crate) fn published_intent_awaiting_completion(
    goal: &Goal,
    active: &ModelWorkItem,
    task_id: &TaskId,
) -> Option<String> {
    if active.role() != WorkRole::Writer || active.state() != WorkItemState::Submitted {
        return None;
    }
    if active.task_id() != Some(task_id.as_str()) {
        return None;
    }
    let task = goal.tasks().get(task_id)?;
    let attempt = task.latest_attempt()?;
    if active.attempt_id() != Some(attempt.id().as_str()) {
        return None;
    }
    let intent = attempt.mutation_intent()?;
    if !matches!(
        intent.state(),
        MutationIntentState::Applied | MutationIntentState::ReconciledPerformed
    ) {
        return None;
    }
    Some(intent.operation_id().to_owned())
}

/// Publish one prepared external writer intent through the existing host write
/// boundary.
pub(crate) async fn publish_prepared_writer_intent<B: WriteBoundary + ?Sized>(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    operation_id: &str,
    stage: &WriteStage,
    boundary: &B,
) -> Result<PublishOutcome, OrchestratorError> {
    let goal = store.load_goal(&session.id, goal_id)?;
    // A cancelled, pausing, or terminal Goal must never publish, even if its Task
    // still looks RUNNING (cancellation rests in CANCELLING while the host
    // reconciles the active Attempt). This is checked before the Task so a
    // cancelled Goal is refused for the right reason.
    if !matches!(goal.status(), crate::goal::GoalStatus::Running) {
        return Ok(PublishOutcome::Refused {
            code: "WRITER_GOAL_NOT_ACTIVE",
        });
    }
    let Some(task) = goal.tasks().get(task_id) else {
        return Ok(PublishOutcome::Refused {
            code: "WRITER_TASK_MISSING",
        });
    };
    if task.status() != TaskStatus::Running {
        return Ok(PublishOutcome::Refused {
            code: "WRITER_TASK_NOT_RUNNING",
        });
    }
    if !writer::holds_workspace_mutation_lease(task) {
        return Ok(PublishOutcome::Refused {
            code: "WRITER_LEASE_LOST",
        });
    }
    let Some(attempt) = task.latest_attempt() else {
        return Ok(PublishOutcome::Refused {
            code: "WRITER_ATTEMPT_MISSING",
        });
    };
    let Some(intent) = attempt.mutation_intent() else {
        return Ok(PublishOutcome::Refused {
            code: "WRITER_INTENT_MISSING",
        });
    };
    if intent.operation_id() != operation_id || intent.state() != MutationIntentState::Prepared {
        return Ok(PublishOutcome::Refused {
            code: "WRITER_INTENT_NOT_PREPARED",
        });
    }
    let attempt_id = attempt.id().as_str().to_owned();

    // The stage must still prove these exact bytes, and current path authority
    // and preimages must still hold, before the first durable Begun record.
    let mut contents = Vec::with_capacity(intent.operations().len());
    for operation in intent.operations() {
        let content = match stage.read_content(operation.index(), operation.intended_after_sha256())
        {
            Ok(content) => content,
            Err(_) => {
                return Ok(PublishOutcome::Refused {
                    code: "WRITER_STAGE_INTEGRITY_MISMATCH",
                });
            }
        };
        if let Err(error) = writer::revalidate_intent_operation_before_publish(
            &goal,
            session,
            task_id,
            operation.path(),
            operation.expected_preimage(),
        ) {
            return Ok(PublishOutcome::Refused {
                code: match error {
                    WriterError::PreimageMismatch(_) => "WRITER_PREIMAGE_CHANGED",
                    _ => "WRITER_PATH_AUTHORITY_CHANGED",
                },
            });
        }
        contents.push(content);
    }

    let binding = match intent.authorization() {
        Some(binding) => binding.clone(),
        None => {
            return Ok(PublishOutcome::Refused {
                code: GrantRefusal::Required.code(),
            });
        }
    };
    let operation_id = intent.operation_id().to_owned();
    let scope_identity = intent.scope_identity().to_owned();
    let request_ids = intent
        .operations()
        .iter()
        .map(|operation| operation.request_id().to_owned())
        .collect::<Vec<_>>();
    let paths = intent
        .operations()
        .iter()
        .map(|operation| operation.path().to_path_buf())
        .collect::<Vec<_>>();
    let expected_preimages = intent
        .operations()
        .iter()
        .map(|operation| writer::mutation_to_commit_preimage(operation.expected_preimage()))
        .collect::<Vec<_>>();
    let parents = paths
        .iter()
        .map(|path| {
            path.parent()
                .map(std::path::Path::to_path_buf)
                .ok_or_else(|| {
                    OrchestratorError::InvalidDag(
                        "writer target has no parent directory".to_owned(),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut revision = goal.revision();
    let mut changed = Vec::new();
    for (index, content) in contents.iter().enumerate() {
        let index_u32 = u32::try_from(index).map_err(|_| {
            OrchestratorError::InvalidDag("writer operation index overflow".to_owned())
        })?;
        // Authorization B linearization point.
        //
        // The grant state, epoch, anchor, workspace, path, operation and budget
        // are verified under the per-Goal ledger lock, the operation id is
        // charged idempotently, and `BeginOperation` is committed *before* the
        // fence is released. A revocation that finalizes after this point cannot
        // interleave with the begun operation; one that finalized before it is
        // visible here and refuses.
        let fresh_goal = store.load_goal(&session.id, goal_id)?;
        let operation_key = format!("{operation_id}#{index_u32}");
        let bytes = u64::try_from(content.len()).unwrap_or(u64::MAX);
        // The fence is held through BeginOperation, the filesystem write,
        // postimage verification and CompleteOperation. A revocation therefore
        // cannot finalize while this operation is executing: it either runs
        // first (and this operation is refused) or waits for the operation to be
        // durably journaled.
        let fence = match authorization_store::acquire_publication_fence(
            store,
            &fresh_goal,
            &binding,
            &operation_key,
            bytes,
            |grant| {
                let relative = crate::authorization::relative_workspace_path(
                    &paths[index],
                    grant.workspace().root(),
                )?;
                authorization_store::check_operation_scope(grant, &relative)
            },
        ) {
            Ok(fence) => fence,
            Err(authorization_store::AuthorizationGateError::Refusal(refusal)) => {
                return Ok(PublishOutcome::Refused {
                    code: refusal.code(),
                });
            }
            Err(error) => return Ok(PublishOutcome::Refused { code: error.code() }),
        };
        // The fence may have committed the anchor usage floor, which advances
        // the Goal revision. Bind `BeginOperation` to the revision the fence
        // produced, not the pre-fence snapshot.
        revision = fence.goal_revision();
        revision = store
            .mutate_goal_snapshot(&session.id, goal_id, revision, |goal, _now| {
                writer::ensure_writer_task_ready_for_attempt(goal, task_id, &attempt_id)?;
                goal.task_advance_latest_mutation_intent(
                    task_id,
                    &operation_id,
                    MutationIntentUpdate::BeginOperation { index: index_u32 },
                )
            })?
            .revision();
        let output = match boundary
            .write(writer::WriterWriteRequest {
                path: &paths[index],
                parent: &parents[index],
                content,
                expected_preimage: &expected_preimages[index],
                request_id: &request_ids[index],
            })
            .await
        {
            Ok(output) => output,
            Err(_) => {
                // The helper could not run at all: the journal stays in
                // APPLYING, which recovery reconciles. Never retried blindly.
                return Ok(PublishOutcome::Refused {
                    code: "WRITER_PUBLICATION_UNKNOWN",
                });
            }
        };
        if output.status != 0 {
            // A refused publication proves the destination untouched only for a
            // preimage/appeared refusal; anything else stays journaled as unknown.
            return Ok(PublishOutcome::Refused {
                code: "WRITER_PUBLICATION_REFUSED",
            });
        }
        let post = match writer::observe_file_state(&paths[index]) {
            Ok(post) => post,
            Err(_) => {
                return Ok(PublishOutcome::Refused {
                    code: "WRITER_POSTIMAGE_UNKNOWN",
                });
            }
        };
        if post.sha256() != Some(sha256_hex(content.as_bytes()).as_str()) {
            return Ok(PublishOutcome::Refused {
                code: "WRITER_POSTIMAGE_MISMATCH",
            });
        }
        changed.push(paths[index].clone());
        revision = store
            .mutate_goal_snapshot(&session.id, goal_id, revision, |goal, _now| {
                writer::ensure_writer_task_ready_for_attempt(goal, task_id, &attempt_id)?;
                goal.task_advance_latest_mutation_intent(
                    task_id,
                    &operation_id,
                    MutationIntentUpdate::CompleteOperation { index: index_u32 },
                )
            })?
            .revision();
        drop(fence);
    }

    // Record the host-observed performed mutation: worker report, file snapshots,
    // and the confirmed side-effect state. Still RUNNING; the reviewer leg is next.
    let changed_for_report = changed.clone();
    let _ = &changed;
    let committed = store.mutate_goal_snapshot(&session.id, goal_id, revision, |goal, _now| {
        writer::ensure_writer_task_ready_for_attempt(goal, task_id, &attempt_id)?;
        // The Attempt already carries the accepted proposal's worker report;
        // recording a second one is refused by design. The published paths are
        // recorded as FileSnapshot evidence plus the journal's Applied state.
        let attempt = goal.tasks()[task_id]
            .latest_attempt()
            .ok_or_else(|| {
                OrchestratorError::CorruptGoal("writer Task lacks an Attempt".to_owned())
            })?
            .id()
            .clone();
        for path in &changed_for_report {
            let post = writer::observe_file_state(path).map_err(|_| {
                OrchestratorError::PersistenceIo(std::io::Error::other(
                    "writer postimage could not be observed",
                ))
            })?;
            goal.task_add_evidence(
                task_id,
                crate::task::TaskEvidence::FileSnapshot {
                    path: path.clone(),
                    exists: post.exists(),
                    size: post.size(),
                    sha256: post.sha256().map(str::to_owned),
                },
            )?;
        }
        goal.task_bind_latest_attempt_execution(
            task_id,
            Some(operation_id.clone()),
            Some(scope_identity.clone()),
            request_ids.first().cloned(),
            Some(crate::fallback::SideEffectClass::LocalMutation),
            Some(crate::fallback::SideEffectState::ConfirmedPerformed),
            None,
            None,
        )?;
        let _ = attempt;
        Ok(())
    })?;

    Ok(PublishOutcome::Published {
        goal: Box::new(committed),
    })
}

/// Begin the reviewer invocation durably (a host step, no model, no write).
pub(crate) fn begin_reviewer_invocation(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    operation_id: &str,
    expected_revision: u64,
) -> Result<Goal, OrchestratorError> {
    let goal = store.load_goal(&session.id, goal_id)?;
    let attempt_id = goal
        .tasks()
        .get(task_id)
        .and_then(|task| task.latest_attempt())
        .map(|attempt| attempt.id().as_str().to_owned())
        .ok_or_else(|| {
            OrchestratorError::InvalidDag("reviewer Task lacks an Attempt".to_owned())
        })?;
    let invocation_id = format!("e5-reviewer:{attempt_id}");
    store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, _now| {
        goal.task_advance_latest_mutation_intent(
            task_id,
            operation_id,
            MutationIntentUpdate::BeginReviewer {
                invocation_id: invocation_id.clone(),
            },
        )
    })
}

/// Issue one Reviewer WorkItem for a published writer intent (E5-B).
///
/// The request is host-built **inside** the issuance commit and bound to the
/// revision that commit produces, so the client submits against the revision the
/// host will actually hold.
pub(crate) fn issue_external_reviewer_work_item(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    expected_revision: u64,
) -> Result<Option<ModelWorkItem>, OrchestratorError> {
    let mut slot: Option<Result<ModelWorkItem, ()>> = None;
    let precheck = store.load_goal(&session.id, goal_id)?;
    let request = match writer::build_external_reviewer_request(&precheck, task_id) {
        Ok(request) => request,
        Err(_) => return Ok(None),
    };
    // Never ask for a review of a postimage the host no longer observes: a file
    // changed after publication is a typed stop, not a review target.
    if writer::reviewer_request_postimage_is_stale(&request) {
        return Ok(None);
    }
    store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
        let role = WorkRole::Reviewer;
        if external_state(goal)?.check_issue_budget(role).is_err() {
            slot = Some(Err(()));
            return Ok(());
        }
        // The Attempt identity must still be the active one.
        let Some(attempt_id) = goal
            .tasks()
            .get(task_id)
            .and_then(|task| task.latest_attempt())
            .map(|attempt| attempt.id().as_str().to_owned())
        else {
            slot = Some(Err(()));
            return Ok(());
        };
        let bound_revision = goal
            .revision()
            .checked_add(1)
            .ok_or_else(|| OrchestratorError::CorruptGoal("goal revision overflow".to_owned()))?;
        let request =
            writer::build_external_reviewer_request_at_revision(goal, task_id, bound_revision)
                .map_err(|_| {
                    OrchestratorError::InvalidDag(
                        "external reviewer request could not be built".to_owned(),
                    )
                })?;
        let payload = writer::reviewer_request_payload(&request).map_err(|_| {
            OrchestratorError::InvalidDag(
                "external reviewer request could not be serialized".to_owned(),
            )
        })?;
        if payload.len() > crate::external_model_work::MAX_REQUEST_PAYLOAD_BYTES {
            return Err(OrchestratorError::InvalidDag(
                "external reviewer request exceeds the durable WorkItem bound".to_owned(),
            ));
        }
        let issue_counter = external_state(goal)?
            .issue_counter()
            .checked_add(1)
            .ok_or_else(|| {
                OrchestratorError::CorruptGoal("external work item counter overflow".to_owned())
            })?;
        let item = ModelWorkItem::pending(
            WorkId::new(),
            session.id.clone(),
            goal_id.as_str().to_owned(),
            role,
            Some(task_id.as_str().to_owned()),
            Some(attempt_id),
            bound_revision,
            goal.plan_revision(),
            sha256_hex(payload.as_bytes()),
            payload,
            OUTPUT_CONTRACT_REVIEWER_V1,
            issue_counter,
            now,
        )?;
        external_state_mut(goal)?.record_issued(item.clone())?;
        slot = Some(Ok(item));
        Ok(())
    })?;
    Ok(match slot {
        Some(Ok(item)) => Some(item),
        _ => None,
    })
}

/// Validate and record one external Reviewer submission (E5-B).
///
/// The reviewer verdict is advisory: the Task's own `REVIEW_GATE` decides, and a
/// pass only reaches `VERIFYING`. The receipt, the ReviewResult evidence, the
/// journal's `CompleteReviewer`, and the Task transition are one commit.
#[expect(
    clippy::too_many_arguments,
    reason = "Goal, work identity, digest, and the untrusted payload are independent authority channels."
)]
pub(crate) fn commit_external_reviewer_result(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    expected_revision: u64,
    work_id: &WorkId,
    submission_id: &SubmissionId,
    payload_sha256: &str,
    raw: &[u8],
) -> Result<ReviewerOutcome, OrchestratorError> {
    let goal = store.load_goal(&session.id, goal_id)?;
    let Some(active) = external_state(&goal)?.active_work() else {
        return Ok(ReviewerOutcome::Refused {
            code: "WORK_ITEM_NOT_PENDING",
        });
    };
    let Some(task_id) = active.task_id().and_then(|value| TaskId::parse(value).ok()) else {
        return Ok(ReviewerOutcome::Refused {
            code: "WORK_ITEM_TASK_INVALID",
        });
    };
    let Some(attempt_id) = active.attempt_id().map(str::to_owned) else {
        return Ok(ReviewerOutcome::Refused {
            code: "WORK_ITEM_ATTEMPT_INVALID",
        });
    };
    let validated =
        match writer::validate_external_reviewer_output(&goal, &task_id, &attempt_id, raw) {
            Ok(validated) => validated,
            Err(refusal) => {
                // A rejected review is recorded, and the Task is blocked with the
                // reviewer refusal code: a review that cannot be trusted must not
                // leave the Writer leg open.
                let (committed, receipt) = record_reviewer_refusal(
                    store,
                    session,
                    goal_id,
                    expected_revision,
                    work_id,
                    submission_id,
                    payload_sha256,
                    task_id,
                    &attempt_id,
                    refusal.code(),
                    refusal.detail(),
                )?;
                return Ok(match committed {
                    Some(goal) => ReviewerOutcome::Recorded {
                        goal,
                        receipt,
                        blocked: true,
                    },
                    None => ReviewerOutcome::Refused {
                        code: "GOAL_REVISION_STALE",
                    },
                });
            }
        };

    let blocking_findings = validated.blocking_findings();
    let summary = validated.summary().to_owned();
    let operation_id = goal
        .tasks()
        .get(&task_id)
        .and_then(|task| task.latest_attempt())
        .and_then(|attempt| attempt.mutation_intent())
        .map(|intent| intent.operation_id().to_owned())
        .ok_or_else(|| {
            OrchestratorError::CorruptGoal("reviewer Task lacks a MutationIntent".to_owned())
        })?;
    let mut outcome: Option<(SubmissionReceipt, bool)> = None;
    let mut refused: Option<&'static str> = None;
    let committed =
        store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
            let binding_ok = matches!(
                external_state(goal)?.active_work(),
                Some(active)
                    if active.work_id() == work_id
                        && active.state() == WorkItemState::Pending
                        && active.task_id() == Some(task_id.as_str())
                        && active.attempt_id() == Some(attempt_id.as_str())
            );
            if !binding_ok {
                refused = Some("WORK_ITEM_NOT_PENDING");
                return Ok(());
            }
            writer::ensure_writer_task_ready_for_attempt(goal, &task_id, &attempt_id)?;
            writer::apply_reviewer_verdict_to_goal(
                goal,
                &task_id,
                &operation_id,
                &summary,
                blocking_findings,
                now,
            )?;
            let blocked = goal.tasks()[&task_id].status() == TaskStatus::Blocked;
            let receipt = SubmissionReceipt::new(
                work_id.clone(),
                submission_id.clone(),
                payload_sha256.to_owned(),
                SubmissionDecision::Committed,
                Some(goal.revision().checked_add(1).ok_or_else(|| {
                    OrchestratorError::CorruptGoal("goal revision overflow".to_owned())
                })?),
                if blocked {
                    "REVIEW_GATE_BLOCKED"
                } else {
                    REVIEWER_RECORDED
                },
                None,
                now,
            );
            external_state_mut(goal)?.record_submission(receipt.clone(), now)?;
            outcome = Some((receipt, blocked));
            Ok(())
        });

    match committed {
        Ok(goal) => {
            if let Some(code) = refused {
                return Ok(ReviewerOutcome::Refused { code });
            }
            let (receipt, blocked) = outcome.ok_or_else(|| {
                OrchestratorError::CorruptGoal("reviewer commit produced no receipt".to_owned())
            })?;
            Ok(ReviewerOutcome::Recorded {
                goal,
                receipt,
                blocked,
            })
        }
        Err(OrchestratorError::RevisionConflict { .. }) => Ok(ReviewerOutcome::Refused {
            code: "GOAL_REVISION_STALE",
        }),
        Err(error) => Err(error),
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "The refusal keeps Goal, work identity, digest, and bounded verdict channels explicit."
)]
fn record_reviewer_refusal(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    expected_revision: u64,
    work_id: &WorkId,
    submission_id: &SubmissionId,
    payload_sha256: &str,
    task_id: TaskId,
    attempt_id: &str,
    code: &'static str,
    detail: &'static str,
) -> Result<(Option<Goal>, SubmissionReceipt), OrchestratorError> {
    let mut receipt_slot: Option<SubmissionReceipt> = None;
    let committed =
        store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
            writer::ensure_writer_task_ready_for_attempt(goal, &task_id, attempt_id)?;
            goal.task_add_blocker(&task_id, crate::task::TaskBlocker::new(code, detail, true))?;
            goal.transition_task(
                &task_id,
                TaskStatus::Blocked,
                TaskTransitionContext::default(),
                now,
            )?;
            let receipt = SubmissionReceipt::new(
                work_id.clone(),
                submission_id.clone(),
                payload_sha256.to_owned(),
                SubmissionDecision::RejectedSchema,
                None,
                code,
                Some(detail.to_owned()),
                now,
            );
            external_state_mut(goal)?.record_submission(receipt.clone(), now)?;
            receipt_slot = Some(receipt);
            Ok(())
        });
    match committed {
        Ok(goal) => {
            let receipt = receipt_slot.ok_or_else(|| {
                OrchestratorError::CorruptGoal("reviewer refusal produced no receipt".to_owned())
            })?;
            Ok((Some(goal), receipt))
        }
        Err(OrchestratorError::RevisionConflict { .. }) => Ok((
            None,
            SubmissionReceipt::new(
                work_id.clone(),
                submission_id.clone(),
                payload_sha256.to_owned(),
                SubmissionDecision::RejectedSchema,
                None,
                code,
                Some(detail.to_owned()),
                "",
            ),
        )),
        Err(error) => Err(error),
    }
}

/// Record that authorization was lost or exhausted, and release the active slot
/// when doing so cannot hide a side effect.
///
/// A revoked/expired grant or an exhausted budget must not leave a Goal forever
/// wedged on a SUBMITTED Writer WorkItem. If (and only if) the journal still
/// proves no operation began, the Task is blocked with the authorization code and
/// the WorkItem is cancelled in the same commit. An APPLYING/UNKNOWN intent is
/// left untouched: cancelling it would hide the very side effect recovery must
/// reconcile.
pub(crate) fn record_authorization_refusal(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    code: &'static str,
) -> Result<Option<Goal>, OrchestratorError> {
    let goal = store.load_goal(&session.id, goal_id)?;
    let Some(active) = external_state(&goal)?.active_work() else {
        return Ok(None);
    };
    if active.role() != WorkRole::Writer {
        return Ok(None);
    }
    let Some(task_id) = active.task_id().and_then(|value| TaskId::parse(value).ok()) else {
        return Ok(None);
    };
    let Some(attempt) = goal
        .tasks()
        .get(&task_id)
        .and_then(|task| task.latest_attempt())
    else {
        return Ok(None);
    };
    let Some(intent) = attempt.mutation_intent() else {
        return Ok(None);
    };
    if intent.state() != crate::mutation::MutationIntentState::Prepared {
        return Ok(None);
    }
    let attempt_id = attempt.id().as_str().to_owned();
    let committed =
        store.mutate_goal_snapshot(&session.id, goal_id, goal.revision(), |goal, now| {
            writer::ensure_writer_task_ready_for_attempt(goal, &task_id, &attempt_id)?;
            goal.task_add_blocker(&task_id, crate::task::TaskBlocker::new(code, code, true))?;
            goal.transition_task(
                &task_id,
                TaskStatus::Blocked,
                TaskTransitionContext::default(),
                now,
            )?;
            external_state_mut(goal)?.cancel_active_work(code);
            Ok(())
        })?;
    Ok(Some(committed))
}

/// Whether the driver should stop because a published writer Task has no
/// reviewer WorkItem it can issue (budget or request unavailable).
pub(crate) fn reviewer_unavailable_reason(goal: &Goal, task_id: &TaskId) -> Option<&'static str> {
    let task = goal.tasks().get(task_id)?;
    if !task.blockers().is_empty() {
        return Some("REVIEWER_TASK_BLOCKED");
    }
    if task.status() != TaskStatus::Running {
        return Some("REVIEWER_TASK_NOT_RUNNING");
    }
    None
}
