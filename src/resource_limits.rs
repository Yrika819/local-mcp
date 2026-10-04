//! Central resource policy for Resource Bounds V1.
//!
//! Every bound in GoalLatch is named here, measured in bytes unless stated, and
//! enforced at the place named by [`docs/RESOURCE_BOUNDS_V1_DESIGN.md`].
//!
//! Two properties are load-bearing:
//!
//! * **One policy surface.** Production code never restates a bound as a literal.
//!   Unrelated resources deliberately keep independent constants rather than
//!   sharing one arbitrary ceiling.
//! * **Mechanical consistency.** Relationships between limits — a base64-encoded
//!   image has to fit inside a response frame — are asserted at compile time, so
//!   an inconsistent set fails the build rather than a test.
//!
//! These limits are a *resource* policy. Reaching one is terminal for that
//! attempt and grants no authority: it never implies permission, never proves a
//! command did not run, and never restores retry budget.

use std::fmt;

// ---------------------------------------------------------------------------
// MCP transport
// ---------------------------------------------------------------------------

/// Largest inbound request frame, measured as the UTF-8 bytes of one
/// newline-delimited JSON-RPC frame, excluding the newline.
///
/// Enforced by the bounded incremental frame reader *before* JSON parsing and
/// before dispatch admission, so an oversized frame never allocates fully and
/// never consumes a worker permit.
///
/// Derived from the largest realistic request (a `write_file` plus argv plus a
/// plan proposal plus evidence is roughly 1.1 MiB) with about 7x margin.
pub(crate) const MAX_MCP_REQUEST_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Largest outbound response frame, measured as serialized UTF-8 bytes excluding
/// the trailing newline.
///
/// Enforced as a defence-in-depth assertion in the writer, *after* method-level
/// bounds have already kept every realistic result well inside it.
pub(crate) const MAX_MCP_RESPONSE_FRAME_BYTES: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Generic command output capture
// ---------------------------------------------------------------------------

/// Largest captured stdout for a generic `execute` / `start_command` /
/// `without_sandbox` invocation.
///
/// Bounded independently of stderr, and enforced while reading rather than after
/// the full stream has been accumulated.
pub(crate) const MAX_COMMAND_STDOUT_BYTES: usize = 4 * 1024 * 1024;

/// Largest captured stderr for a generic invocation.
///
/// Matches the existing trusted-Git diagnostic bound. Trusted-Git and
/// model/agent paths keep their own stricter caps and are not bound by these.
pub(crate) const MAX_COMMAND_STDERR_BYTES: usize = 1024 * 1024;

/// Bytes retained for activity-timeline diagnostics.
///
/// The start UI echoes command output, so the summary itself needs a bound
/// independent of the capture limit.
pub(crate) const MAX_COMMAND_SUMMARY_BYTES: usize = 8 * 1024;

/// How long the host waits for both pipes to reach EOF after the group leader
/// has terminated, before concluding that a descendant is still holding them.
///
/// A descendant that outlives its parent while inheriting stdout/stderr is the
/// one way capture can outlive the command. It is bounded here so a stray
/// descendant cannot hold the capture open indefinitely; once this grace elapses
/// the process group is terminated, which is the same authority `stop_job`
/// already uses.
pub(crate) const COMMAND_CAPTURE_DRAIN_GRACE: std::time::Duration =
    std::time::Duration::from_secs(5);

/// How long cleanup waits, after the group has been terminated, for the leader
/// to terminate and for capture tasks to unwind.
pub(crate) const COMMAND_CLEANUP_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

// ---------------------------------------------------------------------------
// File and directory tools
// ---------------------------------------------------------------------------

/// Largest file `read_file` will return in full.
pub(crate) const MAX_READ_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Largest **raw** image file, before base64 expansion.
pub(crate) const MAX_IMAGE_RAW_BYTES: usize = 8 * 1024 * 1024;

/// Largest `write_file` content, enforced before any filesystem mutation.
pub(crate) const MAX_WRITE_FILE_CONTENT_BYTES: usize = 256 * 1024;

