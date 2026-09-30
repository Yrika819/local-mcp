//! Managed Worktrees V1 — Phase 2 read-only discovery and reconciliation tests.
//!
//! Covers `docs/MANAGED_WORKTREES_V1_DESIGN.md` section 25 groups B (eligibility)
//! and D (creation/recovery classification), restricted to the read-only
//! observation model. No test here creates, locks, unlocks, removes, or prunes a
//! worktree, and no test touches the developer's real linked worktrees: every
//! integration fixture is a temporary repository owned by the test alone.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::goal::Goal;
use crate::managed_worktree::{
    ManagedWorktreeCreationIntent, ManagedWorktreeRecord, WorktreeId, WorktreeOperationId,
    managed_branch_ref,
};
use crate::managed_worktree_discovery::{
    DiscoveryError, Eligibility, ExpectedWorktreeTarget, InProgressOperation, Ineligibility,
    ObservedWorktree, PathObservation, PrimaryWorkspaceStatus, Reconciliation,
    RepositoryObservation, UNRESOLVED_HEAD, WorktreeInventory,
    classify_eligibility as classify_eligibility_pure,
    classify_reconciliation as classify_reconciliation_pure, parse_worktree_list_porcelain_z,
    same_path_identity,
};
use crate::managed_worktree_observe::{
    GitCommandOutput, HostGit, READ_ONLY_GIT_SUBCOMMANDS, READ_ONLY_WORKTREE_VERBS, ReadOnlyGit,
    assert_read_only, is_git_environment_variable,
    observe_repository as observe_repository_with_paths,
};

const NOW: &str = "2026-01-01T00:00:00Z";
#[cfg(not(windows))]
const PRIMARY_ROOT: &str = "/repo/primary";
#[cfg(windows)]
const PRIMARY_ROOT: &str = r"C:\repo\primary";
#[cfg(not(windows))]
const COMMON_DIR: &str = "/repo/primary/.git";
#[cfg(windows)]
const COMMON_DIR: &str = r"C:\repo\primary\.git";
#[cfg(not(windows))]
const WORKTREE_ROOT: &str = "/managed/session-1/goal-1";
#[cfg(windows)]
const WORKTREE_ROOT: &str = r"C:\managed\session-1\goal-1";
const BASE: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";

/// One porcelain worktree record as owned lines.
///
/// Records are owned so a caller can bind several of them before building an
/// inventory, which keeps the fixtures free of lifetime noise.
type Record = Vec<String>;

/// A record for a worktree at `path`, at `head`, with the given extra
/// attributes (`branch ...`, `detached`, `locked ...`, `prunable ...`).
fn record(path: &str, head: &str, extra: &[&str]) -> Record {
    let mut lines = vec![format!("worktree {path}"), format!("HEAD {head}")];
    lines.extend(extra.iter().map(|line| (*line).to_owned()));
    lines
}

/// A record for the primary worktree on `main` at `head`.
fn primary(head: &str) -> Record {
    record(PRIMARY_ROOT, head, &["branch refs/heads/main"])
}

/// A record for a detached worktree.
fn detached(path: &str, head: &str, extra: &[&str]) -> Record {
    record(path, head, &["detached"])
        .into_iter()
        .chain(extra.iter().map(|line| (*line).to_owned()))
        .collect()
}

/// Build a `git worktree list --porcelain -z` payload the way Git does.
///
/// Each attribute becomes `KEY[ SP VALUE]\0` and each record is closed by an
/// extra NUL. Generating fixtures through the real serializer keeps the parser
/// tests honest about separators.
fn porcelain(records: &[&[String]]) -> String {
    let mut out = String::new();
    for entry in records {
        for attribute in entry.iter() {
            out.push_str(attribute);
            out.push('\0');
        }
        // Record separator: an additional NUL.
        out.push('\0');
    }
    out
}

fn parse_records(records: &[&[String]]) -> WorktreeInventory {
    parse_worktree_list_porcelain_z(&porcelain(records)).expect("fixture must parse")
}

fn entry<'a>(inventory: &'a WorktreeInventory, path: &str) -> &'a ObservedWorktree {
    inventory
        .entries()
        .iter()
        .find(|entry| entry.path() == Path::new(path))
        .unwrap_or_else(|| panic!("missing {path}"))
}

/// A consistent durable fixture: one Goal, its record, and its host-derived ref.
///
/// The managed branch ref is a pure function of the Goal ID, so a test must use
/// the same Goal to build a record and then classify it. Returning all three
/// together makes that impossible to get wrong.
struct Fixture {
    record: ManagedWorktreeRecord,
    branch: String,
}

impl Fixture {
    fn new(worktree_root: &str) -> Self {
        let goal = Goal::new(
            "session-1",
            PathBuf::from(PRIMARY_ROOT),
            "managed objective",
            None,
            vec![],
            vec!["all checks pass".into()],
            NOW,
        )
        .unwrap();
        let branch = managed_branch_ref(goal.id());
        let record = ManagedWorktreeRecord::requested(
            goal.id(),
            WorktreeId::new(),
            PathBuf::from(PRIMARY_ROOT),
            PathBuf::from(COMMON_DIR),
            PathBuf::from(worktree_root),
            BASE.to_owned(),
            Some("refs/heads/main".to_owned()),
            goal.revision(),
            0,
        )
        .unwrap();
        Self { record, branch }
    }

    fn expected(&self) -> ExpectedWorktreeTarget {
        ExpectedWorktreeTarget::from_record(&self.record)
    }

    fn active_expected(&self) -> ExpectedWorktreeTarget {
        ExpectedWorktreeTarget::from_record(&self.record_with_lifecycle("ACTIVE"))
    }

    fn record_with_lifecycle(&self, lifecycle: &str) -> ManagedWorktreeRecord {
        let mut serialized = serde_json::to_value(&self.record).unwrap();
        serialized["lifecycle"] = serde_json::json!(lifecycle);
        if matches!(lifecycle, "ACTIVE" | "CLEANUP_ELIGIBLE" | "REMOVED") {
            serialized["last_reconciled_head"] = serde_json::json!(BASE);
            serialized["last_reconciled_at"] = serde_json::json!(NOW);
        }
        let record: ManagedWorktreeRecord = serde_json::from_value(serialized).unwrap();
        record.validate().unwrap();
        record
    }

    /// A clean primary plus one managed worktree at `worktree_root`.
    ///
    /// The managed worktree is always on the fixture's host-derived branch and
    /// always carries the design's lock; `extra` appends further attributes.
    fn observation(
        &self,
        worktree_root: &str,
        head: &str,
        extra: &[&str],
    ) -> RepositoryObservation {
        let branch_line = format!("branch {}", self.branch);
        let lock_line = format!("locked {}", self.record.lock_reason());
        let mut attributes: Vec<&str> = vec![branch_line.as_str(), lock_line.as_str()];
        attributes.extend(extra.iter().copied());
        let managed = record(worktree_root, head, &attributes);
        let primary = primary(BASE);
        build_observation(
            &[&primary, &managed],
            vec![],
            PrimaryWorkspaceStatus::default(),
            std::slice::from_ref(&self.branch),
            COMMON_DIR,
            Some(BASE),
        )
    }
}

