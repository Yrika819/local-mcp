//! Pure derivation of writer-mutated workspace paths from durable Goal history.
//!
//! The durable mutation intent is the authority. Worker reports and current Git
//! state cannot replace it. This module performs no filesystem or Git I/O.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use crate::goal::Goal;
use crate::managed_worktree::ManagedWorktreeLifecycle;
use crate::mutation::MutationIntentState;
use crate::orchestrator_error::OrchestratorError;
use crate::task::Task;
use crate::workspace_snapshot::{MAX_WRITER_MUTATED_PATHS, NormalizedWorkspacePath};

/// Derive the deterministic, bounded path set from every Task and every
/// historical Attempt in a managed Goal.
#[expect(
    dead_code,
    reason = "Consumed by managed snapshot persistence in Slice 3C."
)]
pub(crate) fn derive_writer_mutated_paths(
    goal: &Goal,
) -> Result<Vec<NormalizedWorkspacePath>, OrchestratorError> {
    goal.validate()?;
    let record = goal.managed_worktree().ok_or_else(|| {
        corrupt("writer path derivation requires durable managed-worktree authority")
    })?;
    record.validate()?;
    if record.lifecycle() != ManagedWorktreeLifecycle::Active {
        return Err(corrupt(
            "managed worktree is not active for path derivation",
        ));
    }

    derive_paths_from_tasks(goal.tasks().values(), record.worktree_root())
}

/// Derive over every supplied Task. Callers must pass the complete durable Goal
/// task collection; task status is deliberately not an authority filter here.
fn derive_paths_from_tasks<'a>(
    tasks: impl IntoIterator<Item = &'a Task>,
    worktree_root: &Path,
) -> Result<Vec<NormalizedWorkspacePath>, OrchestratorError> {
    let mut paths = BTreeSet::new();
    for task in tasks {
        for attempt in task.attempts() {
            let Some(intent) = attempt.mutation_intent() else {
                continue;
            };
            intent.validate()?;
            match intent.state() {
                MutationIntentState::Applied | MutationIntentState::ReconciledPerformed => {
                    for operation in intent.operations() {
                        let path = normalize_durable_writer_path(operation.path(), worktree_root)?;
                        if paths.insert(path) && paths.len() > MAX_WRITER_MUTATED_PATHS {
                            return Err(corrupt("writer-mutated path limit exceeded"));
                        }
                    }
                }
                MutationIntentState::ReconciledNotPerformed => {}
                MutationIntentState::Prepared | MutationIntentState::Applying => {
                    return Err(corrupt(
                        "unresolved writer mutation intent prevents complete path derivation",
                    ));
                }
                MutationIntentState::Partial | MutationIntentState::Unknown => {
                    return Err(corrupt(
                        "unknown or partial writer mutation prevents complete path derivation",
                    ));
                }
            }
        }
    }
    Ok(paths.into_iter().collect())
}

/// Convert one already validated durable absolute writer path to the logical
/// workspace-relative identity used by snapshot evidence.
fn normalize_durable_writer_path(
    path: &Path,
    worktree_root: &Path,
) -> Result<NormalizedWorkspacePath, OrchestratorError> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(corrupt("durable writer path is not absolute"));
    }
    ensure_canonical_spelling(path, "durable writer path")?;
    let path = normalize_writer_root_spelling(path, worktree_root)?;
    let relative = path
        .strip_prefix(worktree_root)
        .map_err(|_| corrupt("durable writer path is outside the exact managed workspace root"))?;
    if relative.as_os_str().is_empty() {
        return Err(corrupt("writer operation cannot target the workspace root"));
    }

    let mut components = Vec::new();
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err(corrupt("writer path contains a malformed component"));
        };
        let text = value
            .to_str()
            .ok_or_else(|| corrupt("writer path contains non-UTF-8 data"))?;
        if text.is_empty()
            || text == "."
            || text == ".."
            || text.contains(['/', '\\', '\0', ':'])
            || text.ends_with([' ', '.'])
        {
            return Err(corrupt("writer path contains an invalid component"));
        }
        if text.eq_ignore_ascii_case(".git") {
            return Err(corrupt("writer path targets Git administrative data"));
        }
        // Windows reserves device names and accepts verbatim/device spellings
        // with semantics that differ from ordinary path parsing. Reject these
        // components in the logical target identity on every host.
        if is_windows_device_component(text) {
            return Err(corrupt("writer path contains a Windows device component"));
        }
        components.push(text);
    }
    if components.is_empty() {
        return Err(corrupt("writer operation cannot target the workspace root"));
    }
    let normalized = components.join("/");
    NormalizedWorkspacePath::parse(normalized)
        .map_err(|error| corrupt(format!("writer path normalization failed: {error}")))
}

