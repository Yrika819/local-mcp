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

/// Worst-case JSON expansion applied to caller-visible text, in bytes out per byte
/// in.
///
/// `serde_json` renders a control byte as `\u0000` (6 bytes). A command result is
/// serialized *twice* — once by `render_output`, then again when it is embedded as
/// a string value by `text_result` — so the second pass re-escapes each backslash
/// and a single raw byte costs up to 7. A single-pass value such as `read_file`
/// costs up to 6.
///
/// This factor exists because a bound derived from raw byte counts alone is wrong:
/// 4 MiB of NUL bytes on stdout is a perfectly ordinary command, and it
/// serializes to a frame well over twice its own size. Every text-returning bound
/// below is asserted against this factor at compile time.
pub(crate) const JSON_ESCAPE_WORST_CASE: usize = 7;

// ---------------------------------------------------------------------------
// Generic command output capture
// ---------------------------------------------------------------------------

/// Largest captured stdout for a generic `execute` / `start_command` /
/// `without_sandbox` invocation.
///
/// Bounded independently of stderr, and enforced while reading rather than after
/// the full stream has been accumulated.
pub(crate) const MAX_COMMAND_STDOUT_BYTES: usize = 1536 * 1024;

/// Largest captured stderr for a generic invocation.
///
/// Trusted-Git and model/agent paths keep their own stricter caps and are not
/// bound by these.
pub(crate) const MAX_COMMAND_STDERR_BYTES: usize = 256 * 1024;

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

/// How long the host waits to finish writing a command's stdin.
///
/// This is deliberately *not* [`COMMAND_CLEANUP_GRACE`]. A stdin write only
/// completes as fast as the child drains its pipe, so the budget has to cover the
/// child's startup as well as the transfer: `codex_fallback` writes a prompt of
/// roughly 192 KiB, several times any pipe buffer, into a Node CLI whose startup
/// alone can take seconds. A post-termination cleanup budget is the wrong
/// quantity here and would turn a working call into a reliable failure.
pub(crate) const COMMAND_STDIN_WRITE_GRACE: std::time::Duration =
    std::time::Duration::from_secs(30);

// ---------------------------------------------------------------------------
// File and directory tools
// ---------------------------------------------------------------------------

/// Largest file `read_file` will return in full.
pub(crate) const MAX_READ_FILE_BYTES: usize = 2 * 1024 * 1024;

/// Largest **raw** image file, before base64 expansion.
pub(crate) const MAX_IMAGE_RAW_BYTES: usize = 8 * 1024 * 1024;

/// Largest `write_file` content, enforced before any filesystem mutation.
pub(crate) const MAX_WRITE_FILE_CONTENT_BYTES: usize = 256 * 1024;

/// Largest existing file `write_file` will read to build a preimage and diff.
pub(crate) const MAX_WRITE_PREIMAGE_BYTES: usize = 2 * 1024 * 1024;

/// Largest number of entries `list_directory` will accumulate.
pub(crate) const MAX_DIRECTORY_ENTRIES: usize = 20_000;

/// Largest rendered `list_directory` listing.
/// Largest aggregate rendered-listing byte bound.
///
/// Deliberately independent of [`MAX_DIRECTORY_ENTRIES`]: many short names stay
/// under the byte cap, while fewer long names can breach it.
pub(crate) const MAX_DIRECTORY_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

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

// base64 expands by 4/3 and adds up to four padding characters. The base64
// alphabet needs no JSON escaping, so a raw image needs only the encoded size.
const _: () = assert!(MAX_IMAGE_RAW_BYTES / 3 * 4 + 4 <= MAX_MCP_RESPONSE_FRAME_BYTES);

// A command result is escaped twice on its way into a response frame: once by
// `render_output`, then again when `text_result` embeds it as a string value.
// Deriving this bound from raw byte counts alone would let an ordinary binary
// command produce a frame several times its own size.
const _: () = assert!(
    (MAX_COMMAND_STDOUT_BYTES + MAX_COMMAND_STDERR_BYTES) * JSON_ESCAPE_WORST_CASE
        <= MAX_MCP_RESPONSE_FRAME_BYTES
);

// These are escaped once, but are held to the stricter two-pass factor so a
// single constant covers every text-returning bound.
const _: () = assert!(MAX_READ_FILE_BYTES * JSON_ESCAPE_WORST_CASE <= MAX_MCP_RESPONSE_FRAME_BYTES);
const _: () =
    assert!(MAX_DIRECTORY_OUTPUT_BYTES * JSON_ESCAPE_WORST_CASE <= MAX_MCP_RESPONSE_FRAME_BYTES);

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

