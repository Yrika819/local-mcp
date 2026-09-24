use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::config;
use crate::goal::{GOAL_SCHEMA_VERSION, GOAL_STORE_FORMAT, Goal, GoalId};
use crate::orchestrator_error::OrchestratorError;
use crate::secure_fs;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FaultPoint {
    BeforeTempWrite,
    BeforeReplace,
}

pub(crate) struct TaskStore {
    state_root: PathBuf,
    #[cfg(test)]
    fault: Option<FaultPoint>,
}

impl TaskStore {
    pub(crate) fn new() -> Result<Self, OrchestratorError> {
        Ok(Self {
            state_root: config::state_dir().map_err(|error| {
                OrchestratorError::PersistenceIo(std::io::Error::other(error.to_string()))
            })?,
            #[cfg(test)]
            fault: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_state_root(state_root: PathBuf) -> Self {
        Self {
            state_root,
            fault: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn goal_path_for_test(
        &self,
        session_id: &str,
        goal_id: &GoalId,
    ) -> Result<PathBuf, OrchestratorError> {
        self.goal_path(session_id, goal_id)
    }

    #[cfg(test)]
    pub(crate) fn with_fault(state_root: PathBuf, fault: FaultPoint) -> Self {
        Self {
            state_root,
            fault: Some(fault),
        }
    }

    pub(crate) fn create_goal(&self, goal: &Goal) -> Result<(), OrchestratorError> {
        self.validate_session_id(goal.session_id())?;
        if goal.revision() != 1 {
            return Err(OrchestratorError::CorruptGoal(
                "new goal must begin at revision 1".to_owned(),
            ));
        }
        goal.validate()?;
        self.with_session_lock(goal.session_id(), || {
            let existing = self.load_all_unlocked(goal.session_id())?;
            if existing.iter().any(|candidate| !candidate.is_terminal()) {
                return Err(OrchestratorError::ActiveGoalAlreadyExists);
            }
            let path = self.goal_path(goal.session_id(), goal.id())?;
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    return Err(OrchestratorError::CorruptGoal(
                        "goal ID already exists".to_owned(),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            self.commit_goal_unlocked(goal)
        })
    }

    pub(crate) fn load_goal(
        &self,
        session_id: &str,
        goal_id: &GoalId,
    ) -> Result<Goal, OrchestratorError> {
        self.validate_session_id(session_id)?;
        goal_id_validate(goal_id)?;
        self.with_session_lock(session_id, || self.load_goal_unlocked(session_id, goal_id))
    }

    pub(crate) fn list_goals_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<Goal>, OrchestratorError> {
        self.validate_session_id(session_id)?;
        self.with_session_lock(session_id, || self.load_all_unlocked(session_id))
    }

    pub(crate) fn load_active_goal(
        &self,
        session_id: &str,
    ) -> Result<Option<Goal>, OrchestratorError> {
        let goals = self.list_goals_for_session(session_id)?;
        let active = goals
            .into_iter()
            .filter(|goal| !goal.is_terminal())
            .collect::<Vec<_>>();
        match active.len() {
            0 => Ok(None),
            1 => Ok(active.into_iter().next()),
            _ => Err(OrchestratorError::CorruptGoal(
                "more than one non-terminal Goal exists for the session".to_owned(),
            )),
        }
    }

    #[expect(
        dead_code,
        reason = "Frozen transactional mutation entrypoint is retained for staged orchestrator integration."
    )]
    pub(crate) fn mutate_goal<T, F>(
        &self,
        session_id: &str,
        goal_id: &GoalId,
        expected_revision: u64,
        mutate: F,
    ) -> Result<T, OrchestratorError>
    where
        F: FnOnce(&mut Goal, &str) -> Result<T, OrchestratorError>,
    {
        self.validate_session_id(session_id)?;
        goal_id_validate(goal_id)?;
        self.with_session_lock(session_id, || {
            let mut goal = self.load_goal_unlocked(session_id, goal_id)?;
            if goal.schema_version() == 1 {
                return Err(OrchestratorError::SchemaUpgradeRequired(1));
            }
            if goal.revision() != expected_revision {
                return Err(OrchestratorError::RevisionConflict {
                    expected: expected_revision,
                    actual: goal.revision(),
                });
            }
            let now = utc_now_rfc3339();
            let result = mutate(&mut goal, &now)?;
            let next_revision = goal.revision().checked_add(1).ok_or_else(|| {
                OrchestratorError::CorruptGoal("goal revision overflow".to_owned())
            })?;
            goal.set_committed_revision(next_revision, &now);
            goal.validate()?;
            self.commit_goal_unlocked(&goal)?;
            Ok(result)
        })
    }

    pub(crate) fn mutate_goal_snapshot<F>(
        &self,
        session_id: &str,
        goal_id: &GoalId,
        expected_revision: u64,
        mutate: F,
    ) -> Result<Goal, OrchestratorError>
    where
        F: FnOnce(&mut Goal, &str) -> Result<(), OrchestratorError>,
    {
        self.validate_session_id(session_id)?;
        goal_id_validate(goal_id)?;
        self.with_session_lock(session_id, || {
            let mut goal = self.load_goal_unlocked(session_id, goal_id)?;
            if goal.schema_version() == 1 {
                return Err(OrchestratorError::SchemaUpgradeRequired(1));
            }
            if goal.revision() != expected_revision {
                return Err(OrchestratorError::RevisionConflict {
                    expected: expected_revision,
                    actual: goal.revision(),
                });
            }
            let before = goal.clone();
            let now = utc_now_rfc3339();
            mutate(&mut goal, &now)?;
            if goal == before {
                return Ok(goal);
            }
            let next_revision = goal.revision().checked_add(1).ok_or_else(|| {
                OrchestratorError::CorruptGoal("goal revision overflow".to_owned())
            })?;
            goal.set_committed_revision(next_revision, &now);
            goal.validate()?;
            self.commit_goal_unlocked(&goal)?;
            Ok(goal)
        })
    }

    #[expect(
        dead_code,
        reason = "Frozen recovery entrypoint is retained for staged orchestrator integration."
    )]
    pub(crate) fn recover_goal(
        &self,
        session_id: &str,
        goal_id: &GoalId,
        expected_revision: u64,
    ) -> Result<Goal, OrchestratorError> {
        self.validate_session_id(session_id)?;
        goal_id_validate(goal_id)?;
        self.with_session_lock(session_id, || {
            let mut goal = self.load_goal_unlocked(session_id, goal_id)?;
            if goal.schema_version() == 1 {
                return Err(OrchestratorError::SchemaUpgradeRequired(1));
            }
            if goal.revision() != expected_revision {
                return Err(OrchestratorError::RevisionConflict {
                    expected: expected_revision,
                    actual: goal.revision(),
                });
            }
            let now = utc_now_rfc3339();
            if !goal.recover_stale_running(&now)? {
                return Ok(goal);
            }
            let next_revision = goal.revision().checked_add(1).ok_or_else(|| {
                OrchestratorError::CorruptGoal("goal revision overflow".to_owned())
            })?;
            goal.set_committed_revision(next_revision, &now);
            goal.validate()?;
            self.commit_goal_unlocked(&goal)?;
            Ok(goal)
        })
    }

    fn load_all_unlocked(&self, session_id: &str) -> Result<Vec<Goal>, OrchestratorError> {
        let directory = self.session_goal_dir(session_id)?;
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                if !metadata.file_type().is_file() {
                    return Err(OrchestratorError::PersistenceIo(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "goal JSON entry is not a regular file",
                    )));
                }
                paths.push(path);
            }
        }
        paths.sort();
        let mut goals = Vec::with_capacity(paths.len());
        for path in paths {
            let goal = self.load_path_unlocked(&path)?;
            if goal.session_id() != session_id {
                return Err(OrchestratorError::CorruptGoal(
                    "goal session binding does not match containing directory".to_owned(),
                ));
            }
            goals.push(goal);
        }
        Ok(goals)
    }

