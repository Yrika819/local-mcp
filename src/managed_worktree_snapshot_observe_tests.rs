//! Slice 2A tests: pure parser/contract tests plus real temporary-Git
//! repository tests. No filesystem hashing, no `cat-file`, no manifest
//! assembly, no coherence logic is exercised here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::managed_worktree_snapshot_observe::{
    ALL_SNAPSHOT_QUERIES, GitFileModePolicy, SnapshotCommandOutput, SnapshotGit, SnapshotGitQuery,
    SnapshotIndexEntry, SnapshotIndexMode, SnapshotMetadataError, check_index_flags,
    enforce_complete_process_output, observe_snapshot_metadata, parse_file_mode_policy, parse_head,
    parse_index_stage, parse_nul_name_set, parse_status, snapshot_argv,
};
use crate::workspace_snapshot::NormalizedWorkspacePath;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn path(value: &str) -> NormalizedWorkspacePath {
    NormalizedWorkspacePath::parse(value.to_owned()).unwrap()
}

fn ok_output(stdout: Vec<u8>) -> SnapshotCommandOutput {
    SnapshotCommandOutput {
        stdout,
        stderr: String::new(),
        exit_code: 0,
    }
}

fn failed_output(query_exit: i32, stderr: &str) -> SnapshotCommandOutput {
    SnapshotCommandOutput {
        stdout: Vec::new(),
        stderr: stderr.to_owned(),
        exit_code: query_exit,
    }
}

/// A deterministic [`SnapshotGit`] double keyed by query.
#[derive(Default)]
struct FakeSnapshotGit {
    outputs: HashMap<SnapshotGitQuery, SnapshotCommandOutput>,
}

impl FakeSnapshotGit {
    fn with(mut self, query: SnapshotGitQuery, output: SnapshotCommandOutput) -> Self {
        self.outputs.insert(query, output);
        self
    }

    fn clean(head: &str) -> Self {
        Self::default()
            .with(
                SnapshotGitQuery::Head,
                ok_output(format!("{head}\n").into_bytes()),
            )
            .with(SnapshotGitQuery::FileMode, ok_output(b"true\n".to_vec()))
            .with(SnapshotGitQuery::Status, ok_output(Vec::new()))
            .with(SnapshotGitQuery::IndexStage, ok_output(Vec::new()))
            .with(SnapshotGitQuery::IndexFlags, ok_output(Vec::new()))
            .with(SnapshotGitQuery::StagedNamesNoIta, ok_output(Vec::new()))
            .with(
                SnapshotGitQuery::StagedNamesVisibleIta,
                ok_output(Vec::new()),
            )
    }
}

impl SnapshotGit for FakeSnapshotGit {
    fn run(
        &self,
        query: SnapshotGitQuery,
        _cwd: &Path,
    ) -> Result<SnapshotCommandOutput, SnapshotMetadataError> {
        self.outputs.get(&query).cloned().ok_or_else(|| {
            SnapshotMetadataError::GitObservationUnavailable(format!(
                "fake has no output for {}",
                query.label()
            ))
        })
    }
}

const HEAD_A: &str = "0123456789abcdef0123456789abcdef01234567";
const HEAD_64: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OID_40: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OID_64: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn stage_record(mode: &str, oid: &str, stage: &str, path: &str) -> Vec<u8> {
    format!("{mode} {oid} {stage}\t{path}\0").into_bytes()
}

// ---------------------------------------------------------------------------
// Closed query enum + envelope
// ---------------------------------------------------------------------------

#[test]
fn all_snapshot_queries_cover_seven_closed_variants() {
    assert_eq!(ALL_SNAPSHOT_QUERIES.len(), 7);
    assert!(ALL_SNAPSHOT_QUERIES.contains(&SnapshotGitQuery::FileMode));
    for query in ALL_SNAPSHOT_QUERIES {
        assert!(!query.tail().is_empty());
        assert!(!query.label().is_empty());
    }
}

#[test]
fn file_mode_parser_accepts_only_canonical_git_boolean_and_unset_default() {
    assert_eq!(
        parse_file_mode_policy(&ok_output(b"true\n".to_vec())).unwrap(),
        GitFileModePolicy::TrustExecutableBit
    );
    assert_eq!(
        parse_file_mode_policy(&ok_output(b"false\n".to_vec())).unwrap(),
        GitFileModePolicy::IgnoreExecutableBit
    );
    assert_eq!(
        parse_file_mode_policy(&failed_output(1, "")).unwrap(),
        GitFileModePolicy::TrustExecutableBit
    );
    for stdout in [
        b"yes\n".as_slice(),
        b"true",
        b"true\nfalse\n",
        b"true\n\n",
        b"TRUE\n",
    ] {
        assert!(
            parse_file_mode_policy(&ok_output(stdout.to_vec())).is_err(),
            "{stdout:?}"
        );
    }
    for output in [
        failed_output(2, "fatal"),
        SnapshotCommandOutput {
            stdout: b"false\n".to_vec(),
            stderr: String::new(),
            exit_code: 1,
        },
    ] {
        assert!(parse_file_mode_policy(&output).is_err());
    }
}

#[test]
fn snapshot_argv_is_host_shaped_and_read_only() {
    let host_git = crate::execution::host_git_path().unwrap();
    for query in ALL_SNAPSHOT_QUERIES {
        let argv = snapshot_argv(query).unwrap();
        assert_eq!(argv[0], host_git.to_str().unwrap(), "{query:?}");
        let has_pair = |key: &str, value: &str| {
            argv.windows(2)
                .any(|pair| pair[0] == key && pair[1] == value)
        };
        // Replace objects are disabled for every snapshot query.
        assert!(
            argv.contains(&"--no-replace-objects".to_owned()),
            "{query:?}"
        );
        // Hooks cannot run and an external filesystem monitor is disabled.
        assert!(
            has_pair(
                "-c",
                &format!("core.hooksPath={}", crate::sandbox::null_device_path())
            ),
            "{query:?} did not disable hooks"
        );
        assert!(has_pair("-c", "core.fsmonitor=false"), "{query:?}");
        assert!(argv.contains(&"--no-pager".to_owned()), "{query:?}");
        assert!(
            !argv
                .windows(2)
                .any(|pair| pair[0] == "-c" && pair[1].starts_with("core.pager")),
            "{query:?} must not override core.pager at all; --no-pager wins"
        );
        assert!(
            argv.contains(&"--no-optional-locks".to_owned()),
            "{query:?}"
        );
        // The exact host-selected subcommand tail, and nothing else.
        let tail = argv[argv.len() - query.tail().len()..].to_vec();
        assert_eq!(tail, query.tail().to_vec(), "{query:?}");
        // No observation can reach a remote, mutate, or run a pager.
        for forbidden in [
            "fetch", "push", "pull", "clone", "commit", "add", "reset", "clean", "stash", "merge",
            "rebase", "checkout", "prune", "gc",
        ] {
            assert!(
                !argv.contains(&forbidden.to_owned()),
                "{query:?} can {forbidden}"
            );
        }
    }
}

