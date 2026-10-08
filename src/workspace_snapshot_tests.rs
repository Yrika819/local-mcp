use crate::managed_worktree::WorktreeId;
use crate::workspace_snapshot::{
    CandidateObjectIdentity, GitVisiblePathIdentity, MAX_CANONICAL_MANIFEST_BYTES,
    MAX_CHANGED_PATHS, MAX_GIT_VISIBLE_PATHS, MAX_NORMALIZED_PATH_BYTES,
    MAX_SNAPSHOT_CONTENT_BYTES, MAX_STAGED_PATHS, MAX_UNTRACKED_PATHS, MAX_WRITER_MUTATED_PATHS,
    NormalizedWorkspacePath, WORKSPACE_SNAPSHOT_V1_DOMAIN, WorkspaceSnapshotError,
    WorkspaceSnapshotManifestV1, WriterMutatedPathIdentity,
};

const BASE: &str = "0123456789abcdef0123456789abcdef01234567";
const HEAD: &str = "1123456789abcdef0123456789abcdef01234567";
const OTHER: &str = "2123456789abcdef0123456789abcdef01234567";
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const CONTENT_SHA256: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn path(value: &str) -> NormalizedWorkspacePath {
    NormalizedWorkspacePath::parse(value).unwrap()
}

fn worktree_id() -> WorktreeId {
    WorktreeId::parse("00000000-0000-4000-8000-000000000001").unwrap()
}

fn manifest(
    worktree_id: WorktreeId,
    base: &str,
    head: &str,
    changed: &[&str],
    staged: &[&str],
    untracked: &[&str],
    identities: Vec<WriterMutatedPathIdentity>,
) -> WorkspaceSnapshotManifestV1 {
    WorkspaceSnapshotManifestV1::new(
        worktree_id,
        base,
        head,
        changed.iter().map(|value| path(value)).collect(),
        staged.iter().map(|value| path(value)).collect(),
        untracked.iter().map(|value| path(value)).collect(),
        git_visible_identities_with_writer(
            changed
                .iter()
                .chain(staged.iter())
                .chain(untracked.iter())
                .copied(),
            &identities,
        ),
        identities,
    )
    .unwrap()
}

fn present(path_value: &str, digest: &str, size: u64) -> WriterMutatedPathIdentity {
    WriterMutatedPathIdentity::present(path(path_value), digest, size).unwrap()
}

fn git_visible_identities<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<GitVisiblePathIdentity> {
    git_visible_identities_with_writer(paths, &[])
}