/// Largest existing file `write_file` will read to build a preimage and diff.
pub(crate) const MAX_WRITE_PREIMAGE_BYTES: usize = 8 * 1024 * 1024;

/// Largest number of entries `list_directory` will accumulate.
pub(crate) const MAX_DIRECTORY_ENTRIES: usize = 20_000;

/// Largest rendered `list_directory` listing.
///
/// Deliberately independent of [`MAX_DIRECTORY_ENTRIES`]: many short names stay
/// under the byte cap, while fewer long names can breach it.
pub(crate) const MAX_DIRECTORY_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Command arguments
// ---------------------------------------------------------------------------

/// Largest number of argv items in one `execute` / `start_command` request.
pub(crate) const MAX_EXECUTE_ARGV_ITEMS: usize = 256;

/// Largest single argv item.
pub(crate) const MAX_EXECUTE_ARG_BYTES: usize = 64 * 1024;

/// Largest total argv payload.
pub(crate) const MAX_EXECUTE_ARGV_TOTAL_BYTES: usize = 512 * 1024;

/// Largest `cwd` path field.
pub(crate) const MAX_EXECUTE_PATH_BYTES: usize = 4 * 1024;

// ---------------------------------------------------------------------------
// Background job registry
// ---------------------------------------------------------------------------

/// Maximum retained background jobs for a single Session.
pub(crate) const MAX_BACKGROUND_JOBS_PER_SESSION: usize = 8;

/// Maximum retained background jobs across all Sessions.
pub(crate) const MAX_BACKGROUND_JOBS_GLOBAL: usize = 32;

/// Retention period for a finished, unpolled background result.
///
/// Monotonic. Running jobs are never expired by this.
pub(crate) const FINISHED_JOB_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

// ---------------------------------------------------------------------------
// Control-plane admission
// ---------------------------------------------------------------------------

/// Reserved permits for lightweight control-plane requests, so polling and
/// stopping stay usable while execution work saturates.
pub(crate) const CONTROL_PLANE_PERMITS: usize = 16;

/// Permits for execution and other heavyweight work.
pub(crate) const EXECUTION_PERMITS: usize = 16;

/// Total transport concurrency. Unchanged from before Resource Bounds V1.
pub(crate) const MAX_CONCURRENT_REQUESTS: usize = 32;

// ---------------------------------------------------------------------------
// Compile-time relationships
// ---------------------------------------------------------------------------

// Splitting admission across two pools must not raise or lower total concurrency.
const _: () = assert!(CONTROL_PLANE_PERMITS + EXECUTION_PERMITS == MAX_CONCURRENT_REQUESTS);

// Both pools must be non-empty, or the control plane could not run at all.
const _: () = assert!(CONTROL_PLANE_PERMITS > 0 && EXECUTION_PERMITS > 0);

// base64 expands by 4/3 and adds up to four padding characters. A raw image must
// still fit inside one response frame once encoded and wrapped in JSON.
const _: () = assert!(MAX_IMAGE_RAW_BYTES / 3 * 4 + 4 <= MAX_MCP_RESPONSE_FRAME_BYTES);

// A method result must be able to fit in the frame that carries it.
const _: () = assert!(MAX_READ_FILE_BYTES <= MAX_MCP_RESPONSE_FRAME_BYTES);
const _: () = assert!(MAX_DIRECTORY_OUTPUT_BYTES <= MAX_MCP_RESPONSE_FRAME_BYTES);

// A command result embeds both captured streams in one frame.
const _: () =
    assert!(MAX_COMMAND_STDOUT_BYTES + MAX_COMMAND_STDERR_BYTES <= MAX_MCP_RESPONSE_FRAME_BYTES);

// The request frame is the outer defense for every input-side bound, so no
// method-level input bound may exceed it.
const _: () = assert!(MAX_WRITE_FILE_CONTENT_BYTES <= MAX_MCP_REQUEST_FRAME_BYTES);
const _: () = assert!(MAX_EXECUTE_ARGV_TOTAL_BYTES <= MAX_MCP_REQUEST_FRAME_BYTES);