#[test]
fn status_tail_disables_renames_and_ignores_submodules() {
    let tail = SnapshotGitQuery::Status.tail().to_vec();
    assert!(tail.contains(&"--no-renames"));
    assert!(tail.contains(&"--ignore-submodules=all"));
    assert!(tail.contains(&"--untracked-files=all"));
}

#[test]
fn staged_name_tails_differ_only_in_ita_visibility() {
    let hidden = SnapshotGitQuery::StagedNamesNoIta.tail().to_vec();
    let visible = SnapshotGitQuery::StagedNamesVisibleIta.tail().to_vec();
    assert!(hidden.contains(&"--ita-invisible-in-index"));
    assert!(visible.contains(&"--ita-visible-in-index"));
    assert!(hidden.contains(&"--no-ext-diff"));
    assert!(hidden.contains(&"--no-textconv"));
    assert!(hidden.contains(&"--no-renames"));
    assert!(hidden.contains(&"--ignore-submodules=all"));
    let normalize: Vec<&&str> = hidden
        .iter()
        .filter(|arg| !arg.starts_with("--ita-"))
        .collect();
    let visible_normalize: Vec<&&str> = visible
        .iter()
        .filter(|arg| !arg.starts_with("--ita-"))
        .collect();
    assert_eq!(normalize, visible_normalize);
}

// ---------------------------------------------------------------------------
// HEAD parser
// ---------------------------------------------------------------------------

#[test]
fn head_accepts_40_and_64_char_lowercase_ids() {
    assert_eq!(
        parse_head(format!("{HEAD_A}\n").as_bytes()).unwrap(),
        HEAD_A
    );
    assert_eq!(
        parse_head(format!("{HEAD_64}\n").as_bytes()).unwrap(),
        HEAD_64
    );
}

#[test]
fn head_rejects_malformed_output() {
    for malformed in [
        b"".as_slice(),
        b"abc\ndef\n",
        b"0123456789abcdef0123456789abcdef01234567",
        b"0123456789ABCDEF0123456789ABCDEF01234567\n",
        b"0123456789abcdef0123456789abcdef0123456\n",
        b"0123456789abcdef0123456789abcdef012345678\n",
        b"\n",
        b"HEAD\n",
    ] {
        assert!(parse_head(malformed).is_err(), "{malformed:?}");
    }
}

#[test]
fn head_rejects_all_zero_object_id() {
    let zero40 = "0".repeat(40);
    let error = parse_head(format!("{zero40}\n").as_bytes()).unwrap_err();
    assert!(matches!(
        error,
        SnapshotMetadataError::UnsupportedGitState(_)
    ));
}

// ---------------------------------------------------------------------------
// Status parser
// ---------------------------------------------------------------------------

#[test]
fn status_empty_means_clean_sets() {
    let sets = parse_status(b"").unwrap();
    assert!(sets.changed.is_empty() && sets.staged.is_empty() && sets.untracked.is_empty());
}

#[test]
fn status_classifies_staged_unstaged_and_untracked() {
    let sets = parse_status(b"M  staged.txt\0 M worktree.txt\0MM both.txt\0?? new.txt\0").unwrap();
    assert_eq!(sets.staged, vec![path("both.txt"), path("staged.txt")]);
    assert_eq!(sets.changed, vec![path("both.txt"), path("worktree.txt")]);
    assert_eq!(sets.untracked, vec![path("new.txt")]);
}

#[test]
fn status_accepts_add_delete_and_typechange() {
    let sets = parse_status(
        b"A  added.txt\0D  staged-gone.txt\0T  staged-type.txt\0 D worktree-gone.txt\0 T worktree-type.txt\0",
    )
    .unwrap();
    assert_eq!(
        sets.staged,
        vec![
            path("added.txt"),
            path("staged-gone.txt"),
            path("staged-type.txt")
        ]
    );
    assert_eq!(
        sets.changed,
        vec![path("worktree-gone.txt"), path("worktree-type.txt")]
    );
}

#[test]
fn status_rejects_unmerged_entries() {
    for record in [
        b"UU conflict.txt\0".as_slice(),
        b"AA conflict.txt\0",
        b"DD conflict.txt\0",
        b"UD conflict.txt\0",
        b"DU conflict.txt\0",
        b"AU conflict.txt\0",
        b"UA conflict.txt\0",
    ] {
        assert!(
            matches!(
                parse_status(record),
                Err(SnapshotMetadataError::UnsupportedGitState(_))
            ),
            "{record:?}"
        );
    }
}

#[test]
fn status_accepts_added_then_modified_or_deleted() {
    // `AM` (added, then worktree-modified) and `AD` (added, then
    // worktree-deleted) are ordinary resolvable states, not conflicts.
    let sets = parse_status(b"AM new.txt\0AD gone.txt\0").unwrap();
    assert_eq!(sets.staged, vec![path("gone.txt"), path("new.txt")]);
    assert_eq!(sets.changed, vec![path("gone.txt"), path("new.txt")]);
}

#[test]
fn status_rejects_rename_and_copy_bytes() {
    for record in [b"R  new.txt\0".as_slice(), b" R new.txt\0", b"C  new.txt\0"] {
        assert!(
            matches!(
                parse_status(record),
                Err(SnapshotMetadataError::UnsupportedGitState(_))
            ),
            "{record:?}"
        );
    }
}

#[test]
fn status_rejects_worktree_side_added_as_ita_tripwire() {
    // Only the worktree-side `A` is the ITA tripwire (`AA` is an unmerged
    // state covered by `status_rejects_unmerged_entries`; `AM`/`AD` are
    // ordinary states covered by
    // `status_accepts_added_then_modified_or_deleted`).
    assert!(matches!(
        parse_status(b" A ita.txt\0"),
        Err(SnapshotMetadataError::UnsupportedGitState(_))
    ));
}

#[test]
fn status_rejects_ignored_entries_and_malformed_records() {
    for malformed in [
        b"!! ignored.txt\0".as_slice(),
        b"XX file.txt\0",
        b"   file.txt\0",
        b"M file.txt",
        b"M \0",
        b"M  dup.txt\0M  dup.txt\0",
        b"M  a.txt\0\0",
        b"M ",
    ] {
        assert!(parse_status(malformed).is_err(), "{malformed:?}");
    }
}

#[test]
fn status_rejects_non_utf8_and_invalid_paths() {
    assert!(matches!(
        parse_status(b" M \xff\xfe.txt\0"),
        Err(SnapshotMetadataError::InvalidPath(_))
    ));
    assert!(matches!(
        parse_status(b" M ../escape.txt\0"),
        Err(SnapshotMetadataError::InvalidPath(_))
    ));
    assert!(matches!(
        parse_status(b" M /absolute.txt\0"),
        Err(SnapshotMetadataError::InvalidPath(_))
    ));
}

#[test]
fn status_enforces_path_byte_limit() {
    let long = "a".repeat(4 * 1024 + 1);
    let record = format!("M  {long}\0").into_bytes();
    assert!(matches!(
        parse_status(&record),
        Err(SnapshotMetadataError::LimitExceeded(_))
    ));
}