    fn load_goal_unlocked(
        &self,
        session_id: &str,
        goal_id: &GoalId,
    ) -> Result<Goal, OrchestratorError> {
        let path = self.goal_path(session_id, goal_id)?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => {
                return Err(OrchestratorError::PersistenceIo(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "goal path is not a regular file",
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(OrchestratorError::GoalNotFound);
            }
            Err(error) => return Err(error.into()),
        }
        let goal = self.load_path_unlocked(&path)?;
        if goal.session_id() != session_id || goal.id() != goal_id {
            return Err(OrchestratorError::CorruptGoal(
                "goal identity does not match durable path".to_owned(),
            ));
        }
        Ok(goal)
    }

    fn load_path_unlocked(&self, path: &Path) -> Result<Goal, OrchestratorError> {
        let mut bytes = Vec::new();
        let mut file = secure_fs::open_private_existing(path, false)?;
        file.read_to_end(&mut bytes)?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
            OrchestratorError::CorruptGoal("authoritative Goal JSON does not parse".to_owned())
        })?;
        let object = value.as_object().ok_or_else(|| {
            OrchestratorError::CorruptGoal("authoritative Goal JSON is not an object".to_owned())
        })?;
        match object.get("store_format").and_then(Value::as_str) {
            Some(GOAL_STORE_FORMAT) => {}
            _ => {
                return Err(OrchestratorError::CorruptGoal(
                    "missing or unsupported store_format".to_owned(),
                ));
            }
        }
        let schema_version = object
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| OrchestratorError::CorruptGoal("missing schema_version".to_owned()))?;
        if schema_version == 1 {
            let status = object
                .get("status")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    OrchestratorError::CorruptGoal(
                        "legacy schema-1 Goal is missing status".to_owned(),
                    )
                })?;
            if !matches!(status, "COMPLETED" | "FAILED" | "CANCELLED") {
                return Err(OrchestratorError::SchemaUpgradeRequired(1));
            }
            let mut legacy = value;
            let legacy_object = legacy.as_object_mut().expect("validated object");
            let criteria = legacy_object
                .remove("completion_criteria")
                .unwrap_or_else(|| Value::Array(Vec::new()));
            legacy_object.insert("legacy_completion_criteria".to_owned(), criteria);
            legacy_object.insert("completion_criteria".to_owned(), Value::Array(Vec::new()));
            let old_final = legacy_object
                .remove("final_verification")
                .unwrap_or(Value::Null);
            legacy_object.insert("legacy_final_verification".to_owned(), old_final);
            legacy_object.insert("final_verifications".to_owned(), Value::Array(Vec::new()));
            legacy_object.insert("final_verification_spec".to_owned(), Value::Null);
            let goal: Goal = serde_json::from_value(legacy).map_err(|error| {
                OrchestratorError::CorruptGoal(format!(
                    "legacy durable Goal shape is invalid: {error}"
                ))
            })?;
            goal.validate()?;
            return Ok(goal);
        }
        if schema_version == 2 {
            let mut migrated = value;
            let migrated_object = migrated.as_object_mut().expect("validated object");
            migrated_object.insert(
                "schema_version".to_owned(),
                Value::from(GOAL_SCHEMA_VERSION),
            );
            migrated_object.insert(
                "pristine_plan_supersessions".to_owned(),
                Value::Array(Vec::new()),
            );
            let goal: Goal = serde_json::from_value(migrated).map_err(|error| {
                OrchestratorError::CorruptGoal(format!(
                    "schema-2 durable Goal shape is invalid: {error}"
                ))
            })?;
            goal.validate()?;
            return Ok(goal);
        }
        if schema_version != GOAL_SCHEMA_VERSION as u64 {
            return Err(OrchestratorError::UnsupportedSchema(schema_version));
        }
        let goal: Goal = serde_json::from_value(value).map_err(|error| {
            OrchestratorError::CorruptGoal(format!("durable Goal shape is invalid: {error}"))
        })?;
        goal.validate()?;
        Ok(goal)
    }

    fn commit_goal_unlocked(&self, goal: &Goal) -> Result<(), OrchestratorError> {
        goal.validate()?;
        let path = self.goal_path(goal.session_id(), goal.id())?;
        let goals_root = self.state_root.join("goals");
        let directory = path.parent().expect("goal path always has parent");
        secure_fs::ensure_private_directory(&goals_root, Some(&self.state_root))?;
        secure_fs::ensure_private_directory(directory, Some(&goals_root))?;
        match secure_fs::open_private_existing(&path, false) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let bytes = serde_json::to_vec_pretty(goal)?;

        #[cfg(test)]
        if self.fault == Some(FaultPoint::BeforeTempWrite) {
            return Err(OrchestratorError::PersistenceIo(std::io::Error::other(
                "injected temp write failure",
            )));
        }

        let (temporary, mut file) = secure_fs::create_unique_temp(&path)?;
        let write_result = (|| -> Result<(), OrchestratorError> {
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()?;
            Ok(())
        })();
        drop(file);
        if let Err(error) = write_result {
            let _ = secure_fs::remove_private_temp(&temporary);
            return Err(error);
        }

        #[cfg(test)]
        if self.fault == Some(FaultPoint::BeforeReplace) {
            let _ = secure_fs::remove_private_temp(&temporary);
            return Err(OrchestratorError::PersistenceIo(std::io::Error::other(
                "injected atomic replace failure",
            )));
        }

        if let Err(error) = secure_fs::atomic_replace(&temporary, &path) {
            let _ = secure_fs::remove_private_temp(&temporary);
            return Err(error.into());
        }
        if let Err(error) = secure_fs::sync_parent_directory(directory) {
            let _ = secure_fs::remove_private_temp(&temporary);
            return Err(error.into());
        }
        Ok(())
    }

    fn with_session_lock<T, F>(
        &self,
        session_id: &str,
        operation: F,
    ) -> Result<T, OrchestratorError>
    where
        F: FnOnce() -> Result<T, OrchestratorError>,
    {
        let directory = self.session_goal_dir(session_id)?;
        let goals_root = self.state_root.join("goals");
        secure_fs::ensure_private_directory(&self.state_root, None)?;
        secure_fs::ensure_private_directory(&goals_root, Some(&self.state_root))?;
        secure_fs::ensure_private_directory(&directory, Some(&goals_root))?;
        let lock_path = directory.join(".lock");
        let lock = secure_fs::create_or_open_lock(&lock_path)?;
        lock.lock()?;
        let result = operation();
        let unlock_result = lock.unlock();
        match (result, unlock_result) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(OrchestratorError::PersistenceIo(error)),
            (Ok(value), Ok(())) => Ok(value),
        }
    }

    fn validate_session_id(&self, session_id: &str) -> Result<(), OrchestratorError> {
        config::validate_session_id(session_id)
            .map_err(|_| OrchestratorError::UnsafeIdentifier("session".to_owned()))
    }

    fn session_goal_dir(&self, session_id: &str) -> Result<PathBuf, OrchestratorError> {
        self.validate_session_id(session_id)?;
        Ok(self.state_root.join("goals").join(session_id))
    }

    fn goal_path(&self, session_id: &str, goal_id: &GoalId) -> Result<PathBuf, OrchestratorError> {
        goal_id_validate(goal_id)?;
        Ok(self
            .session_goal_dir(session_id)?
            .join(format!("{}.json", goal_id.as_str())))
    }
}