// A job must be able to hold its own argv within one request frame.
const _: () =
    assert!(MAX_EXECUTE_ARGV_TOTAL_BYTES <= MAX_EXECUTE_ARGV_ITEMS * MAX_EXECUTE_ARG_BYTES);

// One Session may not be able to consume the whole registry.
const _: () = assert!(MAX_BACKGROUND_JOBS_PER_SESSION <= MAX_BACKGROUND_JOBS_GLOBAL);

// ---------------------------------------------------------------------------
// Resource-limit failures
// ---------------------------------------------------------------------------

/// Which bound was reached.
///
/// Carries no captured content, no path list, and no request echo, so a resource
/// error is always itself small and bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    dead_code,
    reason = "Transport, file, directory and job variants are consumed by the remaining Resource Bounds V1 slices."
)]
pub(crate) enum ResourceLimit {
    McpRequestFrame,
    McpResponseFrame,
    CommandStdout,
    CommandStderr,
    ReadFile,
    ImageRaw,
    WriteFileContent,
    WriteFilePreimage,
    DirectoryEntries,
    DirectoryOutputBytes,
    ExecuteArgvItems,
    ExecuteArgBytes,
    ExecuteArgvTotalBytes,
    ExecutePathBytes,
    BackgroundJobsPerSession,
    BackgroundJobsGlobal,
}

impl ResourceLimit {
    /// The frozen ceiling for this resource, in bytes or items as applicable.
    fn ceiling(self) -> usize {
        match self {
            Self::McpRequestFrame => MAX_MCP_REQUEST_FRAME_BYTES,
            Self::McpResponseFrame => MAX_MCP_RESPONSE_FRAME_BYTES,
            Self::CommandStdout => MAX_COMMAND_STDOUT_BYTES,
            Self::CommandStderr => MAX_COMMAND_STDERR_BYTES,
            Self::ReadFile => MAX_READ_FILE_BYTES,
            Self::ImageRaw => MAX_IMAGE_RAW_BYTES,
            Self::WriteFileContent => MAX_WRITE_FILE_CONTENT_BYTES,
            Self::WriteFilePreimage => MAX_WRITE_PREIMAGE_BYTES,
            Self::DirectoryEntries => MAX_DIRECTORY_ENTRIES,
            Self::DirectoryOutputBytes => MAX_DIRECTORY_OUTPUT_BYTES,
            Self::ExecuteArgvItems => MAX_EXECUTE_ARGV_ITEMS,
            Self::ExecuteArgBytes => MAX_EXECUTE_ARG_BYTES,
            Self::ExecuteArgvTotalBytes => MAX_EXECUTE_ARGV_TOTAL_BYTES,
            Self::ExecutePathBytes => MAX_EXECUTE_PATH_BYTES,
            Self::BackgroundJobsPerSession => MAX_BACKGROUND_JOBS_PER_SESSION,
            Self::BackgroundJobsGlobal => MAX_BACKGROUND_JOBS_GLOBAL,
        }
    }

    /// A short, stable name for the resource that was bounded.
    pub(crate) fn resource(self) -> &'static str {
        match self {
            Self::McpRequestFrame => "mcp request frame",
            Self::McpResponseFrame => "mcp response frame",
            Self::CommandStdout => "command stdout",
            Self::CommandStderr => "command stderr",
            Self::ReadFile => "file read",
            Self::ImageRaw => "image bytes",
            Self::WriteFileContent => "write_file content",
            Self::WriteFilePreimage => "write_file preimage",
            Self::DirectoryEntries => "directory entries",
            Self::DirectoryOutputBytes => "directory listing bytes",
            Self::ExecuteArgvItems => "command argument count",
            Self::ExecuteArgBytes => "command argument bytes",
            Self::ExecuteArgvTotalBytes => "command argument total bytes",
            Self::ExecutePathBytes => "command path bytes",
            Self::BackgroundJobsPerSession => "background jobs for this session",
            Self::BackgroundJobsGlobal => "background jobs overall",
        }
    }
}

impl fmt::Display for ResourceLimit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} limit exceeded (maximum {})",
            self.resource(),
            self.ceiling()
        )
    }
}

