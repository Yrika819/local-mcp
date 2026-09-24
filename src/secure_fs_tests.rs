use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::secure_fs;

fn root() -> PathBuf {
    let path = std::env::temp_dir().join(format!("local-mcp-secure-fs-{}", Uuid::new_v4()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn identity(path: &Path) -> (u64, u64) {
    let metadata = fs::metadata(path).unwrap();
    (metadata.dev(), metadata.ino())
}

fn private_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o777)).unwrap();
}

fn private_file(path: &Path) {
    fs::write(path, b"before").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o666)).unwrap();
}

fn run_child_test(test_name: &str, paths: &[(&str, &Path)]) {
    let executable = std::env::current_exe().unwrap();
    let mut command = Command::new(executable);
    command
        .arg("--exact")
        .arg(test_name)
        .arg("--nocapture")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for (name, path) in paths {
        command.env(name, path);
    }
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "child test failed: {status}");
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child test exceeded bounded runtime");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn new_private_directory_file_and_temp_use_exact_modes() {
    let base = root();
    let directory = base.join("state");
    let final_path = directory.join("state.json");

    secure_fs::ensure_private_directory(&directory, None).unwrap();
    let (temporary, mut file) = secure_fs::create_unique_temp(&final_path).unwrap();
    assert_eq!(mode(&directory), 0o700);
    assert_eq!(mode(&temporary), 0o600);
    file.write_all(b"private").unwrap();
    file.sync_all().unwrap();
    secure_fs::atomic_replace(&temporary, &final_path).unwrap();
    secure_fs::sync_parent_directory(&directory).unwrap();

    assert_eq!(mode(&final_path), 0o600);
    assert!(!temporary.exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn fifo_open_and_lock_reject_promptly_without_following() {
    if let Some(path) = std::env::var_os("LOCAL_MCP_SECURE_FIFO_PATH") {
        let path = PathBuf::from(path);
        let read_error = secure_fs::open_private_existing(&path, false).unwrap_err();
        assert_eq!(read_error.kind(), std::io::ErrorKind::InvalidData);
        let lock_error = secure_fs::create_or_open_lock(&path).unwrap_err();
        assert_eq!(lock_error.kind(), std::io::ErrorKind::InvalidData);
        return;
    }

    let base = root();
    let fifo = base.join("pipe");
    let outside = base.join("outside");
    let sentinel = outside.join("sentinel");
    fs::create_dir_all(&outside).unwrap();
    fs::write(&sentinel, b"unchanged").unwrap();
    let sentinel_mode = mode(&sentinel);
    let fifo_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    let result = unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) };
    assert_eq!(result, 0);

    run_child_test(
        "secure_fs_tests::fifo_open_and_lock_reject_promptly_without_following",
        &[
            ("LOCAL_MCP_SECURE_FIFO_PATH", fifo.as_path()),
            ("LOCAL_MCP_SECURE_SENTINEL_PATH", sentinel.as_path()),
        ],
    );

    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
    assert_eq!(mode(&sentinel), sentinel_mode);
    assert!(fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn restrictive_child_umask_fails_closed_for_new_directory() {
    if let Some(path) = std::env::var_os("LOCAL_MCP_SECURE_UMASK_PATH") {
        let path = PathBuf::from(path);
        let previous = unsafe { libc::umask(0o777) };
        let result = secure_fs::ensure_private_directory(&path, None);
        unsafe {
            libc::umask(previous);
        }
        assert!(result.is_err());
        assert!(!path.exists());
        return;
    }

    let base = root();
    let base_mode = mode(&base);
    let path = base.join("state");
    run_child_test(
        "secure_fs_tests::restrictive_child_umask_fails_closed_for_new_directory",
        &[("LOCAL_MCP_SECURE_UMASK_PATH", path.as_path())],
    );
    assert!(!path.exists());
    assert!(!path.join("usable").exists());
    assert_eq!(mode(&base), base_mode);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn missing_base_parent_is_bootstrapped_without_private_chmod() {
    let base = root();
    let reference = base.join("reference");
    fs::create_dir_all(&reference).unwrap();
    let reference_mode = mode(&reference);
    fs::remove_dir(&reference).unwrap();
    let missing_base = base.join("missing-base");
    let state = missing_base.join("state");

    secure_fs::ensure_private_directory(&state, None).unwrap();

    assert_eq!(mode(&missing_base), reference_mode);
    assert_eq!(mode(&state), 0o700);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn long_destination_temp_basename_is_short_and_private() {
    let base = root();
    let directory = base.join("state");
    let id = "a".repeat(64);
    assert!(crate::config::validate_session_id(&id).is_ok());
    let destination = directory.join(format!("{id}.json"));
    secure_fs::ensure_private_directory(&directory, None).unwrap();

    let (temporary, file) = secure_fs::create_unique_temp(&destination).unwrap();
    let name = temporary
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(name.len() < 80);
    assert!(name.starts_with(&secure_fs::temp_prefix(&destination).unwrap()));
    assert!(name.ends_with(".tmp"));
    assert_eq!(mode(&temporary), 0o600);
    drop(file);
    secure_fs::remove_private_temp(&temporary).unwrap();
    assert!(!temporary.exists());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn socket_symlink_is_rejected_without_chmodding_outside_sentinel() {
    let base = root();
    let outside = base.join("outside");
    let sentinel = outside.join("sentinel");
    let socket = base.join("session.sock");
    fs::create_dir_all(&outside).unwrap();
    fs::write(&sentinel, b"unchanged").unwrap();
    let sentinel_mode = mode(&sentinel);
    std::os::unix::fs::symlink(&sentinel, &socket).unwrap();

    assert!(secure_fs::secure_socket(&socket).is_err());
    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
    assert_eq!(mode(&sentinel), sentinel_mode);
    assert!(
        fs::symlink_metadata(&socket)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn existing_directory_file_and_lock_repair_without_inode_replacement() {
    let base = root();
    let directory = base.join("state");
    let file_path = directory.join("state.json");
    let lock_path = directory.join(".lock");
    private_directory(&directory);
    private_file(&file_path);
    private_file(&lock_path);
    let directory_identity = identity(&directory);
    let file_identity = identity(&file_path);
    let lock_identity = identity(&lock_path);

    secure_fs::ensure_private_directory(&directory, None).unwrap();
    secure_fs::open_private_existing(&file_path, false).unwrap();
    secure_fs::create_or_open_lock(&lock_path).unwrap();

    assert_eq!(mode(&directory), 0o700);
    assert_eq!(mode(&file_path), 0o600);
    assert_eq!(mode(&lock_path), 0o600);
    assert_eq!(identity(&directory), directory_identity);
    assert_eq!(identity(&file_path), file_identity);
    assert_eq!(identity(&lock_path), lock_identity);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn final_file_symlink_is_rejected_without_touching_outside_sentinel() {
    let base = root();
    let outside = base.join("outside");
    let directory = base.join("state");
    let sentinel = outside.join("sentinel");
    let final_path = directory.join("state.json");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&directory).unwrap();
    fs::write(&sentinel, b"unchanged").unwrap();
    let sentinel_mode = mode(&sentinel);
    std::os::unix::fs::symlink(&sentinel, &final_path).unwrap();

    assert!(secure_fs::open_private_existing(&final_path, false).is_err());
    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
    assert_eq!(mode(&sentinel), sentinel_mode);
    assert!(
        fs::symlink_metadata(&final_path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn directory_type_and_final_symlink_fail_closed() {
    let base = root();
    let outside = base.join("outside");
    let regular = base.join("regular");
    let linked_directory = base.join("linked");
    fs::create_dir_all(&outside).unwrap();
    fs::write(&regular, b"not a directory").unwrap();
    std::os::unix::fs::symlink(&outside, &linked_directory).unwrap();

    assert!(secure_fs::ensure_private_directory(&regular, None).is_err());
    assert!(secure_fs::ensure_private_directory(&linked_directory, None).is_err());
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn cleanup_accepts_valid_unique_temp_and_rejects_valid_temp_directory() {
    let base = root();
    let destination = base.join("state.json");
    let prefix = secure_fs::temp_prefix(&destination).unwrap();
    let valid_regular = base.join(format!("{prefix}{}.tmp", Uuid::new_v4()));
    let valid_directory = base.join(format!("{prefix}{}.tmp", Uuid::new_v4()));
    let wrong_hash = base.join(format!(".not-a-hash.{}.tmp", Uuid::new_v4()));
    fs::write(&valid_regular, b"remove").unwrap();
    fs::create_dir_all(&valid_directory).unwrap();
    fs::write(&wrong_hash, b"keep").unwrap();

    assert!(secure_fs::remove_private_temp(&valid_regular).is_ok());
    assert!(!valid_regular.exists());
    assert!(secure_fs::remove_private_temp(&valid_directory).is_err());
    assert!(secure_fs::remove_private_temp(&wrong_hash).is_err());
    assert!(valid_directory.exists());
    assert!(wrong_hash.exists());
    fs::remove_dir_all(base).unwrap();
}