/// Require a lossless structural round trip. This rejects dot components,
/// repeated separators, foreign absolute spellings, and non-canonical device
/// prefixes instead of silently changing their identity.
fn ensure_canonical_spelling(path: &Path, field: &str) -> Result<(), OrchestratorError> {
    let components: Vec<_> = path.components().collect();
    if components.iter().any(|component| {
        !matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::Normal(_)
        )
    }) {
        return Err(corrupt(format!("{field} contains traversal components")));
    }
    let rebuilt: PathBuf = components.iter().collect();
    if rebuilt.as_os_str() != path.as_os_str() {
        return Err(corrupt(format!("{field} has a non-canonical spelling")));
    }
    Ok(())
}

/// Writer's `fs::canonicalize` result on Windows uses `\\?\\C:\\...`,
/// whereas managed-worktree ownership deliberately stores the ordinary
/// `C:\\...` spelling for Git compatibility. Bridge only that equivalent
/// verbatim-disk form, and only when its drive is the exact owned root's drive.
/// Other device/namespace prefixes remain untouched and therefore fail the
/// exact-root containment check. This is lexical and performs no I/O.
fn normalize_writer_root_spelling(
    path: &Path,
    worktree_root: &Path,
) -> Result<PathBuf, OrchestratorError> {
    #[cfg(windows)]
    {
        use std::path::Prefix;
        let mut components = path.components();
        if let Some(Component::Prefix(prefix)) = components.next()
            && let Prefix::VerbatimDisk(writer_drive) = prefix.kind()
        {
            let Some(Component::Prefix(root_prefix)) = worktree_root.components().next() else {
                return Err(corrupt("managed workspace root has no drive prefix"));
            };
            let Prefix::Disk(root_drive) = root_prefix.kind() else {
                return Err(corrupt(
                    "managed workspace root is not an ordinary drive path",
                ));
            };
            if !writer_drive.eq_ignore_ascii_case(&root_drive) {
                return Err(corrupt("durable writer path uses a different drive"));
            }
            let mut normalized = PathBuf::from(format!("{}:\\", root_drive as char));
            for component in components {
                if !matches!(component, Component::RootDir) {
                    normalized.push(component.as_os_str());
                }
            }
            return Ok(normalized);
        }
    }
    let _ = worktree_root;
    Ok(path.to_path_buf())
}