fn build_observation(
    records: &[&[String]],
    in_progress: Vec<InProgressOperation>,
    status: PrimaryWorkspaceStatus,
    refs: &[String],
    common_dir: &str,
    head: Option<&str>,
) -> RepositoryObservation {
    let mut path_observations = vec![(PathBuf::from(WORKTREE_ROOT), PathObservation::Missing)];
    for record in records {
        if let Some(path) = record
            .first()
            .and_then(|attribute| attribute.strip_prefix("worktree "))
        {
            let path = PathBuf::from(path);
            if let Some((_, state)) = path_observations
                .iter_mut()
                .find(|(observed, _)| same_path_identity(observed, &path))
            {
                *state = PathObservation::Directory;
            } else {
                path_observations.push((path, PathObservation::Directory));
            }
        }
    }
    RepositoryObservation::new(
        PathBuf::from(PRIMARY_ROOT),
        PathBuf::from(common_dir),
        head.map(str::to_owned),
        Some("refs/heads/main".to_owned()),
        parse_records(records),
        in_progress,
        status,
        path_observations,
        refs.iter()
            .cloned()
            .map(|ref_name| (ref_name, true))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn clean() -> PrimaryWorkspaceStatus {
    PrimaryWorkspaceStatus::default()
}

fn classify_eligibility(
    cwd: &Path,
    expected: &ExpectedWorktreeTarget,
    observation: &RepositoryObservation,
    path_occupied: bool,
) -> Eligibility {
    let observed = observation.clone();
    let observed = if path_occupied {
        observed.with_path_observation(
            expected.worktree_root().to_path_buf(),
            PathObservation::Occupied,
        )
    } else {
        observed
    };
    classify_eligibility_pure(cwd, expected, &observed)
}

fn classify_reconciliation(
    expected: &ExpectedWorktreeTarget,
    observation: &RepositoryObservation,
    path_occupied: bool,
) -> Reconciliation {
    let observed = observation.clone();
    let observed = if path_occupied {
        observed.with_path_observation(
            expected.worktree_root().to_path_buf(),
            PathObservation::Occupied,
        )
    } else {
        observed
    };
    classify_reconciliation_pure(expected, &observed)
}

fn observe_repository(
    git: &dyn ReadOnlyGit,
    cwd: &Path,
    refs_of_interest: &[String],
) -> Result<RepositoryObservation, DiscoveryError> {
    observe_repository_with_paths(git, cwd, refs_of_interest, &[])
}

struct FakeGit {
    outputs: BTreeMap<String, GitCommandOutput>,
    refs: BTreeSet<String>,
    ref_query_error: bool,
}

impl FakeGit {
    fn standard(root: &Path) -> Self {
        let root = std::fs::canonicalize(root).unwrap();
        let common_dir = root.join(".git");
        let mut fake = Self {
            outputs: BTreeMap::new(),
            refs: BTreeSet::new(),
            ref_query_error: false,
        };
        fake.set(
            &["rev-parse", "--show-toplevel"],
            0,
            format!("{}\n", root.display()).into_bytes(),
        );
        fake.set(
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            0,
            format!("{}\n", common_dir.display()).into_bytes(),
        );
        fake.set(
            &["worktree", "list", "--porcelain", "-z"],
            0,
            format!(
                "worktree {}\0HEAD {BASE}\0branch refs/heads/main\0\0",
                root.display()
            )
            .into_bytes(),
        );
        fake.set(
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            0,
            Vec::new(),
        );
        for marker in [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "BISECT_START",
            "rebase-merge",
            "rebase-apply",
            "rebase-apply/applying",
            "sequencer/todo",
            "BISECT_LOG",
        ] {
            fake.set(
                &["rev-parse", "--git-path", marker],
                0,
                format!("{}\n", common_dir.join(marker).display()).into_bytes(),
            );
        }
        fake
    }

    fn set(&mut self, args: &[&str], exit_code: i32, stdout: Vec<u8>) {
        self.outputs.insert(
            args.join("\0"),
            GitCommandOutput {
                stdout,
                stderr: if exit_code == 0 {
                    String::new()
                } else {
                    "injected failure".to_owned()
                },
                exit_code,
            },
        );
    }
}

impl ReadOnlyGit for FakeGit {
    fn run(&self, args: &[&str], _cwd: &Path) -> Result<GitCommandOutput, DiscoveryError> {
        self.outputs.get(&args.join("\0")).cloned().ok_or_else(|| {
            DiscoveryError::ObservationUnavailable(format!("fake Git has no response for {args:?}"))
        })
    }

    fn ref_exists(&self, ref_name: &str, _cwd: &Path) -> Result<bool, DiscoveryError> {
        if self.ref_query_error {
            Err(DiscoveryError::ObservationUnavailable(
                "injected ref-query failure".to_owned(),
            ))
        } else {
            Ok(self.refs.contains(ref_name))
        }
    }
}

struct FakeObservationRoot(PathBuf);

impl FakeObservationRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("local-mcp-mw-fake-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(path.join(".git")).unwrap();
        Self(path)
    }
}

impl Drop for FakeObservationRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn observer_records_canonical_identity_and_missing_target_path() {
    let root = FakeObservationRoot::new();
    let git = FakeGit::standard(&root.0);
    let candidate = root.0.join("managed-target");
    let observation =
        observe_repository_with_paths(&git, &root.0, &[], &[candidate.as_path()]).unwrap();

    assert_eq!(
        observation.top_level(),
        std::fs::canonicalize(&root.0).unwrap()
    );
    assert_eq!(
        observation.common_dir(),
        std::fs::canonicalize(root.0.join(".git")).unwrap()
    );
    assert_eq!(observation.head(), Some(BASE));
    assert_eq!(observation.head_ref(), Some("refs/heads/main"));
    assert_eq!(
        observation.path_observation(&candidate),
        PathObservation::Missing
    );
    assert!(observation.is_trustworthy().is_ok());
}

#[test]
fn observer_fails_closed_on_nonzero_required_commands_and_ref_queries() {
    let root = FakeObservationRoot::new();
    let status_args = ["status", "--porcelain=v1", "-z", "--untracked-files=all"];
    let mut git = FakeGit::standard(&root.0);
    git.set(&status_args, 128, Vec::new());
    assert!(matches!(
        observe_repository(&git, &root.0, &[]),
        Err(DiscoveryError::GitCommand { .. })
    ));

    let mut git = FakeGit::standard(&root.0);
    git.set(
        &["rev-parse", "--git-path", "rebase-merge"],
        128,
        Vec::new(),
    );
    assert!(observe_repository(&git, &root.0, &[]).is_err());

    let mut git = FakeGit::standard(&root.0);
    git.ref_query_error = true;
    assert!(
        observe_repository(
            &git,
            &root.0,
            &["refs/heads/local-mcp/goal/test".to_owned()]
        )
        .is_err()
    );
}

#[test]
fn observer_rejects_non_utf8_worktree_paths_instead_of_lossy_collapsing() {
    let root = FakeObservationRoot::new();
    let mut git = FakeGit::standard(&root.0);
    git.set(
        &["worktree", "list", "--porcelain", "-z"],
        0,
        b"worktree /repo/invalid-\xff\0HEAD 0123456789abcdef0123456789abcdef01234567\0branch refs/heads/main\0\0".to_vec(),
    );
    assert!(matches!(
        observe_repository(&git, &root.0, &[]),
        Err(DiscoveryError::ObservationUnavailable(_))
    ));
}

#[test]
fn observer_handles_sequencer_state_and_nul_safe_status_paths() {
    let root = FakeObservationRoot::new();
    let mut git = FakeGit::standard(&root.0);
    git.set(
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        0,
        b"R  renamed.txt\0old-name.txt\0?? non-utf8-\xff\0".to_vec(),
    );
    let todo = root.0.join(".git/sequencer/todo");
    std::fs::create_dir_all(todo.parent().unwrap()).unwrap();
    std::fs::write(&todo, b"# sequencer state\npick 0123456 test\n").unwrap();

    let observation = observe_repository(&git, &root.0, &[]).unwrap();
    assert_eq!(observation.in_progress(), [InProgressOperation::CherryPick]);
    assert!(observation.status().staged);
    assert!(observation.status().untracked);

    let apply = root.0.join(".git/rebase-apply");
    std::fs::create_dir_all(&apply).unwrap();
    std::fs::write(apply.join("applying"), b"").unwrap();
    assert!(observe_repository(&git, &root.0, &[]).is_err());
}

#[test]
fn observer_rejects_malformed_nonempty_status_records() {
    for invalid in [
        b"\0".as_slice(),
        b"\0\0".as_slice(),
        b"?? missing-terminator".as_slice(),
        b"   clean-looking.txt\0".as_slice(),
        b"Q? invalid-status.txt\0".as_slice(),
        b"?  invalid-special-pair.txt\0".as_slice(),
        b"!M invalid-special-pair.txt\0".as_slice(),
    ] {
        let root = FakeObservationRoot::new();
        let mut git = FakeGit::standard(&root.0);
        git.set(
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            0,
            invalid.to_vec(),
        );
        assert!(matches!(
            observe_repository(&git, &root.0, &[]),
            Err(DiscoveryError::ObservationUnavailable(_))
        ));
    }
}

#[cfg(unix)]
#[test]
fn observer_rejects_dangling_and_oversized_sequencer_markers() {
    let root = FakeObservationRoot::new();
    let git = FakeGit::standard(&root.0);
    let todo = root.0.join(".git/sequencer/todo");
    std::fs::create_dir_all(todo.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("missing-todo", &todo).unwrap();
    assert!(matches!(
        observe_repository(&git, &root.0, &[]),
        Err(DiscoveryError::ObservationUnavailable(_))
    ));

    std::fs::remove_file(&todo).unwrap();
    std::fs::write(&todo, vec![b'x'; 1024 * 1024 + 1]).unwrap();
    assert!(matches!(
        observe_repository(&git, &root.0, &[]),
        Err(DiscoveryError::ObservationUnavailable(_))
    ));
}

// --- Pure parser -----------------------------------------------------------

#[test]
fn empty_output_is_an_empty_inventory() {
    let inventory = parse_worktree_list_porcelain_z("").unwrap();
    assert!(inventory.is_empty());
    assert!(inventory.contradictions().is_empty());
}

#[test]
fn single_worktree_on_a_branch_is_parsed() {
    let p = primary(BASE);
    let inventory = parse_records(&[&p]);
    assert_eq!(inventory.entries().len(), 1);
    let observed = entry(&inventory, PRIMARY_ROOT);
    assert_eq!(observed.path(), Path::new(PRIMARY_ROOT));
    assert_eq!(observed.head(), Some(BASE));
    assert_eq!(observed.resolved_head(), Some(BASE));
    assert_eq!(observed.branch_ref(), Some("refs/heads/main"));
    assert!(!observed.is_detached());
    assert!(!observed.is_bare());
    assert!(!observed.is_locked());
    assert!(observed.lock_reason().is_none());
    assert!(observed.prunable_reason().is_none());
    assert!(observed.unknown_attributes().is_empty());
}

#[test]
fn multiple_worktrees_are_parsed_in_order() {
    let p = primary(BASE);
    let a = record("/managed/a", BASE, &["branch refs/heads/local-mcp/goal/a"]);
    let b = record("/managed/b", BASE, &["branch refs/heads/local-mcp/goal/b"]);
    let inventory = parse_records(&[&p, &a, &b]);
    assert_eq!(inventory.entries().len(), 3);
    assert_eq!(inventory.entries()[0].path(), Path::new(PRIMARY_ROOT));
    assert_eq!(inventory.entries()[1].path(), Path::new("/managed/a"));
    assert_eq!(inventory.entries()[2].path(), Path::new("/managed/b"));
    assert!(inventory.contradictions().is_empty());
}

#[test]
fn detached_head_has_no_branch() {
    let d = detached("/managed/detached", BASE, &[]);
    let inventory = parse_records(&[&d]);
    let observed = entry(&inventory, "/managed/detached");
    assert!(observed.is_detached());
    assert_eq!(observed.branch_ref(), None);
    assert_eq!(observed.resolved_head(), Some(BASE));
}

#[test]
fn unborn_head_is_reported_as_unresolved() {
    let p = primary(UNRESOLVED_HEAD);
    let d = detached("/repo/empty-sub", UNRESOLVED_HEAD, &[]);
    let inventory = parse_records(&[&p, &d]);
    assert_eq!(
        entry(&inventory, PRIMARY_ROOT).head(),
        Some(UNRESOLVED_HEAD)
    );
    assert_eq!(entry(&inventory, PRIMARY_ROOT).resolved_head(), None);
    assert_eq!(entry(&inventory, "/repo/empty-sub").resolved_head(), None);
}

#[test]
fn locked_worktree_captures_reason_and_lock_state() {
    let p = primary(BASE);
    let l = record(
        "/managed/locked",
        BASE,
        &[
            "branch refs/heads/local-mcp/goal/a",
            "locked local-mcp goal 11111111-1111-1111-1111-111111111111",
        ],
    );
    let inventory = parse_records(&[&p, &l]);
    let observed = entry(&inventory, "/managed/locked");
    assert!(observed.is_locked());
    assert_eq!(
        observed.lock_reason(),
        Some("local-mcp goal 11111111-1111-1111-1111-111111111111")
    );
}

#[test]
fn locked_worktree_without_a_reason_is_still_locked() {
    let p = primary(BASE);
    let l = record("/managed/locked", BASE, &["detached", "locked"]);
    let inventory = parse_records(&[&p, &l]);
    let observed = entry(&inventory, "/managed/locked");
    assert!(observed.is_locked());
    assert_eq!(observed.lock_reason(), None);
}

#[test]
fn lock_reason_may_contain_spaces_and_colons() {
    let p = primary(BASE);
    let l = record(
        "/managed/locked",
        BASE,
        &["detached", "locked reason with: colons and = signs"],
    );
    let inventory = parse_records(&[&p, &l]);
    assert_eq!(
        entry(&inventory, "/managed/locked").lock_reason(),
        Some("reason with: colons and = signs")
    );
}

#[test]
fn unusual_but_valid_paths_are_preserved_verbatim() {
    // `-z` is used precisely so paths are not quoted, so a path may contain
    // spaces, quotes, or non-ASCII characters.
    let p = primary(BASE);
    let spaced = record("/managed/work tree with spaces", BASE, &["detached"]);
    let unicode = record("/managed/ユニコード", BASE, &["detached"]);
    let quoted = record("/managed/quote\"and'apostrophe", BASE, &["detached"]);
    let inventory = parse_records(&[&p, &spaced, &unicode, &quoted]);
    assert_eq!(inventory.entries().len(), 4);
    for path in [
        "/managed/work tree with spaces",
        "/managed/ユニコード",
        "/managed/quote\"and'apostrophe",
    ] {
        assert_eq!(entry(&inventory, path).path(), Path::new(path));
    }
}

#[test]
fn prunable_metadata_is_captured() {
    let p = primary(BASE);
    let s = record(
        "/managed/stale",
        BASE,
        &[
            "detached",
            "prunable gitdir file points to non-existent location",
        ],
    );
    let inventory = parse_records(&[&p, &s]);
    assert_eq!(
        entry(&inventory, "/managed/stale").prunable_reason(),
        Some("gitdir file points to non-existent location")
    );
}

#[test]
fn bare_repository_record_is_parsed() {
    let bare = vec!["worktree /srv/bare.git".to_owned(), "bare".to_owned()];
    let inventory = parse_records(&[&bare]);
    let observed = entry(&inventory, "/srv/bare.git");
    assert!(observed.is_bare());
    assert_eq!(observed.head(), None);
}

#[test]
fn unknown_attributes_are_preserved_not_dropped() {
    let p = primary(BASE);
    let f = record(
        "/managed/future",
        BASE,
        &["detached", "some-future-attribute value"],
    );
    let inventory = parse_records(&[&p, &f]);
    assert_eq!(
        entry(&inventory, "/managed/future").unknown_attributes(),
        ["some-future-attribute value"]
    );
}

#[test]
fn truncated_final_record_is_rejected() {
    // Git's machine format terminates every record with an extra NUL. A partial
    // response must not be accepted as a complete ownership observation.
    for truncated in [
        format!("worktree /repo/primary\0HEAD {BASE}\0branch refs/heads/main"),
        format!("worktree /repo/primary\0HEAD {BASE}\0branch refs/heads/main\0"),
    ] {
        assert!(matches!(
            parse_worktree_list_porcelain_z(&truncated),
            Err(DiscoveryError::MalformedWorktreeList(_))
        ));
    }
}

#[test]
fn nul_separation_means_newlines_inside_values_are_preserved() {
    // A path may legitimately contain a newline. Splitting on newlines would
    // corrupt it; NUL separation does not.
    let p = primary(BASE);
    let n = record("/managed/with\nnewline", BASE, &["detached"]);
    let inventory = parse_records(&[&p, &n]);
    assert_eq!(inventory.entries().len(), 2);
    assert_eq!(
        entry(&inventory, "/managed/with\nnewline").path(),
        Path::new("/managed/with\nnewline")
    );
}

#[test]
fn duplicate_worktree_paths_are_a_contradiction_not_a_silent_overwrite() {
    let p = primary(BASE);
    let dup = record(PRIMARY_ROOT, OTHER, &["branch refs/heads/other"]);
    let inventory = parse_records(&[&p, &dup]);
    assert_eq!(inventory.entries().len(), 2);
    let contradictions = inventory.contradictions();
    assert_eq!(contradictions.len(), 1);
    assert!(contradictions[0].contains("registered more than once"));
}

#[test]
fn duplicate_branch_registration_is_a_contradiction() {
    let p = primary(BASE);
    let a = record("/managed/a", BASE, &["branch refs/heads/shared"]);
    let b = record("/managed/b", OTHER, &["branch refs/heads/shared"]);
    let inventory = parse_records(&[&p, &a, &b]);
    assert!(
        inventory
            .contradictions()
            .iter()
            .any(|reason| reason.contains("checked out by more than one worktree"))
    );
}

#[test]
fn sha256_length_object_ids_are_accepted_and_zero_ids_are_unresolved() {
    let long = "a".repeat(64);
    let d = detached(PRIMARY_ROOT, long.as_str(), &[]);
    let inventory = parse_records(&[&d]);
    assert_eq!(
        entry(&inventory, PRIMARY_ROOT).resolved_head(),
        Some(long.as_str())
    );

    let zero = "0".repeat(64);
    let unborn = detached("/repo/empty", zero.as_str(), &[]);
    let inventory = parse_records(&[&unborn]);
    assert_eq!(entry(&inventory, "/repo/empty").resolved_head(), None);
}

// --- Pure parser: malformed input ------------------------------------------

#[test]
fn malformed_records_are_rejected_rather_than_guessed() {
    let head = format!("HEAD {BASE}");
    let other = format!("HEAD {OTHER}");
    let path = "worktree /repo/primary".to_owned();
    let cases: Vec<(&str, Record)> = vec![
        (
            "record without a worktree path",
            vec![head.clone(), "branch refs/heads/main".to_owned()],
        ),
        (
            "duplicate worktree path",
            vec![path.clone(), head.clone(), path.clone()],
        ),
        (
            "duplicate HEAD",
            vec![path.clone(), head.clone(), other.clone()],
        ),
        (
            "detached and on a branch",
            vec![
                path.clone(),
                head.clone(),
                "branch refs/heads/main".to_owned(),
                "detached".to_owned(),
            ],
        ),
        (
            "bare and detached",
            vec![
                "worktree /srv/bare.git".to_owned(),
                "bare".to_owned(),
                "detached".to_owned(),
            ],
        ),
        (
            "bare with a HEAD",
            vec![
                "worktree /srv/bare.git".to_owned(),
                "bare".to_owned(),
                head.clone(),
            ],
        ),
        (
            "record with no HEAD",
            vec![path.clone(), "branch refs/heads/main".to_owned()],
        ),
        (
            "attribute requiring a value has none",
            vec![path.clone(), "HEAD".to_owned()],
        ),
        (
            "valueless attribute given a value",
            vec![path.clone(), head.clone(), "detached yes".to_owned()],
        ),
        (
            "HEAD is abbreviated",
            vec![path.clone(), "HEAD 0123456".to_owned()],
        ),
        (
            "HEAD is not lowercase hex",
            vec![
                path.clone(),
                "HEAD 0123456789ABCDEF0123456789abcdef01234567".to_owned(),
            ],
        ),
        (
            "branch is not a full ref",
            vec![path.clone(), head.clone(), "branch main".to_owned()],
        ),
        (
            "branch ref contains a forbidden whitespace character",
            vec![
                path.clone(),
                head.clone(),
                "branch refs/heads/bad name".to_owned(),
            ],
        ),
        (
            "branch ref contains a forbidden double dot",
            vec![
                path.clone(),
                head.clone(),
                "branch refs/heads/bad..name".to_owned(),
            ],
        ),
        (
            "duplicate detached attribute",
            vec![
                path.clone(),
                head.clone(),
                "detached".to_owned(),
                "detached".to_owned(),
            ],
        ),
        (
            "duplicate bare attribute",
            vec![
                "worktree /srv/bare.git".to_owned(),
                "bare".to_owned(),
                "bare".to_owned(),
            ],
        ),
        (
            "duplicate locked",
            vec![
                path.clone(),
                head.clone(),
                "locked a".to_owned(),
                "locked b".to_owned(),
            ],
        ),
        (
            "duplicate prunable",
            vec![
                path.clone(),
                head.clone(),
                "prunable a".to_owned(),
                "prunable b".to_owned(),
            ],
        ),
        (
            "empty worktree path value",
            vec!["worktree ".to_owned(), head.clone()],
        ),
    ];
    for (label, lines) in cases {
        assert!(
            matches!(
                parse_worktree_list_porcelain_z(&porcelain(&[&lines])),
                Err(DiscoveryError::MalformedWorktreeList(_))
            ),
            "expected rejection for: {label}"
        );
    }
}

// --- Read-only allowlist: negative authority tests --------------------------

#[test]
fn read_only_seam_refuses_every_mutating_git_operation() {
    for args in [
        vec!["worktree", "add"],
        vec!["worktree", "remove"],
        vec!["worktree", "lock"],
        vec!["worktree", "unlock"],
        vec!["worktree", "move"],
        vec!["worktree", "prune"],
        vec!["worktree", "repair"],
        vec!["worktree", "add", "--force"],
        vec!["add"],
        vec!["commit"],
        vec!["branch", "x"],
        vec!["checkout"],
        vec!["reset", "--hard"],
        vec!["clean", "-fd"],
        vec!["stash"],
        vec!["merge"],
        vec!["rebase"],
        vec!["push"],
        vec!["fetch"],
        vec!["rm"],
        vec!["mv"],
    ] {
        assert!(
            assert_read_only(&args).is_err(),
            "read-only seam accepted a mutating git invocation: {args:?}"
        );
    }
}

#[test]
fn git_environment_filter_removes_configuration_and_trace_overrides() {
    for key in [
        "GIT_TRACE",
        "GIT_TRACE2_EVENT",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_DIR",
        "GIT_INDEX_FILE",
        "git_work_tree",
    ] {
        assert!(
            is_git_environment_variable(std::ffi::OsStr::new(key)),
            "Git environment override was not filtered: {key}"
        );
    }
    assert!(!is_git_environment_variable(std::ffi::OsStr::new("PATH")));
    assert!(!is_git_environment_variable(std::ffi::OsStr::new("HOME")));
}

#[test]
fn read_only_seam_refuses_force_flags() {
    for args in [
        vec!["worktree", "list", "--force"],
        vec!["status", "-f"],
        vec!["rev-parse", "--force-with-lease", "HEAD"],
    ] {
        assert!(
            assert_read_only(&args).is_err(),
            "read-only seam accepted a force flag: {args:?}"
        );
    }
}

#[test]
fn read_only_seam_accepts_only_allowlisted_invocations() {
    for args in [
        vec!["worktree", "list", "--porcelain", "-z"],
        vec!["rev-parse", "--show-toplevel"],
        vec!["rev-parse", "--path-format=absolute", "--git-common-dir"],
        vec!["rev-parse", "--git-path", "MERGE_HEAD"],
        vec!["symbolic-ref", "-q", "HEAD"],
        vec![
            "show-ref",
            "--verify",
            "--quiet",
            "--",
            "refs/heads/local-mcp/goal/11111111-1111-1111-1111-111111111111",
        ],
        vec!["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    ] {
        assert!(
            assert_read_only(&args).is_ok(),
            "read-only seam refused an allowlisted invocation: {args:?}"
        );
    }
    // The allowlist itself must never carry a mutating verb.
    assert!(READ_ONLY_WORKTREE_VERBS == ["list"]);
    for subcommand in READ_ONLY_GIT_SUBCOMMANDS {
        assert!(
            !matches!(
                subcommand,
                "add"
                    | "commit"
                    | "merge"
                    | "rebase"
                    | "push"
                    | "reset"
                    | "clean"
                    | "checkout"
                    | "rm"
                    | "mv"
                    | "stash"
                    | "fetch"
            ),
            "mutating subcommand {subcommand} is on the read-only allowlist"
        );
    }
    assert!(assert_read_only(&[]).is_err());
    // A bare `worktree` invocation with no verb is not a read-only list.
    assert!(assert_read_only(&["worktree"]).is_err());
}

#[cfg(unix)]
#[test]
fn host_git_disables_repository_fsmonitor_helpers() {
    let repo = TempRepo::new("fsmonitor");
    let helper = repo.root.join("fsmonitor-helper");
    let marker = repo.root.join("fsmonitor-ran");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\nprintf invoked > '{}'\nprintf 'token\\n'\n",
            marker.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    repo.git(&[
        "config",
        "core.fsmonitor",
        helper
            .to_str()
            .expect("temporary helper path must be UTF-8"),
    ]);

    let output = HostGit::new()
        .run(
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            &repo.root,
        )
        .unwrap();
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert!(!marker.exists(), "Git ran the configured fsmonitor helper");
}

// --- Pure reconciliation classification -------------------------------------

#[test]
fn exact_owned_active_worktree_is_proven() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let observation = fixture.observation(WORKTREE_ROOT, BASE, &[]);
    let state = classify_reconciliation(&fixture.active_expected(), &observation, false);
    assert_eq!(state, Reconciliation::ActiveExact { head: BASE.into() });
    assert!(state.is_exact());
    assert!(!state.permits_bounded_retry());
    assert!(!state.requires_explicit_recovery());
}

#[test]
fn unknown_ref_observation_never_becomes_no_side_effect_retry() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let p = primary(BASE);
    let observation = build_observation(&[&p], vec![], clean(), &[], COMMON_DIR, Some(BASE));
    let state = classify_reconciliation_pure(&fixture.expected(), &observation);
    assert!(matches!(state, Reconciliation::Ambiguous { .. }));
    assert!(!state.permits_bounded_retry());
}

#[test]
fn no_side_effect_permits_bounded_retry_and_nothing_else_does() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let p = primary(BASE);
    let observation = build_observation(&[&p], vec![], clean(), &[], COMMON_DIR, Some(BASE))
        .with_ref_observation(fixture.branch.clone(), false);
    let expected = fixture.expected();
    let state = classify_reconciliation(&expected, &observation, false);
    assert_eq!(state, Reconciliation::NoSideEffect { operation_id: None });
    assert!(!state.permits_bounded_retry());
    assert!(!state.requires_explicit_recovery());

    let intent = ManagedWorktreeCreationIntent::prepared(
        fixture.record.goal_id(),
        fixture.record.worktree_id().clone(),
        WorktreeOperationId::new(),
        fixture.record.repository_common_dir().to_path_buf(),
        fixture.record.worktree_root().to_path_buf(),
        fixture.record.base_commit().to_owned(),
    )
    .unwrap();
    let expected =
        ExpectedWorktreeTarget::from_record_and_intent(&fixture.record, &intent).unwrap();
    let state = classify_reconciliation(&expected, &observation, false);
    assert_eq!(
        state,
        Reconciliation::NoSideEffect {
            operation_id: Some(intent.operation_id().as_str().to_owned())
        }
    );
    assert!(state.permits_bounded_retry());
}

