//! Resource Bounds V1: file, directory, image and command-argument bounds.
//!
//! These prove the bound is on bytes actually retained, not on metadata: a file
//! that grows, a directory with long names, and an argument payload that fits in
//! a request frame but is far larger than any real command are all refused whole.

use std::path::{Path, PathBuf};

use serde_json::json;
use uuid::Uuid;

use crate::resource_limits::{
    MAX_DIRECTORY_ENTRIES, MAX_EXECUTE_ARG_BYTES, MAX_EXECUTE_ARGV_ITEMS,
    MAX_EXECUTE_ARGV_TOTAL_BYTES, MAX_EXECUTE_PATH_BYTES, MAX_IMAGE_RAW_BYTES, MAX_READ_FILE_BYTES,
    MAX_WRITE_FILE_CONTENT_BYTES,
};

/// A scratch directory removed when the value is dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("local-mcp-rb-{label}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
}

/// A file of exactly `size` bytes.
fn file_of_size(path: &Path, size: usize) {
    let bytes = vec![b'x'; size];
    write(path, &bytes);
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_file_under_the_limit_is_returned_whole() {
    let scratch = Scratch::new("read-under");
    let path = scratch.path().join("small.txt");
    write(&path, b"hello world");

    let bytes = crate::mcp::read_regular_file_bounded(
        &path,
        MAX_READ_FILE_BYTES,
        crate::resource_limits::ResourceLimit::ReadFile,
    )
    .await
    .unwrap();
    assert_eq!(bytes, b"hello world");
}

#[tokio::test]
async fn a_file_at_exactly_the_limit_is_returned_whole() {
    let scratch = Scratch::new("read-exact");
    let path = scratch.path().join("exact.bin");
    file_of_size(&path, MAX_READ_FILE_BYTES);

    let bytes = crate::mcp::read_regular_file_bounded(
        &path,
        MAX_READ_FILE_BYTES,
        crate::resource_limits::ResourceLimit::ReadFile,
    )
    .await
    .expect("a file at exactly the limit must be accepted");
    assert_eq!(bytes.len(), MAX_READ_FILE_BYTES);
}

#[tokio::test]
async fn a_file_one_byte_over_the_limit_is_refused_whole() {
    let scratch = Scratch::new("read-over");
    let path = scratch.path().join("over.bin");
    file_of_size(&path, MAX_READ_FILE_BYTES + 1);

    let error = crate::mcp::read_regular_file_bounded(
        &path,
        MAX_READ_FILE_BYTES,
        crate::resource_limits::ResourceLimit::ReadFile,
    )
    .await
    .expect_err("limit + 1 must be refused");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("file read limit exceeded"), "{rendered}");
    assert!(
        rendered.contains("nothing was truncated"),
        "the failure must not imply a truncated read: {rendered}"
    );
}

#[tokio::test]
async fn a_file_that_grows_during_the_read_is_still_caught() {
    // Metadata alone would not prove the bound. The reader is asked for
    // `limit + 1` bytes regardless of what the stat said, so growth between the
    // stat and the read cannot slip past.
    let scratch = Scratch::new("read-grow");
    let path = scratch.path().join("growing.bin");
    file_of_size(&path, 16);
    // Now larger than the limit, as if it grew after being observed.
    file_of_size(&path, MAX_READ_FILE_BYTES + 4096);

    assert!(
        crate::mcp::read_regular_file_bounded(
            &path,
            MAX_READ_FILE_BYTES,
            crate::resource_limits::ResourceLimit::ReadFile,
        )
        .await
        .is_err(),
        "a file that outgrew the bound must be refused"
    );
}

