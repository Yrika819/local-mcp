//! Windows Job-Object containment for every child process Local MCP launches.
//!
//! # Why a Job Object
//!
//! `ProcessGroup` is Unix-first. On Unix every child is placed in its own process
//! group and terminated as a group, so descendants cannot survive a timeout, a
//! cancellation or a failed run. On Windows the equivalent grouping primitive is a
//! Job Object, and before this module the Windows build had none: a terminated
//! execution killed only the direct child, and any descendant it had spawned
//! outlived the timeout, the output bound, `stop_job` and server shutdown alike.
//!
//! A Job closes that gap with a kernel-enforced guarantee rather than a hint:
//!
//! * Every process assigned to a Job puts **its own children** in the same Job,
//!   unless it asks for `CREATE_BREAKAWAY_FROM_JOB`. The Job does not permit
//!   breakaway, so containment cannot be escaped by a descendant.
//! * The Job carries `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so closing the last
//!   handle to it terminates every process in it. Windows closes every handle a
//!   process owns when that process terminates, **including on abnormal
//!   termination**, so a Local MCP process that is killed or crashes has its Jobs
//!   closed by the kernel and its whole execution trees die with it.
//!
//! The second property is what makes containment survive the owner's death, which
//! is why this is a lifetime mechanism and not merely a cleanup convenience.
//!
//! # Why the spawn/assignment race is closed
//!
//! Assigning a Job after the child is running would leave a window in which the
//! child could spawn a descendant that never joins the Job. Instead the child is
//! created with `CREATE_SUSPENDED`, which means it executes **no** user code — not
//! one instruction — before it is assigned. Only then is its single primary thread
//! resumed. The child is therefore never allowed to run before containment is
//! established, which is a structural property rather than a timing assumption.
//!
//! # Degradation
//!
//! Job assignment can legitimately fail on a host that forbids it. When it does,
//! the child is still suspended and has run nothing, so it is resumed and the
//! lease proceeds with direct-child termination only — strictly weaker, never
//! stronger, and never a silent claim of tree containment. [`Job::is_assigned`]
//! reports the difference so tests can assert containment is genuinely in effect
//! rather than merely compiled.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
};

/// Exit code reported to every process the Job terminates.
///
/// The value is arbitrary and never surfaces to a caller as a meaningful status:
/// these processes were killed, which is exactly what the lifecycle evidence in
/// `sandbox` already records.
const JOB_TERMINATION_EXIT_CODE: u32 = 1;

/// `ResumeThread` returns this when the resume failed.
const RESUME_FAILED: u32 = u32::MAX;

/// A Windows Job Object owning one execution tree.
///
/// Dropping this closes the Job handle. Because the Job carries
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, that close terminates the tree, so
/// `Drop` is the backstop that makes "the host disappeared" equivalent to "the
/// tree was stopped".
pub(crate) struct Job {
    handle: OwnedHandle,
    /// Whether the child was actually assigned to this Job.
    ///
    /// `false` means the host refused the assignment and the lease has fallen
    /// back to direct-child termination. It never means the tree is contained.
    assigned: bool,
}