fn goal_id_validate(goal_id: &GoalId) -> Result<(), OrchestratorError> {
    let canonical = GoalId::parse(goal_id.as_str())?;
    if canonical != *goal_id {
        return Err(OrchestratorError::UnsafeIdentifier("GoalId".to_owned()));
    }
    Ok(())
}

pub(crate) fn utc_now_rfc3339() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    format_unix_seconds(seconds)
}

fn format_unix_seconds(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fallback::{SideEffectClass, SideEffectState};
    use crate::goal::GoalStatus;
    use crate::task::{
        ReplaySafety, TaskEvidence, TaskOperationKind, TaskScope, TaskStatus,
        TaskTransitionContext, WorkerKind,
    };
    use std::fs::OpenOptions;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use uuid::Uuid;

    const NOW: &str = "2026-01-01T00:00:00Z";

    fn state_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("local-mcp-phase2-store-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[cfg(unix)]
    #[test]
    fn task_store_bootstraps_missing_state_base_without_private_chmod() {
        let root = state_root();
        let _cleanup = StateRootCleanup(root.clone());
        let reference = root.join("reference");
        std::fs::create_dir_all(&reference).unwrap();
        let reference_mode = mode(&reference);
        std::fs::remove_dir(&reference).unwrap();
        let missing_base = root.join("missing-base");
        let state_root = missing_base.join("state");
        let store = TaskStore::with_state_root(state_root.clone());
        let session = format!("bootstrap-session-{}", Uuid::new_v4());
        let goal = goal(&session);
        let goal_id = goal.id().clone();

        store.create_goal(&goal).unwrap();

        let goals_root = state_root.join("goals");
        let session_dir = store.session_goal_dir(&session).unwrap();
        assert_eq!(mode(&missing_base), reference_mode);
        assert_eq!(mode(&state_root), 0o700);
        assert_eq!(mode(&goals_root), 0o700);
        assert_eq!(mode(&session_dir), 0o700);
        assert_eq!(mode(&store.goal_path(&session, &goal_id).unwrap()), 0o600);
    }

    #[cfg(unix)]
    struct StateRootCleanup(PathBuf);

    #[cfg(unix)]
    impl Drop for StateRootCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn assert_modes(phase: &str, checks: &[(&str, &std::path::Path, u32, u32)]) {
        let failures = checks
            .iter()
            .filter(|(_, _, expected, actual)| expected != actual)
            .map(|(label, path, expected, actual)| {
                format!(
                    "{phase} {label} {}: actual {:04o}, expected {:04o}",
                    path.display(),
                    actual,
                    expected
                )
            })
            .collect::<Vec<_>>();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    fn goal(session: &str) -> Goal {
        Goal::new(
            session,
            PathBuf::from("/tmp/project"),
            "durable objective",
            None,
            vec![],
            vec![],
            NOW,
        )
        .unwrap()
    }

    fn read_only_scope() -> TaskScope {
        TaskScope::new(
            vec![PathBuf::from("src")],
            vec![],
            TaskOperationKind::ReadOnly,
            ReplaySafety::SafeReadOnly,
        )
    }

    fn mutation_scope() -> TaskScope {
        TaskScope::new(
            vec![PathBuf::from("src")],
            vec![],
            TaskOperationKind::LocalMutation,
            ReplaySafety::VerifyBeforeRetry,
        )
    }

    fn make_running_task(
        goal: &mut Goal,
        scope: TaskScope,
        worker: WorkerKind,
    ) -> crate::task::TaskId {
        let id = goal
            .add_task(
                "task",
                "task objective",
                true,
                worker,
                scope,
                vec![],
                3,
                NOW,
            )
            .unwrap();
        goal.transition_to(GoalStatus::Running, NOW).unwrap();
        goal.transition_task(
            &id,
            TaskStatus::Ready,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.transition_task(
            &id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        id
    }

    #[test]
    fn rfc3339_formatter_has_known_epoch() {
        assert_eq!(format_unix_seconds(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_unix_seconds(86_400), "1970-01-02T00:00:00Z");
    }

    #[test]
    fn create_persist_reload_survives_new_store_instance() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        let goal = goal("session-a");
        store.create_goal(&goal).unwrap();
        drop(store);
        let reloaded_store = TaskStore::with_state_root(root.clone());
        let loaded = reloaded_store.load_goal("session-a", goal.id()).unwrap();
        assert_eq!(loaded.id(), goal.id());
        assert_eq!(loaded.revision(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn task_store_initial_state_modes_are_private() {
        let root = state_root();
        let _cleanup = StateRootCleanup(root.clone());
        let store = TaskStore::with_state_root(root.clone());
        let session = format!("permission-session-{}", Uuid::new_v4());
        let goal = goal(&session);
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let goals_root = root.join("goals");
        let goal_dir = store.session_goal_dir(&session).unwrap();
        let goal_path = store.goal_path(&session, &goal_id).unwrap();
        let lock_path = goal_dir.join(".lock");
        let modes = [
            ("state root", root.as_path(), 0o700, mode(&root)),
            ("goals root", goals_root.as_path(), 0o700, mode(&goals_root)),
            ("goal directory", goal_dir.as_path(), 0o700, mode(&goal_dir)),
            ("Goal JSON", goal_path.as_path(), 0o600, mode(&goal_path)),
            ("lock file", lock_path.as_path(), 0o600, mode(&lock_path)),
        ];
        assert_modes("initial state", &modes);
    }

    #[cfg(unix)]
    #[test]
    fn task_store_normal_operations_repair_permissive_modes() {
        let root = state_root();
        let _cleanup = StateRootCleanup(root.clone());
        let store = TaskStore::with_state_root(root.clone());
        let session = format!("permission-session-{}", Uuid::new_v4());
        let mut initial = goal(&session);
        initial.transition_to(GoalStatus::Failed, NOW).unwrap();
        let initial_id = initial.id().clone();
        store.create_goal(&initial).unwrap();
        let next = goal(&session);
        let next_id = next.id().clone();
        let goals_root = root.join("goals");
        let goal_dir = store.session_goal_dir(&session).unwrap();
        let initial_path = store.goal_path(&session, &initial_id).unwrap();
        let next_path = store.goal_path(&session, &next_id).unwrap();
        let lock_path = goal_dir.join(".lock");

        for path in [&root, &goals_root, &goal_dir] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777)).unwrap();
        }
        for path in [&initial_path, &lock_path] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)).unwrap();
        }
        let lock_before = std::fs::metadata(&lock_path).unwrap();
        let lock_identity = (lock_before.dev(), lock_before.ino());

        store.load_goal(&session, &initial_id).unwrap();
        assert_eq!(store.list_goals_for_session(&session).unwrap().len(), 1);
        store.create_goal(&next).unwrap();
        store.load_goal(&session, &next_id).unwrap();

        let lock_after = std::fs::metadata(&lock_path).unwrap();
        assert_eq!(
            (lock_after.dev(), lock_after.ino()),
            lock_identity,
            "normal operations replaced the session lock inode"
        );
        let modes = [
            ("state root", root.as_path(), 0o700, mode(&root)),
            ("goals root", goals_root.as_path(), 0o700, mode(&goals_root)),
            ("goal directory", goal_dir.as_path(), 0o700, mode(&goal_dir)),
            (
                "initial Goal JSON",
                initial_path.as_path(),
                0o600,
                mode(&initial_path),
            ),
            (
                "created Goal JSON",
                next_path.as_path(),
                0o600,
                mode(&next_path),
            ),
            ("lock file", lock_path.as_path(), 0o600, mode(&lock_path)),
        ];
        assert_modes("normal operations", &modes);
    }

    #[cfg(unix)]
    #[test]
    fn goal_enumeration_rejects_json_symlink_without_reading_target() {
        let root = state_root();
        let _cleanup = StateRootCleanup(root.clone());
        let store = TaskStore::with_state_root(root.clone());
        let session = format!("symlink-session-{}", Uuid::new_v4());
        let goal = goal(&session);
        store.create_goal(&goal).unwrap();
        let outside = root.join("outside");
        let sentinel = outside.join("sentinel");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(&sentinel, b"unchanged").unwrap();
        let directory = store.session_goal_dir(&session).unwrap();
        std::os::unix::fs::symlink(&sentinel, directory.join("foreign.json")).unwrap();

        assert!(store.list_goals_for_session(&session).is_err());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
    }

    #[test]
    fn update_advances_revision_exactly_once_and_reloads() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        let goal = goal("session-b");
        let id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        store
            .mutate_goal("session-b", &id, 1, |goal, now| {
                goal.add_task(
                    "task",
                    "objective",
                    true,
                    WorkerKind::CodexReadonly,
                    read_only_scope(),
                    vec![],
                    2,
                    now,
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(store.load_goal("session-b", &id).unwrap().revision(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_revision_is_rejected_without_overwrite() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        let goal = goal("session-c");
        let id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        store
            .mutate_goal("session-c", &id, 1, |_goal, _now| Ok(()))
            .unwrap();
        let error = store
            .mutate_goal("session-c", &id, 1, |_goal, _now| Ok(()))
            .unwrap_err();
        assert!(matches!(
            error,
            OrchestratorError::RevisionConflict {
                expected: 1,
                actual: 2
            }
        ));
        assert_eq!(store.load_goal("session-c", &id).unwrap().revision(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn second_active_goal_is_rejected_but_terminal_history_can_coexist() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        let active = goal("session-d");
        store.create_goal(&active).unwrap();
        assert!(matches!(
            store.create_goal(&goal("session-d")),
            Err(OrchestratorError::ActiveGoalAlreadyExists)
        ));

        let other_session = "session-e";
        let mut historical = goal(other_session);
        historical.transition_to(GoalStatus::Failed, NOW).unwrap();
        store.create_goal(&historical).unwrap();
        let next = goal(other_session);
        store.create_goal(&next).unwrap();
        assert_eq!(
            store.list_goals_for_session(other_session).unwrap().len(),
            2
        );
        assert_eq!(
            store.load_active_goal(other_session).unwrap().unwrap().id(),
            next.id()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_json_is_rejected_and_preserved() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        let goal = goal("session-f");
        let path = store.goal_path("session-f", goal.id()).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not-json").unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            store.load_goal("session-f", goal.id()),
            Err(OrchestratorError::CorruptGoal(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsupported_and_missing_schema_are_rejected() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        for (session, schema, expected_unsupported) in [
            ("schema-old", Some(0_u64), true),
            ("schema-new", Some((GOAL_SCHEMA_VERSION + 1) as u64), true),
            ("schema-missing", None, false),
        ] {
            let goal = goal(session);
            let path = store.goal_path(session, goal.id()).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut value = serde_json::to_value(&goal).unwrap();
            match schema {
                Some(version) => value["schema_version"] = Value::from(version),
                None => {
                    value.as_object_mut().unwrap().remove("schema_version");
                }
            }
            std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
            let error = store.load_goal(session, goal.id()).unwrap_err();
            if expected_unsupported {
                assert!(matches!(error, OrchestratorError::UnsupportedSchema(_)));
            } else {
                assert!(matches!(error, OrchestratorError::CorruptGoal(_)));
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema_two_loads_as_in_memory_schema_three_without_rewriting() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        let goal = goal("schema-two-migration");
        let path = store.goal_path("schema-two-migration", goal.id()).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut value = serde_json::to_value(&goal).unwrap();
        value["schema_version"] = Value::from(2_u64);
        value
            .as_object_mut()
            .unwrap()
            .remove("pristine_plan_supersessions");
        let bytes = serde_json::to_vec_pretty(&value).unwrap();
        std::fs::write(&path, &bytes).unwrap();

        let loaded = store.load_goal("schema-two-migration", goal.id()).unwrap();
        assert_eq!(loaded.schema_version(), GOAL_SCHEMA_VERSION);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        let updated = store
            .mutate_goal_snapshot(
                "schema-two-migration",
                goal.id(),
                loaded.revision(),
                |goal, _| {
                    goal.add_blocker(crate::goal::GoalBlocker::new(
                        "MIGRATION",
                        "persist v3",
                        false,
                    ))
                },
            )
            .unwrap();
        assert_eq!(updated.schema_version(), GOAL_SCHEMA_VERSION);
        let persisted: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            persisted["schema_version"],
            Value::from(GOAL_SCHEMA_VERSION)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsafe_identifiers_and_missing_goal_are_rejected() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        let goal_id = GoalId::new();
        for unsafe_id in [
            "",
            "..",
            "../escape",
            "/absolute",
            "slash/name",
            "ユニコード",
        ] {
            assert!(matches!(
                store.load_goal(unsafe_id, &goal_id),
                Err(OrchestratorError::UnsafeIdentifier(_))
            ));
        }
        assert!(GoalId::parse("../escape").is_err());
        assert!(matches!(
            store.load_goal("safe-session", &goal_id),
            Err(OrchestratorError::GoalNotFound)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn successful_save_leaves_no_temp_file() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        let goal = goal("session-g");
        store.create_goal(&goal).unwrap();
        let directory = store.session_goal_dir("session-g").unwrap();
        let leaked = std::fs::read_dir(directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!leaked);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn injected_write_and_replace_failures_preserve_authoritative_final() {
        let root = state_root();
        let normal = TaskStore::with_state_root(root.clone());
        let goal = goal("session-h");
        let id = goal.id().clone();
        normal.create_goal(&goal).unwrap();
        let final_path = normal.goal_path("session-h", &id).unwrap();
        let before = std::fs::read(&final_path).unwrap();

        for fault in [FaultPoint::BeforeTempWrite, FaultPoint::BeforeReplace] {
            let failing = TaskStore::with_fault(root.clone(), fault);
            assert!(
                failing
                    .mutate_goal("session-h", &id, 1, |_goal, _now| Ok(()))
                    .is_err()
            );
            assert_eq!(std::fs::read(&final_path).unwrap(), before);
            assert_eq!(normal.load_goal("session-h", &id).unwrap().revision(), 1);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_structural_mutation_preserves_authoritative_revision_and_bytes() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        let mut goal = goal("session-transactional");
        let a = goal
            .add_task(
                "a",
                "a objective",
                true,
                WorkerKind::CodexReadonly,
                read_only_scope(),
                vec![],
                2,
                NOW,
            )
            .unwrap();
        let b = goal
            .add_task(
                "b",
                "b objective",
                true,
                WorkerKind::CodexReadonly,
                read_only_scope(),
                vec![],
                2,
                NOW,
            )
            .unwrap();
        goal.strengthen_task_dependencies(&b, vec![a.clone()])
            .unwrap();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let final_path = store.goal_path("session-transactional", &goal_id).unwrap();
        let before_bytes = std::fs::read(&final_path).unwrap();

        let error = store
            .mutate_goal("session-transactional", &goal_id, 1, |goal, _now| {
                goal.strengthen_task_dependencies(&a, vec![b.clone()])?;
                Ok(())
            })
            .unwrap_err();
        assert!(matches!(error, OrchestratorError::InvalidDag(_)));

        let reloaded = store.load_goal("session-transactional", &goal_id).unwrap();
        assert_eq!(reloaded, goal);
        assert_eq!(reloaded.revision(), 1);
        assert_eq!(std::fs::read(&final_path).unwrap(), before_bytes);
        let directory = store.session_goal_dir("session-transactional").unwrap();
        let leaked = std::fs::read_dir(directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!leaked);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_readonly_running_recovers_durably_to_retryable() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        let mut goal = goal("session-i");
        let task_id = make_running_task(&mut goal, read_only_scope(), WorkerKind::CodexReadonly);
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let recovered = store.recover_goal("session-i", &goal_id, 1).unwrap();
        assert_eq!(recovered.revision(), 2);
        assert_eq!(
            recovered.tasks().get(&task_id).unwrap().status(),
            TaskStatus::Retryable
        );
        assert_eq!(
            store
                .load_goal("session-i", &goal_id)
                .unwrap()
                .tasks()
                .get(&task_id)
                .unwrap()
                .status(),
            TaskStatus::Retryable
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_unknown_mutation_recovers_durably_to_blocked() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        let mut goal = goal("session-j");
        let task_id = make_running_task(&mut goal, mutation_scope(), WorkerKind::LocalOperation);
        goal.task_bind_latest_attempt_execution(
            &task_id,
            Some("operation-1".into()),
            Some("scope-1".into()),
            Some("request-1".into()),
            Some(SideEffectClass::LocalMutation),
            Some(SideEffectState::Unknown),
            Some(1),
            Some(1),
        )
        .unwrap();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let recovered = store.recover_goal("session-j", &goal_id, 1).unwrap();
        assert_eq!(recovered.status(), GoalStatus::Blocked);
        assert_eq!(
            recovered.tasks().get(&task_id).unwrap().status(),
            TaskStatus::Blocked
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn independently_proven_mutation_recovers_to_verifying_never_completed() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        let mut goal = goal("session-k");
        let task_id = make_running_task(&mut goal, mutation_scope(), WorkerKind::LocalOperation);
        goal.task_bind_latest_attempt_execution(
            &task_id,
            Some("operation-2".into()),
            Some("scope-2".into()),
            Some("request-2".into()),
            Some(SideEffectClass::LocalMutation),
            Some(SideEffectState::ConfirmedPerformed),
            Some(0),
            Some(0),
        )
        .unwrap();
        goal.task_add_evidence(
            &task_id,
            TaskEvidence::RecoveryReconciliation {
                summary: "independent postcondition proof".into(),
                side_effect_state: SideEffectState::ConfirmedPerformed,
                postcondition_proven: true,
            },
        )
        .unwrap();
        let goal_id = goal.id().clone();
        store.create_goal(&goal).unwrap();
        let recovered = store.recover_goal("session-k", &goal_id, 1).unwrap();
        assert_eq!(
            recovered.tasks().get(&task_id).unwrap().status(),
            TaskStatus::Verifying
        );
        assert_ne!(
            recovered.tasks().get(&task_id).unwrap().status(),
            TaskStatus::Completed
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn process_lock_is_os_backed_and_exclusive() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        let directory = store.session_goal_dir("lock-session").unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(".lock");
        let first = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let second = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        first.lock().unwrap();
        assert!(second.try_lock().is_err());
        first.unlock().unwrap();
        second.try_lock().unwrap();
        second.unlock().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deterministic_serialization_is_stable_for_same_snapshot() {
        let goal = goal("session-l");
        let first = serde_json::to_vec_pretty(&goal).unwrap();
        let second = serde_json::to_vec_pretty(&goal).unwrap();
        assert_eq!(first, second);
    }

    /// Regression (transport concurrency): when two dispatch tasks race the
    /// same Goal with the same expected revision, the OS-backed session lock
    /// plus the optimistic revision check must guarantee that exactly one
    /// mutation commits, the other fails deterministically with
    /// `RevisionConflict`, and the durable revision advances exactly once.
    #[test]
    fn concurrent_same_goal_mutations_are_serialized_without_duplication() {
        let root = state_root();
        let store = TaskStore::with_state_root(root.clone());
        let goal = goal("session-concurrent");
        let goal_id = goal.id().clone();
        let base_revision = goal.revision();
        store.create_goal(&goal).unwrap();

        let outcomes: Vec<Result<Goal, OrchestratorError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        store.mutate_goal_snapshot(
                            "session-concurrent",
                            &goal_id,
                            base_revision,
                            |goal, now| {
                                goal.add_checkpoint(crate::goal::CheckpointReason::Recovery, now)
                            },
                        )
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });

        let applied = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
        let conflicts = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Err(OrchestratorError::RevisionConflict { .. })))
            .count();
        assert_eq!(applied, 1, "exactly one racing mutation may commit");
        assert_eq!(conflicts, 1, "the loser must fail with RevisionConflict");

        let durable = store.load_goal("session-concurrent", &goal_id).unwrap();
        assert_eq!(
            durable.revision(),
            base_revision + 1,
            "durable revision must advance exactly once: no duplicate durable mutation"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
