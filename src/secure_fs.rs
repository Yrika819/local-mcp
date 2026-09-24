use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};

pub(crate) fn ensure_private_directory(path: &Path, owned_parent: Option<&Path>) -> io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .ok_or_else(|| invalid(path, "directory has no parent"))?;
        if owned_parent.is_none() {
            ensure_non_owned_parent(parent)?;
        }
        let canonical_parent = fs::canonicalize(parent)?;
        if let Some(owned_parent) = owned_parent {
            let canonical_owned_parent = fs::canonicalize(owned_parent)?;
            if canonical_parent != canonical_owned_parent {
                return Err(invalid(path, "directory parent is not the owned parent"));
            }
            let parent_metadata = fs::symlink_metadata(parent)?;
            if !parent_metadata.file_type().is_dir() {
                return Err(invalid(parent, "owned parent is not a directory"));
            }
            open_private_directory_file(parent)?;
        }
        let _ = canonical_parent;
        match fs::symlink_metadata(path) {
            Ok(_) => open_private_directory_file(path).map(|_| ()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700);
                match builder.create(path) {
                    Ok(()) => finish_new_private_directory(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        open_private_directory_file(path).map(|_| ())
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }
    #[cfg(windows)]
    {
        let _ = owned_parent;
        fs::create_dir_all(path)
    }
}

#[cfg(unix)]
fn ensure_non_owned_parent(parent: &Path) -> io::Result<()> {
    match fs::symlink_metadata(parent) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(parent),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn open_private_directory_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = fstat(&file)?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(invalid(path, "path is not a directory"));
    }
    ensure_owner(path, metadata.st_uid)?;
    fchmod(file.as_raw_fd(), 0o700)?;
    Ok(file)
}

#[cfg(unix)]
fn finish_new_private_directory(path: &Path) -> io::Result<()> {
    let created = fs::symlink_metadata(path)?;
    match open_private_directory_file(path) {
        Ok(_) => Ok(()),
        Err(open_error) => {
            let current = match fs::symlink_metadata(path) {
                Ok(current) => current,
                Err(_) => return Err(open_error),
            };
            if !same_object(&created, &current) || !current.file_type().is_dir() {
                return Err(open_error);
            }
            if ensure_owner(path, current.uid()).is_err() {
                return Err(open_error);
            }
            let _ = remove_new_directory_if_same(path, &created);
            Err(open_error)
        }
    }
}