#[test]
fn status_enforces_changed_count_limit() {
    let mut bytes = Vec::new();
    for index in 0..=20_000 {
        bytes.extend_from_slice(format!(" M p{index:05}\0").as_bytes());
    }
    assert!(matches!(
        parse_status(&bytes),
        Err(SnapshotMetadataError::LimitExceeded(_))
    ));
}

// ---------------------------------------------------------------------------
// Index stage parser
// ---------------------------------------------------------------------------

fn stage_entries(stdout: &[u8]) -> Vec<SnapshotIndexEntry> {
    parse_index_stage(stdout).unwrap()
}

#[test]
fn index_stage_accepts_regular_executable_and_symlink() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&stage_record("100644", OID_40, "0", "plain.txt"));
    bytes.extend_from_slice(&stage_record("100755", OID_40, "0", "run.sh"));
    bytes.extend_from_slice(&stage_record("120000", OID_40, "0", "link"));
    let entries = stage_entries(&bytes);
    assert_eq!(entries.len(), 3);
    let by_path = |name: &str| {
        entries
            .iter()
            .find(|entry| entry.path == path(name))
            .unwrap_or_else(|| panic!("{name} is indexed"))
    };
    assert_eq!(
        by_path("plain.txt").mode,
        SnapshotIndexMode::Regular { executable: false }
    );
    assert_eq!(
        by_path("run.sh").mode,
        SnapshotIndexMode::Regular { executable: true }
    );
    assert_eq!(by_path("link").mode, SnapshotIndexMode::Symlink);
    assert_eq!(by_path("plain.txt").oid, OID_40);
}

#[test]
fn index_stage_accepts_64_char_oids() {
    let entries = stage_entries(&stage_record("100644", OID_64, "0", "f.txt"));
    assert_eq!(entries[0].oid, OID_64);
}

#[test]
fn index_stage_rejects_gitlink_and_unknown_modes() {
    for mode in ["160000", "100666", "040000", "100600", "120001"] {
        let error = parse_index_stage(&stage_record(mode, OID_40, "0", "f.txt")).unwrap_err();
        assert!(
            matches!(error, SnapshotMetadataError::UnsupportedGitState(_)),
            "{mode}"
        );
    }
}

#[test]
fn index_stage_rejects_nonzero_stages() {
    for stage in ["1", "2", "3"] {
        let error = parse_index_stage(&stage_record("100644", OID_40, stage, "f.txt")).unwrap_err();
        assert!(
            matches!(error, SnapshotMetadataError::UnsupportedGitState(_)),
            "{stage}"
        );
    }
}

#[test]
fn index_stage_rejects_bad_oids_and_separators() {
    assert!(parse_index_stage(&stage_record("100644", &"A".repeat(40), "0", "f.txt")).is_err());
    assert!(parse_index_stage(&stage_record("100644", &"a".repeat(39), "0", "f.txt")).is_err());
    assert!(parse_index_stage(&stage_record("100644", &"a".repeat(41), "0", "f.txt")).is_err());
    assert!(parse_index_stage(b"100644 aaaa 0 f.txt\0").is_err());
    assert!(parse_index_stage(format!("100644 {OID_40} 0 f.txt\0").as_bytes()).is_err());
    assert!(parse_index_stage(format!("100644 {OID_40}\tf.txt\0").as_bytes()).is_err());
    assert!(parse_index_stage(b"100644 aaaa 0 \0").is_err());
    assert!(parse_index_stage(b"100644 aaaa 0 f.txt").is_err());
    assert!(parse_index_stage(b"100644 aaaa 0 f.txt\0\0").is_err());
    let mut duplicate = stage_record("100644", OID_40, "0", "a.txt");
    duplicate.extend_from_slice(&stage_record("100644", OID_40, "0", "a.txt"));
    assert!(parse_index_stage(&duplicate).is_err());
}

#[test]
fn index_stage_rejects_non_utf8_and_invalid_paths() {
    let mut record = format!("100644 {OID_40} 0\t").into_bytes();
    record.extend_from_slice(b"\xff\xfe");
    record.push(0);
    assert!(matches!(
        parse_index_stage(&record),
        Err(SnapshotMetadataError::InvalidPath(_))
    ));
}

// ---------------------------------------------------------------------------
// Index flags parser
// ---------------------------------------------------------------------------

fn flags_output(tag: u8, path: &str) -> Vec<u8> {
    let mut bytes = vec![tag, b' '];
    bytes.extend_from_slice(path.as_bytes());
    bytes.push(0);
    bytes
}

fn stage_for(path: &str) -> Vec<SnapshotIndexEntry> {
    vec![SnapshotIndexEntry {
        path: self::path(path),
        mode: SnapshotIndexMode::Regular { executable: false },
        oid: OID_40.to_owned(),
    }]
}

#[test]
fn index_flags_accepts_normal_tracked_tag() {
    check_index_flags(&flags_output(b'H', "f.txt"), &stage_for("f.txt")).unwrap();
}

#[test]
fn index_flags_rejects_skip_worktree_and_assume_unchanged() {
    for tag in [b'S', b'h', b's', b'm', b'r', b'c', b'k'] {
        let error =
            check_index_flags(&flags_output(tag, "f.txt"), &stage_for("f.txt")).unwrap_err();
        assert!(
            matches!(error, SnapshotMetadataError::UnsupportedGitState(_)),
            "tag {}",
            tag as char
        );
    }
}

#[test]
fn index_flags_rejects_unmerged_and_unknown_tags() {
    for tag in [b'M', b'R', b'K', b'?', b'Z', b'H' + 1] {
        let error =
            check_index_flags(&flags_output(tag, "f.txt"), &stage_for("f.txt")).unwrap_err();
        assert!(
            matches!(error, SnapshotMetadataError::UnsupportedGitState(_)),
            "tag {}",
            tag as char
        );
    }
}

#[test]
fn index_flags_rejects_coverage_mismatch() {
    // Flagged path missing from the stage scan.
    assert!(check_index_flags(&flags_output(b'H', "other.txt"), &stage_for("f.txt")).is_err());
    // Stage path missing from the flags scan.
    assert!(check_index_flags(&flags_output(b'H', "f.txt"), &[]).is_err());
    // Malformed flag records.
    assert!(check_index_flags(b"Hf.txt\0", &stage_for("f.txt")).is_err());
    assert!(check_index_flags(b"H \0", &stage_for("f.txt")).is_err());
    assert!(check_index_flags(b"H f.txt", &stage_for("f.txt")).is_err());
}

// ---------------------------------------------------------------------------
// Staged cross-check (ITA detection contract)
// ---------------------------------------------------------------------------

fn staged_name_bytes(paths: &[&str]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for path in paths {
        bytes.extend_from_slice(path.as_bytes());
        bytes.push(0);
    }
    bytes
}

