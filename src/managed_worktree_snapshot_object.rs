//! Slice 2B bounded observation of actual working-tree filesystem objects.
//!
//! Git configuration is supplied by the already-completed Slice 2A observer;
//! this layer never runs Git. Unix traversal is descriptor-relative from an
//! opened root, refuses symlinked parents/final opens, and hashes streams with
//! a fixed reusable buffer. Windows uses handle-relative opens and fails
//! closed for reparse objects and unreliable executable metadata.

#![expect(
    dead_code,
    reason = "Slice 2B freezes the bounded filesystem observer before snapshot assembly is authorized."
)]

use std::fmt::Write as _;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::managed_worktree_snapshot_observe::{
    GitFileModePolicy, SnapshotIndexEntry, SnapshotIndexMode,
};
use crate::workspace_snapshot::{
    CandidateObjectIdentity, MAX_SNAPSHOT_CONTENT_BYTES, NormalizedWorkspacePath,
};

const HASH_BUFFER_BYTES: usize = 64 * 1024;
const MAX_SYMLINK_TARGET_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub(crate) enum SnapshotObjectError {
    Io(std::io::Error),
    Unsupported(&'static str),
    LimitExceeded(&'static str),
    Unstable,
}

impl std::fmt::Display for SnapshotObjectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "snapshot object I/O failed: {error}"),
            Self::Unsupported(reason) => write!(formatter, "unsupported snapshot object: {reason}"),
            Self::LimitExceeded(reason) => {
                write!(formatter, "snapshot object {reason} limit exceeded")
            }
            Self::Unstable => formatter.write_str("snapshot object changed while being observed"),
        }
    }
}

impl std::error::Error for SnapshotObjectError {}

impl From<std::io::Error> for SnapshotObjectError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Checked aggregate budget shared by every candidate object in one snapshot.
#[derive(Clone, Debug)]
pub(crate) struct SnapshotHashBudget {
    hashed_bytes: u64,
}

impl SnapshotHashBudget {
    pub(crate) fn new() -> Self {
        Self { hashed_bytes: 0 }
    }

    pub(crate) fn hashed_bytes(&self) -> u64 {
        self.hashed_bytes
    }

    fn remaining(&self) -> u64 {
        MAX_SNAPSHOT_CONTENT_BYTES - self.hashed_bytes
    }

    #[cfg(test)]
    pub(crate) fn with_hashed_bytes_for_test(hashed_bytes: u64) -> Self {
        Self { hashed_bytes }
    }

    fn charge(&mut self, bytes: usize) -> Result<(), SnapshotObjectError> {
        let bytes = u64::try_from(bytes)
            .map_err(|_| SnapshotObjectError::LimitExceeded("content bytes"))?;
        self.hashed_bytes = self
            .hashed_bytes
            .checked_add(bytes)
            .filter(|total| *total <= MAX_SNAPSHOT_CONTENT_BYTES)
            .ok_or(SnapshotObjectError::LimitExceeded(
                "aggregate content bytes",
            ))?;
        Ok(())
    }
}

/// Hash one actual worktree object. `index_entry` is used only when Git says
/// executable-bit differences are ignored; it never causes a Git lookup.
pub(crate) fn observe_worktree_object(
    root: &Path,
    path: &NormalizedWorkspacePath,
    index_entry: Option<&SnapshotIndexEntry>,
    file_mode_policy: GitFileModePolicy,
    budget: &mut SnapshotHashBudget,
) -> Result<CandidateObjectIdentity, SnapshotObjectError> {
    observe_with_hook(root, path, index_entry, file_mode_policy, budget, || {})
}

