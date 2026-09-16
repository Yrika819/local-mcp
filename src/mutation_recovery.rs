use std::io::ErrorKind;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::goal::{Goal, GoalStatus};
use crate::mutation::{
    FileObservation, MutationIntentState, MutationOperationIntent, MutationOperationState,
    MutationPreimage,
};
use crate::orchestrator_error::OrchestratorError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReconciliationDecision {
    NotPerformed,
    Performed,
    Partial,
    Unknown,
}

pub(crate) fn classify_observations(
    intent_state: MutationIntentState,
    operations: &[MutationOperationIntent],
    current: &[FileObservation],
) -> Result<ReconciliationDecision, OrchestratorError> {
    if operations.is_empty() || operations.len() != current.len() {
        return Err(OrchestratorError::CorruptGoal(
            "MutationIntent observation count does not match operations".to_owned(),
        ));
    }
    let mut before_count = 0;
    let mut after_count = 0;
    for (operation, observed) in operations.iter().zip(current) {
        if matches_before(operation, observed) {
            before_count += 1;
        } else if matches_after(operation, observed) {
            after_count += 1;
        } else {
            return Ok(ReconciliationDecision::Unknown);
        }
    }
    if before_count == operations.len()
        && intent_state == MutationIntentState::Prepared
        && operations
            .iter()
            .all(|operation| operation.state() == MutationOperationState::Prepared)
    {
        return Ok(ReconciliationDecision::NotPerformed);
    }
    if after_count == operations.len()
        && matches!(intent_state, MutationIntentState::Applying | MutationIntentState::Applied)
    {
        return Ok(ReconciliationDecision::Performed);
    }
    if before_count > 0 && after_count > 0 {
        return Ok(ReconciliationDecision::Partial);
    }
    Ok(ReconciliationDecision::Unknown)
}

fn matches_before(operation: &MutationOperationIntent, observed: &FileObservation) -> bool {
    match operation.expected_preimage() {
        MutationPreimage::Absent => !observed.is_exists(),
        MutationPreimage::Sha256 { sha256 } => {
            observed.is_exists() && observed.sha256() == Some(sha256.as_str())
        }
    }
}

fn matches_after(operation: &MutationOperationIntent, observed: &FileObservation) -> bool {
    observed.is_exists() && observed.sha256() == Some(operation.intended_after_sha256())
}

pub(crate) fn reconcile_goal_mutations(
    goal: &mut Goal,
    now: &str,
) -> Result<bool, OrchestratorError> {
    let task_ids = goal
        .tasks()
        .iter()
        .filter_map(|(task_id, task)| {
            if !matches!(task.status(), crate::task::TaskStatus::Running) {
                return None;
            }
            let intent = task.latest_attempt()?.mutation_intent()?;
            matches!(
                intent.state(),
                MutationIntentState::Prepared
                    | MutationIntentState::Applying
                    | MutationIntentState::Applied
            )
            .then_some(task_id.clone())
        })
        .collect::<Vec<_>>();
    let mut changed = false;
    for task_id in task_ids {
        let intent = goal.tasks()[&task_id]
            .latest_attempt()
            .and_then(|attempt| attempt.mutation_intent())
            .cloned()
            .ok_or_else(|| {
                OrchestratorError::CorruptGoal(
                    "mutation reconciliation target disappeared".to_owned(),
                )
            })?;
        let mut observation_failed = false;
        let mut observation_available = Vec::with_capacity(intent.operations().len());
        let observations = intent
            .operations()
            .iter()
            .map(|operation| match observe_path(operation.path()) {
                Ok(observation) => {
                    observation_available.push(true);
                    observation
                }
                Err(_) => {
                    observation_failed = true;
                    observation_available.push(false);
                    FileObservation::absent()
                }
            })
            .collect::<Vec<_>>();
        let decision = if observation_failed {
            ReconciliationDecision::Unknown
        } else {
            classify_observations(intent.state(), intent.operations(), &observations)?
        };
        let state = match decision {
            ReconciliationDecision::NotPerformed => MutationIntentState::ReconciledNotPerformed,
            ReconciliationDecision::Performed => MutationIntentState::ReconciledPerformed,
            ReconciliationDecision::Partial => MutationIntentState::Partial,
            ReconciliationDecision::Unknown => MutationIntentState::Unknown,
        };
        goal.task_reconcile_latest_mutation_intent(
            &task_id,
            intent.operation_id(),
            state,
            format!(
                "reconciled operation {} as {:?} from {} target observation(s)",
                intent.operation_id(),
                decision,
                observations.len()
            ),
            now,
        )?;
        for ((operation, observation), available) in intent
            .operations()
            .iter()
            .zip(&observations)
            .zip(&observation_available)
        {
            if !available {
                continue;
            }
            goal.task_add_evidence(
                &task_id,
                crate::task::TaskEvidence::FileSnapshot {
                    path: operation.path().to_path_buf(),
                    exists: observation.is_exists(),
                    size: observation.size(),
                    sha256: observation.sha256().map(str::to_owned),
                },
            )?;
        }
        changed = true;
        if decision == ReconciliationDecision::Performed
            && intent.reviewer_state() == crate::mutation::ReviewerInvocationState::Invoking
        {
            let task = goal.tasks().get(&task_id).ok_or_else(|| {
                OrchestratorError::InvalidDag("task is missing".to_owned())
            })?;
            if task.status() != crate::task::TaskStatus::Blocked {
                goal.task_add_blocker(
                    &task_id,
                    crate::task::TaskBlocker::new(
                        "REVIEWER_RESULT_UNCERTAIN",
                        "Reviewer invocation was in flight at the durable boundary; duplicate invocation is forbidden",
                        true,
                    ),
                )?;
                goal.transition_task(
                    &task_id,
                    crate::task::TaskStatus::Blocked,
                    crate::task::TaskTransitionContext::default(),
                    now,
                )?;
            }
        }
        if matches!(decision, ReconciliationDecision::Partial | ReconciliationDecision::Unknown)
        {
            let task = goal.tasks().get(&task_id).ok_or_else(|| {
                OrchestratorError::InvalidDag("task is missing".to_owned())
            })?;
            if task.status() != crate::task::TaskStatus::Blocked {
                goal.task_add_blocker(
                    &task_id,
                    crate::task::TaskBlocker::new(
                        "MUTATION_RECONCILIATION_REQUIRED",
                        format!(
                            "durable Writer mutation is {decision:?}; blind retry is forbidden"
                        ),
                        true,
                    ),
                )?;
                goal.transition_task(
                    &task_id,
                    crate::task::TaskStatus::Blocked,
                    crate::task::TaskTransitionContext::default(),
                    now,
                )?;
            }
        }
    }
    if changed
        && goal.status() != GoalStatus::Blocked
        && goal
            .tasks()
            .values()
            .any(|task| task.mandatory() && task.status() == crate::task::TaskStatus::Blocked)
    {
        goal.transition_to(GoalStatus::Blocked, now)?;
    }
    Ok(changed)
}