#[test]
fn staged_cross_check_accepts_agreeing_sets() {
    let git = FakeSnapshotGit::clean(HEAD_A)
        .with(
            SnapshotGitQuery::Status,
            ok_output(b"M  staged.txt\0".to_vec()),
        )
        .with(
            SnapshotGitQuery::IndexStage,
            ok_output(stage_record("100644", OID_40, "0", "staged.txt")),
        )
        .with(
            SnapshotGitQuery::IndexFlags,
            ok_output(flags_output(b'H', "staged.txt")),
        )
        .with(
            SnapshotGitQuery::StagedNamesNoIta,
            ok_output(staged_name_bytes(&["staged.txt"])),
        )
        .with(
            SnapshotGitQuery::StagedNamesVisibleIta,
            ok_output(staged_name_bytes(&["staged.txt"])),
        );
    let metadata = observe_snapshot_metadata(&git, Path::new("/irrelevant")).unwrap();
    assert_eq!(metadata.head, HEAD_A);
    assert_eq!(metadata.staged_paths, vec![path("staged.txt")]);
    assert!(metadata.changed_paths.is_empty());
    assert_eq!(metadata.index_entries.len(), 1);
}

#[test]
fn staged_cross_check_rejects_intent_to_add_difference() {
    // `visible − invisible = {ita.txt}` is exactly the ITA placeholder set.
    let git = FakeSnapshotGit::clean(HEAD_A)
        .with(
            SnapshotGitQuery::Status,
            ok_output(b" A ita.txt\0".to_vec()),
        )
        .with(
            SnapshotGitQuery::IndexStage,
            ok_output(stage_record(
                "100644",
                crate::managed_worktree_snapshot_observe::EMPTY_BLOB_SHA1,
                "0",
                "ita.txt",
            )),
        )
        .with(
            SnapshotGitQuery::IndexFlags,
            ok_output(flags_output(b'H', "ita.txt")),
        )
        .with(SnapshotGitQuery::StagedNamesNoIta, ok_output(Vec::new()))
        .with(
            SnapshotGitQuery::StagedNamesVisibleIta,
            ok_output(staged_name_bytes(&["ita.txt"])),
        );
    // The status tripwire fires first; either rejection is the correct
    // fail-closed outcome for ITA.
    assert!(matches!(
        observe_snapshot_metadata(&git, Path::new("/irrelevant")),
        Err(SnapshotMetadataError::UnsupportedGitState(_))
    ));
}

#[test]
fn staged_cross_check_rejects_contradictory_name_sets() {
    let base = FakeSnapshotGit::clean(HEAD_A)
        .with(SnapshotGitQuery::Status, ok_output(Vec::new()))
        .with(SnapshotGitQuery::IndexStage, ok_output(Vec::new()))
        .with(SnapshotGitQuery::IndexFlags, ok_output(Vec::new()));
    // invisible − visible is impossible under the flag semantics.
    let flipped = FakeSnapshotGit {
        outputs: base.outputs.clone(),
    }
    .with(
        SnapshotGitQuery::StagedNamesNoIta,
        ok_output(staged_name_bytes(&["ghost.txt"])),
    )
    .with(
        SnapshotGitQuery::StagedNamesVisibleIta,
        ok_output(Vec::new()),
    );
    assert!(observe_snapshot_metadata(&flipped, Path::new("/x")).is_err());
    // status-staged disagreeing with the hidden staged set.
    let drifted = FakeSnapshotGit {
        outputs: base.outputs.clone(),
    }
    .with(
        SnapshotGitQuery::Status,
        ok_output(b"M  drift.txt\0".to_vec()),
    )
    .with(SnapshotGitQuery::StagedNamesNoIta, ok_output(Vec::new()))
    .with(
        SnapshotGitQuery::StagedNamesVisibleIta,
        ok_output(Vec::new()),
    );
    assert!(observe_snapshot_metadata(&drifted, Path::new("/x")).is_err());
}

#[test]
fn observe_fails_closed_when_head_is_unresolvable() {
    let git =
        FakeSnapshotGit::clean(HEAD_A).with(SnapshotGitQuery::Head, failed_output(128, "fatal"));
    assert!(matches!(
        observe_snapshot_metadata(&git, Path::new("/x")),
        Err(SnapshotMetadataError::UnsupportedGitState(_))
    ));
}

#[test]
fn parse_nul_name_set_enforces_staged_bound_and_duplicates() {
    assert!(parse_nul_name_set(SnapshotGitQuery::StagedNamesNoIta, b"a.txt\0a.txt\0").is_err());
    assert!(parse_nul_name_set(SnapshotGitQuery::StagedNamesNoIta, b"a.txt").is_err());
}

// ---------------------------------------------------------------------------
// Real-Git repository tests (temporary repositories only, no network)
// ---------------------------------------------------------------------------

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "local-mcp-snapshot-2a-{label}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&path).unwrap();
    std::fs::canonicalize(&path).unwrap()
}

fn host_git() -> PathBuf {
    crate::execution::host_git_path().unwrap()
}