#[test]
fn prunable_metadata_blocks_and_never_permits_retry() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let observation = fixture.observation(
        WORKTREE_ROOT,
        BASE,
        &["prunable gitdir file points to non-existent location"],
    );
    let state = classify_reconciliation(&fixture.expected(), &observation, false);
    assert!(matches!(state, Reconciliation::PrunableMetadata { .. }));
    assert!(!state.permits_bounded_retry());
    assert!(state.requires_explicit_recovery());
}

#[test]
fn branch_only_side_effect_blocks_instead_of_retrying() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let p = primary(BASE);
    let observation = build_observation(
        &[&p],
        vec![],
        clean(),
        std::slice::from_ref(&fixture.branch),
        COMMON_DIR,
        Some(BASE),
    );
    let state = classify_reconciliation(&fixture.expected(), &observation, false);
    assert_eq!(state, Reconciliation::BranchOnlySideEffect);
    assert!(!state.permits_bounded_retry());
    assert!(state.requires_explicit_recovery());
}

#[test]
fn occupied_path_without_registration_is_classified_not_assumed_safe() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let p = primary(BASE);
    let observation = build_observation(&[&p], vec![], clean(), &[], COMMON_DIR, Some(BASE));
    let state = classify_reconciliation(&fixture.expected(), &observation, true);
    assert_eq!(state, Reconciliation::PathOccupied);
    assert!(!state.permits_bounded_retry());
}

