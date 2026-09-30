use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::future::Future;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::agent::AgentError;
use crate::config;
use crate::execution;
use crate::fallback::{SideEffectClass, SideEffectState};
use crate::goal::{Goal, GoalId, GoalStatus};
use crate::mutation::{
    FileObservation, MutationIntent, MutationIntentUpdate, MutationOperationIntent,
    MutationPreimage,
};
use crate::orchestrator_error::OrchestratorError;
use crate::sandbox;
use crate::task::{
    TaskBlocker, TaskEvidence, TaskId, TaskOperationKind, TaskStatus, TaskTransitionContext,
    VerificationSpec, WorkerKind, WorkerReport,
};
use crate::task_store::TaskStore;

pub(crate) const MAX_WRITER_RESULT_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_WRITER_OPERATIONS: usize = 32;
pub(crate) const MAX_WRITER_PATH_BYTES: usize = 4096;
pub(crate) const MAX_WRITE_CONTENT_BYTES: usize = 256 * 1024;
pub(crate) const MAX_TOTAL_WRITE_CONTENT_BYTES: usize = 512 * 1024;
pub(crate) const MAX_WRITER_SUMMARY_BYTES: usize = 8 * 1024;
pub(crate) const MAX_WORKER_EVIDENCE_ITEMS: usize = 64;
pub(crate) const MAX_WORKER_EVIDENCE_KIND_BYTES: usize = 128;
pub(crate) const MAX_WORKER_EVIDENCE_VALUE_BYTES: usize = 16 * 1024;
pub(crate) const MAX_WORKER_EVIDENCE_TOTAL_BYTES: usize = 64 * 1024;
pub(crate) const MAX_REVIEW_RESULT_BYTES: usize = 128 * 1024;

#[derive(Debug)]
#[expect(
    dead_code,
    reason = "Frozen writer backend error variant remains part of the staged writer contract."
)]
pub(crate) enum WriterError {
    Backend(String),
    Model(AgentError),
    OutputInvalid(String),
    AuthorityViolation(String),
    PreimageMismatch(PathBuf),
    MutationFailed(String),
    ReviewerInvalid(String),
    Store(OrchestratorError),
}

impl fmt::Display for WriterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(reason) => write!(f, "writer backend failed: {reason}"),
            Self::Model(error) => write!(f, "writer model invocation failed: {error}"),
            Self::OutputInvalid(reason) => write!(f, "writer output is invalid: {reason}"),
            Self::AuthorityViolation(reason) => write!(f, "writer authority violation: {reason}"),
            Self::PreimageMismatch(path) => {
                write!(f, "writer preimage mismatch: {}", path.display())
            }
            Self::MutationFailed(reason) => write!(f, "writer mutation failed: {reason}"),
            Self::ReviewerInvalid(reason) => write!(f, "reviewer output is invalid: {reason}"),
            Self::Store(error) => write!(f, "writer durable-state error: {error}"),
        }
    }
}

impl std::error::Error for WriterError {}

impl From<OrchestratorError> for WriterError {
    fn from(value: OrchestratorError) -> Self {
        Self::Store(value)
    }
}

pub(crate) trait WriterBackend {
    fn propose(&self, request: &WriterRequest) -> Result<Vec<u8>, WriterError>;
}

pub(crate) trait ReviewerBackend {
    fn review(&self, request: &ReviewerRequest) -> Result<Vec<u8>, WriterError>;
}

pub(crate) trait WriteBoundary {
    fn write<'a>(
        &'a self,
        absolute: &'a Path,
        parent: &'a Path,
        content: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<sandbox::Output>> + Send + 'a>>;
}

struct ExecutionWriteBoundary;

impl WriteBoundary for ExecutionWriteBoundary {
    fn write<'a>(
        &'a self,
        absolute: &'a Path,
        parent: &'a Path,
        content: &'a str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<sandbox::Output>> + Send + 'a>> {
        Box::pin(execution::write_file_content(absolute, parent, content))
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct WriterRequest {
    goal_id: String,
    task_id: String,
    attempt_id: String,
    goal_revision: u64,
    plan_revision: u32,
    goal_cwd: PathBuf,
    task_title: String,
    task_objective: String,
    allowed_paths: Vec<PathBuf>,
    forbidden_paths: Vec<PathBuf>,
}

#[expect(
    dead_code,
    reason = "Frozen writer request accessors are retained for staged writer backend consumers."
)]
impl WriterRequest {
    pub(crate) fn goal_id(&self) -> &str {
        &self.goal_id
    }

    pub(crate) fn task_id(&self) -> &str {
        &self.task_id
    }

    pub(crate) fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    pub(crate) fn goal_revision(&self) -> u64 {
        self.goal_revision
    }

    pub(crate) fn plan_revision(&self) -> u32 {
        self.plan_revision
    }

    pub(crate) fn goal_cwd(&self) -> &Path {
        &self.goal_cwd
    }

    pub(crate) fn task_title(&self) -> &str {
        &self.task_title
    }

    pub(crate) fn task_objective(&self) -> &str {
        &self.task_objective
    }

    pub(crate) fn allowed_paths(&self) -> &[PathBuf] {
        &self.allowed_paths
    }

    pub(crate) fn forbidden_paths(&self) -> &[PathBuf] {
        &self.forbidden_paths
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ReviewerRequest {
    goal_id: String,
    task_id: String,
    attempt_id: String,
    goal_revision: u64,
    plan_revision: u32,
    writer_summary: String,
    files: Vec<ReviewFileEvidence>,
}

#[expect(
    dead_code,
    reason = "Frozen reviewer request accessors are retained for staged reviewer backend consumers."
)]
impl ReviewerRequest {
    pub(crate) fn goal_id(&self) -> &str {
        &self.goal_id
    }

    pub(crate) fn task_id(&self) -> &str {
        &self.task_id
    }

    pub(crate) fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    pub(crate) fn goal_revision(&self) -> u64 {
        self.goal_revision
    }

    pub(crate) fn plan_revision(&self) -> u32 {
        self.plan_revision
    }

    pub(crate) fn writer_summary(&self) -> &str {
        &self.writer_summary
    }

    pub(crate) fn files(&self) -> &[ReviewFileEvidence] {
        &self.files
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ReviewFileEvidence {
    path: PathBuf,
    before_sha256: Option<String>,
    after_sha256: Option<String>,
    size: Option<u64>,
}

#[expect(
    dead_code,
    reason = "Frozen review-file evidence accessors are retained for staged reviewer consumers."
)]
impl ReviewFileEvidence {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn before_sha256(&self) -> Option<&str> {
        self.before_sha256.as_deref()
    }

    pub(crate) fn after_sha256(&self) -> Option<&str> {
        self.after_sha256.as_deref()
    }

    pub(crate) fn size(&self) -> Option<u64> {
        self.size
    }
}

#[cfg(test)]
pub(crate) fn writer_request_for_model_backend_test(goal_cwd: PathBuf) -> WriterRequest {
    WriterRequest {
        goal_id: "00000000-0000-4000-8000-000000000001".to_owned(),
        task_id: "00000000-0000-4000-8000-000000000002".to_owned(),
        attempt_id: "00000000-0000-4000-8000-000000000003".to_owned(),
        goal_revision: 7,
        plan_revision: 3,
        goal_cwd,
        task_title: "writer test".to_owned(),
        task_objective: "propose exact changes only".to_owned(),
        allowed_paths: Vec::new(),
        forbidden_paths: Vec::new(),
    }
}