pub(crate) fn observe_with_hook<F: FnMut()>(
    root: &Path,
    path: &NormalizedWorkspacePath,
    index_entry: Option<&SnapshotIndexEntry>,
    file_mode_policy: GitFileModePolicy,
    budget: &mut SnapshotHashBudget,
    #[cfg_attr(windows, allow(unused_mut))] mut before_read: F,
) -> Result<CandidateObjectIdentity, SnapshotObjectError> {
    #[cfg(unix)]
    {
        unix::observe(
            root,
            path,
            index_entry,
            file_mode_policy,
            budget,
            &mut before_read,
        )
    }
    #[cfg(windows)]
    {
        let _ = before_read;
        windows::observe(root, path, index_entry, file_mode_policy, budget)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (
            root,
            path,
            index_entry,
            file_mode_policy,
            budget,
            before_read,
        );
        Err(SnapshotObjectError::Unsupported(
            "platform has no safe object observer",
        ))
    }
}

fn digest_hex(digest: Sha256) -> String {
    let mut result = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(&mut result, "{byte:02x}").expect("writing to String cannot fail");
    }
    result
}

fn executable_for(
    policy: GitFileModePolicy,
    index_entry: Option<&SnapshotIndexEntry>,
    mode: u32,
) -> Result<bool, SnapshotObjectError> {
    match policy {
        GitFileModePolicy::TrustExecutableBit => Ok(mode & 0o100 != 0),
        GitFileModePolicy::IgnoreExecutableBit => Ok(matches!(
            index_entry.map(|entry| entry.mode),
            Some(SnapshotIndexMode::Regular { executable: true })
        )),
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::ffi::CString;
    use std::fs::OpenOptions;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    fn c_name(value: &str) -> Result<CString, SnapshotObjectError> {
        CString::new(value)
            .map_err(|_| SnapshotObjectError::Unsupported("path component contains NUL"))
    }

    fn root_directory(root: &Path) -> Result<File, SnapshotObjectError> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)?;
        if !file.metadata()?.is_dir() {
            return Err(SnapshotObjectError::Unsupported(
                "worktree root is not a directory",
            ));
        }
        Ok(file)
    }

    fn open_parent(parent: &File, name: &str) -> Result<File, SnapshotObjectError> {
        let name = c_name(name)?;
        // SAFETY: parent is a live directory descriptor and name is a single
        // NUL-free normalized component. O_NOFOLLOW prevents parent symlinks.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: openat returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        if !file.metadata()?.is_dir() {
            return Err(SnapshotObjectError::Unsupported(
                "parent component is not a directory",
            ));
        }
        Ok(file)
    }

    fn parent_and_leaf(
        root: &Path,
        path: &NormalizedWorkspacePath,
    ) -> Result<(File, String), SnapshotObjectError> {
        let mut components = path.as_str().split('/').peekable();
        let mut directory = root_directory(root)?;
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                return Ok((directory, component.to_owned()));
            }
            directory = open_parent(&directory, component)?;
        }
        Err(SnapshotObjectError::Unsupported("empty normalized path"))
    }

    fn stat_at(parent: &File, leaf: &CString) -> Result<Option<libc::stat>, SnapshotObjectError> {
        // SAFETY: stat is fully initialized by fstatat on success; the name is
        // a single NUL-free component and AT_SYMLINK_NOFOLLOW observes the leaf.
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        let result = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == 0 {
            return Ok(Some(stat));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error.into())
        }
    }

    fn same_stat_metadata(stat: &libc::stat, metadata: &std::fs::Metadata) -> bool {
        let kind = stat.st_mode & libc::S_IFMT;
        let metadata_kind = metadata.file_type();
        let same_kind = match kind {
            libc::S_IFREG => metadata_kind.is_file(),
            libc::S_IFLNK => metadata_kind.is_symlink(),
            libc::S_IFDIR => metadata_kind.is_dir(),
            _ => false,
        };
        stat_device_number(stat) == Some(metadata.dev())
            && stat.st_ino == metadata.ino()
            && u64::try_from(stat.st_size).ok() == Some(metadata.size())
            && same_kind
            && stat.st_mtime == metadata.mtime()
            && stat.st_ctime == metadata.ctime()
            && stat_mtime_nsec(stat) == metadata.mtime_nsec()
            && stat_ctime_nsec(stat) == metadata.ctime_nsec()
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn stat_device_number(stat: &libc::stat) -> Option<u64> {
        Some(stat.st_dev)
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn stat_device_number(stat: &libc::stat) -> Option<u64> {
        u64::try_from(stat.st_dev).ok()
    }

    fn stat_mtime_nsec(stat: &libc::stat) -> i64 {
        stat.st_mtime_nsec
    }
    fn stat_ctime_nsec(stat: &libc::stat) -> i64 {
        stat.st_ctime_nsec
    }

    fn stable_metadata(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.size() == after.size()
            && before.mode() == after.mode()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }

    fn hash_file(
        mut file: File,
        initial: &libc::stat,
        index_entry: Option<&SnapshotIndexEntry>,
        policy: GitFileModePolicy,
        budget: &mut SnapshotHashBudget,
        before_read: &mut dyn FnMut(),
    ) -> Result<CandidateObjectIdentity, SnapshotObjectError> {
        let before = file.metadata()?;
        if !before.is_file() || !same_stat_metadata(initial, &before) {
            return Err(SnapshotObjectError::Unstable);
        }
        if before.size() > budget.remaining() {
            return Err(SnapshotObjectError::LimitExceeded(
                "aggregate content bytes",
            ));
        }
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; HASH_BUFFER_BYTES];
        before_read();
        loop {
            let read_capacity = usize::try_from(budget.remaining())
                .unwrap_or(usize::MAX)
                .min(buffer.len());
            if read_capacity == 0 {
                break;
            }
            let read = file.read(&mut buffer[..read_capacity])?;
            if read == 0 {
                break;
            }
            budget.charge(read)?;
            size = size
                .checked_add(read as u64)
                .ok_or(SnapshotObjectError::LimitExceeded("file size"))?;
            digest.update(&buffer[..read]);
        }
        let after = file.metadata()?;
        if !stable_metadata(&before, &after) || size != after.size() {
            return Err(SnapshotObjectError::Unstable);
        }
        let executable = executable_for(policy, index_entry, before.mode())?;
        Ok(CandidateObjectIdentity::RegularFile {
            content_sha256: digest_hex(digest),
            size_bytes: size,
            executable,
        })
    }

    fn hash_symlink(
        parent: &File,
        leaf: &CString,
        initial: &libc::stat,
        budget: &mut SnapshotHashBudget,
        before_read: &mut dyn FnMut(),
    ) -> Result<CandidateObjectIdentity, SnapshotObjectError> {
        let mut bytes = vec![0_u8; MAX_SYMLINK_TARGET_BYTES + 1];
        before_read();
        // SAFETY: buffer is writable and parent/name are valid; readlinkat
        // writes at most the supplied capacity and does not append a NUL.
        let length = unsafe {
            libc::readlinkat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
            )
        };
        if length < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let length = length as usize;
        if length > MAX_SYMLINK_TARGET_BYTES {
            return Err(SnapshotObjectError::LimitExceeded("symlink target bytes"));
        }
        let after = stat_at(parent, leaf)?.ok_or(SnapshotObjectError::Unstable)?;
        if !same_stat_stat(initial, &after) {
            return Err(SnapshotObjectError::Unstable);
        }
        budget.charge(length)?;
        bytes.truncate(length);
        let mut digest = Sha256::new();
        digest.update(&bytes);
        Ok(CandidateObjectIdentity::Symlink {
            target_sha256: digest_hex(digest),
            target_size_bytes: length as u64,
        })
    }

    fn same_stat_stat(left: &libc::stat, right: &libc::stat) -> bool {
        left.st_dev == right.st_dev
            && left.st_ino == right.st_ino
            && left.st_mode == right.st_mode
            && left.st_size == right.st_size
            && left.st_mtime == right.st_mtime
            && left.st_ctime == right.st_ctime
            && stat_mtime_nsec(left) == stat_mtime_nsec(right)
            && stat_ctime_nsec(left) == stat_ctime_nsec(right)
    }

    pub(super) fn observe(
        root: &Path,
        path: &NormalizedWorkspacePath,
        index_entry: Option<&SnapshotIndexEntry>,
        policy: GitFileModePolicy,
        budget: &mut SnapshotHashBudget,
        before_read: &mut dyn FnMut(),
    ) -> Result<CandidateObjectIdentity, SnapshotObjectError> {
        let (parent, leaf_name) = parent_and_leaf(root, path)?;
        let leaf = c_name(&leaf_name)?;
        let initial = match stat_at(&parent, &leaf)? {
            Some(stat) => stat,
            None => {
                if stat_at(&parent, &leaf)?.is_none() {
                    return Ok(CandidateObjectIdentity::Absent);
                }
                return Err(SnapshotObjectError::Unstable);
            }
        };
        match initial.st_mode & libc::S_IFMT {
            libc::S_IFREG => {
                // O_NOFOLLOW refuses a raced-in symlink; O_NONBLOCK prevents a
                // raced-in FIFO from hanging before fstat can reject it.
                let fd = unsafe {
                    libc::openat(
                        parent.as_raw_fd(),
                        leaf.as_ptr(),
                        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                    )
                };
                if fd < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                let file = unsafe { File::from_raw_fd(fd) };
                hash_file(file, &initial, index_entry, policy, budget, before_read)
            }
            libc::S_IFLNK => hash_symlink(&parent, &leaf, &initial, budget, before_read),
            libc::S_IFDIR => Err(SnapshotObjectError::Unsupported("directory candidate")),
            _ => Err(SnapshotObjectError::Unsupported(
                "special filesystem object",
            )),
        }
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ffi::c_void;
    use std::fs::OpenOptions;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};

    fn file_witness(file: &File) -> Result<(u32, u64, i64, i64), SnapshotObjectError> {
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandle,
            GetFileInformationByHandleEx,
        };
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `file` owns a live handle and `information` is a writable
        // output structure for the synchronous Windows API call.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut information) } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let file_index =
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
        let mut basic = FILE_BASIC_INFO::default();
        // SAFETY: `basic` is a writable output structure and `file` owns a
        // live handle for the duration of the call.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle().cast(),
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok((
            information.dwVolumeSerialNumber,
            file_index,
            basic.ChangeTime,
            basic.LastWriteTime,
        ))
    }

    fn nt_open_relative(
        parent: &File,
        component: &str,
        directory: bool,
    ) -> Result<Option<File>, SnapshotObjectError> {
        use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
        use windows_sys::Wdk::Storage::FileSystem::{
            FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
            FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
        };
        use windows_sys::Win32::Foundation::{
            HANDLE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_NORMAL, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, FILE_TRAVERSE, SYNCHRONIZE,
        };
        use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

        let mut wide: Vec<u16> = component.encode_utf16().collect();
        let byte_length = u16::try_from(wide.len().checked_mul(2).ok_or(
            SnapshotObjectError::Unsupported("Windows path component is too long"),
        )?)
        .map_err(|_| SnapshotObjectError::Unsupported("Windows path component is too long"))?;
        let mut name = UNICODE_STRING {
            Length: byte_length,
            MaximumLength: byte_length,
            Buffer: wide.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle().cast(),
            ObjectName: &mut name,
            Attributes: OBJ_CASE_INSENSITIVE,
            ..Default::default()
        };
        let mut handle: HANDLE = std::ptr::null_mut();
        let mut io_status = IO_STATUS_BLOCK::default();
        let options = FILE_OPEN_REPARSE_POINT
            | FILE_SYNCHRONOUS_IO_NONALERT
            | if directory {
                FILE_DIRECTORY_FILE
            } else {
                FILE_NON_DIRECTORY_FILE
            };
        // SAFETY: all pointers remain live for the synchronous NtCreateFile
        // call; RootDirectory is an open directory handle; ObjectName is one
        // normalized path component; FILE_OPEN_REPARSE_POINT prevents following
        // a raced-in junction/symlink at this component.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                FILE_READ_ATTRIBUTES
                    | SYNCHRONIZE
                    | if directory {
                        FILE_TRAVERSE
                    } else {
                        1 | 0x8000_0000
                    },
                &attributes,
                &mut io_status,
                std::ptr::null(),
                if directory {
                    windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY
                } else {
                    FILE_ATTRIBUTE_NORMAL
                },
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
                options,
                std::ptr::null(),
                0,
            )
        };
        if status < 0 {
            // STATUS_OBJECT_NAME_NOT_FOUND and STATUS_OBJECT_PATH_NOT_FOUND.
            if status as u32 == 0xC000_0034 || status as u32 == 0xC000_003A {
                return Ok(None);
            }
            let win32 = unsafe { RtlNtStatusToDosError(status) };
            return Err(std::io::Error::from_raw_os_error(win32 as i32).into());
        }
        if handle.is_null() || handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(SnapshotObjectError::Io(std::io::Error::other(
                "NtCreateFile succeeded without a handle",
            )));
        }
        // SAFETY: successful NtCreateFile returned a new owned handle.
        let file = unsafe { File::from_raw_handle(handle.cast::<c_void>()) };
        Ok(Some(file))
    }

    pub(super) fn observe(
        root: &Path,
        path: &NormalizedWorkspacePath,
        index_entry: Option<&SnapshotIndexEntry>,
        policy: GitFileModePolicy,
        budget: &mut SnapshotHashBudget,
    ) -> Result<CandidateObjectIdentity, SnapshotObjectError> {
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        let root_file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(root)?;
        let root_metadata = root_file.metadata()?;
        if root_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || !root_metadata.is_dir()
        {
            return Err(SnapshotObjectError::Unsupported(
                "worktree root is a reparse point or non-directory",
            ));
        }
        let components: Vec<&str> = path.as_str().split('/').collect();
        let mut parent = root_file;
        for component in &components[..components.len() - 1] {
            let Some(next_parent) = nt_open_relative(&parent, component, true)? else {
                return Ok(CandidateObjectIdentity::Absent);
            };
            parent = next_parent;
            let metadata = parent.metadata()?;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_dir()
            {
                return Err(SnapshotObjectError::Unsupported(
                    "parent reparse point or non-directory",
                ));
            }
        }
        let Some(mut file) = nt_open_relative(&parent, components[components.len() - 1], false)?
        else {
            return Ok(CandidateObjectIdentity::Absent);
        };
        let before = file.metadata()?;
        if before.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(SnapshotObjectError::Unsupported("Windows reparse point"));
        }
        if !before.is_file() {
            return Err(SnapshotObjectError::Unsupported(
                "special filesystem object",
            ));
        }
        if policy == GitFileModePolicy::TrustExecutableBit {
            return Err(SnapshotObjectError::Unsupported(
                "Windows executable metadata is not a reliable Git-equivalent",
            ));
        }
        let opened = before;
        let opened_witness = file_witness(&file)?;
        if opened.file_size() > budget.remaining() {
            return Err(SnapshotObjectError::LimitExceeded(
                "aggregate content bytes",
            ));
        }
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; HASH_BUFFER_BYTES];
        loop {
            let read_capacity = usize::try_from(budget.remaining())
                .unwrap_or(usize::MAX)
                .min(buffer.len());
            if read_capacity == 0 {
                break;
            }
            let read = file.read(&mut buffer[..read_capacity])?;
            if read == 0 {
                break;
            }
            budget.charge(read)?;
            size = size
                .checked_add(read as u64)
                .ok_or(SnapshotObjectError::LimitExceeded("file size"))?;
            digest.update(&buffer[..read]);
        }
        let after = file.metadata()?;
        let after_witness = file_witness(&file)?;
        if after.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || after_witness != opened_witness
            || after.file_size() != opened.file_size()
            || after.last_write_time() != opened.last_write_time()
            || size != after.file_size()
        {
            return Err(SnapshotObjectError::Unstable);
        }
        let executable = matches!(
            index_entry.map(|entry| entry.mode),
            Some(SnapshotIndexMode::Regular { executable: true })
        );
        Ok(CandidateObjectIdentity::RegularFile {
            content_sha256: digest_hex(digest),
            size_bytes: size,
            executable,
        })
    }
}