#[test]
fn removed_worktree_is_exact_when_the_design_retains_its_branch() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let removed = fixture.record_with_lifecycle("REMOVED");
    let expected = ExpectedWorktreeTarget::from_record(&removed);
    let p = primary(BASE);
    let observation = build_observation(
        &[&p],
        vec![],
        clean(),
        std::slice::from_ref(&fixture.branch),
        COMMON_DIR,
        Some(BASE),
    );
    let state = classify_reconciliation_pure(&expected, &observation);
    assert_eq!(state, Reconciliation::RemovedExact);
    assert!(state.is_exact());
    assert!(!state.permits_bounded_retry());

    let absent_branch = observation
        .clone()
        .with_ref_observation(fixture.branch.clone(), false);
    assert_eq!(
        classify_reconciliation_pure(&expected, &absent_branch),
        Reconciliation::RemovedExact
    );

    let unobserved_branch = build_observation(&[&p], vec![], clean(), &[], COMMON_DIR, Some(BASE));
    let state = classify_reconciliation_pure(&expected, &unobserved_branch);
    assert!(matches!(state, Reconciliation::Ambiguous { .. }));
    assert!(!state.permits_bounded_retry());
}

#[test]
fn registered_but_missing_path_is_not_active_or_retryable() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let observation = fixture
        .observation(WORKTREE_ROOT, BASE, &[])
        .with_path_observation(PathBuf::from(WORKTREE_ROOT), PathObservation::Missing);
    let state = classify_reconciliation(&fixture.active_expected(), &observation, false);
    assert_eq!(state, Reconciliation::MissingPath);
    assert!(!state.permits_bounded_retry());
    assert!(state.requires_explicit_recovery());
}