fn git(repo: &Path, args: &[&str]) {
    let output = std::process::Command::new(host_git())
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_may_fail(repo: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(host_git())
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git runs")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn shell_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn marker_writer(root: &Path, marker: &Path, label: &str) -> String {
    #[cfg(windows)]
    {
        // Git for Windows executes fsmonitor commands and hooks through its
        // bundled POSIX shell. Delegate marker creation to Windows PowerShell
        // rather than relying on a Unix utility such as `touch`.
        let script = root.join(format!("{label}.ps1"));
        let marker = shell_path(marker).replace('\'', "''");
        std::fs::write(
            &script,
            format!("[System.IO.File]::WriteAllText('{marker}', 'ran')\nexit 0\n"),
        )
        .unwrap();
        format!(
            "powershell.exe -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {}",
            shell_quote(&shell_path(&script))
        )
    }
    #[cfg(not(windows))]
    {
        let script = root.join(label);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s' ran > {}\n",
                shell_quote(&shell_path(marker))
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        shell_quote(&shell_path(&script))
    }
}

fn assert_process_success(label: &str, output: &std::process::Output) {
    assert!(
        output.status.success(),
        "{label} failed: status={:?}, stdout={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main", "."]);
    git(
        &repo,
        &["config", "user.email", "snapshot2a@example.invalid"],
    );
    git(&repo, &["config", "user.name", "Snapshot 2A Test"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["config", "gc.auto", "0"]);
    git(&repo, &["config", "maintenance.auto", "false"]);
    std::fs::write(repo.join("base.txt"), b"base\n").unwrap();
    git(&repo, &["add", "base.txt"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    std::fs::canonicalize(&repo).unwrap()
}

fn head_oid(repo: &Path) -> String {
    let output = std::process::Command::new(host_git())
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .expect("git runs");
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn real_git() -> crate::managed_worktree_snapshot_observe::HostSnapshotGit {
    crate::managed_worktree_snapshot_observe::HostSnapshotGit
}

#[test]
fn real_git_clean_repo_has_empty_sets_and_valid_head() {
    let base = temp_dir("clean");
    let repo = init_repo(&base);
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(metadata.head, head_oid(&repo));
    assert!(metadata.changed_paths.is_empty());
    assert!(metadata.staged_paths.is_empty());
    assert!(metadata.untracked_paths.is_empty());
    // Candidate retention: a fully clean repository retains no index
    // entries, although the whole-index safety scan must have seen
    // `base.txt` as mode 100644 (covered by the stage parser tests).
    assert!(metadata.index_entries.is_empty());
    let _ = std::fs::remove_dir_all(base);
}

#[cfg(unix)]
#[test]
fn file_mode_policy_matches_git_status_and_effective_included_config() {
    use std::os::unix::fs::PermissionsExt;

    for (setting, expected_policy, expects_mode_change) in [
        (Some("true"), GitFileModePolicy::TrustExecutableBit, true),
        (Some("false"), GitFileModePolicy::IgnoreExecutableBit, false),
        (None, GitFileModePolicy::TrustExecutableBit, true),
    ] {
        let base = temp_dir("filemode");
        let repo = init_repo(&base);
        if let Some(setting) = setting {
            git(&repo, &["config", "core.fileMode", setting]);
        } else {
            let _ = git_may_fail(&repo, &["config", "--unset", "core.fileMode"]);
        }
        let before = std::fs::metadata(repo.join("base.txt"))
            .unwrap()
            .permissions();
        std::fs::set_permissions(
            repo.join("base.txt"),
            std::fs::Permissions::from_mode(before.mode() | 0o100),
        )
        .unwrap();
        let status = git_may_fail(&repo, &["status", "--porcelain"]);
        assert_process_success("git status", &status);
        assert_eq!(
            String::from_utf8(status.stdout)
                .unwrap()
                .contains(" M base.txt"),
            expects_mode_change
        );
        let observed = observe_snapshot_metadata(&real_git(), &repo).unwrap();
        assert_eq!(observed.file_mode_policy, expected_policy);
        let _ = std::fs::remove_dir_all(base);
    }

    let base = temp_dir("filemode-include");
    let repo = init_repo(&base);
    let included = base.join("included.gitconfig");
    std::fs::write(&included, b"[core]\n\tfileMode = false\n").unwrap();
    git(
        &repo,
        &[
            "config",
            "--add",
            "include.path",
            included.to_str().unwrap(),
        ],
    );
    let observed = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(
        observed.file_mode_policy,
        GitFileModePolicy::IgnoreExecutableBit
    );
    let before = std::fs::metadata(repo.join("base.txt"))
        .unwrap()
        .permissions();
    std::fs::set_permissions(
        repo.join("base.txt"),
        std::fs::Permissions::from_mode(before.mode() | 0o100),
    )
    .unwrap();
    let status = git_may_fail(&repo, &["status", "--porcelain"]);
    assert_process_success("git status with included core.fileMode", &status);
    assert!(
        !String::from_utf8(status.stdout)
            .unwrap()
            .contains(" M base.txt")
    );
    let _ = std::fs::remove_dir_all(base);
}

#[cfg(unix)]
#[test]
fn file_mode_duplicate_config_values_follow_git_last_value_precedence() {
    let base = temp_dir("filemode-duplicates");
    let repo = init_repo(&base);
    git(&repo, &["config", "--unset-all", "core.fileMode"]);
    git(&repo, &["config", "--add", "core.fileMode", "false"]);
    git(&repo, &["config", "--add", "core.fileMode", "true"]);
    let observed = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(
        observed.file_mode_policy,
        GitFileModePolicy::TrustExecutableBit
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn file_mode_query_uses_fixed_key_and_existing_hardened_envelope() {
    assert_eq!(
        SnapshotGitQuery::FileMode.tail(),
        &["config", "--bool", "--get", "core.fileMode"]
    );
    let argv = snapshot_argv(SnapshotGitQuery::FileMode).unwrap();
    assert!(
        argv.windows(2)
            .any(|pair| pair[0] == "-c" && pair[1].starts_with("core.hooksPath="))
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair[0] == "-c" && pair[1] == "core.fsmonitor=false")
    );
    assert!(argv.contains(&"--no-optional-locks".to_owned()));
    assert!(
        !argv
            .iter()
            .any(|arg| arg == "--local" || arg.starts_with("--file"))
    );
}

#[test]
fn real_git_staged_regular_file() {
    let base = temp_dir("staged");
    let repo = init_repo(&base);
    std::fs::write(repo.join("base.txt"), b"staged change\n").unwrap();
    git(&repo, &["add", "base.txt"]);
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(metadata.staged_paths, vec![path("base.txt")]);
    assert!(metadata.changed_paths.is_empty());
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_unstaged_regular_file() {
    let base = temp_dir("unstaged");
    let repo = init_repo(&base);
    std::fs::write(repo.join("base.txt"), b"worktree change\n").unwrap();
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(metadata.changed_paths, vec![path("base.txt")]);
    assert!(metadata.staged_paths.is_empty());
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_staged_and_unstaged_same_path() {
    let base = temp_dir("both");
    let repo = init_repo(&base);
    std::fs::write(repo.join("base.txt"), b"staged\n").unwrap();
    git(&repo, &["add", "base.txt"]);
    std::fs::write(repo.join("base.txt"), b"staged plus worktree\n").unwrap();
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(metadata.staged_paths, vec![path("base.txt")]);
    assert_eq!(metadata.changed_paths, vec![path("base.txt")]);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_untracked_file() {
    let base = temp_dir("untracked");
    let repo = init_repo(&base);
    std::fs::write(repo.join("new.txt"), b"new\n").unwrap();
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(metadata.untracked_paths, vec![path("new.txt")]);
    assert!(
        metadata
            .index_entries
            .iter()
            .all(|entry| entry.path != path("new.txt"))
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_worktree_and_staged_deletions() {
    let base = temp_dir("deletion");
    let repo = init_repo(&base);
    std::fs::remove_file(repo.join("base.txt")).unwrap();
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(metadata.changed_paths, vec![path("base.txt")]);
    assert!(metadata.staged_paths.is_empty());
    git(&repo, &["rm", "-q", "base.txt"]);
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(metadata.staged_paths, vec![path("base.txt")]);
    let _ = std::fs::remove_dir_all(base);
}

#[cfg(unix)]
#[test]
fn real_git_executable_index_mode_is_observed() {
    use std::os::unix::fs::PermissionsExt;
    let base = temp_dir("exec");
    let repo = init_repo(&base);
    std::fs::write(repo.join("run.sh"), b"#!/bin/sh\n").unwrap();
    let mut permissions = std::fs::metadata(repo.join("run.sh"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(repo.join("run.sh"), permissions).unwrap();
    git(&repo, &["add", "run.sh"]);
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    let entry = metadata
        .index_entries
        .iter()
        .find(|entry| entry.path == path("run.sh"))
        .expect("executable is indexed");
    assert_eq!(entry.mode, SnapshotIndexMode::Regular { executable: true });
    let _ = std::fs::remove_dir_all(base);
}

#[cfg(unix)]
#[test]
fn real_git_symlink_index_entry_is_observed() {
    let base = temp_dir("symlink");
    let repo = init_repo(&base);
    std::os::unix::fs::symlink("base.txt", repo.join("link")).unwrap();
    git(&repo, &["add", "link"]);
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    let entry = metadata
        .index_entries
        .iter()
        .find(|entry| entry.path == path("link"))
        .expect("symlink is indexed");
    assert_eq!(entry.mode, SnapshotIndexMode::Symlink);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_intent_to_add_is_rejected() {
    let base = temp_dir("ita");
    let repo = init_repo(&base);
    std::fs::write(repo.join("ita.txt"), b"intended\n").unwrap();
    git(&repo, &["add", "-N", "ita.txt"]);
    // The empty-blob placeholder is what makes OID-only detection unsound:
    // prove the fixture really carries it before asserting rejection.
    let output = std::process::Command::new(host_git())
        .args(["ls-files", "--stage", "--", "ita.txt"])
        .current_dir(&repo)
        .output()
        .expect("git runs");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains(crate::managed_worktree_snapshot_observe::EMPTY_BLOB_SHA1)
    );
    let error = observe_snapshot_metadata(&real_git(), &repo).unwrap_err();
    assert!(
        matches!(error, SnapshotMetadataError::UnsupportedGitState(_)),
        "unexpected: {error}"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_assume_unchanged_is_rejected() {
    let base = temp_dir("assume");
    let repo = init_repo(&base);
    git(&repo, &["update-index", "--assume-unchanged", "base.txt"]);
    let error = observe_snapshot_metadata(&real_git(), &repo).unwrap_err();
    assert!(
        matches!(error, SnapshotMetadataError::UnsupportedGitState(_)),
        "unexpected: {error}"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_skip_worktree_is_rejected() {
    let base = temp_dir("skip");
    let repo = init_repo(&base);
    git(&repo, &["update-index", "--skip-worktree", "base.txt"]);
    // Status alone would look clean; the global index scan must still refuse.
    let error = observe_snapshot_metadata(&real_git(), &repo).unwrap_err();
    assert!(
        matches!(error, SnapshotMetadataError::UnsupportedGitState(_)),
        "unexpected: {error}"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_gitlink_is_rejected_without_network() {
    let base = temp_dir("gitlink");
    let repo = init_repo(&base);
    std::fs::write(repo.join("gitlink-blob.txt"), b"gitlink stand-in\n").unwrap();
    let hash = std::process::Command::new(host_git())
        .args(["hash-object", "-w", "gitlink-blob.txt"])
        .current_dir(&repo)
        .output()
        .expect("git runs");
    assert!(hash.status.success());
    std::fs::remove_file(repo.join("gitlink-blob.txt")).unwrap();
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!(
                "160000,{},sublink",
                String::from_utf8_lossy(&hash.stdout).trim()
            ),
        ],
    );
    let error = observe_snapshot_metadata(&real_git(), &repo).unwrap_err();
    assert!(
        matches!(error, SnapshotMetadataError::UnsupportedGitState(_)),
        "unexpected: {error}"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_merge_conflict_is_rejected() {
    let base = temp_dir("conflict");
    let repo = init_repo(&base);
    std::fs::write(repo.join("conflict.txt"), b"base\n").unwrap();
    git(&repo, &["add", "conflict.txt"]);
    git(&repo, &["commit", "-q", "-m", "conflict base"]);
    git(&repo, &["checkout", "-q", "-b", "other"]);
    std::fs::write(repo.join("conflict.txt"), b"other\n").unwrap();
    git(&repo, &["commit", "-q", "-am", "other side"]);
    git(&repo, &["checkout", "-q", "main"]);
    std::fs::write(repo.join("conflict.txt"), b"main\n").unwrap();
    git(&repo, &["commit", "-q", "-am", "main side"]);
    let merge = git_may_fail(&repo, &["merge", "--no-commit", "other"]);
    assert!(!merge.status.success(), "fixture must conflict");
    let error = observe_snapshot_metadata(&real_git(), &repo).unwrap_err();
    assert!(
        matches!(error, SnapshotMetadataError::UnsupportedGitState(_)),
        "unexpected: {error}"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_rename_config_does_not_leak_heuristics() {
    let base = temp_dir("rename");
    let repo = init_repo(&base);
    git(&repo, &["config", "status.renames", "true"]);
    git(&repo, &["config", "diff.renames", "true"]);
    std::fs::write(repo.join("similar.txt"), b"base\n").unwrap();
    git(&repo, &["add", "similar.txt"]);
    git(&repo, &["commit", "-q", "-m", "similar"]);
    git(&repo, &["mv", "similar.txt", "renamed.txt"]);
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    // With `--no-renames` the move is delete-plus-add, never a heuristic `R`.
    assert_eq!(
        metadata.staged_paths,
        vec![path("renamed.txt"), path("similar.txt")]
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_hostile_fsmonitor_is_never_executed() {
    let base = temp_dir("fsmonitor");
    let repo = init_repo(&base);
    let marker = base.join("fsmonitor-ran");
    git(
        &repo,
        &[
            "config",
            "core.fsmonitor",
            &format!("touch {}", marker.display()),
        ],
    );
    std::fs::write(repo.join("base.txt"), b"changed\n").unwrap();
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(metadata.changed_paths, vec![path("base.txt")]);
    assert!(
        !marker.exists(),
        "observation ran a repository-chosen monitor"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_repository_hooks_are_never_executed() {
    let base = temp_dir("hooks");
    let repo = init_repo(&base);
    let hooks = repo.join(".git").join("attacker-hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let marker = base.join("hook-ran");
    for hook in [
        "post-index-change",
        "post-rewrite",
        "post-checkout",
        "pre-auto-gc",
    ] {
        let script = hooks.join(hook);
        std::fs::write(&script, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&script).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script, permissions).unwrap();
        }
    }
    git(
        &repo,
        &["config", "core.hooksPath", &hooks.to_string_lossy()],
    );
    std::fs::write(repo.join("base.txt"), b"changed\n").unwrap();
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(metadata.changed_paths, vec![path("base.txt")]);
    assert!(!marker.exists(), "observation ran a repository-chosen hook");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_replace_refs_do_not_rewrite_observed_ids() {
    let base = temp_dir("replace");
    let repo = init_repo(&base);
    let original = head_oid(&repo);
    std::fs::write(repo.join("other.txt"), b"other\n").unwrap();
    git(&repo, &["add", "other.txt"]);
    git(&repo, &["commit", "-q", "-m", "other"]);
    let replacement = head_oid(&repo);
    assert_ne!(original, replacement);
    // Replace HEAD itself with its parent: repository-influenced Git now
    // reports a staged addition that does not exist in the true HEAD tree.
    git(&repo, &["replace", &replacement, &original]);
    let plain = std::process::Command::new(host_git())
        .args(["diff", "--cached", "--name-only", "--ignore-submodules=all"])
        .current_dir(&repo)
        .output()
        .expect("git runs");
    assert!(
        String::from_utf8_lossy(&plain.stdout).trim() == "other.txt",
        "replace fixture is not live"
    );
    // The observer carries `--no-replace-objects` on every query, so it must
    // report the true HEAD and the truly clean candidate rather than the
    // repository-chosen rewriting.
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(metadata.head, replacement);
    assert!(metadata.staged_paths.is_empty());
    assert!(metadata.changed_paths.is_empty());
    assert!(metadata.untracked_paths.is_empty());
    let _ = std::fs::remove_dir_all(base);
}

// ---------------------------------------------------------------------------
// Pre-freeze hardening amendments
// ---------------------------------------------------------------------------

#[test]
fn process_output_completeness_gate_rejects_every_incomplete_variant() {
    assert!(enforce_complete_process_output(SnapshotGitQuery::Status, false, false, false).is_ok());
    for (timed_out, capture_incomplete, output_overflow) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
        (true, true, true),
    ] {
        assert!(matches!(
            enforce_complete_process_output(
                SnapshotGitQuery::IndexStage,
                timed_out,
                capture_incomplete,
                output_overflow
            ),
            Err(SnapshotMetadataError::GitCommand { .. })
        ));
    }
    // A capture truncated exactly on a NUL record boundary parses as a
    // shorter but well-formed index to the parser, so the runner-flag gate
    // is the only barrier; prove the parser alone accepts it and the gate
    // alone rejects it.
    assert!(
        parse_nul_name_set(SnapshotGitQuery::StagedNamesNoIta, b"a.txt\0b.txt\0").is_ok(),
        "truncated-but-NUL-aligned output must parse as a shorter valid index"
    );
    assert!(
        enforce_complete_process_output(SnapshotGitQuery::StagedNamesNoIta, false, true, false)
            .is_err(),
        "the completeness gate, not the parser, rejects it"
    );
}

#[test]
fn full_index_scan_does_not_conflate_union_limit() {
    // >60,000 normal stage-0 entries: a valid large clean repository must
    // still scan. MAX_GIT_VISIBLE_PATHS bounds the snapshot candidate
    // union, not the whole-index safety scan.
    let mut bytes = Vec::new();
    for index in 0..60_001 {
        bytes.extend_from_slice(&stage_record(
            "100644",
            OID_40,
            "0",
            &format!("dir/p{index:05}.txt"),
        ));
    }
    let entries = parse_index_stage(&bytes).unwrap();
    assert_eq!(entries.len(), 60_001);
    let mut flags = Vec::new();
    for index in 0..60_001 {
        flags.extend_from_slice(&flags_output(b'H', &format!("dir/p{index:05}.txt")));
    }
    check_index_flags(&flags, &entries).unwrap();
}

#[test]
fn observe_retains_only_candidate_index_entries() {
    let mut stage_bytes = stage_record("100644", OID_40, "0", "a.txt");
    stage_bytes.extend_from_slice(&stage_record("100644", OID_40, "0", "b.txt"));
    let mut flags_bytes = flags_output(b'H', "a.txt");
    flags_bytes.extend_from_slice(&flags_output(b'H', "b.txt"));
    let git = FakeSnapshotGit::clean(HEAD_A)
        .with(SnapshotGitQuery::Status, ok_output(b"M  a.txt\0".to_vec()))
        .with(SnapshotGitQuery::IndexStage, ok_output(stage_bytes))
        .with(SnapshotGitQuery::IndexFlags, ok_output(flags_bytes))
        .with(
            SnapshotGitQuery::StagedNamesNoIta,
            ok_output(staged_name_bytes(&["a.txt"])),
        )
        .with(
            SnapshotGitQuery::StagedNamesVisibleIta,
            ok_output(staged_name_bytes(&["a.txt"])),
        );
    let metadata = observe_snapshot_metadata(&git, Path::new("/irrelevant")).unwrap();
    assert_eq!(metadata.index_entries.len(), 1);
    assert_eq!(metadata.index_entries[0].path, path("a.txt"));
}

#[test]
fn snapshot_observation_environment_drops_pager_overrides() {
    let environment = crate::sandbox::clean_git_environment();
    for key in ["GIT_PAGER", "PAGER", "GIT_EXTERNAL_DIFF", "GIT_DIFF_OPTS"] {
        assert!(!environment.contains_key(key), "{key}");
    }
    assert_eq!(
        environment.get("GIT_CONFIG_NOSYSTEM").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        environment.get("GIT_CONFIG_GLOBAL").map(String::as_str),
        Some(crate::sandbox::null_device_path().as_str())
    );
    for key in [
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_PARAMETERS",
    ] {
        assert!(!environment.contains_key(key), "{key}");
    }
}

#[test]
fn real_git_ls_files_v_keeps_h_for_ordinary_modified_and_deleted() {
    let base = temp_dir("hflag");
    let repo = init_repo(&base);
    std::fs::write(repo.join("gone.txt"), b"gone\n").unwrap();
    git(&repo, &["add", "gone.txt"]);
    git(&repo, &["commit", "-q", "-m", "gone"]);
    std::fs::write(repo.join("base.txt"), b"worktree edit\n").unwrap();
    std::fs::remove_file(repo.join("gone.txt")).unwrap();
    // Positive control: the tag is `H` for ordinary modified and deleted
    // tracked paths, so rejecting every non-`H` tag never rejects a normal
    // dirty repository.
    let output = std::process::Command::new(host_git())
        .args(["ls-files", "-v", "--full-name"])
        .current_dir(&repo)
        .output()
        .expect("git runs");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.lines().all(|line| line.starts_with("H ")),
        "unexpected tag: {text}"
    );
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(
        metadata.changed_paths,
        vec![path("base.txt"), path("gone.txt")]
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_hostile_pager_is_never_executed() {
    let base = temp_dir("pager");
    let repo = init_repo(&base);
    let marker = base.join("pager-ran");
    git(
        &repo,
        &[
            "config",
            "core.pager",
            &format!("touch {}", marker.display()),
        ],
    );
    std::fs::write(repo.join("base.txt"), b"changed\n").unwrap();
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(metadata.changed_paths, vec![path("base.txt")]);
    assert!(!marker.exists(), "observation consulted the pager");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_hostile_diff_drivers_are_never_executed() {
    let base = temp_dir("diffext");
    let repo = init_repo(&base);
    let external_marker = base.join("diff-external-ran");
    let textconv_marker = base.join("textconv-ran");
    git(
        &repo,
        &[
            "config",
            "diff.external",
            &format!("touch {}", external_marker.display()),
        ],
    );
    std::fs::write(repo.join(".gitattributes"), b"*.bin diff=mark\n").unwrap();
    git(
        &repo,
        &[
            "config",
            "diff.mark.textconv",
            &format!("touch {}", textconv_marker.display()),
        ],
    );
    git(&repo, &["add", ".gitattributes"]);
    git(&repo, &["commit", "-q", "-m", "attributes"]);
    std::fs::write(repo.join("data.bin"), b"\x00\x01").unwrap();
    git(&repo, &["add", "data.bin"]);
    git(&repo, &["commit", "-q", "-m", "bin"]);
    std::fs::write(repo.join("data.bin"), b"\x00\x02").unwrap();
    // Plain `git diff` with a forced external tool can only not fire in a
    // piped harness, so this test asserts the negative half: the observer
    // must never consult either hook (its diff queries carry
    // --no-ext-diff/--no-textconv, and the envelope clears GIT_EXTERNAL_DIFF).
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert!(metadata.changed_paths.contains(&path("data.bin")));
    assert!(!external_marker.exists(), "observation ran diff.external");
    assert!(!textconv_marker.exists(), "observation ran textconv");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_fsmonitor_marker_positive_control() {
    let base = temp_dir("fsmonpc");
    let repo = init_repo(&base);
    let marker = base.join("fsmonitor-ran");
    let command = marker_writer(&base, &marker, "write-fsmonitor-marker");
    git(&repo, &["config", "core.fsmonitor", &command]);

    // Positive control: plain Git must execute the configured helper before
    // the observer runs. The helper is platform-specific, not `touch`.
    let output = std::process::Command::new(host_git())
        .args(["status", "--porcelain"])
        .current_dir(&repo)
        .output()
        .expect("git runs");
    assert_process_success("git status positive control", &output);
    assert!(marker.exists(), "fsmonitor fixture must be live");
    std::fs::remove_file(&marker).unwrap();

    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert!(metadata.head.len() == 40 || metadata.head.len() == 64);
    assert!(
        !marker.exists(),
        "observer ran the repository-chosen monitor"
    );
    let _ = std::fs::remove_dir_all(base);
}

// Git for Windows attempts to execute the extensionless hook path directly;
// its runner reports `Exec format error` for POSIX hook scripts. Keep runtime
// positive-control evidence on Unix and retain the Windows observer-negative
// and exact-argv coverage without claiming a Windows hook positive control.
#[cfg(unix)]
#[test]
fn real_git_hooks_marker_positive_control() {
    let base = temp_dir("hookspc");
    let repo = init_repo(&base);
    let hooks = repo.join(".git").join("attacker-hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let marker = base.join("hook-ran");
    let writer = marker_writer(&base, &marker, "write-hook-marker");
    let hook = hooks.join("post-checkout");
    std::fs::write(&hook, format!("#!/bin/sh\n{writer}\nexit $?\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
    }
    git(&repo, &["config", "core.hooksPath", &shell_path(&hooks)]);

    // Make a real branch transition with a changed tracked file so Git
    // deterministically invokes post-checkout (unlike checking out HEAD).
    git(&repo, &["checkout", "-q", "-b", "other"]);
    std::fs::write(repo.join("base.txt"), b"other branch\n").unwrap();
    git(&repo, &["add", "base.txt"]);
    git(&repo, &["commit", "-q", "-m", "other branch"]);
    let checkout = std::process::Command::new(host_git())
        .args(["checkout", "main"])
        .current_dir(&repo)
        .output()
        .expect("git runs");
    assert_process_success("git checkout main positive control", &checkout);
    assert!(marker.exists(), "hooks fixture must be live");
    std::fs::remove_file(&marker).unwrap();

    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert!(metadata.head.len() == 40 || metadata.head.len() == 64);
    assert!(!marker.exists(), "observer ran a repository-chosen hook");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_rename_copy_heuristics_config_is_inert() {
    let base = temp_dir("renamecopy");
    let repo = init_repo(&base);
    std::fs::write(repo.join("similar.txt"), b"content\n").unwrap();
    git(&repo, &["add", "similar.txt"]);
    git(&repo, &["commit", "-q", "-m", "similar"]);
    git(&repo, &["config", "diff.renames", "copies"]);
    git(&repo, &["config", "status.renames", "true"]);
    git(&repo, &["rm", "-q", "similar.txt"]);
    std::fs::write(repo.join("copied.txt"), b"content\n").unwrap();
    git(&repo, &["add", "copied.txt"]);
    // Positive control: a plain `git diff --cached --name-status` applies
    // the copy heuristic and reports a rename/copy record.
    let plain = std::process::Command::new(host_git())
        .args(["diff", "--cached", "--name-status"])
        .current_dir(&repo)
        .output()
        .expect("git runs");
    let text = String::from_utf8_lossy(&plain.stdout);
    let first = text.lines().next().unwrap_or("");
    assert!(
        first.starts_with('R') || first.starts_with('C'),
        "copy/rename heuristic not live: {text}"
    );
    // The observer passes --no-renames to every rename-sensitive query, so
    // both staged delete and staged add survive as ordinary records, and
    // the ITA cross-check stays exact.
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(
        metadata.staged_paths,
        vec![path("copied.txt"), path("similar.txt")]
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_genuine_empty_staged_file_is_accepted() {
    let base = temp_dir("emptystaged");
    let repo = init_repo(&base);
    std::fs::write(repo.join("empty.txt"), b"").unwrap();
    git(&repo, &["add", "empty.txt"]);
    // A genuinely staged empty file carries the ITA empty-blob OID but is
    // **not** ITA: it appears in both ITA staged-name views.
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(metadata.staged_paths, vec![path("empty.txt")]);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_ita_empty_working_file_is_rejected() {
    let base = temp_dir("itaempty");
    let repo = init_repo(&base);
    std::fs::write(repo.join("ita.txt"), b"").unwrap();
    git(&repo, &["add", "-N", "ita.txt"]);
    let error = observe_snapshot_metadata(&real_git(), &repo).unwrap_err();
    assert!(
        matches!(error, SnapshotMetadataError::UnsupportedGitState(_)),
        "unexpected: {error}"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_ita_mixed_with_normal_staged_entries_is_rejected() {
    let base = temp_dir("itamixed");
    let repo = init_repo(&base);
    std::fs::write(repo.join("normal.txt"), b"normal\n").unwrap();
    git(&repo, &["add", "normal.txt"]);
    std::fs::write(repo.join("ita.txt"), b"intended\n").unwrap();
    git(&repo, &["add", "-N", "ita.txt"]);
    let error = observe_snapshot_metadata(&real_git(), &repo).unwrap_err();
    assert!(
        matches!(error, SnapshotMetadataError::UnsupportedGitState(_)),
        "unexpected: {error}"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_am_and_ad_states_are_accepted() {
    let base = temp_dir("amad");
    let repo = init_repo(&base);
    std::fs::write(repo.join("am.txt"), b"added\n").unwrap();
    git(&repo, &["add", "am.txt"]);
    std::fs::write(repo.join("am.txt"), b"added plus worktree\n").unwrap();
    std::fs::write(repo.join("ad.txt"), b"added then deleted\n").unwrap();
    git(&repo, &["add", "ad.txt"]);
    std::fs::remove_file(repo.join("ad.txt")).unwrap();
    let metadata = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(metadata.staged_paths, vec![path("ad.txt"), path("am.txt")]);
    assert_eq!(metadata.changed_paths, vec![path("ad.txt"), path("am.txt")]);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn real_git_observation_leaves_index_and_worktree_untouched() {
    let base = temp_dir("immut");
    let repo = init_repo(&base);
    std::fs::write(repo.join("base.txt"), b"worktree edit\n").unwrap();
    let index = repo.join(".git").join("index");
    let index_before = std::fs::read(&index).unwrap();
    let index_mtime_before = std::fs::metadata(&index)
        .ok()
        .map(|entry| entry.modified().ok());
    let outside = base.join("outside.txt");
    std::fs::write(&outside, b"untouched\n").unwrap();
    let observed = observe_snapshot_metadata(&real_git(), &repo).unwrap();
    assert_eq!(observed.changed_paths, vec![path("base.txt")]);
    assert_eq!(
        std::fs::read(&index).unwrap(),
        index_before,
        "observation must not rewrite the index"
    );
    assert_eq!(
        std::fs::metadata(&index)
            .ok()
            .map(|entry| entry.modified().ok()),
        index_mtime_before,
        "observation must not even refresh index stat metadata"
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"untouched\n");
    let _ = std::fs::remove_dir_all(base);
}