/// A resource bound was reached.
///
/// This is a typed marker, not just a message: the transport downcasts to it to
/// emit a distinct JSON-RPC code, so a resource failure is never reported as an
/// ordinary server error and never as a permission or authority failure.
///
/// The rendered message is fixed, host-authored text plus the resource name and
/// ceiling. It never embeds caller-supplied content, so a rejected request cannot
/// inflate the diagnostic that reports its rejection.
#[derive(Debug)]
pub(crate) struct ResourceLimitError {
    limit: ResourceLimit,
    detail: &'static str,
}

impl fmt::Display for ResourceLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.limit, self.detail)
    }
}

impl std::error::Error for ResourceLimitError {}

impl ResourceLimitError {
    pub(crate) fn new(limit: ResourceLimit, detail: &'static str) -> Self {
        Self { limit, detail }
    }

    /// The bounded resource that was reached.
    pub(crate) fn limit(&self) -> ResourceLimit {
        self.limit
    }
}

/// Build a resource-limit error carrying a fixed sentence.
pub(crate) fn limit_error(limit: ResourceLimit, detail: &'static str) -> anyhow::Error {
    ResourceLimitError::new(limit, detail).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceilings_match_the_documented_values() {
        assert_eq!(MAX_MCP_REQUEST_FRAME_BYTES, 8 * 1024 * 1024);
        assert_eq!(MAX_MCP_RESPONSE_FRAME_BYTES, 16 * 1024 * 1024);
        assert_eq!(MAX_COMMAND_STDOUT_BYTES, 1536 * 1024);
        assert_eq!(MAX_COMMAND_STDERR_BYTES, 256 * 1024);
        assert_eq!(MAX_READ_FILE_BYTES, 2 * 1024 * 1024);
        assert_eq!(MAX_IMAGE_RAW_BYTES, 8 * 1024 * 1024);
        assert_eq!(MAX_WRITE_FILE_CONTENT_BYTES, 256 * 1024);
        assert_eq!(MAX_DIRECTORY_ENTRIES, 20_000);
        assert_eq!(MAX_DIRECTORY_OUTPUT_BYTES, 2 * 1024 * 1024);
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
    fn an_ordinary_binary_command_result_fits_inside_one_response_frame() {
        // The regression that motivated `JSON_ESCAPE_WORST_CASE`: a command that
        // writes its entire stdout budget of NUL bytes exits successfully, and its
        // result must still fit, or an ordinary command would produce a frame the
        // transport refuses to write.
        let raw = MAX_COMMAND_STDOUT_BYTES;
        let rendered = serde_json::to_string(&"\0".repeat(raw)).unwrap();
        let framed = serde_json::json!({"content":[{"type":"text","text":rendered}]});
        assert!(
            framed.to_string().len() <= MAX_MCP_RESPONSE_FRAME_BYTES,
            "measured worst-case command frame {} must fit in {MAX_MCP_RESPONSE_FRAME_BYTES}",
            framed.to_string().len()
        );
        // The declared factor must not be optimistic about the measurement. The first
        // serialization pass alone costs exactly 6 bytes per control byte (plus the
        // two surrounding quotes); the second pass re-escapes each backslash,
        // which is what makes the full frame 7x.
        assert!(
            rendered.len() <= raw * 6 + 2,
            "measured first-pass size {} exceeds 6x plus quotes",
            rendered.len()
        );
        assert!(
            framed.to_string().len() <= raw * JSON_ESCAPE_WORST_CASE + 4096,
            "measured frame size {} exceeds the declared factor {JSON_ESCAPE_WORST_CASE}",
            framed.to_string().len()
        );
    }

    #[test]
    fn a_maximum_read_file_result_fits_inside_one_response_frame() {
        let raw = MAX_READ_FILE_BYTES;
        let framed = serde_json::json!({"content":[{"type":"text","text":"\0".repeat(raw)}]});
        assert!(
            framed.to_string().len() <= MAX_MCP_RESPONSE_FRAME_BYTES,
            "measured worst-case read_file frame {} must fit in {MAX_MCP_RESPONSE_FRAME_BYTES}",
            framed.to_string().len()
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
        let error = limit_error(ResourceLimit::CommandStdout, "output truncated");
        let rendered = error.to_string();
        assert!(rendered.contains("command stdout"));
        assert!(rendered.contains(&MAX_COMMAND_STDOUT_BYTES.to_string()));
        assert!(rendered.contains("output truncated"));
        assert!(
            error.downcast_ref::<ResourceLimitError>().is_some(),
            "a resource failure must be a typed marker, not just a string"
        );
    }

    #[test]
    fn a_resource_error_stays_small_and_never_echoes_caller_content() {
        // The worst realistic detail is still a fixed sentence; nothing derived
        // from a request is ever rendered.
        let error = ResourceLimitError::new(
            ResourceLimit::McpRequestFrame,
            "request frame exceeded the maximum and was discarded",
        );
        assert!(error.to_string().len() < 256);
        assert_eq!(error.limit(), ResourceLimit::McpRequestFrame);
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