#[test]
fn eligibility_requires_exact_primary_repository_and_base_identity() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let p = primary(BASE);
    let common_mismatch =
        build_observation(&[&p], vec![], clean(), &[], "/repo/other/.git", Some(BASE));
    assert!(matches!(
        classify_eligibility(
            Path::new(PRIMARY_ROOT),
            &fixture.expected(),
            &common_mismatch,
            false
        ),
        Eligibility::Ineligible(Ineligibility::CommonDirMismatch { .. })
    ));

    let moved_primary = primary(OTHER);
    let head_mismatch = build_observation(
        &[&moved_primary],
        vec![],
        clean(),
        &[],
        COMMON_DIR,
        Some(OTHER),
    );
    assert!(matches!(
        classify_eligibility(
            Path::new(PRIMARY_ROOT),
            &fixture.expected(),
            &head_mismatch,
            false
        ),
        Eligibility::Ineligible(Ineligibility::BaseCommitMismatch { .. })
    ));

    let mut serialized = serde_json::to_value(&fixture.record).unwrap();
    serialized["primary_root"] =
        serde_json::to_value(PathBuf::from(PRIMARY_ROOT).with_file_name("another-primary"))
            .unwrap();
    let mismatched: ManagedWorktreeRecord = serde_json::from_value(serialized).unwrap();
    mismatched.validate().unwrap();
    assert!(matches!(
        classify_eligibility(
            Path::new(PRIMARY_ROOT),
            &ExpectedWorktreeTarget::from_record(&mismatched),
            &build_observation(&[&p], vec![], clean(), &[], COMMON_DIR, Some(BASE)),
            false
        ),
        Eligibility::Ineligible(Ineligibility::PrimaryRootMismatch { .. })
    ));
}

#[test]
fn common_dir_mismatch_is_detected_before_anything_else() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let p = primary(BASE);
    let observation = build_observation(
        &[&p],
        vec![],
        clean(),
        &[],
        "/somewhere/else/.git",
        Some(BASE),
    );
    assert!(matches!(
        classify_reconciliation(&fixture.expected(), &observation, false),
        Reconciliation::CommonDirMismatch { .. }
    ));
}

#[test]
fn branch_mismatch_head_mismatch_and_lock_mismatch_are_distinct() {
    // Correct branch, HEAD, and durable lock reason.
    let fixture = Fixture::new(WORKTREE_ROOT);
    let p = primary(BASE);
    let branch_line = format!("branch {}", fixture.branch);
    let lock_line = format!("locked {}", fixture.record.lock_reason());
    let locked = record(
        WORKTREE_ROOT,
        BASE,
        &[branch_line.as_str(), lock_line.as_str()],
    );
    let observation = build_observation(
        &[&p, &locked],
        vec![],
        clean(),
        std::slice::from_ref(&fixture.branch),
        COMMON_DIR,
        Some(BASE),
    );
    // A locked exact worktree is ACTIVE_EXACT; the unlocked case is below.
    assert!(matches!(
        classify_reconciliation(&fixture.active_expected(), &observation, false),
        Reconciliation::ActiveExact { .. }
    ));

    let p = primary(BASE);
    let branch_line = format!("branch {}", fixture.branch);
    let no_lock = record(WORKTREE_ROOT, BASE, &[branch_line.as_str()]);
    let observation = build_observation(
        &[&p, &no_lock],
        vec![],
        clean(),
        std::slice::from_ref(&fixture.branch),
        COMMON_DIR,
        Some(BASE),
    );
    assert!(matches!(
        classify_reconciliation(&fixture.expected(), &observation, false),
        Reconciliation::LockMismatch { .. }
    ));

    let wrong_lock = record(
        WORKTREE_ROOT,
        BASE,
        &[branch_line.as_str(), "locked another goal"],
    );
    let observation = build_observation(
        &[&p, &wrong_lock],
        vec![],
        clean(),
        std::slice::from_ref(&fixture.branch),
        COMMON_DIR,
        Some(BASE),
    );
    assert!(matches!(
        classify_reconciliation(&fixture.active_expected(), &observation, false),
        Reconciliation::LockMismatch { .. }
    ));

    // Correct branch, different HEAD.
    let p = primary(BASE);
    let branch_line = format!("branch {}", fixture.branch);
    let lock_line = format!("locked {}", fixture.record.lock_reason());
    let moved = record(
        WORKTREE_ROOT,
        OTHER,
        &[branch_line.as_str(), lock_line.as_str()],
    );
    let observation = build_observation(
        &[&p, &moved],
        vec![],
        clean(),
        std::slice::from_ref(&fixture.branch),
        COMMON_DIR,
        Some(BASE),
    );
    assert!(matches!(
        classify_reconciliation(&fixture.expected(), &observation, false),
        Reconciliation::HeadMismatch { .. }
    ));

    // Different branch at the expected path.
    let p = primary(BASE);
    let other_branch = record(
        WORKTREE_ROOT,
        BASE,
        &["branch refs/heads/someone-else", "locked reason"],
    );
    let observation = build_observation(
        &[&p, &other_branch],
        vec![],
        clean(),
        std::slice::from_ref(&fixture.branch),
        COMMON_DIR,
        Some(BASE),
    );
    assert!(matches!(
        classify_reconciliation(&fixture.expected(), &observation, false),
        Reconciliation::BranchMismatch { .. }
    ));
}