fn is_windows_device_component(value: &str) -> bool {
    let stem = value
        .split('.')
        .next()
        .unwrap_or(value)
        .trim_end_matches([' ', '.']);
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || (upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'))
}

fn corrupt(message: impl Into<String>) -> OrchestratorError {
    OrchestratorError::CorruptGoal(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::FailedTaskReplacementMutation;
    use crate::goal::GoalId;
    use crate::managed_worktree::ManagedWorktreeRecord;
    use crate::mutation::{
        FileObservation, MutationIntent, MutationIntentUpdate, MutationOperationIntent,
        MutationPreimage,
    };
    use crate::task::{
        ReplaySafety, Task, TaskOperationKind, TaskScope, TaskStatus, TaskTransitionContext,
        VerificationCheckResult, VerificationOutcome, VerificationResult, VerificationSpec,
        WorkerKind,
    };
    use std::collections::BTreeMap;

    const NOW: &str = "2026-10-13T00:00:00Z";
    const BASE_OID: &str = "0123456789abcdef0123456789abcdef01234567";
    const OBSERVED_OID: &str = "1123456789abcdef0123456789abcdef01234567";

    fn root() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\managed\worktree")
        } else {
            PathBuf::from("/managed/worktree")
        }
    }

    fn managed_goal() -> (Goal, crate::task::TaskId) {
        let primary = if cfg!(windows) {
            PathBuf::from(r"C:\managed\primary")
        } else {
            PathBuf::from("/managed/primary")
        };
        let common = if cfg!(windows) {
            PathBuf::from(r"C:\managed\repo-common")
        } else {
            PathBuf::from("/managed/repo-common")
        };
        let goal_id = GoalId::parse("00000000-0000-4000-8000-000000000002").unwrap();
        let record = ManagedWorktreeRecord::requested(
            &goal_id,
            crate::managed_worktree::WorktreeId::parse("00000000-0000-4000-8000-000000000001")
                .unwrap(),
            primary.clone(),
            common,
            root(),
            BASE_OID.to_owned(),
            None,
            1,
            0,
        )
        .unwrap()
        .to_prepared()
        .unwrap()
        .to_active(OBSERVED_OID, NOW)
        .unwrap();
        let mut goal = Goal::new_managed_with_id(
            goal_id,
            "slice-3b-test",
            primary,
            "derive writer paths",
            None,
            Vec::new(),
            vec!["writer paths are durable".to_owned()],
            NOW,
            record,
        )
        .unwrap();
        let task = Task::new(
            "writer",
            "write a file",
            true,
            WorkerKind::CodexWriter,
            TaskScope::new(
                vec![root()],
                Vec::new(),
                TaskOperationKind::LocalMutation,
                ReplaySafety::VerifyBeforeRetry,
            ),
            vec![VerificationSpec::StructuredEvidence {
                requirement_id: "writer-proof".to_owned(),
            }],
            3,
            1,
            NOW,
        )
        .unwrap();
        let task_id = task.id().clone();
        goal.materialize_initial_plan(vec![task], NOW).unwrap();
        (goal, task_id)
    }

    fn managed_goal_at(
        primary: PathBuf,
        common: PathBuf,
        worktree_root: PathBuf,
    ) -> (Goal, crate::task::TaskId) {
        let goal_id = GoalId::parse("00000000-0000-4000-8000-000000000002").unwrap();
        let record = ManagedWorktreeRecord::requested(
            &goal_id,
            crate::managed_worktree::WorktreeId::parse("00000000-0000-4000-8000-000000000001")
                .unwrap(),
            primary.clone(),
            common,
            worktree_root.clone(),
            BASE_OID.to_owned(),
            None,
            1,
            0,
        )
        .unwrap()
        .to_prepared()
        .unwrap()
        .to_active(OBSERVED_OID, NOW)
        .unwrap();
        let mut goal = Goal::new_managed_with_id(
            goal_id,
            "slice-3b-test",
            primary,
            "derive writer paths",
            None,
            Vec::new(),
            vec!["writer paths are durable".to_owned()],
            NOW,
            record,
        )
        .unwrap();
        let task = Task::new(
            "writer",
            "write a file",
            true,
            WorkerKind::CodexWriter,
            TaskScope::new(
                vec![worktree_root],
                Vec::new(),
                TaskOperationKind::LocalMutation,
                ReplaySafety::VerifyBeforeRetry,
            ),
            vec![VerificationSpec::StructuredEvidence {
                requirement_id: "writer-proof".to_owned(),
            }],
            3,
            1,
            NOW,
        )
        .unwrap();
        let task_id = task.id().clone();
        goal.materialize_initial_plan(vec![task], NOW).unwrap();
        (goal, task_id)
    }

    fn intent(path: PathBuf) -> MutationIntent {
        intent_with_paths(vec![path], "operation-1")
    }

    fn intent_with_id(path: PathBuf, operation_id: &str) -> MutationIntent {
        intent_with_paths(vec![path], operation_id)
    }

    fn intent_with_paths(paths: Vec<PathBuf>, operation_id: &str) -> MutationIntent {
        MutationIntent::new(
            operation_id.to_owned(),
            "scope-1".to_owned(),
            paths
                .into_iter()
                .enumerate()
                .map(|(index, path)| {
                    MutationOperationIntent::new(
                        index as u32,
                        path,
                        MutationPreimage::Absent,
                        FileObservation::absent(),
                        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            .to_owned(),
                        format!("request-{index}"),
                    )
                })
                .collect(),
        )
        .unwrap()
    }

    fn prepare_goal_intent(goal: &mut Goal, task_id: &crate::task::TaskId, path: PathBuf) {
        prepare_goal_intent_value(goal, task_id, intent(path));
    }

    fn prepare_goal_intent_value(
        goal: &mut Goal,
        task_id: &crate::task::TaskId,
        intent: MutationIntent,
    ) {
        goal.transition_task(
            task_id,
            TaskStatus::Running,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        goal.task_prepare_latest_mutation_intent(task_id, intent)
            .unwrap();
    }

    fn apply_intent(goal: &mut Goal, task_id: &crate::task::TaskId) {
        let operation_count = goal.tasks()[task_id]
            .latest_attempt()
            .unwrap()
            .mutation_intent()
            .unwrap()
            .operations()
            .len();
        for index in 0..operation_count {
            goal.task_advance_latest_mutation_intent(
                task_id,
                "operation-1",
                MutationIntentUpdate::BeginOperation {
                    index: index as u32,
                },
            )
            .unwrap();
            goal.task_advance_latest_mutation_intent(
                task_id,
                "operation-1",
                MutationIntentUpdate::CompleteOperation {
                    index: index as u32,
                },
            )
            .unwrap();
        }
    }

    fn superseded_writer_task(path: PathBuf) -> Task {
        let mut task = Task::new(
            "historical writer",
            "perform a mutation before replacement",
            true,
            WorkerKind::CodexWriter,
            TaskScope::new(
                vec![root()],
                Vec::new(),
                TaskOperationKind::LocalMutation,
                ReplaySafety::VerifyBeforeRetry,
            ),
            vec![VerificationSpec::StructuredEvidence {
                requirement_id: "historical-writer-proof".to_owned(),
            }],
            3,
            1,
            NOW,
        )
        .unwrap();
        let context = TaskTransitionContext {
            active_worker_stopped: true,
            side_effect_reconciled: true,
        };
        task.transition_to(TaskStatus::Ready, true, context, NOW)
            .unwrap();
        task.transition_to(TaskStatus::Running, true, context, NOW)
            .unwrap();
        task.prepare_latest_mutation_intent(intent(path)).unwrap();
        task.advance_latest_mutation_intent(
            "operation-1",
            MutationIntentUpdate::BeginOperation { index: 0 },
        )
        .unwrap();
        task.advance_latest_mutation_intent(
            "operation-1",
            MutationIntentUpdate::CompleteOperation { index: 0 },
        )
        .unwrap();
        task.transition_to(TaskStatus::Verifying, true, context, NOW)
            .unwrap();
        task.transition_to(TaskStatus::Retryable, true, context, NOW)
            .unwrap();
        task.supersede_for_host(NOW).unwrap();
        task
    }

    #[test]
    fn normalizes_absolute_paths_component_wise() {
        let root = Path::new(if cfg!(windows) {
            r"C:\managed\worktree"
        } else {
            "/managed/worktree"
        });
        let target = root.join("src").join("main.rs");
        assert_eq!(
            normalize_durable_writer_path(&target, root)
                .unwrap()
                .as_str(),
            "src/main.rs"
        );
    }

    #[test]
    fn rejects_root_escape_prefix_collision_and_git_metadata() {
        let root = Path::new(if cfg!(windows) {
            r"C:\managed\worktree"
        } else {
            "/managed/worktree"
        });
        assert!(normalize_durable_writer_path(root, root).is_err());
        assert!(normalize_durable_writer_path(&root.join("..").join("outside"), root).is_err());
        assert!(
            normalize_durable_writer_path(&root.with_file_name("worktree-other").join("x"), root)
                .is_err()
        );
        assert!(normalize_durable_writer_path(&root.join(".git").join("config"), root).is_err());
        assert!(normalize_durable_writer_path(&root.join("CON.txt"), root).is_err());
        assert!(normalize_durable_writer_path(&root.join("trailing."), root).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_path_identity_without_lossy_conversion() {
        use std::os::unix::ffi::OsStringExt;
        let root = Path::new("/managed/worktree");
        let target = root.join(std::ffi::OsString::from_vec(vec![b'x', 0xff]));
        assert!(normalize_durable_writer_path(&target, root).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_canonical_writer_paths_keep_exact_managed_drive_identity() {
        let root = std::env::temp_dir().join(format!("local-mcp-path-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let managed_root = crate::config::canonical_path(&root).unwrap();
        std::fs::create_dir_all(managed_root.join("existing")).unwrap();
        let existing = managed_root.join("existing").join("file.txt");
        std::fs::write(&existing, b"existing").unwrap();
        let new_path = managed_root.join("existing").join("new.txt");

        for writer_path in [std::fs::canonicalize(&existing).unwrap(), {
            let canonical_parent = std::fs::canonicalize(new_path.parent().unwrap()).unwrap();
            canonical_parent.join(new_path.file_name().unwrap())
        }] {
            assert!(writer_path.to_string_lossy().starts_with(r"\\?\"));
            let actual = normalize_writer_root_spelling(&writer_path, &managed_root).unwrap();
            assert!(actual.starts_with(&managed_root));
            assert_eq!(
                normalize_durable_writer_path(&writer_path, &managed_root)
                    .unwrap()
                    .as_str(),
                if writer_path.ends_with("file.txt") {
                    "existing/file.txt"
                } else {
                    "existing/new.txt"
                }
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(not(windows))]
    #[test]
    fn rejects_foreign_windows_absolute_spelling() {
        assert!(
            normalize_durable_writer_path(
                Path::new(r"C:\managed\worktree\x"),
                Path::new("/managed/worktree")
            )
            .is_err()
        );
    }

    #[test]
    fn derives_applied_writer_path_and_preserves_it_after_reload() {
        let (mut goal, task_id) = managed_goal();
        prepare_goal_intent(&mut goal, &task_id, root().join("src").join("main.rs"));
        apply_intent(&mut goal, &task_id);
        let expected = vec![NormalizedWorkspacePath::parse("src/main.rs").unwrap()];
        assert_eq!(derive_writer_mutated_paths(&goal).unwrap(), expected);
        let reloaded: Goal = serde_json::from_slice(&serde_json::to_vec(&goal).unwrap()).unwrap();
        reloaded.validate().unwrap();
        assert_eq!(derive_writer_mutated_paths(&reloaded).unwrap(), expected);
    }

    #[test]
    fn multiple_operations_tasks_and_duplicates_are_sorted_and_deduplicated() {
        let (mut goal, first_task) = managed_goal();
        prepare_goal_intent_value(
            &mut goal,
            &first_task,
            intent_with_paths(
                vec![root().join("z.rs"), root().join("a.rs")],
                "operation-1",
            ),
        );
        apply_intent(&mut goal, &first_task);
        let context = TaskTransitionContext {
            active_worker_stopped: true,
            side_effect_reconciled: true,
        };
        goal.transition_task(&first_task, TaskStatus::Verifying, context, NOW)
            .unwrap();
        goal.task_record_verification_result(
            &first_task,
            VerificationResult::new(
                VerificationOutcome::Passed,
                vec![VerificationCheckResult::new(0, true, None)],
                NOW,
                NOW,
            ),
        )
        .unwrap();
        goal.transition_task(&first_task, TaskStatus::Completed, context, NOW)
            .unwrap();

        let second_task = goal
            .add_task(
                "writer two",
                "write a duplicate target",
                false,
                WorkerKind::CodexWriter,
                TaskScope::new(
                    vec![root()],
                    Vec::new(),
                    TaskOperationKind::LocalMutation,
                    ReplaySafety::VerifyBeforeRetry,
                ),
                vec![VerificationSpec::StructuredEvidence {
                    requirement_id: "writer-proof-two".to_owned(),
                }],
                2,
                NOW,
            )
            .unwrap();
        goal.transition_task(
            &second_task,
            TaskStatus::Ready,
            TaskTransitionContext::default(),
            NOW,
        )
        .unwrap();
        prepare_goal_intent(&mut goal, &second_task, root().join("a.rs"));
        apply_intent(&mut goal, &second_task);

        let expected = vec![
            NormalizedWorkspacePath::parse("a.rs").unwrap(),
            NormalizedWorkspacePath::parse("z.rs").unwrap(),
        ];
        assert_eq!(derive_writer_mutated_paths(&goal).unwrap(), expected);
        assert_eq!(derive_writer_mutated_paths(&goal).unwrap(), expected);
    }

    #[test]
    fn excludes_reconciled_not_performed_and_fails_closed_for_in_flight() {
        let (mut not_performed, task_id) = managed_goal();
        prepare_goal_intent(
            &mut not_performed,
            &task_id,
            root().join("src").join("no.rs"),
        );
        not_performed
            .task_reconcile_latest_mutation_intent(
                &task_id,
                "operation-1",
                MutationIntentState::ReconciledNotPerformed,
                "confirmed absent".to_owned(),
                NOW,
            )
            .unwrap();
        let not_performed: Goal =
            serde_json::from_slice(&serde_json::to_vec(&not_performed).unwrap()).unwrap();
        assert!(
            derive_writer_mutated_paths(&not_performed)
                .unwrap()
                .is_empty()
        );

        let (mut applying, task_id) = managed_goal();
        prepare_goal_intent(
            &mut applying,
            &task_id,
            root().join("src").join("pending.rs"),
        );
        applying
            .task_advance_latest_mutation_intent(
                &task_id,
                "operation-1",
                MutationIntentUpdate::BeginOperation { index: 0 },
            )
            .unwrap();
        assert!(derive_writer_mutated_paths(&applying).is_err());
    }

    #[test]
    fn reconciled_performed_contributes_durable_path() {
        let (mut goal, task_id) = managed_goal();
        prepare_goal_intent(&mut goal, &task_id, root().join("src").join("recovered.rs"));
        goal.task_reconcile_latest_mutation_intent(
            &task_id,
            "operation-1",
            MutationIntentState::ReconciledPerformed,
            "postimage confirmed".to_owned(),
            NOW,
        )
        .unwrap();
        let reloaded: Goal = serde_json::from_slice(&serde_json::to_vec(&goal).unwrap()).unwrap();
        assert_eq!(
            derive_writer_mutated_paths(&reloaded).unwrap()[0].as_str(),
            "src/recovered.rs"
        );
    }

    #[test]
    fn includes_every_historical_attempt_on_the_same_task() {
        let (mut goal, task_id) = managed_goal();
        prepare_goal_intent(&mut goal, &task_id, root().join("old.rs"));
        apply_intent(&mut goal, &task_id);
        let context = TaskTransitionContext {
            active_worker_stopped: true,
            side_effect_reconciled: true,
        };
        goal.transition_task(&task_id, TaskStatus::Verifying, context, NOW)
            .unwrap();
        goal.transition_task(&task_id, TaskStatus::Retryable, context, NOW)
            .unwrap();
        goal.transition_task(&task_id, TaskStatus::Ready, context, NOW)
            .unwrap();
        goal.transition_task(&task_id, TaskStatus::Running, context, NOW)
            .unwrap();
        goal.task_prepare_latest_mutation_intent(
            &task_id,
            intent_with_id(root().join("new.rs"), "operation-2"),
        )
        .unwrap();
        goal.task_advance_latest_mutation_intent(
            &task_id,
            "operation-2",
            MutationIntentUpdate::BeginOperation { index: 0 },
        )
        .unwrap();
        goal.task_advance_latest_mutation_intent(
            &task_id,
            "operation-2",
            MutationIntentUpdate::CompleteOperation { index: 0 },
        )
        .unwrap();
        assert_eq!(
            derive_writer_mutated_paths(&goal)
                .unwrap()
                .into_iter()
                .map(|path| path.as_str().to_owned())
                .collect::<Vec<_>>(),
            vec!["new.rs", "old.rs"]
        );
        let reloaded: Goal = serde_json::from_slice(&serde_json::to_vec(&goal).unwrap()).unwrap();
        assert_eq!(
            derive_writer_mutated_paths(&reloaded)
                .unwrap()
                .into_iter()
                .map(|path| path.as_str().to_owned())
                .collect::<Vec<_>>(),
            vec!["new.rs", "old.rs"]
        );
    }

    #[test]
    fn superseded_task_mutation_history_contributes_after_reload() {
        let task = superseded_writer_task(root().join("superseded.rs"));
        assert_eq!(task.status(), TaskStatus::Superseded);
        let reloaded: Task = serde_json::from_slice(&serde_json::to_vec(&task).unwrap()).unwrap();
        reloaded.validate_local().unwrap();
        let tasks = BTreeMap::from([(reloaded.id().clone(), reloaded)]);
        assert_eq!(
            derive_paths_from_tasks(tasks.values(), &root()).unwrap()[0].as_str(),
            "superseded.rs"
        );
    }

    #[test]
    fn fails_closed_for_partial_and_unknown_intents() {
        for state in [MutationIntentState::Partial, MutationIntentState::Unknown] {
            let (mut goal, task_id) = managed_goal();
            prepare_goal_intent(&mut goal, &task_id, root().join("uncertain.rs"));
            goal.task_reconcile_latest_mutation_intent(
                &task_id,
                "operation-1",
                state,
                "outcome cannot be proven".to_owned(),
                NOW,
            )
            .unwrap();
            assert!(derive_writer_mutated_paths(&goal).is_err());
        }
    }

    #[test]
    #[cfg(unix)]
    fn rejects_oversized_normalized_path() {
        let root = Path::new("/");
        let huge = root.join("x".repeat(crate::workspace_snapshot::MAX_NORMALIZED_PATH_BYTES + 1));
        assert!(normalize_durable_writer_path(&huge, root).is_err());
    }

    /// Real-filesystem Writer materialization through a durable managed Goal.
    ///
    /// The Writer resolves targets with `fs::canonicalize` (verbatim
    /// `\\?\C:\...` on Windows, ordinary absolute spelling elsewhere). That
    /// resolved spelling is what the durable `MutationIntent` stores. This test
    /// exercises the full durable chain on the real filesystem for both an
    /// existing file and a new file inside an existing parent directory:
    /// canonicalized Writer path -> `MutationIntent` -> Goal persistence ->
    /// reload -> `derive_writer_mutated_paths()`.
    #[test]
    fn real_writer_materialization_roundtrip_through_managed_goal() {
        let base =
            std::env::temp_dir().join(format!("local-mcp-writer-goal-{}", uuid::Uuid::new_v4()));
        // The managed root must not overlap the primary workspace identity.
        let primary_dir = base.join("primary");
        let wt_dir = base.join("wt");
        std::fs::create_dir_all(wt_dir.join("existing")).unwrap();
        std::fs::create_dir_all(&primary_dir).unwrap();
        let primary = crate::config::canonical_path(&primary_dir).unwrap();
        let common = primary.clone();
        let managed_root = crate::config::canonical_path(&wt_dir).unwrap();
        std::fs::create_dir_all(managed_root.join("existing")).unwrap();
        let existing = managed_root.join("existing").join("file.txt");
        std::fs::write(&existing, b"existing").unwrap();
        let new_path = managed_root.join("existing").join("new.txt");

        // Exactly what Writer `materialize_operations` resolves: an existing
        // target canonicalizes directly, while a new target joins its file
        // name onto the canonicalized parent.
        let writer_existing = std::fs::canonicalize(&existing).unwrap();
        let writer_new = {
            let canonical_parent = std::fs::canonicalize(new_path.parent().unwrap()).unwrap();
            canonical_parent.join(new_path.file_name().unwrap())
        };
        #[cfg(windows)]
        {
            assert!(writer_existing.to_string_lossy().starts_with(r"\\?\"));
            assert!(writer_new.to_string_lossy().starts_with(r"\\?\"));
        }

        let (mut goal, task_id) = managed_goal_at(primary, common, managed_root);
        prepare_goal_intent_value(
            &mut goal,
            &task_id,
            intent_with_paths(vec![writer_existing, writer_new], "operation-1"),
        );
        apply_intent(&mut goal, &task_id);

        let expected = vec![
            NormalizedWorkspacePath::parse("existing/file.txt").unwrap(),
            NormalizedWorkspacePath::parse("existing/new.txt").unwrap(),
        ];
        assert_eq!(derive_writer_mutated_paths(&goal).unwrap(), expected);
        let reloaded: Goal = serde_json::from_slice(&serde_json::to_vec(&goal).unwrap()).unwrap();
        reloaded.validate().unwrap();
        assert_eq!(derive_writer_mutated_paths(&reloaded).unwrap(), expected);

        std::fs::remove_dir_all(base).unwrap();
    }

    /// Failed-task replacement cannot supersede a Writer history Task.
    ///
    /// Design contract: `Goal::request_failed_task_replan` admits only
    /// deterministic size/budget failure classes, and
    /// `Task::request_failed_task_replan` admits only
    /// `READ_ONLY + SAFE_READ_ONLY` trigger scopes, while
    /// `Goal::apply_task_replacements` records history only against a prior
    /// request entry. A plain `CodexWriter` `LocalMutation` Task with durable
    /// Applied history is therefore not legally supersedable through the
    /// canonical replacement transaction. This test
    /// pins that reachable behavior instead of fabricating an illegal state:
    /// both entry points refuse, and the Goal (lease, plan revision, history,
    /// and derived writer paths) is left untouched.
    #[test]
    fn writer_applied_history_is_not_supersedable_via_failed_replacement() {
        use crate::goal::{FailedTaskReplanPolicy, FailedTaskReplanTriggerKind};

        let (mut goal, task_id) = managed_goal();
        let writer_path = root().join("superseded-via-replacement.rs");
        prepare_goal_intent(&mut goal, &task_id, writer_path);
        apply_intent(&mut goal, &task_id);

        let context = TaskTransitionContext {
            active_worker_stopped: true,
            side_effect_reconciled: true,
        };
        goal.transition_task(&task_id, TaskStatus::Verifying, context, NOW)
            .unwrap();
        goal.transition_task(&task_id, TaskStatus::Retryable, context, NOW)
            .unwrap();

        let plan_revision = goal.plan_revision();
        let goal_revision = goal.revision();
        let request_error = goal
            .request_failed_task_replan(
                "slice-3b-supersede-1".to_owned(),
                goal_revision,
                plan_revision,
                task_id.clone(),
                "writer history cannot enter the readonly replacement path".to_owned(),
                FailedTaskReplanPolicy::RequireDecomposition,
                FailedTaskReplanTriggerKind::PostAttemptFailure,
                None,
                NOW,
            )
            .unwrap_err();
        assert!(
            request_error.to_string().contains("does not admit")
                || request_error.to_string().contains("READ_ONLY"),
            "unexpected request refusal: {request_error}"
        );

        let replacement = Task::new(
            "writer replacement",
            "recover superseded writer authority",
            true,
            WorkerKind::CodexWriter,
            TaskScope::new(
                vec![root()],
                Vec::new(),
                TaskOperationKind::LocalMutation,
                ReplaySafety::VerifyBeforeRetry,
            ),
            vec![VerificationSpec::StructuredEvidence {
                requirement_id: "writer-proof-replacement".to_owned(),
            }],
            2,
            plan_revision + 1,
            NOW,
        )
        .unwrap();
        let replacement_id = replacement.id().clone();
        let criterion_rebindings = goal
            .completion_criteria()
            .iter()
            .map(|criterion| (criterion.id().clone(), vec![replacement_id.clone()]))
            .collect::<Vec<_>>();
        let replacement_error = goal
            .apply_task_replacements(
                vec![FailedTaskReplacementMutation {
                    replan_request_id: "slice-3b-supersede-1".to_owned(),
                    replaced_task_id: task_id.clone(),
                    replaced_plan_revision: plan_revision,
                    committed_plan_revision: plan_revision + 1,
                    canonical_proposal_digest:
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                            .to_owned(),
                    completion_closure_task_ids: vec![replacement_id.clone()],
                    criterion_rebindings,
                    new_tasks: vec![replacement],
                }],
                NOW,
            )
            .unwrap_err();
        assert!(
            replacement_error
                .to_string()
                .contains("invalid or duplicate record"),
            "unexpected replacement refusal: {replacement_error}"
        );

        // The refused transaction preserves lease, plan revision, and history.
        assert_eq!(goal.plan_revision(), plan_revision);
        assert_eq!(goal.tasks()[&task_id].status(), TaskStatus::Retryable);
        assert!(goal.failed_task_replacements().is_empty());
        goal.validate().unwrap();
        let reloaded: Goal = serde_json::from_slice(&serde_json::to_vec(&goal).unwrap()).unwrap();
        reloaded.validate().unwrap();
        let paths = derive_writer_mutated_paths(&reloaded).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].as_str(), "superseded-via-replacement.rs");
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_foreign_drive_unc_device_and_escape() {
        let root = Path::new(r"C:\managed\worktree");
        // Different drive verbatim form must not bridge onto the owned drive.
        assert!(
            normalize_durable_writer_path(Path::new(r"\\?\D:\managed\worktree\file.txt"), root)
                .is_err()
        );
        // UNC and device namespaces have no compact-drive equivalent.
        assert!(
            normalize_durable_writer_path(Path::new(r"\\server\share\file.txt"), root).is_err()
        );
        assert!(
            normalize_durable_writer_path(Path::new(r"\\?\UNC\server\share\file.txt"), root)
                .is_err()
        );
        assert!(
            normalize_durable_writer_path(Path::new(r"\\.\C:\managed\worktree\file.txt"), root)
                .is_err()
        );
        // Parent escape and prefix collision stay rejected even in verbatim form.
        assert!(
            normalize_durable_writer_path(
                Path::new(r"\\?\C:\managed\worktree\..\outside.txt"),
                root
            )
            .is_err()
        );
        assert!(
            normalize_durable_writer_path(
                Path::new(r"\\?\C:\managed\worktree-other\file.txt"),
                root
            )
            .is_err()
        );
        // Outside the exact root and Git internals stay rejected.
        assert!(normalize_durable_writer_path(Path::new(r"\\?\C:\other\file.txt"), root).is_err());
        assert!(
            normalize_durable_writer_path(Path::new(r"\\?\C:\managed\worktree\.git\config"), root)
                .is_err()
        );
        // The legitimate same-drive verbatim form is still accepted.
        assert_eq!(
            normalize_durable_writer_path(Path::new(r"\\?\C:\managed\worktree\src\file.txt"), root)
                .unwrap()
                .as_str(),
            "src/file.txt"
        );
    }
}