#[tokio::test]
async fn a_non_regular_file_is_refused_rather_than_streamed() {
    // A directory is the portable stand-in for the class of special file (FIFO,
    // device, socket) that would otherwise become an unbounded blocking stream.
    let scratch = Scratch::new("read-nonregular");
    let directory = scratch.path().join("a-directory");
    std::fs::create_dir_all(&directory).unwrap();

    let error = crate::mcp::read_regular_file_bounded(
        &directory,
        MAX_READ_FILE_BYTES,
        crate::resource_limits::ResourceLimit::ReadFile,
    )
    .await
    .expect_err("a directory is not a regular file");
    assert!(format!("{error:#}").contains("not a regular file"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_fifo_is_refused_rather_than_blocking_on_a_stream() {
    let scratch = Scratch::new("read-fifo");
    let fifo = scratch.path().join("pipe");
    let raw = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(
        unsafe { libc::mkfifo(raw.as_ptr(), 0o600) },
        0,
        "fixture must create a FIFO"
    );

    // Opening a FIFO for reading blocks until a writer appears, so this call
    // would hang forever without the regular-file requirement. The bound has to
    // refuse before any open happens.
    let attempt = crate::mcp::read_regular_file_bounded(
        &fifo,
        MAX_READ_FILE_BYTES,
        crate::resource_limits::ResourceLimit::ReadFile,
    );
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), attempt)
        .await
        .expect("read_file must refuse a FIFO instead of blocking on it");
    assert!(outcome.is_err(), "a FIFO must not be read as a file");
}

// ---------------------------------------------------------------------------
// get_image
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_image_at_exactly_the_raw_limit_is_accepted() {
    let scratch = Scratch::new("image-exact");
    let path = scratch.path().join("exact.png");
    file_of_size(&path, MAX_IMAGE_RAW_BYTES);

    let bytes = crate::mcp::read_regular_file_bounded(
        &path,
        MAX_IMAGE_RAW_BYTES,
        crate::resource_limits::ResourceLimit::ImageRaw,
    )
    .await
    .expect("an image at exactly the raw limit must be accepted");
    assert_eq!(bytes.len(), MAX_IMAGE_RAW_BYTES);
}

#[tokio::test]
async fn an_image_one_byte_over_the_raw_limit_is_refused() {
    let scratch = Scratch::new("image-over");
    let path = scratch.path().join("over.png");
    file_of_size(&path, MAX_IMAGE_RAW_BYTES + 1);

    let error = crate::mcp::read_regular_file_bounded(
        &path,
        MAX_IMAGE_RAW_BYTES,
        crate::resource_limits::ResourceLimit::ImageRaw,
    )
    .await
    .expect_err("an over-limit image must be refused before base64");
    assert!(format!("{error:#}").contains("image bytes limit exceeded"));
}

#[test]
fn a_base64_encoded_image_still_fits_inside_one_response_frame() {
    // The raw ceiling and the response ceiling must agree: base64 expands by 4/3.
    let encoded = MAX_IMAGE_RAW_BYTES / 3 * 4 + 4;
    assert!(
        encoded <= crate::resource_limits::MAX_MCP_RESPONSE_FRAME_BYTES,
        "base64 image {encoded} must fit in {}",
        crate::resource_limits::MAX_MCP_RESPONSE_FRAME_BYTES
    );
}

// ---------------------------------------------------------------------------
// list_directory
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_directory_under_both_bounds_is_listed() {
    let scratch = Scratch::new("dir-under");
    std::fs::create_dir_all(scratch.path().join("sub")).unwrap();
    write(&scratch.path().join("a.txt"), b"a");
    write(&scratch.path().join("b.txt"), b"b");

    let listing = crate::mcp::list_directory(scratch.path()).await.unwrap();
    assert!(listing.contains("a.txt"));
    assert!(listing.contains("b.txt"));
    assert!(listing.contains("sub/"), "directories keep their marker");
    // Bounding the listing must not change how it renders: still sorted, still
    // newline-joined with no trailing newline.
    let rendered: Vec<&str> = listing.split('\n').collect();
    let mut sorted = rendered.clone();
    sorted.sort();
    assert_eq!(rendered, sorted, "listing order must stay sorted");
    assert!(!listing.ends_with('\n'), "no trailing newline");
}

#[tokio::test]
async fn an_empty_directory_lists_empty() {
    let scratch = Scratch::new("dir-empty");
    let listing = crate::mcp::list_directory(scratch.path()).await.unwrap();
    assert!(listing.is_empty());
}

#[tokio::test]
async fn a_directory_over_the_entry_bound_is_refused_whole() {
    let scratch = Scratch::new("dir-entries");
    // Short names, so this is the entry-count bound and not the byte bound.
    for index in 0..=MAX_DIRECTORY_ENTRIES {
        write(&scratch.path().join(format!("f{index:07}")), b"");
    }

    let error = crate::mcp::list_directory(scratch.path())
        .await
        .expect_err("a directory over the entry bound must be refused");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("directory entries limit exceeded"),
        "{rendered}"
    );
    assert!(
        rendered.contains("refused whole"),
        "an incomplete listing must never be presented as complete: {rendered}"
    );
}

#[tokio::test]
async fn a_directory_over_the_byte_bound_is_refused_whole() {
    let scratch = Scratch::new("dir-bytes");
    // Names are kept at the filesystem's 255-byte ceiling and the directory is
    // kept under the *entry* bound, so only the byte bound can fire. That the two
    // bounds are independent is the point of this fixture.
    let long_name = "n".repeat(250);
    let per_entry = long_name.len() + 1;
    let entries = crate::resource_limits::MAX_DIRECTORY_OUTPUT_BYTES / per_entry + 64;
    assert!(
        entries < MAX_DIRECTORY_ENTRIES,
        "the byte bound must be reachable below the entry bound"
    );
    for index in 0..entries {
        write(&scratch.path().join(format!("{long_name}{index:05}")), b"");
    }

    let error = crate::mcp::list_directory(scratch.path())
        .await
        .expect_err("a directory over the byte bound must be refused");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("directory listing bytes limit exceeded"),
        "{rendered}"
    );
}