#[test]
fn path_owned_by_another_worktree_is_blocked() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let p = primary(BASE);
    // The managed branch is checked out somewhere else entirely.
    let branch_line = format!("branch {}", fixture.branch);
    let elsewhere = record("/somewhere/else", BASE, &[branch_line.as_str()]);
    let observation = build_observation(
        &[&p, &elsewhere],
        vec![],
        clean(),
        std::slice::from_ref(&fixture.branch),
        COMMON_DIR,
        Some(BASE),
    );
    assert!(matches!(
        classify_reconciliation(&fixture.expected(), &observation, false),
        Reconciliation::PathOwnedByOtherWorktree { .. }
    ));
}

#[test]
fn ambiguous_observations_never_become_safe_to_retry() {
    let fixture = Fixture::new(WORKTREE_ROOT);

    // Duplicate registration of the expected path.
    let p = primary(BASE);
    let owned = record(WORKTREE_ROOT, BASE, &[&fixture.branch, "locked reason"]);
    let duplicate = record(WORKTREE_ROOT, OTHER, &["detached"]);
    let observation = build_observation(
        &[&p, &owned, &duplicate],
        vec![],
        clean(),
        std::slice::from_ref(&fixture.branch),
        COMMON_DIR,
        Some(BASE),
    );
    let state = classify_reconciliation(&fixture.expected(), &observation, false);
    assert!(matches!(state, Reconciliation::Ambiguous { .. }));
    assert!(!state.permits_bounded_retry());
    // Ambiguity is not "explicit recovery" either: nothing may act on it.
    assert!(!state.requires_explicit_recovery());

    // An unrecognized attribute on the owned worktree.
    let p = primary(BASE);
    let future = record(
        WORKTREE_ROOT,
        BASE,
        &[
            &fixture.branch,
            "locked reason",
            "future-attribute something",
        ],
    );
    let observation = build_observation(
        &[&p, &future],
        vec![],
        clean(),
        std::slice::from_ref(&fixture.branch),
        COMMON_DIR,
        Some(BASE),
    );
    let state = classify_reconciliation(&fixture.expected(), &observation, false);
    assert!(matches!(state, Reconciliation::Ambiguous { .. }));
    assert!(!state.permits_bounded_retry());
}

#[test]
fn unknown_attributes_on_any_registered_worktree_are_ambiguous() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let intent = ManagedWorktreeCreationIntent::prepared(
        fixture.record.goal_id(),
        fixture.record.worktree_id().clone(),
        WorktreeOperationId::new(),
        PathBuf::from(COMMON_DIR),
        PathBuf::from(WORKTREE_ROOT),
        BASE.to_owned(),
    )
    .unwrap();
    let expected =
        ExpectedWorktreeTarget::from_record_and_intent(&fixture.record, &intent).unwrap();

    let unknown_primary = record(
        PRIMARY_ROOT,
        BASE,
        &["branch refs/heads/main", "future-attribute primary"],
    );
    let primary_observation = build_observation(
        &[&unknown_primary],
        vec![],
        clean(),
        std::slice::from_ref(&fixture.branch),
        COMMON_DIR,
        Some(BASE),
    );
    assert!(matches!(
        classify_eligibility_pure(
            Path::new(PRIMARY_ROOT),
            &fixture.expected(),
            &primary_observation
        ),
        Eligibility::Ineligible(Ineligibility::Ambiguous(_))
    ));
    let primary_reconciliation = classify_reconciliation_pure(&expected, &primary_observation);
    assert!(matches!(
        primary_reconciliation,
        Reconciliation::Ambiguous { .. }
    ));
    assert!(!primary_reconciliation.permits_bounded_retry());

    let p = primary(BASE);
    let unknown_unrelated = record(
        "/repo/unrelated",
        OTHER,
        &["detached", "future-attribute unrelated"],
    );
    let unrelated_observation = build_observation(
        &[&p, &unknown_unrelated],
        vec![],
        clean(),
        std::slice::from_ref(&fixture.branch),
        COMMON_DIR,
        Some(BASE),
    );
    assert!(matches!(
        classify_eligibility_pure(
            Path::new(PRIMARY_ROOT),
            &fixture.expected(),
            &unrelated_observation
        ),
        Eligibility::Ineligible(Ineligibility::Ambiguous(_))
    ));
    let unrelated_reconciliation = classify_reconciliation_pure(&expected, &unrelated_observation);
    assert!(matches!(
        unrelated_reconciliation,
        Reconciliation::Ambiguous { .. }
    ));
    assert!(!unrelated_reconciliation.permits_bounded_retry());
}

#[test]
fn empty_inventory_is_ambiguous_not_absent() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let observation = build_observation(&[], vec![], clean(), &[], COMMON_DIR, Some(BASE));
    let state = classify_reconciliation(&fixture.expected(), &observation, false);
    assert!(matches!(state, Reconciliation::Ambiguous { .. }));
    assert!(!state.permits_bounded_retry());
}

// --- Pure eligibility classification ---------------------------------------

#[test]
fn clean_top_level_primary_is_eligible() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let p = primary(BASE);
    let observation = build_observation(&[&p], vec![], clean(), &[], COMMON_DIR, Some(BASE))
        .with_ref_observation(fixture.branch.clone(), false);
    let verdict = classify_eligibility(
        Path::new(PRIMARY_ROOT),
        &fixture.expected(),
        &observation,
        false,
    );
    assert_eq!(verdict, Eligibility::Eligible);
    assert!(verdict.is_eligible());
}

#[test]
fn non_top_level_cwd_is_ineligible() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let p = primary(BASE);
    let observation = build_observation(&[&p], vec![], clean(), &[], COMMON_DIR, Some(BASE));
    let subdirectory = PathBuf::from(PRIMARY_ROOT).join("src");
    assert_eq!(
        classify_eligibility(&subdirectory, &fixture.expected(), &observation, false),
        Eligibility::Ineligible(Ineligibility::NonTopLevelCwd {
            top_level: PathBuf::from(PRIMARY_ROOT)
        })
    );
}

#[test]
fn dirty_staged_and_untracked_primary_are_all_ineligible() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    for status in [
        PrimaryWorkspaceStatus {
            tracked_dirty: true,
            staged: false,
            untracked: false,
        },
        PrimaryWorkspaceStatus {
            tracked_dirty: false,
            staged: true,
            untracked: false,
        },
        PrimaryWorkspaceStatus {
            tracked_dirty: false,
            staged: false,
            untracked: true,
        },
    ] {
        let p = primary(BASE);
        let observation = build_observation(&[&p], vec![], status, &[], COMMON_DIR, Some(BASE));
        assert!(
            !classify_eligibility(
                Path::new(PRIMARY_ROOT),
                &fixture.expected(),
                &observation,
                false
            )
            .is_eligible(),
            "dirty primary was treated as eligible: {status:?}"
        );
    }
}

#[test]
fn in_progress_operations_make_the_primary_ineligible() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    for operation in [
        InProgressOperation::Merge,
        InProgressOperation::Rebase,
        InProgressOperation::CherryPick,
        InProgressOperation::Revert,
        InProgressOperation::Bisect,
    ] {
        let p = primary(BASE);
        let observation =
            build_observation(&[&p], vec![operation], clean(), &[], COMMON_DIR, Some(BASE));
        assert_eq!(
            classify_eligibility(
                Path::new(PRIMARY_ROOT),
                &fixture.expected(),
                &observation,
                false
            ),
            Eligibility::Ineligible(Ineligibility::OperationInProgress(vec![operation])),
            "{operation} did not block eligibility"
        );
    }
}

#[test]
fn unborn_head_occupied_path_and_existing_branch_are_ineligible() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let p = primary(BASE);

    // Unresolved HEAD.
    let unresolved = primary(UNRESOLVED_HEAD);
    let observation = build_observation(&[&unresolved], vec![], clean(), &[], COMMON_DIR, None);
    assert_eq!(
        classify_eligibility(
            Path::new(PRIMARY_ROOT),
            &fixture.expected(),
            &observation,
            false
        ),
        Eligibility::Ineligible(Ineligibility::HeadNotACommit)
    );

    // Occupied target path.
    let observation = build_observation(&[&p], vec![], clean(), &[], COMMON_DIR, Some(BASE));
    assert_eq!(
        classify_eligibility(
            Path::new(PRIMARY_ROOT),
            &fixture.expected(),
            &observation,
            true
        ),
        Eligibility::Ineligible(Ineligibility::ExpectedPathOccupied)
    );

    // The managed branch already exists without matching durable ownership.
    let observation = build_observation(
        &[&p],
        vec![],
        clean(),
        std::slice::from_ref(&fixture.branch),
        COMMON_DIR,
        Some(BASE),
    );
    assert_eq!(
        classify_eligibility(
            Path::new(PRIMARY_ROOT),
            &fixture.expected(),
            &observation,
            false
        ),
        Eligibility::Ineligible(Ineligibility::ExpectedBranchExistsWithoutOwnership)
    );
}