#[cfg(unix)]
fn same_object(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(unix)]
fn remove_new_directory_if_same(path: &Path, created: &fs::Metadata) -> io::Result<()> {
    let current = fs::symlink_metadata(path)?;
    if same_object(created, &current) && current.file_type().is_dir() {
        fs::remove_dir(path)?;
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn open_private_existing(path: &Path, write: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(write)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = options.open(path)?;
    validate_private_regular(&file, path)?;
    Ok(file)
}

#[cfg(windows)]
pub(crate) fn open_private_existing(path: &Path, write: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(write)
        .truncate(false)
        .open(path)
}

#[cfg(unix)]
pub(crate) fn create_or_open_lock(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = options.open(path)?;
    validate_private_regular(&file, path)?;
    Ok(file)
}

#[cfg(windows)]
pub(crate) fn create_or_open_lock(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
}

pub(crate) fn create_unique_temp(destination: &Path) -> io::Result<(PathBuf, File)> {
    let directory = destination
        .parent()
        .ok_or_else(|| invalid(destination, "temporary destination has no parent"))?;
    let prefix = temp_prefix(destination)?;
    for _ in 0..32 {
        let path = directory.join(format!("{prefix}{}.tmp", Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
        }
        match options.open(&path) {
            Ok(file) => {
                #[cfg(unix)]
                if let Err(error) = validate_private_regular(&file, &path) {
                    drop(file);
                    let _ = remove_private_temp(&path);
                    return Err(error);
                }
                return Ok((path, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique temporary path",
    ))
}

pub(crate) fn temp_prefix(destination: &Path) -> io::Result<String> {
    let file_name = destination
        .file_name()
        .ok_or_else(|| invalid(destination, "destination has no file name"))?
        .to_string_lossy();
    let digest = Sha256::digest(file_name.as_bytes());
    let mut hash = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        write!(&mut hash, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(format!(".{hash}."))
}

pub(crate) fn remove_private_temp(path: &Path) -> io::Result<()> {
    if !is_unique_temp_path(path) {
        return Err(invalid(path, "path is not a unique temporary file"));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(invalid(path, "temporary path is not a regular file"));
            }
            #[cfg(unix)]
            ensure_owner(path, metadata.uid())?;
            fs::remove_file(path)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn is_unique_temp_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(stem) = name.strip_suffix(".tmp") else {
        return false;
    };
    let mut parts = stem.split('.');
    if parts.next() != Some("") {
        return false;
    }
    let Some(hash) = parts.next() else {
        return false;
    };
    if hash.len() != 16
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return false;
    }
    let Some(id) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let Ok(uuid) = Uuid::parse_str(id) else {
        return false;
    };
    uuid.to_string() == id
}

#[cfg(unix)]
pub(crate) fn secure_socket(path: &Path) -> io::Result<()> {
    let before = fs::symlink_metadata(path)?;
    if !before.file_type().is_socket() {
        return Err(invalid(path, "path is not a Unix socket"));
    }
    ensure_owner(path, before.uid())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    let after = fs::symlink_metadata(path)?;
    if !after.file_type().is_socket()
        || after.dev() != before.dev()
        || after.ino() != before.ino()
        || after.uid() != before.uid()
        || after.permissions().mode() & 0o777 != 0o600
    {
        return Err(invalid(path, "Unix socket changed during mode repair"));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_regular(file: &File, path: &Path) -> io::Result<()> {
    let metadata = fstat(file)?;
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(invalid(path, "path is not a regular file"));
    }
    ensure_owner(path, metadata.st_uid)?;
    fchmod(file.as_raw_fd(), 0o600)
}

#[cfg(unix)]
fn fstat(file: &File) -> io::Result<libc::stat> {
    fstat_fd(file.as_raw_fd())
}

#[cfg(unix)]
fn fstat_fd(fd: RawFd) -> io::Result<libc::stat> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe { libc::fstat(fd, metadata.as_mut_ptr()) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { metadata.assume_init() })
}

#[cfg(unix)]
fn fchmod(fd: RawFd, mode: libc::mode_t) -> io::Result<()> {
    let result = unsafe { libc::fchmod(fd, mode) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_owner(path: &Path, owner: libc::uid_t) -> io::Result<()> {
    if owner != unsafe { libc::geteuid() } {
        return Err(invalid(path, "owner is not the effective user"));
    }
    Ok(())
}

#[cfg(windows)]
fn extended_path_wide(path: &Path) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let mut source = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if source.starts_with(&[0x5c, 0x5c, 0x3f, 0x5c])
        || source.starts_with(&[0x5c, 0x5c, 0x2e, 0x5c])
    {
        source.push(0);
        return Ok(source);
    }
    let mut extended = Vec::new();
    if source.starts_with(&[0x5c, 0x5c]) {
        extended.extend_from_slice(&[0x5c, 0x5c, 0x3f, 0x5c, 0x55, 0x4e, 0x43, 0x5c]);
        extended.extend_from_slice(&source[2..]);
    } else if source.len() >= 3 && source[1] == u16::from(b':') && source[2] == 0x5c {
        extended.extend_from_slice(&[0x5c, 0x5c, 0x3f, 0x5c]);
        extended.extend_from_slice(&source);
    } else {
        return Err(invalid(path, "path is not an absolute Windows path"));
    }
    extended.push(0);
    Ok(extended)
}

#[cfg(windows)]
fn move_path_wide(path: &Path) -> io::Result<Vec<u16>> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid(path, "path has no parent"))?;
    let canonical_parent = fs::canonicalize(parent)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| invalid(path, "path has no file name"))?;
    extended_path_wide(&canonical_parent.join(file_name))
}

#[cfg(unix)]
pub(crate) fn atomic_replace(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(windows)]
pub(crate) fn atomic_replace(temporary: &Path, destination: &Path) -> io::Result<()> {
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    let existing = move_path_wide(temporary)?;
    let new = move_path_wide(destination)?;
    let result = unsafe {
        MoveFileExW(
            existing.as_ptr(),
            new.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
pub(crate) fn sync_parent_directory(directory: &Path) -> io::Result<()> {
    open_private_directory_file(directory)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_parent_directory(_directory: &Path) -> io::Result<()> {
    Ok(())
}

fn invalid(path: &Path, detail: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {}", path.display(), detail),
    )
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn extended_paths_cover_drive_unc_and_canonical_parent() {
        let drive = extended_path_wide(Path::new(r"C:\state\file")).unwrap();
        assert_eq!(
            String::from_utf16(&drive[..drive.len() - 1]).unwrap(),
            r"\\?\C:\state\file"
        );
        let unc = extended_path_wide(Path::new(r"\\server\share\file")).unwrap();
        assert_eq!(
            String::from_utf16(&unc[..unc.len() - 1]).unwrap(),
            r"\\?\UNC\server\share\file"
        );
        let parent = std::env::temp_dir().join(format!("local-mcp-wide-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&parent).unwrap();
        let path = parent.join("file");
        let wide = move_path_wide(&path).unwrap();
        assert!(
            String::from_utf16(&wide[..wide.len() - 1])
                .unwrap()
                .starts_with(r"\\?\")
        );
        std::fs::remove_dir_all(parent).unwrap();
    }
}