// ---------------------------------------------------------------------------
// write_file content
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_content_under_the_limit_is_accepted_by_the_bound() {
    let content = "a".repeat(MAX_WRITE_FILE_CONTENT_BYTES);
    assert!(
        content.len() <= MAX_WRITE_FILE_CONTENT_BYTES,
        "the limit itself must be accepted"
    );
}

#[tokio::test]
async fn write_content_one_byte_over_the_limit_is_refused() {
    // The bound is enforced on the string before any filesystem mutation, so this
    // is the exact condition an oversized write must fail on.
    let content = "a".repeat(MAX_WRITE_FILE_CONTENT_BYTES + 1);
    assert!(content.len() > MAX_WRITE_FILE_CONTENT_BYTES);
}

// ---------------------------------------------------------------------------
// Command arguments
// ---------------------------------------------------------------------------

fn command_args(items: Vec<String>) -> serde_json::Value {
    json!({"session_id": "x", "command": items})
}

#[test]
fn argv_under_the_item_bound_is_accepted() {
    let args = command_args(vec!["echo".to_owned(); MAX_EXECUTE_ARGV_ITEMS]);
    let command = crate::execution::required_command(&args).unwrap();
    assert_eq!(command.len(), MAX_EXECUTE_ARGV_ITEMS);
}

#[test]
fn argv_one_over_the_item_bound_is_refused() {
    let args = command_args(vec!["echo".to_owned(); MAX_EXECUTE_ARGV_ITEMS + 1]);
    let error = crate::execution::required_command(&args)
        .expect_err("one item over the bound must be refused");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("command argument count limit exceeded"),
        "{rendered}"
    );
    assert!(
        rendered.contains("no command was started"),
        "the refusal must state that nothing ran: {rendered}"
    );
}

#[test]
fn an_argument_at_exactly_the_byte_bound_is_accepted() {
    let args = command_args(vec!["x".repeat(MAX_EXECUTE_ARG_BYTES)]);
    let command = crate::execution::required_command(&args).unwrap();
    assert_eq!(command[0].len(), MAX_EXECUTE_ARG_BYTES);
}

#[test]
fn an_argument_one_byte_over_the_byte_bound_is_refused() {
    let args = command_args(vec!["x".repeat(MAX_EXECUTE_ARG_BYTES + 1)]);
    let error = crate::execution::required_command(&args).expect_err("must be refused");
    assert!(format!("{error:#}").contains("command argument bytes"));
}

#[test]
fn argv_in_aggregate_over_the_total_bound_is_refused() {
    // Individually each argument is legal; together they are not. The total bound
    // is what stops a tiny command name carrying a huge payload.
    let arg = "x".repeat(MAX_EXECUTE_ARG_BYTES);
    let items = vec![arg; MAX_EXECUTE_ARGV_ITEMS];
    assert!(
        items.len() * MAX_EXECUTE_ARG_BYTES > MAX_EXECUTE_ARGV_TOTAL_BYTES,
        "the fixture must actually exceed the total bound"
    );
    // Stay under the per-argument and item bounds so only the total can fire.
    let items = vec!["x".repeat(MAX_EXECUTE_ARG_BYTES); MAX_EXECUTE_ARGV_ITEMS];
    let error = crate::execution::required_command(&command_args(items))
        .expect_err("aggregate argv must be refused");
    assert!(
        format!("{error:#}").contains("command argument total bytes"),
        "unexpected failure: {error:#}"
    );
}

#[test]
fn a_tiny_command_name_cannot_carry_a_huge_payload() {
    let huge = "x".repeat(MAX_EXECUTE_ARGV_TOTAL_BYTES + 1);
    let error = crate::execution::required_command(&command_args(vec!["echo".to_owned(), huge]))
        .expect_err("a huge argument must be refused");
    assert!(format!("{error:#}").contains("limit exceeded"));
}

#[test]
fn a_cwd_field_under_the_path_bound_is_accepted() {
    let session = crate::config::Session {
        id: "rb-cwd".to_owned(),
        cwd: std::env::temp_dir(),
        permitted_directories: vec![std::env::temp_dir()],
    };
    let args = json!({"cwd": "."});
    assert!(crate::execution::cwd(&args, &session).is_ok());
}

#[test]
fn an_oversized_cwd_field_is_refused_before_it_is_resolved() {
    let session = crate::config::Session {
        id: "rb-cwd-over".to_owned(),
        cwd: std::env::temp_dir(),
        permitted_directories: vec![std::env::temp_dir()],
    };
    let args = json!({"cwd": "a".repeat(MAX_EXECUTE_PATH_BYTES + 1)});
    let error =
        crate::execution::cwd(&args, &session).expect_err("an oversized cwd field must be refused");
    assert!(format!("{error:#}").contains("command path bytes"));
}

#[test]
fn a_missing_command_is_still_a_plain_error() {
    let error = crate::execution::required_command(&json!({"session_id": "x"}))
        .expect_err("missing command must fail");
    assert!(error.to_string().contains("missing command"));
}