impl Job {
    /// Create an empty Job configured to kill its contents when the handle closes.
    pub(crate) fn create() -> io::Result<Self> {
        // Safety: a null name and null security attributes ask for an unnamed Job
        // with default security, which is correct — the handle is never shared and
        // never named, so no other process can open it.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // Safety: `CreateJobObjectW` returns a fresh handle that this function
        // takes ownership of; `OwnedHandle` closes it exactly once on drop.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) };

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // Safety: the structure is initialized, its `LimitFlags` is set, and the
        // length matches the structure the API is being asked to read.
        let configured = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle() as HANDLE,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .unwrap_or(u32::MAX),
            )
        };
        if configured == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            handle,
            assigned: false,
        })
    }

    /// Put a suspended process into this Job.
    ///
    /// The process must still be suspended. That is what makes this safe to do
    /// without a race: nothing in the child has run yet, so it cannot have
    /// produced an uncontained descendant.
    ///
    /// `handle` is the process handle and `pid` its identifier. Taking them
    /// separately lets both the asynchronous and the blocking spawn paths share
    /// this one containment primitive rather than reimplementing it.
    pub(crate) fn assign(&mut self, handle: HANDLE, pid: u32) -> io::Result<()> {
        // Safety: `handle` is a live process handle supplied by the caller for
        // exactly this purpose, and assignment needs only `PROCESS_SET_QUOTA` and
        // `PROCESS_TERMINATE`, which a creating handle carries.
        let assigned =
            unsafe { AssignProcessToJobObject(self.handle.as_raw_handle() as HANDLE, handle) };
        if assigned == 0 {
            return Err(io::Error::last_os_error());
        }
        self.assigned = true;
        debug_assert!(pid > 0, "a spawned process always has an identifier");
        Ok(())
    }

    /// Resume a suspended process's single primary thread.
    ///
    /// A process created with `CREATE_SUSPENDED` has exactly one thread and it
    /// cannot create another until it runs, so the first thread found for the
    /// process is unambiguously the primary thread. This is the documented way to
    /// resume a child whose thread handle the spawner did not retain.
    pub(crate) fn resume(pid: u32) -> io::Result<()> {
        // Safety: a thread snapshot handle is returned by the OS and closed by this
        // function on every path.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // Safety: the snapshot handle is owned by this function and closed exactly
        // once before returning, on both the success and failure paths.
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot as RawHandle) };

        let mut entry = THREADENTRY32 {
            dwSize: u32::try_from(std::mem::size_of::<THREADENTRY32>()).unwrap_or(u32::MAX),
            ..THREADENTRY32::default()
        };
        // Safety: `Thread32First` only writes into the `THREADENTRY32` it is given,
        // whose `dwSize` this function has just set to its own size.
        let mut more = unsafe { Thread32First(snapshot.as_raw_handle() as HANDLE, &mut entry) };
        while more != 0 {
            if entry.dwth32OwnerProcessID == pid {
                // Safety: the thread identifier comes from the kernel's own
                // snapshot and is opened for resume only.
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                // Safety: a fresh thread handle owned by this function.
                let thread = unsafe { OwnedHandle::from_raw_handle(thread as RawHandle) };
                // Safety: the handle is open for `THREAD_SUSPEND_RESUME` and the
                // process was created suspended, so the count is at least one.
                let previous = unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) };
                // A failed resume must not be reported as success. The process
                // would stay suspended forever while the Job — which *was*
                // assigned — kept answering "contained", so containment would be
                // claimed for a process that never executes a single instruction.
                if previous == RESUME_FAILED {
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
            // Safety: the entry carries the size the kernel reported on the
            // previous successful call.
            more = unsafe { Thread32Next(snapshot.as_raw_handle() as HANDLE, &mut entry) };
        }
        Err(io::Error::other(
            "suspended process has no resumable primary thread",
        ))
    }

    /// Terminate every process in the Job.
    ///
    /// Used on the orderly paths — timeout, output overflow, `stop_job`,
    /// cancellation and shutdown — so tree teardown does not have to wait for the
    /// lease to be dropped.
    pub(crate) fn terminate(&self) {
        if !self.assigned {
            return;
        }
        // Safety: terminating a Job this process created is always permitted, and
        // an unused result is the normal outcome of a best-effort teardown.
        let _ = unsafe {
            TerminateJobObject(
                self.handle.as_raw_handle() as HANDLE,
                JOB_TERMINATION_EXIT_CODE,
            )
        };
    }

    /// Whether a child was genuinely assigned to this Job.
    ///
    /// Tests assert this rather than assuming containment, so a host that refuses
    /// job assignment shows up as a failed guarantee instead of a silent one.
    pub(crate) fn is_assigned(&self) -> bool {
        self.assigned
    }

    /// How many processes the kernel currently counts in this Job.
    ///
    /// This is the witness the Job Object tests use. The count is reported by the
    /// kernel itself and covers the *whole* Job, so it observes the entire tree
    /// rather than a direct child, and it cannot be satisfied by an idle or
    /// merely unreaped process. Reaching zero means no process in the tree is
    /// still running.
    #[cfg(test)]
    pub(crate) fn active_processes(&self) -> io::Result<u32> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // Safety: the structure is initialized and the length matches the
        // structure the API is being asked to fill.
        let queried = unsafe {
            QueryInformationJobObject(
                self.handle.as_raw_handle() as HANDLE,
                JobObjectBasicAccountingInformation,
                std::ptr::from_mut(&mut accounting).cast(),
                u32::try_from(std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>())
                    .unwrap_or(u32::MAX),
                std::ptr::null_mut(),
            )
        };
        if queried == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(accounting.ActiveProcesses)
    }
}