impl std::error::Error for ResourceLimit {}

/// Build a resource-limit error carrying a fixed sentence.
///
/// The sentence is host-authored and never embeds caller-supplied content, so a
/// rejected request cannot inflate the diagnostic that reports its rejection.
#[expect(
    dead_code,
    reason = "Used by the transport and file slices of Resource Bounds V1."
)]
pub(crate) fn limit_error(limit: ResourceLimit, detail: &'static str) -> anyhow::Error {
    anyhow::anyhow!("{limit}: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceilings_match_the_documented_values() {
        assert_eq!(MAX_MCP_REQUEST_FRAME_BYTES, 8 * 1024 * 1024);
        assert_eq!(MAX_MCP_RESPONSE_FRAME_BYTES, 16 * 1024 * 1024);
        assert_eq!(MAX_COMMAND_STDOUT_BYTES, 4 * 1024 * 1024);
        assert_eq!(MAX_COMMAND_STDERR_BYTES, 1024 * 1024);
        assert_eq!(MAX_READ_FILE_BYTES, 8 * 1024 * 1024);
        assert_eq!(MAX_IMAGE_RAW_BYTES, 8 * 1024 * 1024);
        assert_eq!(MAX_WRITE_FILE_CONTENT_BYTES, 256 * 1024);
        assert_eq!(MAX_DIRECTORY_ENTRIES, 20_000);
        assert_eq!(MAX_DIRECTORY_OUTPUT_BYTES, 4 * 1024 * 1024);
        assert_eq!(MAX_EXECUTE_ARGV_ITEMS, 256);
        assert_eq!(MAX_EXECUTE_ARG_BYTES, 64 * 1024);
        assert_eq!(MAX_EXECUTE_ARGV_TOTAL_BYTES, 512 * 1024);
        assert_eq!(MAX_EXECUTE_PATH_BYTES, 4 * 1024);
        assert_eq!(MAX_BACKGROUND_JOBS_PER_SESSION, 8);
        assert_eq!(MAX_BACKGROUND_JOBS_GLOBAL, 32);
        assert_eq!(
            CONTROL_PLANE_PERMITS + EXECUTION_PERMITS,
            MAX_CONCURRENT_REQUESTS
        );
    }

    #[test]
    fn base64_encoded_image_fits_inside_one_response_frame() {
        let encoded = MAX_IMAGE_RAW_BYTES / 3 * 4 + 4;
        assert!(
            encoded <= MAX_MCP_RESPONSE_FRAME_BYTES,
            "base64 image {encoded} must fit in {}",
            MAX_MCP_RESPONSE_FRAME_BYTES
        );
    }

    #[test]
    fn limit_error_names_the_resource_and_the_ceiling() {
        let rendered = limit_error(ResourceLimit::CommandStdout, "output truncated").to_string();
        assert!(rendered.contains("command stdout"));
        assert!(rendered.contains(&MAX_COMMAND_STDOUT_BYTES.to_string()));
        assert!(rendered.contains("output truncated"));
    }

    #[test]
    fn every_resource_limit_reports_a_non_zero_ceiling() {
        for limit in [
            ResourceLimit::McpRequestFrame,
            ResourceLimit::McpResponseFrame,
            ResourceLimit::CommandStdout,
            ResourceLimit::CommandStderr,
            ResourceLimit::ReadFile,
            ResourceLimit::ImageRaw,
            ResourceLimit::WriteFileContent,
            ResourceLimit::WriteFilePreimage,
            ResourceLimit::DirectoryEntries,
            ResourceLimit::DirectoryOutputBytes,
            ResourceLimit::ExecuteArgvItems,
            ResourceLimit::ExecuteArgBytes,
            ResourceLimit::ExecuteArgvTotalBytes,
            ResourceLimit::ExecutePathBytes,
            ResourceLimit::BackgroundJobsPerSession,
            ResourceLimit::BackgroundJobsGlobal,
        ] {
            assert!(
                limit.ceiling() > 0,
                "{} must have a ceiling",
                limit.resource()
            );
            assert!(!limit.resource().is_empty());
        }
    }
}
