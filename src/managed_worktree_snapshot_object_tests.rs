use sha2::Digest as _;
use std::path::Path;

use crate::managed_worktree_snapshot_object::{
    SnapshotHashBudget, SnapshotObjectError, observe_worktree_object,
};
use crate::managed_worktree_snapshot_observe::{
    GitFileModePolicy, SnapshotIndexEntry, SnapshotIndexMode,
};
use crate::workspace_snapshot::{CandidateObjectIdentity, NormalizedWorkspacePath};

fn normalized(value: &str) -> NormalizedWorkspacePath {
    NormalizedWorkspacePath::parse(value).unwrap()
}

fn temp_dir(label: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "local-mcp-snapshot-2b-{label}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&path).unwrap();
    std::fs::canonicalize(path).unwrap()
}

fn git(repo: &Path, args: &[&str]) {
    let output = std::process::Command::new(crate::execution::host_git_path().unwrap())
        .args(args)
        .current_dir(repo)
        .output()
        .expect("host Git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn index_entry(value: &str, executable: bool) -> SnapshotIndexEntry {
    SnapshotIndexEntry {
        path: normalized(value),
        mode: SnapshotIndexMode::Regular { executable },
        oid: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
    }
}

fn observe(
    root: &Path,
    value: &str,
    entry: Option<&SnapshotIndexEntry>,
    policy: GitFileModePolicy,
    budget: &mut SnapshotHashBudget,
) -> Result<CandidateObjectIdentity, SnapshotObjectError> {
    observe_worktree_object(root, &normalized(value), entry, policy, budget)
}

#[cfg(unix)]
#[test]
fn regular_file_is_stream_hashed_and_trust_policy_uses_owner_execute_bit() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_dir("regular");
    std::fs::write(root.join("file"), b"payload").unwrap();
    let metadata = std::fs::metadata(root.join("file")).unwrap();
    std::fs::set_permissions(
        root.join("file"),
        std::fs::Permissions::from_mode(metadata.permissions().mode() | 0o100),
    )
    .unwrap();
    let mut budget = SnapshotHashBudget::new();
    let object = observe(
        &root,
        "file",
        None,
        GitFileModePolicy::TrustExecutableBit,
        &mut budget,
    )
    .unwrap();
    assert_eq!(
        object,
        CandidateObjectIdentity::RegularFile {
            content_sha256: format!("{:x}", sha2::Sha256::digest(b"payload")),
            size_bytes: 7,
            executable: true,
        }
    );
    assert_eq!(budget.hashed_bytes(), 7);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn ignored_filemode_uses_existing_regular_index_mode_and_defaults_false() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_dir("ignored-mode");
    std::fs::write(root.join("file"), b"x").unwrap();
    let metadata = std::fs::metadata(root.join("file")).unwrap();
    std::fs::set_permissions(
        root.join("file"),
        std::fs::Permissions::from_mode(metadata.permissions().mode() | 0o100),
    )
    .unwrap();
    let mut budget = SnapshotHashBudget::new();
    let indexed = index_entry("file", true);
    let object = observe(
        &root,
        "file",
        Some(&indexed),
        GitFileModePolicy::IgnoreExecutableBit,
        &mut budget,
    )
    .unwrap();
    assert!(matches!(
        object,
        CandidateObjectIdentity::RegularFile {
            executable: true,
            ..
        }
    ));
    let object = observe(
        &root,
        "file",
        None,
        GitFileModePolicy::IgnoreExecutableBit,
        &mut budget,
    )
    .unwrap();
    assert!(matches!(
        object,
        CandidateObjectIdentity::RegularFile {
            executable: false,
            ..
        }
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn symlink_target_is_hashed_raw_and_dangling_links_are_supported() {
    let root = temp_dir("symlink");
    std::os::unix::fs::symlink("missing-target", root.join("link")).unwrap();
    let mut budget = SnapshotHashBudget::new();
    let object = observe(
        &root,
        "link",
        None,
        GitFileModePolicy::TrustExecutableBit,
        &mut budget,
    )
    .unwrap();
    assert_eq!(
        object,
        CandidateObjectIdentity::Symlink {
            target_sha256: format!("{:x}", sha2::Sha256::digest(b"missing-target")),
            target_size_bytes: 14,
        }
    );
    assert_eq!(budget.hashed_bytes(), 14);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn absent_paths_and_symlinked_parent_components_are_distinct() {
    let root = temp_dir("parents");
    let outside = temp_dir("outside");
    std::fs::write(outside.join("file"), b"secret").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
    let mut budget = SnapshotHashBudget::new();
    assert_eq!(
        observe(
            &root,
            "missing",
            None,
            GitFileModePolicy::TrustExecutableBit,
            &mut budget
        )
        .unwrap(),
        CandidateObjectIdentity::Absent
    );
    assert!(
        observe(
            &root,
            "linked/file",
            None,
            GitFileModePolicy::TrustExecutableBit,
            &mut budget
        )
        .is_err()
    );
    assert_eq!(budget.hashed_bytes(), 0);
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(outside);
}

#[cfg(unix)]
#[test]
fn replacing_parent_with_symlink_after_open_does_not_redirect_the_read() {
    let root = temp_dir("parent-race");
    let outside = temp_dir("parent-race-outside");
    std::fs::create_dir(root.join("dir")).unwrap();
    std::fs::write(root.join("dir/file"), b"inside").unwrap();
    std::fs::write(outside.join("file"), b"outside-secret").unwrap();
    let moved = root.join("original-dir");
    let mut budget = SnapshotHashBudget::new();
    let result = crate::managed_worktree_snapshot_object::observe_with_hook(
        &root,
        &normalized("dir/file"),
        None,
        GitFileModePolicy::TrustExecutableBit,
        &mut budget,
        || {
            std::fs::rename(root.join("dir"), &moved).unwrap();
            std::os::unix::fs::symlink(&outside, root.join("dir")).unwrap();
        },
    )
    .unwrap();
    assert_eq!(
        result,
        CandidateObjectIdentity::RegularFile {
            content_sha256: format!("{:x}", sha2::Sha256::digest(b"inside")),
            size_bytes: 6,
            executable: false,
        }
    );
    assert_eq!(budget.hashed_bytes(), 6);
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(outside);
}

#[cfg(unix)]
#[test]
fn special_files_fail_closed_without_blocking() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let root = temp_dir("special");
    let fifo = CString::new(root.join("fifo").as_os_str().as_bytes()).unwrap();
    // SAFETY: fifo is a live NUL-terminated path; mkfifo only creates a test FIFO.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let mut budget = SnapshotHashBudget::new();
    assert!(matches!(
        observe(
            &root,
            "fifo",
            None,
            GitFileModePolicy::TrustExecutableBit,
            &mut budget
        ),
        Err(SnapshotObjectError::Unsupported(_))
    ));
    assert_eq!(budget.hashed_bytes(), 0);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn metadata_change_during_stream_is_rejected() {
    let root = temp_dir("race");
    std::fs::write(root.join("file"), b"before").unwrap();
    let mut budget = SnapshotHashBudget::new();
    let result = crate::managed_worktree_snapshot_object::observe_with_hook(
        &root,
        &normalized("file"),
        None,
        GitFileModePolicy::TrustExecutableBit,
        &mut budget,
        || std::fs::write(root.join("file"), b"after-content-is-different").unwrap(),
    );
    assert!(matches!(result, Err(SnapshotObjectError::Unstable)));
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn real_git_filemode_policy_and_index_mode_drive_executable_mapping() {
    use crate::managed_worktree_snapshot_observe::{HostSnapshotGit, observe_snapshot_metadata};
    use std::os::unix::fs::PermissionsExt;

    let base = temp_dir("git-filemode-map");
    let repo = base.join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(
        &repo,
        &["config", "user.email", "snapshot2b@example.invalid"],
    );
    git(&repo, &["config", "user.name", "Snapshot 2B Test"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["config", "core.fileMode", "true"]);
    std::fs::write(repo.join("plain"), b"plain\n").unwrap();
    std::fs::write(repo.join("executable"), b"executable\n").unwrap();
    let plain_mode = std::fs::metadata(repo.join("plain"))
        .unwrap()
        .permissions()
        .mode();
    let exec_mode = std::fs::metadata(repo.join("executable"))
        .unwrap()
        .permissions()
        .mode();
    std::fs::set_permissions(
        repo.join("plain"),
        std::fs::Permissions::from_mode(plain_mode & !0o111),
    )
    .unwrap();
    std::fs::set_permissions(
        repo.join("executable"),
        std::fs::Permissions::from_mode(exec_mode | 0o100),
    )
    .unwrap();
    git(&repo, &["add", "plain", "executable"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    git(&repo, &["config", "core.fileMode", "false"]);

    std::fs::write(repo.join("plain"), b"plain changed\n").unwrap();
    std::fs::write(repo.join("executable"), b"executable changed\n").unwrap();
    let plain_mode = std::fs::metadata(repo.join("plain"))
        .unwrap()
        .permissions()
        .mode();
    let exec_mode = std::fs::metadata(repo.join("executable"))
        .unwrap()
        .permissions()
        .mode();
    std::fs::set_permissions(
        repo.join("plain"),
        std::fs::Permissions::from_mode(plain_mode | 0o100),
    )
    .unwrap();
    std::fs::set_permissions(
        repo.join("executable"),
        std::fs::Permissions::from_mode(exec_mode & !0o100),
    )
    .unwrap();
    std::fs::write(repo.join("new"), b"new\n").unwrap();
    let new_mode = std::fs::metadata(repo.join("new"))
        .unwrap()
        .permissions()
        .mode();
    std::fs::set_permissions(
        repo.join("new"),
        std::fs::Permissions::from_mode(new_mode | 0o100),
    )
    .unwrap();

    let metadata = observe_snapshot_metadata(&HostSnapshotGit, &repo).unwrap();
    assert_eq!(
        metadata.file_mode_policy,
        GitFileModePolicy::IgnoreExecutableBit
    );
    assert!(metadata.changed_paths.contains(&normalized("plain")));
    assert!(metadata.changed_paths.contains(&normalized("executable")));
    let plain_entry = metadata
        .index_entries
        .iter()
        .find(|entry| entry.path == normalized("plain"))
        .unwrap();
    let executable_entry = metadata
        .index_entries
        .iter()
        .find(|entry| entry.path == normalized("executable"))
        .unwrap();
    assert!(matches!(
        plain_entry.mode,
        SnapshotIndexMode::Regular { executable: false }
    ));
    assert!(matches!(
        executable_entry.mode,
        SnapshotIndexMode::Regular { executable: true }
    ));
    let mut budget = SnapshotHashBudget::new();
    assert!(matches!(
        observe(
            &repo,
            "plain",
            Some(plain_entry),
            metadata.file_mode_policy,
            &mut budget
        )
        .unwrap(),
        CandidateObjectIdentity::RegularFile {
            executable: false,
            ..
        }
    ));
    assert!(matches!(
        observe(
            &repo,
            "executable",
            Some(executable_entry),
            metadata.file_mode_policy,
            &mut budget
        )
        .unwrap(),
        CandidateObjectIdentity::RegularFile {
            executable: true,
            ..
        }
    ));
    assert!(matches!(
        observe(&repo, "new", None, metadata.file_mode_policy, &mut budget).unwrap(),
        CandidateObjectIdentity::RegularFile {
            executable: false,
            ..
        }
    ));

    let trust_base = temp_dir("git-filemode-trust");
    let trust_repo = trust_base.join("repo");
    std::fs::create_dir(&trust_repo).unwrap();
    git(&trust_repo, &["init", "-q", "-b", "main"]);
    git(
        &trust_repo,
        &["config", "user.email", "snapshot2b@example.invalid"],
    );
    git(&trust_repo, &["config", "user.name", "Snapshot 2B Test"]);
    git(&trust_repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(trust_repo.join("file"), b"tracked\n").unwrap();
    git(&trust_repo, &["add", "file"]);
    git(&trust_repo, &["commit", "-q", "-m", "initial"]);
    let file_mode = std::fs::metadata(trust_repo.join("file"))
        .unwrap()
        .permissions()
        .mode();
    std::fs::set_permissions(
        trust_repo.join("file"),
        std::fs::Permissions::from_mode(file_mode | 0o100),
    )
    .unwrap();
    let trust_metadata = observe_snapshot_metadata(&HostSnapshotGit, &trust_repo).unwrap();
    assert_eq!(
        trust_metadata.file_mode_policy,
        GitFileModePolicy::TrustExecutableBit
    );
    assert!(trust_metadata.changed_paths.contains(&normalized("file")));
    let entry = trust_metadata
        .index_entries
        .iter()
        .find(|entry| entry.path == normalized("file"))
        .unwrap();
    assert!(matches!(
        observe(
            &trust_repo,
            "file",
            Some(entry),
            trust_metadata.file_mode_policy,
            &mut budget
        )
        .unwrap(),
        CandidateObjectIdentity::RegularFile {
            executable: true,
            ..
        }
    ));
    let _ = std::fs::remove_dir_all(base);
    let _ = std::fs::remove_dir_all(trust_base);
}

#[cfg(windows)]
#[test]
fn windows_trusted_executable_metadata_fails_closed() {
    let root = temp_dir("windows-mode");
    std::fs::write(root.join("file"), b"x").unwrap();
    let mut budget = SnapshotHashBudget::new();
    assert!(matches!(
        observe(
            &root,
            "file",
            None,
            GitFileModePolicy::TrustExecutableBit,
            &mut budget
        ),
        Err(SnapshotObjectError::Unsupported(_))
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(windows)]
#[test]
fn windows_ignored_filemode_maps_only_existing_regular_index_mode() {
    let root = temp_dir("windows-index-mode");
    std::fs::write(root.join("tracked"), b"tracked").unwrap();
    std::fs::write(root.join("new"), b"new").unwrap();
    let entry = index_entry("tracked", true);
    let mut budget = SnapshotHashBudget::new();
    assert!(matches!(
        observe(
            &root,
            "tracked",
            Some(&entry),
            GitFileModePolicy::IgnoreExecutableBit,
            &mut budget
        )
        .unwrap(),
        CandidateObjectIdentity::RegularFile {
            executable: true,
            ..
        }
    ));
    assert!(matches!(
        observe(
            &root,
            "new",
            None,
            GitFileModePolicy::IgnoreExecutableBit,
            &mut budget
        )
        .unwrap(),
        CandidateObjectIdentity::RegularFile {
            executable: false,
            ..
        }
    ));
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(test)]
#[test]
fn aggregate_budget_overflow_is_checked_before_digest_growth() {
    let root = temp_dir("budget");
    std::fs::write(root.join("file"), b"four").unwrap();
    let mut budget = SnapshotHashBudget::with_hashed_bytes_for_test(256 * 1024 * 1024 - 3);
    assert!(matches!(
        observe(
            &root,
            "file",
            None,
            GitFileModePolicy::TrustExecutableBit,
            &mut budget
        ),
        Err(SnapshotObjectError::LimitExceeded(_))
    ));
    assert_eq!(budget.hashed_bytes(), 256 * 1024 * 1024 - 3);
    let _ = std::fs::remove_dir_all(root);
}