fn git_visible_identities_with_writer<'a>(
    paths: impl Iterator<Item = &'a str>,
    writer: &[WriterMutatedPathIdentity],
) -> Vec<GitVisiblePathIdentity> {
    let mut unique = paths.collect::<Vec<_>>();
    unique.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    unique.dedup();
    unique
        .into_iter()
        .map(|value| {
            let working = writer
                .iter()
                .find_map(|identity| match identity {
                    WriterMutatedPathIdentity::Present {
                        path: candidate,
                        object,
                    } if *candidate == path(value) => Some(object.clone()),
                    WriterMutatedPathIdentity::Absent { path: candidate }
                        if *candidate == path(value) =>
                    {
                        Some(CandidateObjectIdentity::Absent)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| CandidateObjectIdentity::RegularFile {
                    content_sha256: CONTENT_SHA256.to_owned(),
                    size_bytes: 1,
                    executable: false,
                });
            GitVisiblePathIdentity::new(path(value), CandidateObjectIdentity::Absent, working)
                .unwrap()
        })
        .collect()
}

#[test]
fn digest_is_deterministic_and_lowercase_sha256() {
    let snapshot = manifest(
        worktree_id(),
        BASE,
        HEAD,
        &["src/main.rs"],
        &[],
        &[],
        vec![present("src/main.rs", CONTENT_SHA256, 0)],
    );
    assert_eq!(snapshot.digest(), snapshot.digest());
    assert_eq!(snapshot.digest().len(), 64);
    assert!(
        snapshot
            .digest()
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
}

#[test]
fn collection_order_does_not_change_digest_and_sets_allow_cross_category_overlap() {
    let first = manifest(
        worktree_id(),
        BASE,
        HEAD,
        &["z.rs", "a.rs"],
        &["z.rs", "b.rs"],
        &["c.rs", "a.rs"],
        vec![
            present("z.rs", CONTENT_SHA256, 9),
            WriterMutatedPathIdentity::absent(path("a.rs")),
        ],
    );
    let second = manifest(
        worktree_id(),
        BASE,
        HEAD,
        &["a.rs", "z.rs"],
        &["b.rs", "z.rs"],
        &["a.rs", "c.rs"],
        vec![
            WriterMutatedPathIdentity::absent(path("a.rs")),
            present("z.rs", CONTENT_SHA256, 9),
        ],
    );
    assert_eq!(first.digest(), second.digest());
    assert!(
        first
            .canonical_bytes()
            .starts_with(WORKSPACE_SNAPSHOT_V1_DOMAIN)
    );
}

#[test]
fn every_identity_and_path_component_changes_digest() {
    let baseline = manifest(
        worktree_id(),
        BASE,
        HEAD,
        &["changed.rs"],
        &["staged.rs"],
        &["new.rs"],
        vec![present("writer.rs", CONTENT_SHA256, 1)],
    );
    let variants = [
        manifest(
            WorktreeId::parse("00000000-0000-4000-8000-000000000002").unwrap(),
            BASE,
            HEAD,
            &["changed.rs"],
            &["staged.rs"],
            &["new.rs"],
            vec![present("writer.rs", CONTENT_SHA256, 1)],
        ),
        manifest(
            worktree_id(),
            OTHER,
            HEAD,
            &["changed.rs"],
            &["staged.rs"],
            &["new.rs"],
            vec![present("writer.rs", CONTENT_SHA256, 1)],
        ),
        manifest(
            worktree_id(),
            BASE,
            OTHER,
            &["changed.rs"],
            &["staged.rs"],
            &["new.rs"],
            vec![present("writer.rs", CONTENT_SHA256, 1)],
        ),
        manifest(
            worktree_id(),
            BASE,
            HEAD,
            &["changed-2.rs"],
            &["staged.rs"],
            &["new.rs"],
            vec![present("writer.rs", CONTENT_SHA256, 1)],
        ),
        manifest(
            worktree_id(),
            BASE,
            HEAD,
            &["changed.rs"],
            &["staged-2.rs"],
            &["new.rs"],
            vec![present("writer.rs", CONTENT_SHA256, 1)],
        ),
        manifest(
            worktree_id(),
            BASE,
            HEAD,
            &["changed.rs"],
            &["staged.rs"],
            &["new-2.rs"],
            vec![present("writer.rs", CONTENT_SHA256, 1)],
        ),
        manifest(
            worktree_id(),
            BASE,
            HEAD,
            &["changed.rs"],
            &["staged.rs"],
            &["new.rs"],
            vec![present("writer.rs", EMPTY_SHA256, 1)],
        ),
        manifest(
            worktree_id(),
            BASE,
            HEAD,
            &["changed.rs"],
            &["staged.rs"],
            &["new.rs"],
            vec![present("writer.rs", CONTENT_SHA256, 2)],
        ),
        manifest(
            worktree_id(),
            BASE,
            HEAD,
            &["changed.rs"],
            &["staged.rs"],
            &["new.rs"],
            vec![WriterMutatedPathIdentity::absent(path("writer.rs"))],
        ),
    ];
    for variant in variants {
        assert_ne!(baseline.digest(), variant.digest());
    }
}

#[test]
fn present_empty_and_absent_have_distinct_identity() {
    let empty = manifest(
        worktree_id(),
        BASE,
        HEAD,
        &[],
        &[],
        &[],
        vec![present("empty", EMPTY_SHA256, 0)],
    );
    let absent = manifest(
        worktree_id(),
        BASE,
        HEAD,
        &[],
        &[],
        &[],
        vec![WriterMutatedPathIdentity::absent(path("empty"))],
    );
    assert_ne!(empty.digest(), absent.digest());
}

#[test]
fn git_visible_content_mode_and_symlink_target_are_digest_bound() {
    let make = |index_object, worktree_object| {
        WorkspaceSnapshotManifestV1::new(
            worktree_id(),
            BASE,
            HEAD,
            vec![path("candidate")],
            vec![],
            vec![],
            vec![
                GitVisiblePathIdentity::new(path("candidate"), index_object, worktree_object)
                    .unwrap(),
            ],
            vec![],
        )
        .unwrap()
    };
    let index_object = CandidateObjectIdentity::RegularFile {
        content_sha256: CONTENT_SHA256.to_owned(),
        size_bytes: 7,
        executable: false,
    };
    let non_executable = make(index_object.clone(), index_object.clone());
    let executable = make(
        index_object.clone(),
        CandidateObjectIdentity::RegularFile {
            content_sha256: CONTENT_SHA256.to_owned(),
            size_bytes: 7,
            executable: true,
        },
    );
    let symlink_same_bytes = make(
        index_object.clone(),
        CandidateObjectIdentity::Symlink {
            target_sha256: CONTENT_SHA256.to_owned(),
            target_size_bytes: 7,
        },
    );
    let index_changed = make(
        CandidateObjectIdentity::RegularFile {
            content_sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .to_owned(),
            size_bytes: 7,
            executable: false,
        },
        index_object.clone(),
    );
    let index_mode_changed = make(
        CandidateObjectIdentity::RegularFile {
            content_sha256: CONTENT_SHA256.to_owned(),
            size_bytes: 7,
            executable: true,
        },
        index_object.clone(),
    );
    let symlink_target_changed = make(
        index_object.clone(),
        CandidateObjectIdentity::Symlink {
            target_sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                .to_owned(),
            target_size_bytes: 7,
        },
    );
    let absent = make(
        CandidateObjectIdentity::Absent,
        CandidateObjectIdentity::Absent,
    );
    assert_ne!(non_executable.digest(), executable.digest());
    assert_ne!(non_executable.digest(), symlink_same_bytes.digest());
    assert_ne!(non_executable.digest(), index_changed.digest());
    assert_ne!(non_executable.digest(), index_mode_changed.digest());
    assert_ne!(symlink_same_bytes.digest(), symlink_target_changed.digest());
    assert_ne!(absent.digest(), non_executable.digest());
}

#[test]
fn every_git_visible_path_requires_exactly_one_content_identity() {
    let missing = WorkspaceSnapshotManifestV1::new(
        worktree_id(),
        BASE,
        HEAD,
        vec![path("candidate")],
        vec![],
        vec![],
        vec![],
        vec![],
    );
    assert!(matches!(
        missing,
        Err(WorkspaceSnapshotError::InvalidCandidateIdentityCoverage)
    ));

    let extra = WorkspaceSnapshotManifestV1::new(
        worktree_id(),
        BASE,
        HEAD,
        vec![],
        vec![],
        vec![],
        vec![
            GitVisiblePathIdentity::new(
                path("not-visible"),
                CandidateObjectIdentity::Absent,
                CandidateObjectIdentity::RegularFile {
                    content_sha256: CONTENT_SHA256.to_owned(),
                    size_bytes: 1,
                    executable: false,
                },
            )
            .unwrap(),
        ],
        vec![],
    );
    assert!(matches!(
        extra,
        Err(WorkspaceSnapshotError::InvalidCandidateIdentityCoverage)
    ));
}

#[test]
fn aggregate_content_hashing_budget_is_enforced() {
    let over = WorkspaceSnapshotManifestV1::new(
        worktree_id(),
        BASE,
        HEAD,
        vec![path("candidate")],
        vec![],
        vec![],
        vec![
            GitVisiblePathIdentity::new(
                path("candidate"),
                CandidateObjectIdentity::Absent,
                CandidateObjectIdentity::RegularFile {
                    content_sha256: CONTENT_SHA256.to_owned(),
                    size_bytes: MAX_SNAPSHOT_CONTENT_BYTES,
                    executable: false,
                },
            )
            .unwrap(),
        ],
        vec![present("writer-only", CONTENT_SHA256, 1)],
    );
    assert!(matches!(
        over,
        Err(WorkspaceSnapshotError::LimitExceeded(
            "snapshot content byte"
        ))
    ));
}

#[test]
fn versioned_domain_has_a_fixed_golden_digest() {
    let snapshot = manifest(worktree_id(), BASE, HEAD, &["a"], &[], &[], vec![]);
    assert_eq!(
        snapshot.digest(),
        "a99383cb80eb09ad801ab5ecdc4972c1cda4b55832f218c35c932bd0d7047cf0"
    );
    assert!(WORKSPACE_SNAPSHOT_V1_DOMAIN.starts_with(b"managed-worktree-workspace-snapshot-v1"));
}

#[test]
fn duplicate_entries_are_rejected_within_each_logical_set() {
    for (changed, staged, untracked) in [
        (vec!["x", "x"], vec![], vec![]),
        (vec![], vec!["x", "x"], vec![]),
        (vec![], vec![], vec!["x", "x"]),
    ] {
        assert!(matches!(
            WorkspaceSnapshotManifestV1::new(
                worktree_id(),
                BASE,
                HEAD,
                changed.iter().map(|value| path(value)).collect(),
                staged.iter().map(|value| path(value)).collect(),
                untracked.iter().map(|value| path(value)).collect(),
                git_visible_identities(
                    changed
                        .iter()
                        .chain(staged.iter())
                        .chain(untracked.iter())
                        .copied(),
                ),
                vec![]
            ),
            Err(WorkspaceSnapshotError::DuplicatePath(_))
        ));
    }
    assert!(matches!(
        WorkspaceSnapshotManifestV1::new(
            worktree_id(),
            BASE,
            HEAD,
            vec![],
            vec![],
            vec![],
            vec![],
            vec![
                WriterMutatedPathIdentity::absent(path("x")),
                WriterMutatedPathIdentity::absent(path("x"))
            ]
        ),
        Err(WorkspaceSnapshotError::DuplicatePath(_))
    ));
}

#[test]
fn invalid_logical_paths_are_rejected() {
    for invalid in [
        "",
        "/absolute",
        "../escape",
        "a/../b",
        "./a",
        "a\0b",
        "C:/drive",
        "C:relative",
        "a//b",
        "a/",
        r"\\server\share",
        r"a\b",
    ] {
        assert!(
            matches!(
                NormalizedWorkspacePath::parse(invalid),
                Err(WorkspaceSnapshotError::InvalidPath)
            ),
            "accepted {invalid:?}"
        );
    }
}

#[test]
fn canonical_path_order_is_utf8_byte_order() {
    let snapshot = manifest(
        worktree_id(),
        BASE,
        HEAD,
        &["z", "a", "é", "ä"],
        &[],
        &[],
        vec![],
    );
    let expected = ["a", "z", "ä", "é"];
    let encoded = snapshot.canonical_bytes();
    let mut cursor = WORKSPACE_SNAPSHOT_V1_DOMAIN.len()
        + field_size(worktree_id().as_str().len())
        + field_size(BASE.len())
        + field_size(HEAD.len());
    assert_eq!(encoded[cursor], 0x10);
    cursor += 9;
    for expected_path in expected {
        assert_eq!(encoded[cursor], 0x01);
        cursor += 1;
        let length = u64::from_be_bytes(encoded[cursor..cursor + 8].try_into().unwrap()) as usize;
        cursor += 8;
        assert_eq!(&encoded[cursor..cursor + length], expected_path.as_bytes());
        cursor += length;
    }
}

fn field_size(value_len: usize) -> usize {
    1 + 8 + value_len
}

#[test]
fn content_digest_requires_exact_lowercase_sha256() {
    for invalid in [
        "A".repeat(64),
        "a".repeat(63),
        "a".repeat(65),
        format!("{}g", "a".repeat(63)),
    ] {
        assert!(matches!(
            WriterMutatedPathIdentity::present(path("x"), invalid, 0),
            Err(WorkspaceSnapshotError::InvalidContentDigest)
        ));
    }
    assert!(WriterMutatedPathIdentity::present(path("x"), EMPTY_SHA256, 0).is_ok());
}

#[test]
fn worktree_and_git_object_ids_are_validated() {
    assert!(matches!(
        WorkspaceSnapshotManifestV1::new(
            WorktreeId::parse("00000000-0000-4000-8000-000000000001").unwrap(),
            "bad",
            HEAD,
            vec![],
            vec![],
            vec![],
            vec![],
            vec![]
        ),
        Err(WorkspaceSnapshotError::InvalidObjectId("base_commit"))
    ));
    assert!(matches!(
        WorkspaceSnapshotManifestV1::new(
            worktree_id(),
            BASE,
            "A123456789abcdef0123456789abcdef01234567",
            vec![],
            vec![],
            vec![],
            vec![],
            vec![]
        ),
        Err(WorkspaceSnapshotError::InvalidObjectId("observed_head"))
    ));
}

#[test]
fn count_limits_accept_the_ceiling_and_reject_one_more() {
    for (limit, category) in [
        (MAX_CHANGED_PATHS, 0),
        (MAX_STAGED_PATHS, 1),
        (MAX_UNTRACKED_PATHS, 2),
        (MAX_WRITER_MUTATED_PATHS, 3),
    ] {
        let paths = (0..=limit)
            .map(|index| format!("p{index:05}"))
            .collect::<Vec<_>>();
        let writer_paths = (0..=limit)
            .map(|index| WriterMutatedPathIdentity::absent(path(&format!("p{index:05}"))))
            .collect::<Vec<_>>();
        let build = |count: usize| {
            let selected = paths[..count]
                .iter()
                .map(|value| path(value))
                .collect::<Vec<_>>();
            let identities = writer_paths[..count].to_vec();
            match category {
                0 => WorkspaceSnapshotManifestV1::new(
                    worktree_id(),
                    BASE,
                    HEAD,
                    selected,
                    vec![],
                    vec![],
                    git_visible_identities(paths[..count].iter().map(String::as_str)),
                    vec![],
                ),
                1 => WorkspaceSnapshotManifestV1::new(
                    worktree_id(),
                    BASE,
                    HEAD,
                    vec![],
                    selected,
                    vec![],
                    git_visible_identities(paths[..count].iter().map(String::as_str)),
                    vec![],
                ),
                2 => WorkspaceSnapshotManifestV1::new(
                    worktree_id(),
                    BASE,
                    HEAD,
                    vec![],
                    vec![],
                    selected,
                    git_visible_identities(paths[..count].iter().map(String::as_str)),
                    vec![],
                ),
                _ => WorkspaceSnapshotManifestV1::new(
                    worktree_id(),
                    BASE,
                    HEAD,
                    vec![],
                    vec![],
                    vec![],
                    vec![],
                    identities,
                ),
            }
        };
        assert!(matches!(
            build(limit),
            Ok(_) | Err(WorkspaceSnapshotError::LimitExceeded("canonical byte"))
        ));
        assert!(matches!(
            build(limit + 1),
            Err(WorkspaceSnapshotError::LimitExceeded(_))
        ));
    }
}

#[test]
fn git_visible_collection_count_is_bounded() {
    let paths = (0..=MAX_GIT_VISIBLE_PATHS)
        .map(|index| format!("p{index:05}"))
        .collect::<Vec<_>>();
    let identities = git_visible_identities(paths.iter().map(String::as_str));
    let result = WorkspaceSnapshotManifestV1::new(
        worktree_id(),
        BASE,
        HEAD,
        paths[..MAX_CHANGED_PATHS]
            .iter()
            .map(|value| path(value))
            .collect(),
        paths[MAX_CHANGED_PATHS..MAX_CHANGED_PATHS + MAX_STAGED_PATHS]
            .iter()
            .map(|value| path(value))
            .collect(),
        paths[MAX_CHANGED_PATHS + MAX_STAGED_PATHS..MAX_GIT_VISIBLE_PATHS]
            .iter()
            .map(|value| path(value))
            .collect(),
        identities,
        vec![],
    );
    assert!(matches!(
        result,
        Err(WorkspaceSnapshotError::LimitExceeded(
            "Git-visible path count"
        ))
    ));
}

#[test]
fn path_byte_limit_accepts_the_ceiling_and_rejects_one_more() {
    let at_limit = "a".repeat(MAX_NORMALIZED_PATH_BYTES);
    let over_limit = format!("{}a", at_limit);
    assert!(NormalizedWorkspacePath::parse(at_limit).is_ok());
    assert!(matches!(
        NormalizedWorkspacePath::parse(over_limit),
        Err(WorkspaceSnapshotError::LimitExceeded(
            "normalized path byte"
        ))
    ));
}

#[test]
fn canonical_manifest_limit_is_enforced_without_truncation() {
    let paths = |count: usize| {
        (0..count)
            .map(|index| {
                let prefix = format!("p{index:04}/");
                format!(
                    "{prefix}{}",
                    "x".repeat(MAX_NORMALIZED_PATH_BYTES - prefix.len())
                )
            })
            .collect::<Vec<_>>()
    };
    let within = WorkspaceSnapshotManifestV1::new(
        worktree_id(),
        BASE,
        HEAD,
        vec![],
        vec![],
        paths(240).iter().map(|value| path(value)).collect(),
        git_visible_identities(paths(240).iter().map(String::as_str)),
        vec![],
    )
    .unwrap();
    assert!(within.canonical_bytes().len() <= MAX_CANONICAL_MANIFEST_BYTES);

    let over = WorkspaceSnapshotManifestV1::new(
        worktree_id(),
        BASE,
        HEAD,
        vec![],
        vec![],
        paths(260).iter().map(|value| path(value)).collect(),
        git_visible_identities(paths(260).iter().map(String::as_str)),
        vec![],
    );
    assert!(matches!(
        over,
        Err(WorkspaceSnapshotError::LimitExceeded("canonical byte"))
    ));
}