#[test]
fn ambiguous_observation_is_never_eligible() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let observation = build_observation(&[], vec![], clean(), &[], COMMON_DIR, Some(BASE));
    let verdict = classify_eligibility(
        Path::new(PRIMARY_ROOT),
        &fixture.expected(),
        &observation,
        false,
    );
    assert!(matches!(
        verdict,
        Eligibility::Ineligible(Ineligibility::Ambiguous(_))
    ));
    assert!(!verdict.is_eligible());
}

#[test]
fn unobserved_expected_ref_is_not_eligible() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let p = primary(BASE);
    let observation = build_observation(&[&p], vec![], clean(), &[], COMMON_DIR, Some(BASE));
    assert_eq!(observation.ref_exists(&fixture.branch), None);
    assert!(matches!(
        classify_eligibility_pure(Path::new(PRIMARY_ROOT), &fixture.expected(), &observation),
        Eligibility::Ineligible(Ineligibility::Ambiguous(_))
    ));
}

#[test]
fn unknown_expected_path_observation_is_not_eligible() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let p = primary(BASE);
    let observation = build_observation(&[&p], vec![], clean(), &[], COMMON_DIR, Some(BASE))
        .with_path_observation(PathBuf::from(WORKTREE_ROOT), PathObservation::Unknown);
    assert_eq!(
        classify_eligibility_pure(Path::new(PRIMARY_ROOT), &fixture.expected(), &observation),
        Eligibility::Ineligible(Ineligibility::ExpectedPathUnknown)
    );
}

#[test]
fn eligibility_rejects_lifecycles_without_creation_authority() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let p = primary(BASE);
    let observation = build_observation(
        &[&p],
        vec![],
        clean(),
        std::slice::from_ref(&fixture.branch),
        COMMON_DIR,
        Some(BASE),
    );

    for lifecycle in [
        "PREPARED",
        "ACTIVE",
        "CLEANUP_ELIGIBLE",
        "BLOCKED",
        "REMOVED",
    ] {
        let record = fixture.record_with_lifecycle(lifecycle);
        let verdict = classify_eligibility_pure(
            Path::new(PRIMARY_ROOT),
            &ExpectedWorktreeTarget::from_record(&record),
            &observation,
        );
        assert_eq!(
            verdict,
            Eligibility::Ineligible(Ineligibility::LifecycleDoesNotPermitCreation(
                record.lifecycle()
            )),
            "{lifecycle} unexpectedly passed creation eligibility"
        );
    }

    let blocked = fixture.record_with_lifecycle("BLOCKED");
    let blocked_intent = ManagedWorktreeCreationIntent::prepared(
        blocked.goal_id(),
        blocked.worktree_id().clone(),
        WorktreeOperationId::new(),
        PathBuf::from(COMMON_DIR),
        PathBuf::from(WORKTREE_ROOT),
        BASE.to_owned(),
    )
    .unwrap();
    let blocked_expected =
        ExpectedWorktreeTarget::from_record_and_intent(&blocked, &blocked_intent).unwrap();
    assert_eq!(
        classify_eligibility_pure(Path::new(PRIMARY_ROOT), &blocked_expected, &observation),
        Eligibility::Ineligible(Ineligibility::LifecycleDoesNotPermitCreation(
            crate::managed_worktree::ManagedWorktreeLifecycle::Blocked
        ))
    );

    let prepared = fixture.record_with_lifecycle("PREPARED");
    let intent = ManagedWorktreeCreationIntent::prepared(
        prepared.goal_id(),
        prepared.worktree_id().clone(),
        WorktreeOperationId::new(),
        PathBuf::from(COMMON_DIR),
        PathBuf::from(WORKTREE_ROOT),
        BASE.to_owned(),
    )
    .unwrap();
    let expected = ExpectedWorktreeTarget::from_record_and_intent(&prepared, &intent).unwrap();
    let observation = observation.with_ref_observation(expected.branch_ref().to_owned(), false);
    assert_eq!(
        classify_eligibility_pure(Path::new(PRIMARY_ROOT), &expected, &observation),
        Eligibility::Eligible
    );
}

#[test]
fn expected_target_is_derived_from_the_durable_record_only() {
    let fixture = Fixture::new(WORKTREE_ROOT);
    let expected = fixture.expected();
    assert_eq!(expected.worktree_root(), Path::new(WORKTREE_ROOT));
    assert_eq!(expected.branch_ref(), fixture.branch);
    assert_eq!(expected.base_commit(), BASE);
    assert_eq!(expected.repository_common_dir(), Path::new(COMMON_DIR));
    // Deterministic and observation-free.
    assert_eq!(
        expected,
        ExpectedWorktreeTarget::from_record(&fixture.record)
    );
}

// --- Integration against real temporary repositories ------------------------

/// A temporary Git repository owned exclusively by one test.
///
/// Every integration fixture creates its own repository under the system temp
/// directory and removes it on drop. The developer's real linked worktrees are
/// never inspected, modified, or removed.
#[cfg(not(windows))]
struct TempRepo {
    root: PathBuf,
}