fn observe_path(path: &Path) -> Result<FileObservation, OrchestratorError> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(FileObservation::absent()),
        Err(error) => Err(OrchestratorError::PersistenceIo(error)),
        Ok(_) => {
            let metadata = std::fs::metadata(path).map_err(OrchestratorError::PersistenceIo)?;
            if !metadata.is_file() {
                return Err(OrchestratorError::CorruptGoal(
                    "mutation target is not a regular file".to_owned(),
                ));
            }
            let bytes = std::fs::read(path).map_err(OrchestratorError::PersistenceIo)?;
            Ok(FileObservation::exists(bytes.len() as u64, sha256_hex(&bytes)))
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutation::{
        FileObservation, MutationIntentState, MutationOperationIntent, MutationPreimage,
    };

    fn operation(index: u32, before: FileObservation, after: &str) -> MutationOperationIntent {
        MutationOperationIntent::new(
            index,
            format!("/tmp/{index}").into(),
            MutationPreimage::Absent,
            before,
            after.to_owned(),
            format!("request-{index}"),
        )
    }

    #[test]
    fn reconciliation_decision_table_is_conservative() {
        let before = vec![FileObservation::absent(), FileObservation::absent()];
        let after = vec![
            FileObservation::exists(1, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()),
            FileObservation::exists(1, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned()),
        ];
        let operations = vec![
            operation(0, before[0].clone(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            operation(1, before[1].clone(), "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        ];
        assert_eq!(
            classify_observations(MutationIntentState::Prepared, &operations, &before).unwrap(),
            ReconciliationDecision::NotPerformed
        );
        assert_eq!(
            classify_observations(MutationIntentState::Applied, &operations, &after).unwrap(),
            ReconciliationDecision::Performed
        );
        assert_eq!(
            classify_observations(
                MutationIntentState::Applying,
                &operations,
                &[after[0].clone(), before[1].clone()],
            )
            .unwrap(),
            ReconciliationDecision::Partial
        );
        assert_eq!(
            classify_observations(
                MutationIntentState::Applying,
                &operations,
                &[FileObservation::exists(1, "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".to_owned()), before[1].clone()],
            )
            .unwrap(),
            ReconciliationDecision::Unknown
        );
        assert_eq!(
            classify_observations(
                MutationIntentState::Applying,
                &operations,
                &before,
            )
            .unwrap(),
            ReconciliationDecision::Unknown
        );
    }
}