#[cfg(test)]
pub(crate) fn reviewer_request_for_model_backend_test() -> ReviewerRequest {
    ReviewerRequest {
        goal_id: "00000000-0000-4000-8000-000000000001".to_owned(),
        task_id: "00000000-0000-4000-8000-000000000002".to_owned(),
        attempt_id: "00000000-0000-4000-8000-000000000003".to_owned(),
        goal_revision: 8,
        plan_revision: 3,
        writer_summary: "host-observed candidate".to_owned(),
        files: Vec::new(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WriterStatus {
    CandidateComplete,
    Blocked,
    NeedsReplan,
    Failed,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriterResult {
    goal_id: String,
    task_id: String,
    attempt_id: String,
    goal_revision: u64,
    plan_revision: u32,
    status: WriterStatus,
    summary: String,
    evidence: Vec<WriterEvidence>,
    proposed_operations: Vec<WriterOperation>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriterEvidence {
    kind: String,
    value: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
enum WriterOperation {
    WriteUtf8 {
        path: String,
        expected_preimage: PreimageExpectation,
        content: String,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
enum PreimageExpectation {
    Sha256 { sha256: String },
    Absent,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewerResult {
    goal_id: String,
    task_id: String,
    attempt_id: String,
    goal_revision: u64,
    plan_revision: u32,
    summary: String,
    blocking_findings: u32,
    evidence: Vec<WriterEvidence>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileState {
    exists: bool,
    size: Option<u64>,
    sha256: Option<String>,
}

#[derive(Clone, Debug)]
struct MaterializedWrite {
    path: PathBuf,
    parent: PathBuf,
    expected_preimage: PreimageExpectation,
    content: String,
    before: FileState,
}

pub(crate) fn begin_writer_attempt(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    expected_revision: u64,
) -> Result<WriterRequest, WriterError> {
    let snapshot =
        store.mutate_goal_snapshot(&session.id, goal_id, expected_revision, |goal, now| {
            validate_goal_session_binding(goal, session)?;
            crate::planner::execution_root_for_goal(goal, session).map_err(|error| {
                OrchestratorError::InvalidDag(format!(
                    "writer execution-root gate refused: {error}"
                ))
            })?;
            if goal.status() != GoalStatus::Running {
                return Err(OrchestratorError::InvalidDag(
                    "writer attempt requires a RUNNING Goal".to_owned(),
                ));
            }
            let task = goal.tasks().get(task_id).ok_or_else(|| {
                OrchestratorError::InvalidDag("writer Task is missing".to_owned())
            })?;
            if task.status() != TaskStatus::Ready {
                return Err(OrchestratorError::InvalidDag(
                    "writer attempt requires a READY Task".to_owned(),
                ));
            }
            if task.worker() != WorkerKind::CodexWriter {
                return Err(OrchestratorError::InvalidDag(
                    "writer mutation requires CODEX_WRITER".to_owned(),
                ));
            }
            if task.scope().operation_kind() == TaskOperationKind::ReadOnly {
                return Err(OrchestratorError::InvalidDag(
                    "writer mutation requires a mutating TaskScope".to_owned(),
                ));
            }
            if task.semantic_attempts_remaining() == 0 {
                return Err(OrchestratorError::InvalidDag(
                    "writer Task attempt budget is exhausted".to_owned(),
                ));
            }
            if goal.tasks().iter().any(|(candidate_id, candidate)| {
                candidate_id != task_id && holds_workspace_mutation_lease(candidate)
            }) {
                return Err(OrchestratorError::InvalidDag(
                    "WORKSPACE_MUTATION lease is already held".to_owned(),
                ));
            }
            goal.transition_task(
                task_id,
                TaskStatus::Running,
                TaskTransitionContext::default(),
                now,
            )
        })?;
    build_writer_request(&snapshot, task_id, session)
}

pub(crate) async fn run_writer_attempt<W: WriterBackend, R: ReviewerBackend>(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    expected_revision: u64,
    writer: &W,
    reviewer: &R,
) -> Result<Goal, WriterError> {
    let boundary = ExecutionWriteBoundary;
    run_writer_attempt_with_boundary(
        store,
        session,
        goal_id,
        task_id,
        expected_revision,
        writer,
        reviewer,
        &boundary,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "Writer authority, revision, mutation, reviewer, and boundary channels remain explicit by design."
)]
pub(crate) async fn run_writer_attempt_with_boundary<
    W: WriterBackend,
    R: ReviewerBackend,
    B: WriteBoundary,
>(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    expected_revision: u64,
    writer: &W,
    reviewer: &R,
    boundary: &B,
) -> Result<Goal, WriterError> {
    let request = begin_writer_attempt(store, session, goal_id, task_id, expected_revision)?;

    let raw = match writer.propose(&request) {
        Ok(raw) => raw,
        Err(error) => {
            let detail = error.to_string();
            block_before_mutation(
                store,
                session,
                goal_id,
                task_id,
                &request,
                "WRITER_BACKEND_ERROR",
                &detail,
            )?;
            return Err(error);
        }
    };

    let result = match parse_and_validate_writer_result(&raw, &request) {
        Ok(result) => result,
        Err(error) => {
            let detail = error.to_string();
            block_before_mutation(
                store,
                session,
                goal_id,
                task_id,
                &request,
                "WRITER_OUTPUT_REJECTED",
                &detail,
            )?;
            return Err(error);
        }
    };
    let report_digest = sha256_hex(&raw);

    if result.status != WriterStatus::CandidateComplete {
        return finish_valid_non_mutating_result(
            store,
            session,
            goal_id,
            task_id,
            &request,
            &result,
            &report_digest,
        );
    }

    let current = store.load_goal(&session.id, goal_id)?;
    if current.revision() != request.goal_revision {
        return Err(WriterError::AuthorityViolation(format!(
            "writer proposal revision is stale: expected {}, actual {}",
            request.goal_revision,
            current.revision()
        )));
    }
    ensure_active_attempt(&current, task_id, &request)?;

    let operations =
        match materialize_operations(&current, session, task_id, &result.proposed_operations) {
            Ok(operations) => operations,
            Err(WriterError::PreimageMismatch(_)) => {
                return finish_valid_non_mutating_result(
                    store,
                    session,
                    goal_id,
                    task_id,
                    &request,
                    &WriterResult {
                        status: WriterStatus::NeedsReplan,
                        proposed_operations: Vec::new(),
                        ..result
                    },
                    &report_digest,
                );
            }
            Err(error) => {
                let detail = error.to_string();
                block_before_mutation(
                    store,
                    session,
                    goal_id,
                    task_id,
                    &request,
                    "WRITER_OPERATION_REJECTED",
                    &detail,
                )?;
                return Err(error);
            }
        };

    let scope_identity = sha256_hex(
        operations
            .iter()
            .flat_map(|operation| {
                let mut bytes = operation.path.to_string_lossy().as_bytes().to_vec();
                bytes.push(0);
                bytes.extend_from_slice(sha256_hex(operation.content.as_bytes()).as_bytes());
                bytes
            })
            .collect::<Vec<_>>()
            .as_slice(),
    );
    let operation_id = format!("phase5-writer:{}", request.attempt_id);

    let request_ids = operations
        .iter()
        .map(|_| Uuid::new_v4().to_string())
        .collect::<Vec<_>>();
    let intent = MutationIntent::new(
        operation_id.clone(),
        scope_identity.clone(),
        operations
            .iter()
            .zip(&request_ids)
            .enumerate()
            .map(|(index, (operation, request_id))| {
                MutationOperationIntent::new(
                    index as u32,
                    operation.path.clone(),
                    mutation_preimage(&operation.expected_preimage),
                    mutation_observation(&operation.before),
                    sha256_hex(operation.content.as_bytes()),
                    request_id.clone(),
                )
            })
            .collect(),
    )
    .map_err(|error| WriterError::AuthorityViolation(error.to_string()))?;
    let mut durable =
        store.mutate_goal_snapshot(&session.id, goal_id, request.goal_revision, |goal, _now| {
            ensure_active_attempt_store(goal, task_id, &request)?;
            goal.task_prepare_latest_mutation_intent(task_id, intent.clone())?;
            for request_id in &request_ids {
                goal.task_bind_latest_attempt_execution(
                    task_id,
                    None,
                    None,
                    Some(request_id.clone()),
                    None,
                    None,
                    None,
                    None,
                )?;
            }
            Ok(())
        })?;

    for (index, (operation, request_id)) in operations.iter().zip(&request_ids).enumerate() {
        durable = store.mutate_goal_snapshot(
            &session.id,
            goal_id,
            durable.revision(),
            |goal, _now| {
                ensure_active_attempt_store(goal, task_id, &request)?;
                goal.task_advance_latest_mutation_intent(
                    task_id,
                    &operation_id,
                    MutationIntentUpdate::BeginOperation {
                        index: index as u32,
                    },
                )?;
                Ok(())
            },
        )?;
        let output = boundary
            .write(&operation.path, &operation.parent, &operation.content)
            .await
            .map_err(|error| WriterError::MutationFailed(error.to_string()))?;
        execution::render_output(output)
            .map_err(|error| WriterError::MutationFailed(error.to_string()))?;
        let post = observe_file_state(&operation.path)?;
        let expected_digest = sha256_hex(operation.content.as_bytes());
        if !post.exists || post.sha256.as_deref() != Some(expected_digest.as_str()) {
            return Err(WriterError::MutationFailed(format!(
                "postimage mismatch for {}",
                operation.path.display()
            )));
        }
        durable = store.mutate_goal_snapshot(
            &session.id,
            goal_id,
            durable.revision(),
            |goal, _now| {
                ensure_active_attempt_store(goal, task_id, &request)?;
                goal.task_advance_latest_mutation_intent(
                    task_id,
                    &operation_id,
                    MutationIntentUpdate::CompleteOperation {
                        index: index as u32,
                    },
                )?;
                Ok(())
            },
        )?;
        let _ = request_id;
    }

    let post_states = operations
        .iter()
        .map(|operation| observe_file_state(&operation.path))
        .collect::<Result<Vec<_>, _>>()?;

    let changed_paths = operations
        .iter()
        .zip(&post_states)
        .filter_map(|(operation, post)| {
            (operation.before != *post).then_some(operation.path.clone())
        })
        .collect::<Vec<_>>();

    let post_snapshot =
        store.mutate_goal_snapshot(&session.id, goal_id, durable.revision(), |goal, _now| {
            ensure_active_attempt_store(goal, task_id, &request)?;
            goal.task_record_latest_worker_report(
                task_id,
                WorkerReport::new(result.summary.clone(), changed_paths.clone()),
            )?;
            let attempt_id = goal.tasks()[task_id]
                .latest_attempt()
                .expect("active attempt checked")
                .id()
                .clone();
            goal.task_add_evidence(
                task_id,
                TaskEvidence::WorkerReport {
                    attempt_id,
                    report_digest: report_digest.clone(),
                },
            )?;
            bind_execution_metadata(
                goal,
                task_id,
                &operation_id,
                &scope_identity,
                &request_ids,
                SideEffectState::ConfirmedPerformed,
            )?;
            for (operation, post) in operations.iter().zip(&post_states) {
                goal.task_add_evidence(
                    task_id,
                    TaskEvidence::FileSnapshot {
                        path: operation.path.clone(),
                        exists: post.exists,
                        size: post.size,
                        sha256: post.sha256.clone(),
                    },
                )?;
            }
            Ok(())
        })?;

    let mut reviewer_request = ReviewerRequest {
        goal_id: post_snapshot.id().as_str().to_owned(),
        task_id: task_id.as_str().to_owned(),
        attempt_id: request.attempt_id.clone(),
        goal_revision: post_snapshot.revision(),
        plan_revision: post_snapshot.plan_revision(),
        writer_summary: result.summary.clone(),
        files: operations
            .iter()
            .zip(&post_states)
            .map(|(operation, post)| ReviewFileEvidence {
                path: operation.path.clone(),
                before_sha256: operation.before.sha256.clone(),
                after_sha256: post.sha256.clone(),
                size: post.size,
            })
            .collect(),
    };

    let reviewer_invocation_id = format!("reviewer:{}", request.attempt_id);
    let reviewer_snapshot = store.mutate_goal_snapshot(
        &session.id,
        goal_id,
        post_snapshot.revision(),
        |goal, _now| {
            ensure_active_attempt_store(goal, task_id, &request)?;
            goal.task_advance_latest_mutation_intent(
                task_id,
                &operation_id,
                MutationIntentUpdate::BeginReviewer {
                    invocation_id: reviewer_invocation_id.clone(),
                },
            )?;
            Ok(())
        },
    )?;
    reviewer_request.goal_revision = reviewer_snapshot.revision();

    let review_raw = match reviewer.review(&reviewer_request) {
        Ok(raw) => raw,
        Err(error) => {
            let detail = error.to_string();
            block_after_mutation(
                store,
                session,
                goal_id,
                task_id,
                &reviewer_request,
                "REVIEWER_BACKEND_ERROR",
                &detail,
            )?;
            return Err(error);
        }
    };
    let review = match parse_and_validate_reviewer_result(&review_raw, &reviewer_request) {
        Ok(review) => review,
        Err(error) => {
            let detail = error.to_string();
            block_after_mutation(
                store,
                session,
                goal_id,
                task_id,
                &reviewer_request,
                "REVIEWER_OUTPUT_REJECTED",
                &detail,
            )?;
            return Err(error);
        }
    };

    store
        .mutate_goal_snapshot(
            &session.id,
            goal_id,
            reviewer_request.goal_revision,
            |goal, now| {
                ensure_reviewer_context_store(goal, task_id, &reviewer_request)?;
                let gate_exceeded = goal.tasks()[task_id]
                    .verification_specs()
                    .iter()
                    .any(|spec| {
                        matches!(
                            spec,
                            VerificationSpec::ReviewGate { max_blocking_findings }
                                if review.blocking_findings > *max_blocking_findings
                        )
                    });
                goal.task_add_evidence(
                    task_id,
                    TaskEvidence::ReviewResult {
                        summary: review.summary.clone(),
                        blocking_findings: review.blocking_findings,
                    },
                )?;
                goal.task_advance_latest_mutation_intent(
                    task_id,
                    &operation_id,
                    MutationIntentUpdate::CompleteReviewer,
                )?;
                if gate_exceeded {
                    goal.task_add_blocker(
                        task_id,
                        TaskBlocker::new(
                            "REVIEW_GATE_BLOCKED",
                            format!(
                                "reviewer reported {} blocking finding(s)",
                                review.blocking_findings
                            ),
                            true,
                        ),
                    )?;
                    goal.transition_task(
                        task_id,
                        TaskStatus::Blocked,
                        TaskTransitionContext::default(),
                        now,
                    )?;
                } else {
                    goal.transition_task(
                        task_id,
                        TaskStatus::Verifying,
                        TaskTransitionContext::default(),
                        now,
                    )?;
                }
                Ok(())
            },
        )
        .map_err(WriterError::from)
}

/// Continue the reviewer leg of a Writer attempt whose filesystem mutation was
/// durably reconciled as performed. This route never calls the Writer backend
/// and never creates a second attempt or replays a filesystem operation.
pub(crate) async fn resume_writer_reviewer<R: ReviewerBackend>(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    expected_revision: u64,
    reviewer: &R,
) -> Result<Goal, WriterError> {
    let snapshot = store.load_goal(&session.id, goal_id)?;
    if snapshot.revision() != expected_revision {
        return Err(WriterError::Store(OrchestratorError::RevisionConflict {
            expected: expected_revision,
            actual: snapshot.revision(),
        }));
    }
    validate_goal_session_binding(&snapshot, session).map_err(WriterError::from)?;
    let task = snapshot
        .tasks()
        .get(task_id)
        .ok_or_else(|| WriterError::AuthorityViolation("reviewer Task is missing".to_owned()))?;
    if !task.needs_reviewer_recovery() {
        return Err(WriterError::AuthorityViolation(
            "reviewer recovery requires a reconciled performed Writer attempt".to_owned(),
        ));
    }
    let attempt = task.latest_attempt().ok_or_else(|| {
        WriterError::AuthorityViolation("reviewer Task lacks an Attempt".to_owned())
    })?;
    let intent = attempt.mutation_intent().ok_or_else(|| {
        WriterError::AuthorityViolation("reviewer recovery lacks MutationIntent".to_owned())
    })?;
    let writer_summary = attempt
        .worker_report()
        .map(|report| report.summary().to_owned())
        .unwrap_or_else(|| "recovered host-observed Writer mutation".to_owned());
    let files = intent
        .operations()
        .iter()
        .map(|operation| {
            let current = observe_file_state(operation.path())?;
            Ok(ReviewFileEvidence {
                path: operation.path().to_path_buf(),
                before_sha256: match operation.expected_preimage() {
                    crate::mutation::MutationPreimage::Absent => None,
                    crate::mutation::MutationPreimage::Sha256 { sha256 } => Some(sha256.clone()),
                },
                after_sha256: current.sha256.clone(),
                size: current.size,
            })
        })
        .collect::<Result<Vec<_>, WriterError>>()?;
    let operation_id = intent.operation_id().to_owned();
    let attempt_id = attempt.id().as_str().to_owned();
    let mut request = ReviewerRequest {
        goal_id: snapshot.id().as_str().to_owned(),
        task_id: task_id.as_str().to_owned(),
        attempt_id: attempt_id.clone(),
        goal_revision: snapshot.revision(),
        plan_revision: snapshot.plan_revision(),
        writer_summary,
        files,
    };
    let invocation_id = format!("reviewer:{attempt_id}");
    let reviewer_snapshot =
        store.mutate_goal_snapshot(&session.id, goal_id, snapshot.revision(), |goal, _now| {
            ensure_reviewer_context_store(goal, task_id, &request)?;
            goal.task_advance_latest_mutation_intent(
                task_id,
                &operation_id,
                MutationIntentUpdate::BeginReviewer {
                    invocation_id: invocation_id.clone(),
                },
            )?;
            Ok(())
        })?;
    request.goal_revision = reviewer_snapshot.revision();

    let review_raw = match reviewer.review(&request) {
        Ok(raw) => raw,
        Err(error) => {
            let detail = error.to_string();
            block_after_mutation(
                store,
                session,
                goal_id,
                task_id,
                &request,
                "REVIEWER_BACKEND_ERROR",
                &detail,
            )?;
            return Err(error);
        }
    };
    let review = match parse_and_validate_reviewer_result(&review_raw, &request) {
        Ok(review) => review,
        Err(error) => {
            let detail = error.to_string();
            block_after_mutation(
                store,
                session,
                goal_id,
                task_id,
                &request,
                "REVIEWER_OUTPUT_REJECTED",
                &detail,
            )?;
            return Err(error);
        }
    };

    store
        .mutate_goal_snapshot(&session.id, goal_id, request.goal_revision, |goal, now| {
            ensure_reviewer_context_store(goal, task_id, &request)?;
            let gate_exceeded = goal.tasks()[task_id]
                .verification_specs()
                .iter()
                .any(|spec| {
                    matches!(
                        spec,
                        VerificationSpec::ReviewGate { max_blocking_findings }
                            if review.blocking_findings > *max_blocking_findings
                    )
                });
            goal.task_add_evidence(
                task_id,
                TaskEvidence::ReviewResult {
                    summary: review.summary.clone(),
                    blocking_findings: review.blocking_findings,
                },
            )?;
            goal.task_advance_latest_mutation_intent(
                task_id,
                &operation_id,
                MutationIntentUpdate::CompleteReviewer,
            )?;
            if gate_exceeded {
                goal.task_add_blocker(
                    task_id,
                    TaskBlocker::new(
                        "REVIEW_GATE_BLOCKED",
                        format!(
                            "reviewer reported {} blocking finding(s)",
                            review.blocking_findings
                        ),
                        true,
                    ),
                )?;
                goal.transition_task(
                    task_id,
                    TaskStatus::Blocked,
                    TaskTransitionContext::default(),
                    now,
                )?;
            } else {
                goal.transition_task(
                    task_id,
                    TaskStatus::Verifying,
                    TaskTransitionContext::default(),
                    now,
                )?;
            }
            Ok(())
        })
        .map_err(WriterError::from)
}

fn validate_goal_session_binding(
    goal: &Goal,
    session: &config::Session,
) -> Result<(), OrchestratorError> {
    if goal.session_id() != session.id {
        return Err(OrchestratorError::InvalidDag(
            "writer Goal/session binding mismatch".to_owned(),
        ));
    }
    let goal_cwd = fs::canonicalize(goal.cwd()).map_err(|_| {
        OrchestratorError::InvalidDag("writer Goal cwd cannot be canonicalized".to_owned())
    })?;
    let session_cwd = fs::canonicalize(&session.cwd).map_err(|_| {
        OrchestratorError::InvalidDag("writer session cwd cannot be canonicalized".to_owned())
    })?;
    if goal_cwd != session_cwd {
        return Err(OrchestratorError::InvalidDag(
            "writer Goal cwd does not match session cwd".to_owned(),
        ));
    }
    config::validate_path_authority(session, &goal_cwd, config::PathIntent::ExecutionCwd).map_err(
        |error| {
            OrchestratorError::InvalidDag(format!(
                "writer Goal cwd is outside session roots: {error}"
            ))
        },
    )?;
    crate::planner::execution_root_for_goal(goal, session).map_err(|error| {
        OrchestratorError::InvalidDag(format!("writer execution-root gate refused: {error}"))
    })?;
    Ok(())
}

pub(crate) fn holds_workspace_mutation_lease(task: &crate::task::Task) -> bool {
    if task.scope().operation_kind() == TaskOperationKind::ReadOnly || task.is_terminal() {
        return false;
    }
    if matches!(task.status(), TaskStatus::Running | TaskStatus::Verifying) {
        return true;
    }
    task.latest_attempt().is_some_and(|attempt| {
        matches!(
            attempt.side_effect_state(),
            Some(SideEffectState::ConfirmedPerformed | SideEffectState::Unknown)
        )
    })
}

fn build_writer_request(
    goal: &Goal,
    task_id: &TaskId,
    session: &config::Session,
) -> Result<WriterRequest, WriterError> {
    let task = goal.tasks().get(task_id).ok_or_else(|| {
        WriterError::AuthorityViolation("writer Task disappeared after checkpoint".to_owned())
    })?;
    let attempt = task.latest_attempt().ok_or_else(|| {
        WriterError::AuthorityViolation("RUNNING writer Task lacks an Attempt".to_owned())
    })?;
    let execution_root = crate::planner::execution_root_for_goal(goal, session)
        .map_err(|error| WriterError::AuthorityViolation(error.to_string()))?;
    Ok(WriterRequest {
        goal_id: goal.id().as_str().to_owned(),
        task_id: task_id.as_str().to_owned(),
        attempt_id: attempt.id().as_str().to_owned(),
        goal_revision: goal.revision(),
        plan_revision: goal.plan_revision(),
        goal_cwd: execution_root,
        task_title: task.title().to_owned(),
        task_objective: task.objective().to_owned(),
        allowed_paths: task.scope().allowed_paths().to_vec(),
        forbidden_paths: task.scope().forbidden_paths().to_vec(),
    })
}

fn parse_and_validate_writer_result(
    raw: &[u8],
    request: &WriterRequest,
) -> Result<WriterResult, WriterError> {
    if raw.len() > MAX_WRITER_RESULT_BYTES {
        return Err(WriterError::OutputInvalid(format!(
            "writer result exceeds {MAX_WRITER_RESULT_BYTES} bytes"
        )));
    }
    let result: WriterResult = serde_json::from_slice(raw)
        .map_err(|error| WriterError::OutputInvalid(error.to_string()))?;
    if result.goal_id != request.goal_id
        || result.task_id != request.task_id
        || result.attempt_id != request.attempt_id
        || result.goal_revision != request.goal_revision
        || result.plan_revision != request.plan_revision
    {
        return Err(WriterError::AuthorityViolation(
            "writer result execution-context binding mismatch".to_owned(),
        ));
    }
    validate_bounded_text(&result.summary, MAX_WRITER_SUMMARY_BYTES, "writer summary")?;
    validate_evidence(&result.evidence, false)?;
    match result.status {
        WriterStatus::CandidateComplete => {
            if result.proposed_operations.is_empty()
                || result.proposed_operations.len() > MAX_WRITER_OPERATIONS
            {
                return Err(WriterError::OutputInvalid(format!(
                    "candidate_complete requires 1..={MAX_WRITER_OPERATIONS} operations"
                )));
            }
        }
        WriterStatus::Blocked | WriterStatus::NeedsReplan | WriterStatus::Failed => {
            if !result.proposed_operations.is_empty() {
                return Err(WriterError::AuthorityViolation(
                    "non-candidate writer status cannot carry mutation operations".to_owned(),
                ));
            }
        }
    }
    let mut total_content = 0usize;
    for operation in &result.proposed_operations {
        match operation {
            WriterOperation::WriteUtf8 {
                path,
                expected_preimage,
                content,
            } => {
                if path.is_empty() || path.len() > MAX_WRITER_PATH_BYTES || path.contains('\0') {
                    return Err(WriterError::OutputInvalid(
                        "WRITE_UTF8 path is empty, too large, or contains NUL".to_owned(),
                    ));
                }
                if content.len() > MAX_WRITE_CONTENT_BYTES {
                    return Err(WriterError::OutputInvalid(format!(
                        "WRITE_UTF8 content exceeds {MAX_WRITE_CONTENT_BYTES} bytes"
                    )));
                }
                total_content = total_content.checked_add(content.len()).ok_or_else(|| {
                    WriterError::OutputInvalid("writer content size overflow".to_owned())
                })?;
                if total_content > MAX_TOTAL_WRITE_CONTENT_BYTES {
                    return Err(WriterError::OutputInvalid(format!(
                        "total writer content exceeds {MAX_TOTAL_WRITE_CONTENT_BYTES} bytes"
                    )));
                }
                if let PreimageExpectation::Sha256 { sha256 } = expected_preimage
                    && !is_canonical_sha256(sha256)
                {
                    return Err(WriterError::OutputInvalid(
                        "expected preimage SHA-256 must be 64 lowercase hex characters".to_owned(),
                    ));
                }
            }
        }
    }
    Ok(result)
}

fn parse_and_validate_reviewer_result(
    raw: &[u8],
    request: &ReviewerRequest,
) -> Result<ReviewerResult, WriterError> {
    if raw.len() > MAX_REVIEW_RESULT_BYTES {
        return Err(WriterError::ReviewerInvalid(format!(
            "reviewer result exceeds {MAX_REVIEW_RESULT_BYTES} bytes"
        )));
    }
    let result: ReviewerResult = serde_json::from_slice(raw)
        .map_err(|error| WriterError::ReviewerInvalid(error.to_string()))?;
    if result.goal_id != request.goal_id
        || result.task_id != request.task_id
        || result.attempt_id != request.attempt_id
        || result.goal_revision != request.goal_revision
        || result.plan_revision != request.plan_revision
    {
        return Err(WriterError::AuthorityViolation(
            "reviewer result execution-context binding mismatch".to_owned(),
        ));
    }
    validate_bounded_text(
        &result.summary,
        MAX_WRITER_SUMMARY_BYTES,
        "reviewer summary",
    )
    .map_err(|error| WriterError::ReviewerInvalid(error.to_string()))?;
    validate_evidence(&result.evidence, true)?;
    Ok(result)
}

fn validate_evidence(evidence: &[WriterEvidence], reviewer: bool) -> Result<(), WriterError> {
    if evidence.len() > MAX_WORKER_EVIDENCE_ITEMS {
        return Err(if reviewer {
            WriterError::ReviewerInvalid("too many reviewer evidence items".to_owned())
        } else {
            WriterError::OutputInvalid("too many writer evidence items".to_owned())
        });
    }
    let mut total = 0usize;
    for item in evidence {
        if item.kind.trim().is_empty()
            || item.kind.len() > MAX_WORKER_EVIDENCE_KIND_BYTES
            || item.value.trim().is_empty()
            || item.value.len() > MAX_WORKER_EVIDENCE_VALUE_BYTES
        {
            return Err(if reviewer {
                WriterError::ReviewerInvalid("invalid reviewer evidence item".to_owned())
            } else {
                WriterError::OutputInvalid("invalid writer evidence item".to_owned())
            });
        }
        total = total
            .checked_add(item.kind.len() + item.value.len())
            .ok_or_else(|| WriterError::OutputInvalid("evidence size overflow".to_owned()))?;
        if total > MAX_WORKER_EVIDENCE_TOTAL_BYTES {
            return Err(if reviewer {
                WriterError::ReviewerInvalid(
                    "reviewer evidence exceeds total size limit".to_owned(),
                )
            } else {
                WriterError::OutputInvalid("writer evidence exceeds total size limit".to_owned())
            });
        }
    }
    Ok(())
}

fn validate_bounded_text(value: &str, max_bytes: usize, label: &str) -> Result<(), WriterError> {
    if value.trim().is_empty() || value.len() > max_bytes {
        return Err(WriterError::OutputInvalid(format!(
            "{label} must be non-empty and at most {max_bytes} bytes"
        )));
    }
    Ok(())
}

fn is_canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn materialize_operations(
    goal: &Goal,
    session: &config::Session,
    task_id: &TaskId,
    operations: &[WriterOperation],
) -> Result<Vec<MaterializedWrite>, WriterError> {
    let task = goal.tasks().get(task_id).ok_or_else(|| {
        WriterError::AuthorityViolation("writer Task is missing during materialization".to_owned())
    })?;
    if task.worker() != WorkerKind::CodexWriter {
        return Err(WriterError::AuthorityViolation(
            "only CODEX_WRITER may materialize writer operations".to_owned(),
        ));
    }
    if task.scope().operation_kind() == TaskOperationKind::ReadOnly {
        return Err(WriterError::AuthorityViolation(
            "READ_ONLY TaskScope cannot materialize writer operations".to_owned(),
        ));
    }
    let goal_root = crate::planner::execution_root_for_goal(goal, session)
        .map_err(|error| WriterError::AuthorityViolation(error.to_string()))?;
    let goal_root = fs::canonicalize(goal_root).map_err(|_| {
        WriterError::AuthorityViolation("execution root cannot be canonicalized".to_owned())
    })?;
    if task.scope().allowed_paths().is_empty() {
        return Err(WriterError::AuthorityViolation(
            "mutating TaskScope has no allowed paths".to_owned(),
        ));
    }

    let mut seen = BTreeSet::new();
    let mut materialized = Vec::with_capacity(operations.len());
    for operation in operations {
        match operation {
            WriterOperation::WriteUtf8 {
                path,
                expected_preimage,
                content,
            } => {
                let (resolved, parent) = resolve_scoped_write_path(
                    path,
                    &goal_root,
                    task.scope().allowed_paths(),
                    task.scope().forbidden_paths(),
                )?;
                if !seen.insert(resolved.clone()) {
                    return Err(WriterError::AuthorityViolation(
                        "duplicate WRITE_UTF8 target after canonicalization".to_owned(),
                    ));
                }
                let before = observe_file_state(&resolved)?;
                if !preimage_matches(&before, expected_preimage) {
                    return Err(WriterError::PreimageMismatch(resolved));
                }
                materialized.push(MaterializedWrite {
                    path: resolved,
                    parent,
                    expected_preimage: expected_preimage.clone(),
                    content: content.clone(),
                    before,
                });
            }
        }
    }
    Ok(materialized)
}

fn resolve_scoped_write_path(
    raw: &str,
    goal_root: &Path,
    allowed_paths: &[PathBuf],
    forbidden_paths: &[PathBuf],
) -> Result<(PathBuf, PathBuf), WriterError> {
    let path = Path::new(raw);
    if raw.starts_with(":(") {
        return Err(WriterError::AuthorityViolation(
            "Git pathspec magic is not allowed in writer paths".to_owned(),
        ));
    }
    if path
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Err(WriterError::AuthorityViolation(
            "parent-directory traversal is not allowed in writer paths".to_owned(),
        ));
    }
    if contains_git_internal(path) {
        return Err(WriterError::AuthorityViolation(
            ".git internal mutation is forbidden".to_owned(),
        ));
    }

    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        goal_root.join(path)
    };
    if !absolute.starts_with(goal_root) {
        return Err(WriterError::AuthorityViolation(
            "writer path escapes the Goal cwd".to_owned(),
        ));
    }
    let normalized_allowed = allowed_paths
        .iter()
        .map(|path| canonicalize_scope_boundary(path))
        .collect::<Result<Vec<_>, _>>()?;
    let normalized_forbidden = forbidden_paths
        .iter()
        .map(|path| canonicalize_scope_boundary(path))
        .collect::<Result<Vec<_>, _>>()?;

    let file_name = absolute.file_name().ok_or_else(|| {
        WriterError::AuthorityViolation("WRITE_UTF8 target has no file name".to_owned())
    })?;
    let parent = absolute.parent().ok_or_else(|| {
        WriterError::AuthorityViolation("WRITE_UTF8 target has no parent".to_owned())
    })?;
    let canonical_parent = fs::canonicalize(parent).map_err(|_| {
        WriterError::AuthorityViolation(format!(
            "WRITE_UTF8 parent does not exist or cannot be canonicalized: {}",
            parent.display()
        ))
    })?;
    if !canonical_parent.is_dir() || !canonical_parent.starts_with(goal_root) {
        return Err(WriterError::AuthorityViolation(
            "writer path parent escapes the Goal cwd".to_owned(),
        ));
    }
    if contains_git_internal(&canonical_parent) {
        return Err(WriterError::AuthorityViolation(
            "writer path resolves through .git internals".to_owned(),
        ));
    }

    let resolved = match fs::symlink_metadata(&absolute) {
        Ok(_) => fs::canonicalize(&absolute).map_err(|_| {
            WriterError::AuthorityViolation(
                "existing writer target cannot be canonicalized; broken symlinks are rejected"
                    .to_owned(),
            )
        })?,
        Err(error) if error.kind() == ErrorKind::NotFound => canonical_parent.join(file_name),
        Err(error) => {
            return Err(WriterError::AuthorityViolation(format!(
                "writer target metadata failed: {error}"
            )));
        }
    };
    if !resolved.starts_with(goal_root) {
        return Err(WriterError::AuthorityViolation(
            "writer path resolves outside the Goal cwd".to_owned(),
        ));
    }
    if contains_git_internal(&resolved) {
        return Err(WriterError::AuthorityViolation(
            "writer target resolves into .git internals".to_owned(),
        ));
    }
    if !normalized_allowed
        .iter()
        .any(|allowed| resolved == *allowed || resolved.starts_with(allowed))
    {
        return Err(WriterError::AuthorityViolation(
            "writer target is outside TaskScope allowed_paths".to_owned(),
        ));
    }
    if normalized_forbidden
        .iter()
        .any(|forbidden| resolved == *forbidden || resolved.starts_with(forbidden))
    {
        return Err(WriterError::AuthorityViolation(
            "writer target resolves inside TaskScope forbidden_paths".to_owned(),
        ));
    }
    Ok((resolved, canonical_parent))
}

fn canonicalize_scope_boundary(path: &Path) -> Result<PathBuf, WriterError> {
    if path.exists() {
        return fs::canonicalize(path).map_err(|error| {
            WriterError::AuthorityViolation(format!(
                "TaskScope path cannot be canonicalized: {}: {error}",
                path.display()
            ))
        });
    }
    let mut existing = path.to_path_buf();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or_else(|| {
            WriterError::AuthorityViolation(
                "TaskScope path has no canonicalizable ancestor".to_owned(),
            )
        })?;
        suffix.push(name.to_os_string());
        if !existing.pop() {
            return Err(WriterError::AuthorityViolation(
                "TaskScope path has no canonicalizable ancestor".to_owned(),
            ));
        }
    }
    let mut resolved = fs::canonicalize(&existing).map_err(|error| {
        WriterError::AuthorityViolation(format!(
            "TaskScope ancestor cannot be canonicalized: {error}"
        ))
    })?;
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn contains_git_internal(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::Normal(value) if value == ".git"))
}

fn observe_file_state(path: &Path) -> Result<FileState, WriterError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(FileState {
            exists: false,
            size: None,
            sha256: None,
        }),
        Err(error) => Err(WriterError::AuthorityViolation(format!(
            "cannot inspect {}: {error}",
            path.display()
        ))),
        Ok(_) => {
            let metadata = fs::metadata(path).map_err(|error| {
                WriterError::AuthorityViolation(format!(
                    "cannot follow target {}: {error}",
                    path.display()
                ))
            })?;
            if !metadata.is_file() {
                return Err(WriterError::AuthorityViolation(format!(
                    "WRITE_UTF8 target is not a regular file: {}",
                    path.display()
                )));
            }
            let bytes = fs::read(path).map_err(|error| {
                WriterError::AuthorityViolation(format!(
                    "cannot read target {}: {error}",
                    path.display()
                ))
            })?;
            Ok(FileState {
                exists: true,
                size: Some(bytes.len() as u64),
                sha256: Some(sha256_hex(&bytes)),
            })
        }
    }
}

fn preimage_matches(state: &FileState, expectation: &PreimageExpectation) -> bool {
    match expectation {
        PreimageExpectation::Absent => !state.exists,
        PreimageExpectation::Sha256 { sha256 } => {
            state.exists && state.sha256.as_deref() == Some(sha256.as_str())
        }
    }
}

fn finish_valid_non_mutating_result(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    request: &WriterRequest,
    result: &WriterResult,
    report_digest: &str,
) -> Result<Goal, WriterError> {
    let next = match result.status {
        WriterStatus::Blocked => TaskStatus::Blocked,
        WriterStatus::NeedsReplan => TaskStatus::NeedsReplan,
        WriterStatus::Failed => TaskStatus::Failed,
        WriterStatus::CandidateComplete => {
            return Err(WriterError::AuthorityViolation(
                "candidate_complete cannot finish without host mutation".to_owned(),
            ));
        }
    };
    store
        .mutate_goal_snapshot(&session.id, goal_id, request.goal_revision, |goal, now| {
            ensure_active_attempt_store(goal, task_id, request)?;
            goal.task_record_latest_worker_report(
                task_id,
                WorkerReport::new(result.summary.clone(), Vec::new()),
            )?;
            let attempt_id = goal.tasks()[task_id]
                .latest_attempt()
                .expect("active attempt checked")
                .id()
                .clone();
            goal.task_add_evidence(
                task_id,
                TaskEvidence::WorkerReport {
                    attempt_id,
                    report_digest: report_digest.to_owned(),
                },
            )?;
            if next == TaskStatus::Blocked {
                goal.task_add_blocker(
                    task_id,
                    TaskBlocker::new("WRITER_BLOCKED", result.summary.clone(), true),
                )?;
            }
            goal.transition_task(task_id, next, TaskTransitionContext::default(), now)
        })
        .map_err(WriterError::from)
}

fn block_before_mutation(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    request: &WriterRequest,
    code: &str,
    detail: &str,
) -> Result<Goal, WriterError> {
    store
        .mutate_goal_snapshot(&session.id, goal_id, request.goal_revision, |goal, now| {
            ensure_active_attempt_store(goal, task_id, request)?;
            goal.task_add_blocker(
                task_id,
                TaskBlocker::new(code.to_owned(), detail.to_owned(), true),
            )?;
            goal.transition_task(
                task_id,
                TaskStatus::Blocked,
                TaskTransitionContext::default(),
                now,
            )
        })
        .map_err(WriterError::from)
}

fn bind_execution_metadata(
    goal: &mut Goal,
    task_id: &TaskId,
    operation_id: &str,
    scope_identity: &str,
    request_ids: &[String],
    side_effect_state: SideEffectState,
) -> Result<(), OrchestratorError> {
    goal.task_bind_latest_attempt_execution(
        task_id,
        Some(operation_id.to_owned()),
        Some(scope_identity.to_owned()),
        request_ids.first().cloned(),
        Some(SideEffectClass::LocalMutation),
        Some(side_effect_state),
        None,
        None,
    )?;
    for request_id in request_ids.iter().skip(1) {
        goal.task_bind_latest_attempt_execution(
            task_id,
            None,
            None,
            Some(request_id.clone()),
            None,
            None,
            None,
            None,
        )?;
    }
    Ok(())
}

fn block_after_mutation(
    store: &TaskStore,
    session: &config::Session,
    goal_id: &GoalId,
    task_id: &TaskId,
    request: &ReviewerRequest,
    code: &str,
    detail: &str,
) -> Result<Goal, WriterError> {
    store
        .mutate_goal_snapshot(&session.id, goal_id, request.goal_revision, |goal, now| {
            ensure_reviewer_context_store(goal, task_id, request)?;
            goal.task_add_blocker(
                task_id,
                TaskBlocker::new(code.to_owned(), detail.to_owned(), true),
            )?;
            goal.transition_task(
                task_id,
                TaskStatus::Blocked,
                TaskTransitionContext::default(),
                now,
            )
        })
        .map_err(WriterError::from)
}

fn ensure_active_attempt(
    goal: &Goal,
    task_id: &TaskId,
    request: &WriterRequest,
) -> Result<(), WriterError> {
    ensure_active_attempt_store(goal, task_id, request).map_err(WriterError::from)
}

fn ensure_active_attempt_store(
    goal: &Goal,
    task_id: &TaskId,
    request: &WriterRequest,
) -> Result<(), OrchestratorError> {
    if goal.id().as_str() != request.goal_id || goal.plan_revision() != request.plan_revision {
        return Err(OrchestratorError::InvalidDag(
            "writer durable context changed after proposal".to_owned(),
        ));
    }
    let task = goal
        .tasks()
        .get(task_id)
        .ok_or_else(|| OrchestratorError::InvalidDag("writer Task disappeared".to_owned()))?;
    if task.worker() != WorkerKind::CodexWriter || task.status() != TaskStatus::Running {
        return Err(OrchestratorError::InvalidDag(
            "writer Task is no longer the active CODEX_WRITER attempt".to_owned(),
        ));
    }
    let attempt = task.latest_attempt().ok_or_else(|| {
        OrchestratorError::CorruptGoal("RUNNING writer Task lacks an Attempt".to_owned())
    })?;
    if attempt.id().as_str() != request.attempt_id {
        return Err(OrchestratorError::InvalidDag(
            "writer Attempt identity changed".to_owned(),
        ));
    }
    Ok(())
}

fn ensure_reviewer_context_store(
    goal: &Goal,
    task_id: &TaskId,
    request: &ReviewerRequest,
) -> Result<(), OrchestratorError> {
    if goal.id().as_str() != request.goal_id || goal.plan_revision() != request.plan_revision {
        return Err(OrchestratorError::InvalidDag(
            "reviewer durable context changed".to_owned(),
        ));
    }
    let task = goal
        .tasks()
        .get(task_id)
        .ok_or_else(|| OrchestratorError::InvalidDag("reviewed Task disappeared".to_owned()))?;
    if task.worker() != WorkerKind::CodexWriter || task.status() != TaskStatus::Running {
        return Err(OrchestratorError::InvalidDag(
            "reviewed Task is not the active writer attempt".to_owned(),
        ));
    }
    let attempt = task.latest_attempt().ok_or_else(|| {
        OrchestratorError::CorruptGoal("active writer Task lacks an Attempt".to_owned())
    })?;
    if attempt.id().as_str() != request.attempt_id {
        return Err(OrchestratorError::InvalidDag(
            "reviewer Attempt identity changed".to_owned(),
        ));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn mutation_preimage(preimage: &PreimageExpectation) -> MutationPreimage {
    match preimage {
        PreimageExpectation::Absent => MutationPreimage::Absent,
        PreimageExpectation::Sha256 { sha256 } => MutationPreimage::Sha256 {
            sha256: sha256.clone(),
        },
    }
}

fn mutation_observation(state: &FileState) -> FileObservation {
    if state.exists {
        FileObservation::exists(
            state.size.unwrap_or(0),
            state.sha256.clone().unwrap_or_default(),
        )
    } else {
        FileObservation::absent()
    }
}

#[cfg(test)]
fn parse_writer_result_for_test(raw: &[u8]) -> Result<WriterResult, String> {
    serde_json::from_slice(raw).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox;

    #[test]
    fn writer_status_completed_is_not_in_the_host_defined_set() {
        let raw = br#"{
            "goal_id":"00000000-0000-4000-8000-000000000001",
            "task_id":"00000000-0000-4000-8000-000000000002",
            "attempt_id":"00000000-0000-4000-8000-000000000003",
            "goal_revision":2,
            "plan_revision":1,
            "status":"completed",
            "summary":"worker tried to self-authorize completion",
            "evidence":[],
            "proposed_operations":[]
        }"#;

        let error = parse_writer_result_for_test(raw).unwrap_err();
        assert!(error.contains("unknown variant") || error.contains("candidate_complete"));
    }

    struct LeaseFixture {
        root: std::path::PathBuf,
        store: crate::task_store::TaskStore,
        session: crate::config::Session,
        goal_id: crate::goal::GoalId,
        first: crate::task::TaskId,
        second: crate::task::TaskId,
        revision: u64,
    }

    fn lease_fixture() -> LeaseFixture {
        use crate::goal::Goal;
        use crate::task::{ReplaySafety, Task, TaskOperationKind, TaskScope, WorkerKind};
        use crate::task_store::TaskStore;
        use uuid::Uuid;

        let root = std::env::temp_dir().join(format!("local-mcp-phase5-lease-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let state = root.join("state");
        let session = crate::config::Session {
            id: format!("phase5-{}", Uuid::new_v4()),
            cwd: root.clone(),
            permitted_directories: vec![root.clone()],
        };
        let mut goal = Goal::new(
            session.id.clone(),
            root.clone(),
            "phase5 lease test",
            None,
            vec![],
            vec!["writer reaches verifying only through host authority".into()],
            "2026-09-13T00:00:00Z",
        )
        .unwrap();
        let scope = || {
            TaskScope::new(
                vec![root.clone()],
                vec![root.join(".git")],
                TaskOperationKind::LocalMutation,
                ReplaySafety::VerifyBeforeRetry,
            )
        };
        let first_task = Task::new(
            "writer one",
            "write one",
            true,
            WorkerKind::CodexWriter,
            scope(),
            vec![],
            2,
            1,
            "2026-09-13T00:00:00Z",
        )
        .unwrap();
        let first = first_task.id().clone();
        let second_task = Task::new(
            "writer two",
            "write two",
            true,
            WorkerKind::CodexWriter,
            scope(),
            vec![],
            2,
            1,
            "2026-09-13T00:00:00Z",
        )
        .unwrap();
        let second = second_task.id().clone();
        goal.materialize_initial_plan(vec![first_task, second_task], "2026-09-13T00:00:00Z")
            .unwrap();
        let revision = goal.revision();
        let goal_id = goal.id().clone();
        let store = TaskStore::with_state_root(state);
        store.create_goal(&goal).unwrap();
        LeaseFixture {
            root,
            store,
            session,
            goal_id,
            first,
            second,
            revision,
        }
    }

    #[test]
    fn ready_writer_checkpoint_reserves_single_workspace_mutation_lease() {
        use crate::task::TaskStatus;

        let fixture = lease_fixture();
        let request = begin_writer_attempt(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            &fixture.first,
            fixture.revision,
        )
        .unwrap();
        let running = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        assert_eq!(
            running.tasks()[&fixture.first].status(),
            TaskStatus::Running
        );
        assert_eq!(running.tasks()[&fixture.first].attempts().len(), 1);
        assert_eq!(request.goal_revision(), running.revision());

        let error = begin_writer_attempt(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            &fixture.second,
            running.revision(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("WORKSPACE_MUTATION"));

        std::fs::remove_dir_all(fixture.root).unwrap();
    }

    struct ExistingFileWriter;

    impl WriterBackend for ExistingFileWriter {
        fn propose(&self, request: &WriterRequest) -> Result<Vec<u8>, WriterError> {
            assert_eq!(request.task_title(), "writer one");
            assert_eq!(request.task_objective(), "write one");
            assert!(request.goal_cwd().is_absolute());
            assert!(!request.allowed_paths().is_empty());
            assert!(!request.forbidden_paths().is_empty());
            Ok(serde_json::json!({
                "goal_id": request.goal_id(),
                "task_id": request.task_id(),
                "attempt_id": request.attempt_id(),
                "goal_revision": request.goal_revision(),
                "plan_revision": request.plan_revision(),
                "status": "candidate_complete",
                "summary": "replace target through host authority",
                "evidence": [{"kind":"reasoning_reference","value":"deterministic fixture"}],
                "proposed_operations": [{
                    "kind": "WRITE_UTF8",
                    "path": "target.txt",
                    "expected_preimage": {
                        "kind": "SHA256",
                        "sha256": "9160d4be34c8695bd172a76c7c7966587ea5a4d991ad22c87b2b91af54aa9ebb"
                    },
                    "content": "after\n"
                }]
            })
            .to_string()
            .into_bytes())
        }
    }

    struct PassingReviewer;

    impl ReviewerBackend for PassingReviewer {
        fn review(&self, request: &ReviewerRequest) -> Result<Vec<u8>, WriterError> {
            assert_eq!(request.files().len(), 1);
            let file = &request.files()[0];
            assert!(file.path().ends_with("target.txt"));
            match request.writer_summary() {
                "replace target through host authority" => {
                    assert_eq!(
                        file.before_sha256(),
                        Some("9160d4be34c8695bd172a76c7c7966587ea5a4d991ad22c87b2b91af54aa9ebb")
                    );
                    let expected_after = sha256_hex(b"after\n");
                    assert_eq!(file.after_sha256(), Some(expected_after.as_str()));
                    assert_eq!(file.size(), Some(6));
                }
                "create an explicitly absent file" => {
                    assert_eq!(file.before_sha256(), None);
                    let expected_after = sha256_hex(b"created\n");
                    assert_eq!(file.after_sha256(), Some(expected_after.as_str()));
                    assert_eq!(file.size(), Some(8));
                }
                other => panic!("unexpected reviewer fixture summary: {other}"),
            }
            Ok(serde_json::json!({
                "goal_id": request.goal_id(),
                "task_id": request.task_id(),
                "attempt_id": request.attempt_id(),
                "goal_revision": request.goal_revision(),
                "plan_revision": request.plan_revision(),
                "summary": "host-observed mutation matches the proposal",
                "blocking_findings": 0,
                "evidence": [{"kind":"file_digest","value":"target postimage checked"}]
            })
            .to_string()
            .into_bytes())
        }
    }

    #[tokio::test]
    async fn valid_write_utf8_uses_host_authority_and_stops_in_verifying() {
        use crate::task::TaskStatus;

        let fixture = lease_fixture();
        std::fs::write(fixture.root.join("target.txt"), b"before\n").unwrap();

        let result = run_writer_attempt(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            &fixture.first,
            fixture.revision,
            &ExistingFileWriter,
            &PassingReviewer,
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read(fixture.root.join("target.txt")).unwrap(),
            b"after\n"
        );
        assert_eq!(
            result.tasks()[&fixture.first].status(),
            TaskStatus::Verifying
        );
        assert!(result.tasks()[&fixture.first].evidence_count() >= 3);
        assert_ne!(
            result.tasks()[&fixture.first].status(),
            TaskStatus::Completed
        );
        let lease_error = begin_writer_attempt(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            &fixture.second,
            result.revision(),
        )
        .unwrap_err();
        assert!(lease_error.to_string().contains("WORKSPACE_MUTATION"));

        std::fs::remove_dir_all(fixture.root).unwrap();
    }

    struct InspectingWriteBoundary<'a> {
        store: &'a TaskStore,
        session: &'a crate::config::Session,
        goal_id: &'a crate::goal::GoalId,
        task_id: &'a crate::task::TaskId,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl WriteBoundary for InspectingWriteBoundary<'_> {
        fn write<'a>(
            &'a self,
            _absolute: &'a std::path::Path,
            _parent: &'a std::path::Path,
            _content: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<sandbox::Output>> + Send + 'a>,
        > {
            Box::pin(async move {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let goal = self
                    .store
                    .load_goal(&self.session.id, self.goal_id)
                    .unwrap();
                let task = goal.tasks().get(self.task_id).unwrap();
                assert_eq!(
                    task.latest_attempt()
                        .unwrap()
                        .mutation_intent()
                        .unwrap()
                        .state(),
                    crate::mutation::MutationIntentState::Applying
                );
                anyhow::bail!("injected write boundary failure")
            })
        }
    }

    #[tokio::test]
    async fn writer_enters_write_boundary_only_after_durable_prepare() {
        let fixture = lease_fixture();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let boundary = InspectingWriteBoundary {
            store: &fixture.store,
            session: &fixture.session,
            goal_id: &fixture.goal_id,
            task_id: &fixture.first,
            calls: calls.clone(),
        };
        let _ = run_writer_attempt_with_boundary(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            &fixture.first,
            fixture.revision,
            &AbsentFileWriter,
            &PassingReviewer,
            &boundary,
        )
        .await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        std::fs::remove_dir_all(fixture.root).unwrap();
    }

    struct WriteThenFailBoundary;

    impl WriteBoundary for WriteThenFailBoundary {
        fn write<'a>(
            &'a self,
            absolute: &'a std::path::Path,
            _parent: &'a std::path::Path,
            content: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<sandbox::Output>> + Send + 'a>,
        > {
            Box::pin(async move {
                std::fs::write(absolute, content)?;
                anyhow::bail!("injected crash after successful filesystem write")
            })
        }
    }

    struct RecoveredReviewer;

    impl ReviewerBackend for RecoveredReviewer {
        fn review(&self, request: &ReviewerRequest) -> Result<Vec<u8>, WriterError> {
            Ok(serde_json::json!({
                "goal_id": request.goal_id(),
                "task_id": request.task_id(),
                "attempt_id": request.attempt_id(),
                "goal_revision": request.goal_revision(),
                "plan_revision": request.plan_revision(),
                "summary": "recovered review completed",
                "blocking_findings": 0,
                "evidence": [{"kind":"recovery","value":"existing mutation reviewed"}]
            })
            .to_string()
            .into_bytes())
        }
    }

    #[tokio::test]
    async fn successful_write_before_checkpoint_reconciles_as_performed() {
        let fixture = lease_fixture();
        let error = run_writer_attempt_with_boundary(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            &fixture.first,
            fixture.revision,
            &AbsentFileWriter,
            &PassingReviewer,
            &WriteThenFailBoundary,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("successful filesystem write"));
        let mut recovered = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        assert!(
            crate::mutation_recovery::reconcile_goal_mutations(
                &mut recovered,
                "2026-09-16T00:00:01Z"
            )
            .unwrap()
        );
        recovered
            .recover_stale_running("2026-09-16T00:00:02Z")
            .unwrap();
        let stored_revision = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap()
            .revision();
        recovered = fixture
            .store
            .mutate_goal_snapshot(
                &fixture.session.id,
                &fixture.goal_id,
                stored_revision,
                |goal, _| {
                    *goal = recovered.clone();
                    Ok(())
                },
            )
            .unwrap();
        let task = recovered.tasks().get(&fixture.first).unwrap();
        assert_eq!(task.status(), TaskStatus::Running);
        assert!(task.needs_reviewer_recovery());
        assert_eq!(
            task.latest_attempt().unwrap().side_effect_state(),
            Some(SideEffectState::ConfirmedPerformed)
        );
        assert_eq!(
            crate::scheduler::select_next_action(&recovered).unwrap(),
            crate::scheduler::SchedulerDecision::RunReviewer {
                task_id: fixture.first.clone()
            }
        );
        let reviewed = resume_writer_reviewer(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            &fixture.first,
            recovered.revision(),
            &RecoveredReviewer,
        )
        .await
        .unwrap();
        assert_eq!(
            reviewed.tasks()[&fixture.first].status(),
            TaskStatus::Verifying
        );
        assert_eq!(
            reviewed.tasks()[&fixture.first]
                .latest_attempt()
                .unwrap()
                .mutation_intent()
                .unwrap()
                .reviewer_state(),
            crate::mutation::ReviewerInvocationState::Persisted
        );
        std::fs::remove_dir_all(fixture.root).unwrap();
    }

    #[test]
    fn writer_schema_rejects_authority_injection_fields() {
        let raw = br#"{
            "goal_id":"00000000-0000-4000-8000-000000000001",
            "task_id":"00000000-0000-4000-8000-000000000002",
            "attempt_id":"00000000-0000-4000-8000-000000000003",
            "goal_revision":2,
            "plan_revision":1,
            "status":"failed",
            "summary":"malicious authority injection",
            "evidence":[],
            "proposed_operations":[],
            "authorized":true,
            "task_status":"COMPLETED",
            "attempt_budget":99,
            "commit":true
        }"#;

        let error = parse_writer_result_for_test(raw).unwrap_err();
        assert!(error.contains("unknown field"));
    }

    #[test]
    fn writer_result_and_content_limits_are_enforced() {
        let fixture = lease_fixture();
        let request = begin_writer_attempt(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            &fixture.first,
            fixture.revision,
        )
        .unwrap();

        let oversized_result = vec![b'x'; MAX_WRITER_RESULT_BYTES + 1];
        assert!(matches!(
            parse_and_validate_writer_result(&oversized_result, &request),
            Err(WriterError::OutputInvalid(_))
        ));

        let oversized_content = "x".repeat(MAX_WRITE_CONTENT_BYTES + 1);
        let raw = serde_json::json!({
            "goal_id": request.goal_id(),
            "task_id": request.task_id(),
            "attempt_id": request.attempt_id(),
            "goal_revision": request.goal_revision(),
            "plan_revision": request.plan_revision(),
            "status": "candidate_complete",
            "summary": "bounded content test",
            "evidence": [],
            "proposed_operations": [{
                "kind":"WRITE_UTF8",
                "path":"bounded.txt",
                "expected_preimage":{"kind":"ABSENT"},
                "content":oversized_content
            }]
        })
        .to_string();
        assert!(matches!(
            parse_and_validate_writer_result(raw.as_bytes(), &request),
            Err(WriterError::OutputInvalid(_))
        ));

        std::fs::remove_dir_all(fixture.root).unwrap();
    }

    struct AbsentFileWriter;

    impl WriterBackend for AbsentFileWriter {
        fn propose(&self, request: &WriterRequest) -> Result<Vec<u8>, WriterError> {
            Ok(serde_json::json!({
                "goal_id": request.goal_id(),
                "task_id": request.task_id(),
                "attempt_id": request.attempt_id(),
                "goal_revision": request.goal_revision(),
                "plan_revision": request.plan_revision(),
                "status": "candidate_complete",
                "summary": "create an explicitly absent file",
                "evidence": [],
                "proposed_operations": [{
                    "kind":"WRITE_UTF8",
                    "path":"target.txt",
                    "expected_preimage":{"kind":"ABSENT"},
                    "content":"created\n"
                }]
            })
            .to_string()
            .into_bytes())
        }
    }

    struct StalePreimageWriter;

    impl WriterBackend for StalePreimageWriter {
        fn propose(&self, request: &WriterRequest) -> Result<Vec<u8>, WriterError> {
            Ok(serde_json::json!({
                "goal_id": request.goal_id(),
                "task_id": request.task_id(),
                "attempt_id": request.attempt_id(),
                "goal_revision": request.goal_revision(),
                "plan_revision": request.plan_revision(),
                "status": "candidate_complete",
                "summary": "stale preimage must not write",
                "evidence": [],
                "proposed_operations": [{
                    "kind":"WRITE_UTF8",
                    "path":"target.txt",
                    "expected_preimage":{
                        "kind":"SHA256",
                        "sha256":"0000000000000000000000000000000000000000000000000000000000000000"
                    },
                    "content":"must-not-appear\n"
                }]
            })
            .to_string()
            .into_bytes())
        }
    }

    #[tokio::test]
    async fn explicit_absent_preimage_can_create_a_file() {
        let fixture = lease_fixture();
        let result = run_writer_attempt(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            &fixture.first,
            fixture.revision,
            &AbsentFileWriter,
            &PassingReviewer,
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read(fixture.root.join("target.txt")).unwrap(),
            b"created\n"
        );
        assert_eq!(
            result.tasks()[&fixture.first].status(),
            TaskStatus::Verifying
        );
        std::fs::remove_dir_all(fixture.root).unwrap();
    }

    #[tokio::test]
    async fn stale_preimage_never_mutates_and_requires_replan() {
        let fixture = lease_fixture();
        std::fs::write(fixture.root.join("target.txt"), b"before\n").unwrap();

        let result = run_writer_attempt(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            &fixture.first,
            fixture.revision,
            &StalePreimageWriter,
            &PassingReviewer,
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read(fixture.root.join("target.txt")).unwrap(),
            b"before\n"
        );
        assert_eq!(
            result.tasks()[&fixture.first].status(),
            TaskStatus::NeedsReplan
        );
        std::fs::remove_dir_all(fixture.root).unwrap();
    }

    #[test]
    fn writer_path_authority_rejects_escape_git_magic_and_git_internals() {
        let fixture = lease_fixture();
        let root = std::fs::canonicalize(&fixture.root).unwrap();
        let allowed = std::slice::from_ref(&fixture.root);
        std::fs::create_dir_all(fixture.root.join("forbidden")).unwrap();
        let forbidden = vec![fixture.root.join("forbidden")];
        let outside = fixture
            .root
            .parent()
            .unwrap()
            .join(format!("outside-{}", Uuid::new_v4()));

        for raw in [
            "../escape.txt".to_owned(),
            outside.to_string_lossy().into_owned(),
            ".git/config".to_owned(),
            ":(top)target.txt".to_owned(),
            "forbidden/target.txt".to_owned(),
        ] {
            let result = resolve_scoped_write_path(&raw, &root, allowed, &forbidden);
            assert!(
                matches!(result, Err(WriterError::AuthorityViolation(_))),
                "accepted {raw}"
            );
        }
        std::fs::create_dir_all(fixture.root.join("allowed")).unwrap();
        let narrow =
            resolve_scoped_write_path("target.txt", &root, &[fixture.root.join("allowed")], &[]);
        assert!(matches!(narrow, Err(WriterError::AuthorityViolation(_))));

        std::fs::remove_dir_all(fixture.root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn writer_path_authority_rejects_symlink_prefix_escape() {
        use std::os::unix::fs::symlink;

        let fixture = lease_fixture();
        let outside = fixture
            .root
            .parent()
            .unwrap()
            .join(format!("phase5-outside-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, fixture.root.join("link")).unwrap();
        let root = std::fs::canonicalize(&fixture.root).unwrap();

        let result = resolve_scoped_write_path(
            "link/escape.txt",
            &root,
            std::slice::from_ref(&fixture.root),
            &[],
        );
        assert!(matches!(result, Err(WriterError::AuthorityViolation(_))));

        std::fs::remove_dir_all(fixture.root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn non_writer_roles_and_read_only_scope_cannot_start_writer_mutation() {
        use crate::task::{ReplaySafety, Task, TaskOperationKind, TaskScope};

        for worker in [
            WorkerKind::LocalOperation,
            WorkerKind::CodexReadonly,
            WorkerKind::CodexReviewer,
            WorkerKind::Verifier,
        ] {
            let root =
                std::env::temp_dir().join(format!("local-mcp-phase5-role-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let session = crate::config::Session {
                id: format!("phase5-role-{}", Uuid::new_v4()),
                cwd: root.clone(),
                permitted_directories: vec![root.clone()],
            };
            let mut goal = Goal::new(
                session.id.clone(),
                root.clone(),
                "role gate",
                None,
                vec![],
                vec!["gate".into()],
                "2026-09-13T00:00:00Z",
            )
            .unwrap();
            let task = Task::new(
                "role task",
                "must reject",
                true,
                worker,
                TaskScope::new(
                    vec![root.clone()],
                    vec![],
                    TaskOperationKind::LocalMutation,
                    ReplaySafety::VerifyBeforeRetry,
                ),
                vec![],
                1,
                1,
                "2026-09-13T00:00:00Z",
            )
            .unwrap();
            let task_id = task.id().clone();
            goal.materialize_initial_plan(vec![task], "2026-09-13T00:00:00Z")
                .unwrap();
            let revision = goal.revision();
            let goal_id = goal.id().clone();
            let store = TaskStore::with_state_root(root.join("state"));
            store.create_goal(&goal).unwrap();
            let error =
                begin_writer_attempt(&store, &session, &goal_id, &task_id, revision).unwrap_err();
            assert!(error.to_string().contains("CODEX_WRITER"));
            std::fs::remove_dir_all(root).unwrap();
        }

        let root =
            std::env::temp_dir().join(format!("local-mcp-phase5-readonly-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let session = crate::config::Session {
            id: format!("phase5-readonly-{}", Uuid::new_v4()),
            cwd: root.clone(),
            permitted_directories: vec![root.clone()],
        };
        let mut goal = Goal::new(
            session.id.clone(),
            root.clone(),
            "readonly gate",
            None,
            vec![],
            vec!["gate".into()],
            "2026-09-13T00:00:00Z",
        )
        .unwrap();
        let task = Task::new(
            "readonly writer",
            "must reject",
            true,
            WorkerKind::CodexWriter,
            TaskScope::new(
                vec![root.clone()],
                vec![],
                TaskOperationKind::ReadOnly,
                ReplaySafety::SafeReadOnly,
            ),
            vec![],
            1,
            1,
            "2026-09-13T00:00:00Z",
        )
        .unwrap();
        let task_id = task.id().clone();
        goal.materialize_initial_plan(vec![task], "2026-09-13T00:00:00Z")
            .unwrap();
        let revision = goal.revision();
        let goal_id = goal.id().clone();
        let store = TaskStore::with_state_root(root.join("state"));
        store.create_goal(&goal).unwrap();
        let error =
            begin_writer_attempt(&store, &session, &goal_id, &task_id, revision).unwrap_err();
        assert!(error.to_string().contains("mutating TaskScope"));
        std::fs::remove_dir_all(root).unwrap();
    }

    struct BlockingReviewer;

    impl ReviewerBackend for BlockingReviewer {
        fn review(&self, request: &ReviewerRequest) -> Result<Vec<u8>, WriterError> {
            Ok(serde_json::json!({
                "goal_id": request.goal_id(),
                "task_id": request.task_id(),
                "attempt_id": request.attempt_id(),
                "goal_revision": request.goal_revision(),
                "plan_revision": request.plan_revision(),
                "summary": "one blocking finding",
                "blocking_findings": 1,
                "evidence": [{"kind":"review","value":"blocking fixture"}]
            })
            .to_string()
            .into_bytes())
        }
    }

    #[tokio::test]
    async fn review_gate_blocks_but_mutation_lease_remains_reserved() {
        let fixture = lease_fixture();
        std::fs::write(fixture.root.join("target.txt"), b"before\n").unwrap();
        let configured = fixture
            .store
            .mutate_goal_snapshot(
                &fixture.session.id,
                &fixture.goal_id,
                fixture.revision,
                |goal, _now| {
                    goal.strengthen_task_verification(
                        &fixture.first,
                        vec![VerificationSpec::ReviewGate {
                            max_blocking_findings: 0,
                        }],
                    )
                },
            )
            .unwrap();

        let result = run_writer_attempt(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            &fixture.first,
            configured.revision(),
            &ExistingFileWriter,
            &BlockingReviewer,
        )
        .await
        .unwrap();
        assert_eq!(result.tasks()[&fixture.first].status(), TaskStatus::Blocked);
        assert_eq!(
            std::fs::read(fixture.root.join("target.txt")).unwrap(),
            b"after\n"
        );

        let error = begin_writer_attempt(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            &fixture.second,
            result.revision(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("WORKSPACE_MUTATION"));
        std::fs::remove_dir_all(fixture.root).unwrap();
    }

    struct AuthorityInjectingReviewer;

    impl ReviewerBackend for AuthorityInjectingReviewer {
        fn review(&self, request: &ReviewerRequest) -> Result<Vec<u8>, WriterError> {
            Ok(serde_json::json!({
                "goal_id": request.goal_id(),
                "task_id": request.task_id(),
                "attempt_id": request.attempt_id(),
                "goal_revision": request.goal_revision(),
                "plan_revision": request.plan_revision(),
                "summary": "reviewer tried to authorize itself",
                "blocking_findings": 0,
                "evidence": [],
                "authorized": true,
                "task_status": "COMPLETED"
            })
            .to_string()
            .into_bytes())
        }
    }

    #[tokio::test]
    async fn reviewer_authority_injection_is_rejected_after_host_evidence_and_keeps_lease() {
        let fixture = lease_fixture();
        std::fs::write(fixture.root.join("target.txt"), b"before\n").unwrap();

        let error = run_writer_attempt(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            &fixture.first,
            fixture.revision,
            &ExistingFileWriter,
            &AuthorityInjectingReviewer,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, WriterError::ReviewerInvalid(_)));
        assert_eq!(
            std::fs::read(fixture.root.join("target.txt")).unwrap(),
            b"after\n"
        );

        let blocked = fixture
            .store
            .load_goal(&fixture.session.id, &fixture.goal_id)
            .unwrap();
        assert_eq!(
            blocked.tasks()[&fixture.first].status(),
            TaskStatus::Blocked
        );
        assert_eq!(
            blocked.tasks()[&fixture.first]
                .latest_attempt()
                .unwrap()
                .side_effect_state(),
            Some(SideEffectState::ConfirmedPerformed)
        );
        let lease_error = begin_writer_attempt(
            &fixture.store,
            &fixture.session,
            &fixture.goal_id,
            &fixture.second,
            blocked.revision(),
        )
        .unwrap_err();
        assert!(lease_error.to_string().contains("WORKSPACE_MUTATION"));

        std::fs::remove_dir_all(fixture.root).unwrap();
    }
}