#[cfg(not(windows))]
impl Drop for TempRepo {
    fn drop(&mut self) {
        // Scoped to the fixture's own temp directory only.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(not(windows))]
impl TempRepo {
    fn new(label: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("local-mcp-mw-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let repo = Self { root };
        repo.git(&["init", "-q", "-b", "main", "."]);
        repo.git(&["config", "user.email", "test@example.invalid"]);
        repo.git(&["config", "user.name", "Managed Worktrees Test"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.root.join("a.txt"), b"seed\n").unwrap();
        repo.git(&["add", "a.txt"]);
        repo.git(&["commit", "-qm", "seed"]);
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap_or_else(|error| panic!("git {args:?} failed to start: {error}"));
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Run a command that is expected to fail, such as a conflicting merge.
    fn git_allow_failure(&self, args: &[&str]) {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap_or_else(|error| panic!("git {args:?} failed to start: {error}"));
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"]).trim().to_owned()
    }

    fn common_dir(&self) -> PathBuf {
        PathBuf::from(
            self.git(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
                .trim()
                .to_owned(),
        )
    }
}

/// A durable record plus the host-derived branch ref it owns.
///
/// The branch is a pure function of the Goal ID, so the caller must not invent
/// one: it is returned alongside the record so the two always agree.
#[cfg(not(windows))]
fn record_for(
    primary_root: &Path,
    worktree_root: &Path,
    base_commit: &str,
    common_dir: &Path,
) -> (ManagedWorktreeRecord, String) {
    let goal = Goal::new(
        "session-1",
        primary_root.to_path_buf(),
        "managed objective",
        None,
        vec![],
        vec!["all checks pass".into()],
        NOW,
    )
    .unwrap();
    let branch = managed_branch_ref(goal.id());
    let record = ManagedWorktreeRecord::requested(
        goal.id(),
        WorktreeId::new(),
        primary_root.to_path_buf(),
        common_dir.to_path_buf(),
        worktree_root.to_path_buf(),
        base_commit.to_owned(),
        None,
        goal.revision(),
        0,
    )
    .unwrap();
    (record, branch)
}

#[cfg(not(windows))]
fn active_expected_from_record(record: &ManagedWorktreeRecord) -> ExpectedWorktreeTarget {
    let mut serialized = serde_json::to_value(record).unwrap();
    serialized["lifecycle"] = serde_json::json!("ACTIVE");
    serialized["last_reconciled_head"] = serde_json::json!(record.base_commit());
    serialized["last_reconciled_at"] = serde_json::json!(NOW);
    let active: ManagedWorktreeRecord = serde_json::from_value(serialized).unwrap();
    active.validate().unwrap();
    ExpectedWorktreeTarget::from_record(&active)
}

#[cfg(not(windows))]
#[test]
fn real_repository_is_observed_read_only() {
    let repo = TempRepo::new("observe");
    let head = repo.head();
    let git = HostGit::new();
    let observation = observe_repository(&git, &repo.root, &[]).unwrap();

    assert_eq!(
        std::fs::canonicalize(observation.top_level()).unwrap(),
        std::fs::canonicalize(&repo.root).unwrap()
    );
    assert_eq!(
        std::fs::canonicalize(observation.common_dir()).unwrap(),
        std::fs::canonicalize(repo.common_dir()).unwrap()
    );
    assert_eq!(observation.head(), Some(head.as_str()));
    assert_eq!(observation.head_ref(), Some("refs/heads/main"));
    assert_eq!(observation.inventory().entries().len(), 1);
    assert!(observation.in_progress().is_empty());
    assert!(observation.status().is_clean());
    assert!(observation.is_trustworthy().is_ok());
}

#[cfg(not(windows))]
#[test]
fn real_linked_worktree_is_observed_and_classified() {
    let repo = TempRepo::new("linked");
    let head = repo.head();
    let worktree =
        std::env::temp_dir().join(format!("local-mcp-mw-linked-wt-{}", uuid::Uuid::new_v4()));
    // The branch is host-derived from the durable record, not chosen here.
    let (record, branch) = record_for(&repo.root, &worktree, &head, &repo.common_dir());
    repo.git(&[
        "worktree",
        "add",
        "--lock",
        "--reason",
        record.lock_reason(),
        "-b",
        branch.strip_prefix("refs/heads/").unwrap(),
        worktree.to_str().unwrap(),
        &head,
    ]);

    let git = HostGit::new();
    let observation = observe_repository_with_paths(
        &git,
        &repo.root,
        std::slice::from_ref(&branch),
        &[worktree.as_path()],
    )
    .unwrap();
    assert_eq!(observation.inventory().entries().len(), 2);
    let linked = observation
        .inventory()
        .entries()
        .iter()
        .find(|entry| entry.path() == worktree.as_path())
        .expect("linked worktree must be observed");
    assert_eq!(linked.branch_ref(), Some(branch.as_str()));
    assert_eq!(linked.resolved_head(), Some(head.as_str()));
    assert!(linked.is_locked());
    assert_eq!(linked.lock_reason(), Some(record.lock_reason()));
    assert_eq!(observation.ref_exists(&branch), Some(true));

    // An exactly matching durable record reconciles to ACTIVE_EXACT.
    let expected = active_expected_from_record(&record);
    assert_eq!(
        classify_reconciliation(&expected, &observation, false),
        Reconciliation::ActiveExact { head: head.clone() }
    );

    // Removing only the fixture's own directory leaves prunable metadata, which
    // blocks rather than permitting a destructive prune.
    std::fs::remove_dir_all(&worktree).unwrap();
    let observation = observe_repository_with_paths(
        &git,
        &repo.root,
        std::slice::from_ref(&branch),
        &[worktree.as_path()],
    )
    .unwrap();
    let state = classify_reconciliation(&expected, &observation, false);
    assert!(
        matches!(
            state,
            Reconciliation::PrunableMetadata { .. } | Reconciliation::MissingPath
        ),
        "stale metadata classified as {state:?}"
    );
    assert!(!state.permits_bounded_retry());
    assert!(state.requires_explicit_recovery());
}

#[cfg(not(windows))]
#[test]
fn real_dirty_primary_and_merge_in_progress_block_eligibility() {
    let repo = TempRepo::new("dirty");
    let head = repo.head();
    let worktree =
        std::env::temp_dir().join(format!("local-mcp-mw-dirty-wt-{}", uuid::Uuid::new_v4()));
    let (record, branch) = record_for(&repo.root, &worktree, &head, &repo.common_dir());
    let expected = ExpectedWorktreeTarget::from_record(&record);
    let git = HostGit::new();

    // A dirty tracked file blocks.
    std::fs::write(repo.root.join("a.txt"), b"modified\n").unwrap();
    let observation = observe_repository_with_paths(
        &git,
        &repo.root,
        std::slice::from_ref(&branch),
        &[worktree.as_path()],
    )
    .unwrap();
    assert!(!observation.status().is_clean());
    assert!(!classify_eligibility(&repo.root, &expected, &observation, false).is_eligible());
    repo.git(&["checkout", "--", "a.txt"]);

    // An untracked file blocks too: the design never guesses whether untracked
    // user files belong to the intended base.
    std::fs::write(repo.root.join("untracked.txt"), b"stray\n").unwrap();
    let observation = observe_repository_with_paths(
        &git,
        &repo.root,
        std::slice::from_ref(&branch),
        &[worktree.as_path()],
    )
    .unwrap();
    assert!(observation.status().untracked);
    assert!(!classify_eligibility(&repo.root, &expected, &observation, false).is_eligible());
    std::fs::remove_file(repo.root.join("untracked.txt")).unwrap();

    // A real merge conflict puts MERGE_HEAD in place and blocks.
    std::fs::write(repo.root.join("conflict.txt"), b"base\n").unwrap();
    repo.git(&["add", "conflict.txt"]);
    repo.git(&["commit", "-qm", "base"]);
    repo.git(&["checkout", "-q", "-b", "side"]);
    std::fs::write(repo.root.join("conflict.txt"), b"side\n").unwrap();
    repo.git(&["commit", "-qam", "side change"]);
    repo.git(&["checkout", "-q", "main"]);
    std::fs::write(repo.root.join("conflict.txt"), b"main\n").unwrap();
    repo.git(&["commit", "-qam", "main change"]);
    repo.git_allow_failure(&["merge", "side"]);
    let observation = observe_repository_with_paths(
        &git,
        &repo.root,
        std::slice::from_ref(&branch),
        &[worktree.as_path()],
    )
    .unwrap();
    assert_eq!(
        observation.in_progress(),
        [InProgressOperation::Merge],
        "merge in progress was not observed"
    );
    assert!(!classify_eligibility(&repo.root, &expected, &observation, false).is_eligible());
    repo.git(&["merge", "--abort"]);

    // Once the primary HEAD advances, a fresh host-owned observation/target
    // binding is required before eligibility can be considered again.
    let latest_head = repo.head();
    let (latest_record, latest_branch) =
        record_for(&repo.root, &worktree, &latest_head, &repo.common_dir());
    let latest_expected = ExpectedWorktreeTarget::from_record(&latest_record);
    let observation = observe_repository_with_paths(
        &git,
        &repo.root,
        std::slice::from_ref(&latest_branch),
        &[worktree.as_path()],
    )
    .unwrap();
    assert!(observation.status().is_clean());
    assert!(classify_eligibility(&repo.root, &latest_expected, &observation, false).is_eligible());
}

#[cfg(not(windows))]
#[test]
fn real_rebase_in_progress_is_detected_from_operation_metadata() {
    let repo = TempRepo::new("rebase");
    repo.git(&["checkout", "-q", "-b", "topic"]);
    std::fs::write(repo.root.join("a.txt"), b"topic\n").unwrap();
    repo.git(&["commit", "-qam", "topic change"]);
    repo.git(&["checkout", "-q", "main"]);
    std::fs::write(repo.root.join("a.txt"), b"main\n").unwrap();
    repo.git(&["commit", "-qam", "main change"]);
    repo.git(&["checkout", "-q", "topic"]);
    repo.git_allow_failure(&["rebase", "main"]);

    let nested = repo.root.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let observation = observe_repository(&HostGit::new(), &nested, &[]).unwrap();
    assert_eq!(observation.in_progress(), [InProgressOperation::Rebase]);
    repo.git(&["rebase", "--abort"]);
}

#[cfg(not(windows))]
#[test]
fn real_unborn_head_repository_is_trustworthy_but_ineligible() {
    let empty = std::env::temp_dir().join(format!("local-mcp-mw-unborn-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&empty).unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&empty)
        .output()
        .unwrap();
    assert!(init.status.success());

    let git = HostGit::new();
    let observation = observe_repository(&git, &empty, &[]).unwrap();
    assert_eq!(observation.head(), None);
    assert!(observation.is_trustworthy().is_ok());
    // An unborn HEAD is a definite observation, but not an eligible base commit.
    let (record, _) = record_for(
        &empty,
        Path::new(WORKTREE_ROOT),
        BASE,
        observation.common_dir(),
    );
    let expected = ExpectedWorktreeTarget::from_record(&record);
    assert_eq!(
        classify_eligibility(&empty, &expected, &observation, false),
        Eligibility::Ineligible(Ineligibility::HeadNotACommit)
    );
    let _ = std::fs::remove_dir_all(&empty);
}

#[cfg(not(windows))]
#[test]
fn observing_a_non_repository_fails_closed() {
    let directory =
        std::env::temp_dir().join(format!("local-mcp-mw-notrepo-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let git = HostGit::new();
    let result = observe_repository(&git, &directory, &[]);
    assert!(
        result.is_err(),
        "a non-repository must not observe successfully"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[cfg(not(windows))]
#[test]
fn observing_from_a_subdirectory_reports_the_canonical_top_level() {
    let repo = TempRepo::new("subdir");
    let nested = repo.root.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    let git = HostGit::new();
    let observation = observe_repository(&git, &nested, &[]).unwrap();
    assert_eq!(
        std::fs::canonicalize(observation.top_level()).unwrap(),
        std::fs::canonicalize(&repo.root).unwrap()
    );
    // A nested cwd is therefore ineligible for managed creation in V1.
    let worktree =
        std::env::temp_dir().join(format!("local-mcp-mw-subdir-wt-{}", uuid::Uuid::new_v4()));
    let (record, _branch) = record_for(&repo.root, &worktree, &repo.head(), &repo.common_dir());
    let expected = ExpectedWorktreeTarget::from_record(&record);
    assert!(matches!(
        classify_eligibility(&nested, &expected, &observation, false),
        Eligibility::Ineligible(Ineligibility::NonTopLevelCwd { .. })
    ));
}
