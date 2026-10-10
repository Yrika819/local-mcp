//! Slice 2C: bounded, process-contained streaming of raw index blobs.
//!
//! This is intentionally a narrow Git protocol owner, not a reusable process
//! API. One `git cat-file --batch` process serves only validated stage-0 OIDs
//! from Slice 2A entries. Every response is bounded and consumed exactly; any
//! error consumes the session, whose `ProcessGroup` drop terminates its tree.

#![expect(
    dead_code,
    reason = "Slice 2C freezes the index blob reader before 2D snapshot assembly is authorized to call it."
)]

use std::fmt::Write as _;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::time::{Instant, timeout_at};

use crate::execution;
use crate::managed_worktree_snapshot_object::{SnapshotHashBudget, SnapshotObjectError};
use crate::managed_worktree_snapshot_observe::{SnapshotIndexEntry, SnapshotIndexMode};
use crate::process_group::ProcessGroup;
use crate::sandbox;
use crate::workspace_snapshot::{CandidateObjectIdentity, MAX_GIT_VISIBLE_PATHS};

const HEADER_LIMIT: usize = 128;
const STREAM_BUFFER_BYTES: usize = 64 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const STDERR_DRAIN_BYTES: usize = 4096;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const SESSION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug)]
pub(crate) enum IndexBlobError {
    Io(io::Error),
    GitUnavailable(String),
    InvalidOid,
    Protocol(&'static str),
    LimitExceeded(&'static str),
    Timeout,
    ChildFailed(String),
}

impl std::fmt::Display for IndexBlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "index blob I/O failed: {error}"),
            Self::GitUnavailable(detail) => write!(f, "host Git unavailable: {detail}"),
            Self::InvalidOid => f.write_str("invalid index object ID"),
            Self::Protocol(reason) => write!(f, "invalid cat-file batch protocol: {reason}"),
            Self::LimitExceeded(reason) => write!(f, "index blob {reason} limit exceeded"),
            Self::Timeout => f.write_str("index blob batch session timed out"),
            Self::ChildFailed(detail) => write!(f, "index blob batch process failed: {detail}"),
        }
    }
}

impl std::error::Error for IndexBlobError {}

impl From<io::Error> for IndexBlobError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<SnapshotObjectError> for IndexBlobError {
    fn from(error: SnapshotObjectError) -> Self {
        match error {
            SnapshotObjectError::LimitExceeded(_) => Self::LimitExceeded("aggregate content bytes"),
            SnapshotObjectError::Io(error) => Self::Io(error),
            SnapshotObjectError::Unsupported(_) | SnapshotObjectError::Unstable => {
                Self::Protocol("shared snapshot budget rejected the object")
            }
        }
    }
}

/// The sole owner of a persistent, narrowly-configured index blob batch child.
pub(crate) struct SnapshotIndexBlobSession {
    group: ProcessGroup,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    stderr: ChildStderr,
    stderr_evidence: Vec<u8>,
    stderr_bytes: usize,
    stderr_eof: bool,
    request_count: usize,
    started: Instant,
}

impl SnapshotIndexBlobSession {
    pub(crate) async fn spawn(repository: &Path) -> Result<Self, IndexBlobError> {
        let executable = execution::host_git_path()
            .map_err(|error| IndexBlobError::GitUnavailable(format!("{error:#}")))?;
        let mut command = Command::new(executable);
        command
            .env_clear()
            .envs(sandbox::clean_git_environment())
            .args(batch_arguments())
            .current_dir(repository)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut group = ProcessGroup::spawn(&mut command)?;
        let stdin = group
            .take_stdin()
            .ok_or(IndexBlobError::Protocol("child stdin was not piped"))?;
        let stdout = group
            .take_stdout()
            .ok_or(IndexBlobError::Protocol("child stdout was not piped"))?;
        let stderr = group
            .take_stderr()
            .ok_or(IndexBlobError::Protocol("child stderr was not piped"))?;
        Ok(Self {
            group,
            stdin: Some(stdin),
            stdout,
            stderr,
            stderr_evidence: Vec::with_capacity(STDERR_LIMIT),
            stderr_bytes: 0,
            stderr_eof: false,
            request_count: 0,
            started: Instant::now(),
        })
    }

    /// Consume and return the session only after one complete valid response.
    /// Cancellation drops this owned value, terminating the contained process.
    pub(crate) async fn read_entry(
        self,
        entry: &SnapshotIndexEntry,
        budget: &mut SnapshotHashBudget,
    ) -> Result<(Self, CandidateObjectIdentity), IndexBlobError> {
        self.read_entry_with_timeout(entry, budget, RESPONSE_TIMEOUT)
            .await
    }

    #[cfg(test)]
    async fn read_entry_with_test_timeout(
        self,
        entry: &SnapshotIndexEntry,
        budget: &mut SnapshotHashBudget,
        timeout: Duration,
    ) -> Result<(Self, CandidateObjectIdentity), IndexBlobError> {
        self.read_entry_with_timeout(entry, budget, timeout).await
    }

    async fn read_entry_with_timeout(
        mut self,
        entry: &SnapshotIndexEntry,
        budget: &mut SnapshotHashBudget,
        response_timeout: Duration,
    ) -> Result<(Self, CandidateObjectIdentity), IndexBlobError> {
        let preflight_error = if self.request_count >= MAX_GIT_VISIBLE_PATHS {
            Some(IndexBlobError::LimitExceeded("request count"))
        } else if let Err(error) = validate_oid(&entry.oid) {
            Some(error)
        } else if Instant::now() >= self.started + SESSION_TIMEOUT {
            Some(IndexBlobError::Timeout)
        } else {
            None
        };
        if let Some(error) = preflight_error {
            self.terminate_and_wait().await;
            return Err(error);
        }
        let session_deadline = self.started + SESSION_TIMEOUT;
        let response_deadline = (Instant::now() + response_timeout).min(session_deadline);
        let outcome = timeout_at(
            response_deadline,
            self.read_entry_response(&entry.oid, entry.mode, budget),
        )
        .await
        .map_err(|_| IndexBlobError::Timeout)
        .and_then(|result| result);
        let identity = match outcome {
            Ok(identity) => identity,
            Err(error) => {
                self.terminate_and_wait().await;
                return Err(error);
            }
        };
        self.request_count += 1;
        Ok((self, identity))
    }

    async fn read_entry_response(
        &mut self,
        requested_oid: &str,
        mode: SnapshotIndexMode,
        budget: &mut SnapshotHashBudget,
    ) -> Result<CandidateObjectIdentity, IndexBlobError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or(IndexBlobError::Protocol("batch stdin is closed"))?;
        stdin.write_all(requested_oid.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;

        let header = self.read_header().await?;
        let declared_size = parse_header(&header, requested_oid)?;
        if declared_size > budget.remaining() {
            return Err(IndexBlobError::LimitExceeded("aggregate content bytes"));
        }

        let mut digest = Sha256::new();
        let mut buffer = [0_u8; STREAM_BUFFER_BYTES];
        let mut remaining = declared_size;
        while remaining > 0 {
            let capacity = usize::try_from(remaining)
                .unwrap_or(usize::MAX)
                .min(buffer.len());
            let read = self.read_stdout(&mut buffer[..capacity]).await?;
            if read == 0 {
                return Err(IndexBlobError::Protocol("premature EOF in blob body"));
            }
            digest.update(&buffer[..read]);
            remaining -= read as u64;
        }
        let mut delimiter = [0_u8; 1];
        if self.read_stdout(&mut delimiter).await? != 1 || delimiter[0] != b'\n' {
            return Err(IndexBlobError::Protocol("missing body delimiter"));
        }
        budget.charge(
            usize::try_from(declared_size)
                .map_err(|_| IndexBlobError::LimitExceeded("platform-sized object"))?,
        )?;
        let digest = digest_hex(digest);
        Ok(match mode {
            SnapshotIndexMode::Regular { executable } => CandidateObjectIdentity::RegularFile {
                content_sha256: digest,
                size_bytes: declared_size,
                executable,
            },
            SnapshotIndexMode::Symlink => CandidateObjectIdentity::Symlink {
                target_sha256: digest,
                target_size_bytes: declared_size,
            },
        })
    }

    async fn read_header(&mut self) -> Result<Vec<u8>, IndexBlobError> {
        let mut header = Vec::with_capacity(HEADER_LIMIT.min(64));
        let mut byte = [0_u8; 1];
        loop {
            let count = self.read_stdout(&mut byte).await?;
            if count == 0 {
                return Err(IndexBlobError::Protocol("EOF before response header"));
            }
            if byte[0] == b'\n' {
                return Ok(header);
            }
            if header.len() == HEADER_LIMIT {
                return Err(IndexBlobError::LimitExceeded("header bytes"));
            }
            header.push(byte[0]);
        }
    }

    async fn read_stdout(&mut self, output: &mut [u8]) -> Result<usize, IndexBlobError> {
        loop {
            let mut stderr_chunk = [0_u8; STDERR_DRAIN_BYTES];
            let stdout = &mut self.stdout;
            let stderr = &mut self.stderr;
            tokio::select! {
                result = stdout.read(output) => return result.map_err(IndexBlobError::Io),
                result = stderr.read(&mut stderr_chunk), if !self.stderr_eof => {
                    let count = result?;
                    if count == 0 {
                        self.stderr_eof = true;
                    } else {
                        record_stderr(&mut self.stderr_evidence, &mut self.stderr_bytes, &stderr_chunk[..count])?;
                    }
                }
            }
        }
    }

    /// Close stdin, require no unsolicited stdout, and reap the cleanly exited child.
    pub(crate) async fn finish(mut self) -> Result<(), IndexBlobError> {
        let result = self.finish_inner().await;
        if result.is_err() {
            self.terminate_and_wait().await;
        }
        result
    }

    async fn finish_inner(&mut self) -> Result<(), IndexBlobError> {
        self.stdin.take();
        let mut trailing = [0_u8; 1];
        let overall = self.started + SESSION_TIMEOUT;
        let read_end = timeout_at(overall, self.read_stdout(&mut trailing))
            .await
            .map_err(|_| IndexBlobError::Timeout)??;
        if read_end != 0 {
            return Err(IndexBlobError::Protocol(
                "extra stdout after final response",
            ));
        }
        let status = timeout_at(overall, self.wait_for_exit())
            .await
            .map_err(|_| IndexBlobError::Timeout)??;
        self.group.terminate();
        timeout_at(overall, self.drain_stderr_to_eof())
            .await
            .map_err(|_| IndexBlobError::Timeout)??;
        if !status.success() {
            let stderr = String::from_utf8_lossy(&self.stderr_evidence);
            let escaped_stderr = stderr.escape_debug().take(512).collect::<String>();
            return Err(IndexBlobError::ChildFailed(format!(
                "exit code {:?}; stderr={escaped_stderr}",
                status.code()
            )));
        }
        Ok(())
    }

    async fn terminate_and_wait(&mut self) {
        self.stdin.take();
        self.group.terminate();
        let cleanup_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let _ = timeout_at(cleanup_deadline, self.group.wait_termination()).await;
    }

    async fn wait_for_exit(&mut self) -> Result<std::process::ExitStatus, IndexBlobError> {
        let wait = self.group.wait_termination();
        tokio::pin!(wait);
        loop {
            let mut stderr_chunk = [0_u8; STDERR_DRAIN_BYTES];
            tokio::select! {
                status = &mut wait => return status.map_err(IndexBlobError::Io),
                result = self.stderr.read(&mut stderr_chunk), if !self.stderr_eof => {
                    let count = result?;
                    if count == 0 {
                        self.stderr_eof = true;
                    } else {
                        record_stderr(&mut self.stderr_evidence, &mut self.stderr_bytes, &stderr_chunk[..count])?;
                    }
                }
            }
        }
    }

    async fn drain_stderr_to_eof(&mut self) -> Result<(), IndexBlobError> {
        let mut stderr_chunk = [0_u8; STDERR_DRAIN_BYTES];
        while !self.stderr_eof {
            let count = self.stderr.read(&mut stderr_chunk).await?;
            if count == 0 {
                self.stderr_eof = true;
            } else {
                record_stderr(
                    &mut self.stderr_evidence,
                    &mut self.stderr_bytes,
                    &stderr_chunk[..count],
                )?;
            }
        }
        Ok(())
    }
}

fn record_stderr(
    evidence: &mut Vec<u8>,
    total_bytes: &mut usize,
    bytes: &[u8],
) -> Result<(), IndexBlobError> {
    *total_bytes = total_bytes
        .checked_add(bytes.len())
        .ok_or(IndexBlobError::LimitExceeded("stderr bytes"))?;
    if *total_bytes > STDERR_LIMIT {
        return Err(IndexBlobError::LimitExceeded("stderr bytes"));
    }
    evidence.extend_from_slice(bytes);
    Ok(())
}

fn batch_arguments() -> Vec<String> {
    vec![
        "--no-pager".to_owned(),
        "--no-replace-objects".to_owned(),
        "-c".to_owned(),
        format!("core.hooksPath={}", sandbox::null_device_path()),
        "-c".to_owned(),
        "core.fsmonitor=false".to_owned(),
        "--no-optional-locks".to_owned(),
        "cat-file".to_owned(),
        "--batch".to_owned(),
    ]
}

fn validate_oid(oid: &str) -> Result<(), IndexBlobError> {
    if matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(IndexBlobError::InvalidOid)
    }
}

fn parse_header(header: &[u8], requested_oid: &str) -> Result<u64, IndexBlobError> {
    if header.len() > HEADER_LIMIT {
        return Err(IndexBlobError::LimitExceeded("header bytes"));
    }
    let text = std::str::from_utf8(header)
        .map_err(|_| IndexBlobError::Protocol("non-UTF-8 response header"))?;
    let mut fields = text.split(' ');
    let returned_oid = fields
        .next()
        .ok_or(IndexBlobError::Protocol("missing returned OID"))?;
    let object_type = fields
        .next()
        .ok_or(IndexBlobError::Protocol("missing object type"))?;
    let size = fields
        .next()
        .ok_or(IndexBlobError::Protocol("missing object size"))?;
    if fields.next().is_some() || returned_oid != requested_oid {
        return Err(IndexBlobError::Protocol("wrong OID or malformed header"));
    }
    if object_type != "blob" {
        return Err(IndexBlobError::Protocol("object is not a blob"));
    }
    if size.is_empty()
        || !size.bytes().all(|byte| byte.is_ascii_digit())
        || (size.len() > 1 && size.starts_with('0'))
    {
        return Err(IndexBlobError::Protocol("non-canonical blob size"));
    }
    size.parse::<u64>()
        .map_err(|_| IndexBlobError::Protocol("blob size overflow"))
}

fn digest_hex(digest: Sha256) -> String {
    let mut output = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_argv_is_the_closed_hardened_batch_command() {
        let args = batch_arguments();
        assert_eq!(
            args,
            vec![
                "--no-pager",
                "--no-replace-objects",
                "-c",
                &format!("core.hooksPath={}", sandbox::null_device_path()),
                "-c",
                "core.fsmonitor=false",
                "--no-optional-locks",
                "cat-file",
                "--batch",
            ]
        );
    }

    #[test]
    fn validates_full_lowercase_sha1_and_sha256_oids() {
        assert!(validate_oid(&"a".repeat(40)).is_ok());
        assert!(validate_oid(&"f".repeat(64)).is_ok());
        assert!(validate_oid(&"A".repeat(40)).is_err());
        assert!(validate_oid(&"g".repeat(64)).is_err());
        assert!(validate_oid("abc\n--help").is_err());
    }

    #[test]
    fn parses_only_canonical_blob_headers() {
        let oid = "a".repeat(40);
        assert_eq!(
            parse_header(format!("{oid} blob 0").as_bytes(), &oid).unwrap(),
            0
        );
        assert_eq!(
            parse_header(format!("{oid} blob 12").as_bytes(), &oid).unwrap(),
            12
        );
        assert!(parse_header(format!("{oid} blob 012").as_bytes(), &oid).is_err());
        assert!(parse_header(format!("{} blob 1", "b".repeat(40)).as_bytes(), &oid).is_err());
        assert!(parse_header(format!("{oid} tree 1").as_bytes(), &oid).is_err());
        assert!(parse_header(format!("{oid} blob {}", u64::MAX).as_bytes(), &oid).is_ok());
        assert!(parse_header(format!("{oid} blob 18446744073709551616").as_bytes(), &oid).is_err());
        assert!(parse_header(format!("{oid} blob 1 extra").as_bytes(), &oid).is_err());
        assert!(parse_header(&[b'x'; HEADER_LIMIT + 1], &oid).is_err());
        assert!(parse_header(&[0xff], &oid).is_err());
        let oid64 = "c".repeat(64);
        assert_eq!(
            parse_header(format!("{oid64} blob 0").as_bytes(), &oid64).unwrap(),
            0
        );
    }

    fn temp_repo(label: &str, sha256: bool) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "local-mcp-snapshot-2c-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).unwrap();
        let mut args = vec!["init", "--quiet"];
        if sha256 {
            args.push("--object-format=sha256");
        }
        let output = std::process::Command::new(execution::host_git_path().unwrap())
            .args(args)
            .current_dir(&path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        path
    }

    fn git(repo: &Path, args: &[&str]) -> Vec<u8> {
        let output = std::process::Command::new(execution::host_git_path().unwrap())
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn write_blob(repo: &Path, bytes: &[u8]) -> String {
        use std::io::Write as _;
        let mut child = std::process::Command::new(execution::host_git_path().unwrap())
            .args(["hash-object", "-w", "--stdin"])
            .current_dir(repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .trim_end()
            .to_owned()
    }

    fn entry(oid: String, mode: SnapshotIndexMode) -> SnapshotIndexEntry {
        SnapshotIndexEntry {
            path: crate::workspace_snapshot::NormalizedWorkspacePath::parse("item").unwrap(),
            mode,
            oid,
        }
    }

    #[cfg(any(unix, windows))]
    fn fake_session(mut command: Command) -> SnapshotIndexBlobSession {
        let mut group = ProcessGroup::spawn(&mut command).unwrap();
        SnapshotIndexBlobSession {
            stdin: Some(group.take_stdin().unwrap()),
            stdout: group.take_stdout().unwrap(),
            stderr: group.take_stderr().unwrap(),
            group,
            stderr_evidence: Vec::with_capacity(STDERR_LIMIT),
            stderr_bytes: 0,
            stderr_eof: false,
            request_count: 0,
            started: Instant::now(),
        }
    }

    #[cfg(unix)]
    fn fake_shell(script: &str) -> SnapshotIndexBlobSession {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        fake_session(command)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_oid_is_rejected_before_any_stdin_request() {
        let ready =
            std::env::temp_dir().join(format!("snapshot-2c-ready-{}", uuid::Uuid::new_v4()));
        let received =
            std::env::temp_dir().join(format!("snapshot-2c-received-{}", uuid::Uuid::new_v4()));
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "printf ready > \"$READY\"; IFS= read -r oid; printf received > \"$RECEIVED\"",
            ])
            .env("READY", &ready)
            .env("RECEIVED", &received)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let session = fake_session(command);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !ready.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fake process reached its input wait");
        let malicious = entry(
            "a".repeat(40) + "\n--help",
            SnapshotIndexMode::Regular { executable: false },
        );
        let mut budget = SnapshotHashBudget::new();
        assert!(matches!(
            session.read_entry(&malicious, &mut budget).await,
            Err(IndexBlobError::InvalidOid)
        ));
        assert!(!received.exists());
        std::fs::remove_file(ready).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_protocol_rejects_short_body_bad_delimiter_and_trailing_bytes() {
        let oid = "a".repeat(40);
        let entry = entry(
            oid.clone(),
            SnapshotIndexMode::Regular { executable: false },
        );
        let mut budget = SnapshotHashBudget::new();
        let session = fake_shell("IFS= read -r oid; printf '%s blob 3\\nab' \"$oid\"");
        assert!(matches!(
            session.read_entry(&entry, &mut budget).await,
            Err(IndexBlobError::Protocol(_))
        ));

        let session = fake_shell("IFS= read -r oid; printf '%s blob 3\\nabcX' \"$oid\"");
        assert!(matches!(
            session.read_entry(&entry, &mut budget).await,
            Err(IndexBlobError::Protocol(_))
        ));

        let session = fake_shell("IFS= read -r oid; printf '%s blob 0\\n\\nextra' \"$oid\"");
        let (session, identity) = session.read_entry(&entry, &mut budget).await.unwrap();
        assert!(matches!(
            identity,
            CandidateObjectIdentity::RegularFile { size_bytes: 0, .. }
        ));
        assert!(matches!(
            session.finish().await,
            Err(IndexBlobError::Protocol(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_at_exact_limit_is_drained_without_deadlock() {
        let oid = "e".repeat(40);
        let entry = entry(oid, SnapshotIndexMode::Regular { executable: false });
        let session = fake_shell(
            "head -c 65536 /dev/zero >&2; IFS= read -r oid; printf '%s blob 0\\n\\n' \"$oid\"",
        );
        let mut budget = SnapshotHashBudget::new();
        let (session, identity) = session.read_entry(&entry, &mut budget).await.unwrap();
        assert!(matches!(
            identity,
            CandidateObjectIdentity::RegularFile { size_bytes: 0, .. }
        ));
        session.finish().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn finish_checks_stderr_written_after_stdout_completion() {
        let oid = "9".repeat(40);
        let entry = entry(oid, SnapshotIndexMode::Regular { executable: false });
        let session = fake_shell(
            "IFS= read -r oid; printf '%s blob 0\\n\\n' \"$oid\"; head -c 70000 /dev/zero >&2",
        );
        let mut budget = SnapshotHashBudget::new();
        let (session, _) = session.read_entry(&entry, &mut budget).await.unwrap();
        assert!(matches!(
            session.finish().await,
            Err(IndexBlobError::LimitExceeded("stderr bytes"))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_after_valid_response_is_rejected() {
        let oid = "f".repeat(40);
        let entry = entry(oid, SnapshotIndexMode::Regular { executable: false });
        let session = fake_shell("IFS= read -r oid; printf '%s blob 0\\n\\n' \"$oid\"; exit 9");
        let mut budget = SnapshotHashBudget::new();
        let (session, _) = session.read_entry(&entry, &mut budget).await.unwrap();
        assert!(matches!(
            session.finish().await,
            Err(IndexBlobError::ChildFailed(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn request_count_exhaustion_consumes_the_session() {
        let oid = "1".repeat(40);
        let entry = entry(oid, SnapshotIndexMode::Regular { executable: false });
        let mut session = fake_shell("sleep 60");
        session.request_count = MAX_GIT_VISIBLE_PATHS;
        let mut budget = SnapshotHashBudget::new();
        assert!(matches!(
            session.read_entry(&entry, &mut budget).await,
            Err(IndexBlobError::LimitExceeded("request count"))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stderr_overflow_poisoning_terminates_fake_batch() {
        let oid = "b".repeat(40);
        let entry = entry(oid, SnapshotIndexMode::Regular { executable: false });
        let session = fake_shell(
            "head -c 70000 /dev/zero >&2; IFS= read -r oid; printf '%s blob 0\\n\\n' \"$oid\"",
        );
        let mut budget = SnapshotHashBudget::new();
        assert!(matches!(
            session.read_entry(&entry, &mut budget).await,
            Err(IndexBlobError::LimitExceeded("stderr bytes"))
        ));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_timeout_terminates_job_contained_child_tree() {
        let oid = "d".repeat(40);
        let entry = entry(oid, SnapshotIndexMode::Regular { executable: false });
        let marker =
            std::env::temp_dir().join(format!("snapshot-2c-child-{}.pid", uuid::Uuid::new_v4()));
        let script = "$child=Start-Process -FilePath powershell.exe -ArgumentList @('-NoProfile','-Command','Start-Sleep -Seconds 60') -PassThru; [IO.File]::WriteAllText($env:WITNESS,[string]$child.Id); Start-Sleep -Seconds 60";
        let mut command = Command::new("powershell.exe");
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .env("WITNESS", &marker)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let session = fake_session(command);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !marker.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("PowerShell child published its PID witness");
        let child_pid = std::fs::read_to_string(&marker).unwrap();
        let mut budget = SnapshotHashBudget::new();
        let result = session
            .read_entry_with_test_timeout(&entry, &mut budget, Duration::from_millis(50))
            .await;
        assert!(matches!(result, Err(IndexBlobError::Timeout)));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let output = std::process::Command::new("tasklist.exe")
                    .args([
                        "/FI",
                        &format!("PID eq {}", child_pid.trim()),
                        "/FO",
                        "CSV",
                        "/NH",
                    ])
                    .output()
                    .unwrap();
                let listing = String::from_utf8_lossy(&output.stdout);
                if !listing.contains(&format!("\"{}\"", child_pid.trim())) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("Job-contained descendant stopped after session timeout");
        std::fs::remove_file(marker).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_drops_the_session_owner_and_terminates_its_tree() {
        let oid = "d".repeat(40);
        let entry = entry(oid, SnapshotIndexMode::Regular { executable: false });
        let marker =
            std::env::temp_dir().join(format!("snapshot-2c-child-{}.pid", uuid::Uuid::new_v4()));
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "sleep 60 & printf '%s' \"$!\" > \"$WITNESS\"; IFS= read -r oid; sleep 60",
            ])
            .env("WITNESS", &marker)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let session = fake_session(command);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !marker.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fake child published its PID witness");
        let child_pid = std::fs::read_to_string(&marker).unwrap();
        let mut budget = SnapshotHashBudget::new();
        let result = session
            .read_entry_with_test_timeout(&entry, &mut budget, Duration::from_millis(50))
            .await;
        assert!(matches!(result, Err(IndexBlobError::Timeout)));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let status = std::process::Command::new("/bin/ps")
                    .args(["-o", "stat=", "-p", child_pid.trim()])
                    .output()
                    .unwrap();
                let state = String::from_utf8_lossy(&status.stdout);
                if !status.status.success() || state.trim_start().starts_with('Z') {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("contained descendant stopped after session cancellation");
        std::fs::remove_file(marker).unwrap();
    }

    #[tokio::test]
    async fn persistent_reader_streams_raw_binary_blobs_and_maps_index_modes() {
        let repo = temp_repo("raw", false);
        let empty_oid = write_blob(&repo, b"");
        let mut binary = vec![0_u8; 70 * 1024 + 19];
        for (index, byte) in binary.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }
        binary[3] = 0;
        let binary_oid = write_blob(&repo, &binary);
        let symlink_bytes = b"../raw-target\0literal";
        let symlink_oid = write_blob(&repo, symlink_bytes);
        let mut budget = SnapshotHashBudget::new();
        let (session, empty) = SnapshotIndexBlobSession::spawn(&repo)
            .await
            .unwrap()
            .read_entry(
                &entry(
                    empty_oid.clone(),
                    SnapshotIndexMode::Regular { executable: false },
                ),
                &mut budget,
            )
            .await
            .unwrap();
        assert_eq!(
            empty,
            CandidateObjectIdentity::RegularFile {
                content_sha256: format!("{:x}", Sha256::digest(b"")),
                size_bytes: 0,
                executable: false,
            }
        );
        let (session, regular) = session
            .read_entry(
                &entry(
                    binary_oid.clone(),
                    SnapshotIndexMode::Regular { executable: true },
                ),
                &mut budget,
            )
            .await
            .unwrap();
        assert_eq!(
            regular,
            CandidateObjectIdentity::RegularFile {
                content_sha256: format!("{:x}", Sha256::digest(&binary)),
                size_bytes: binary.len() as u64,
                executable: true,
            }
        );
        let (session, symlink) = session
            .read_entry(&entry(symlink_oid, SnapshotIndexMode::Symlink), &mut budget)
            .await
            .unwrap();
        assert_eq!(
            symlink,
            CandidateObjectIdentity::Symlink {
                target_sha256: format!("{:x}", Sha256::digest(symlink_bytes)),
                target_size_bytes: symlink_bytes.len() as u64,
            }
        );
        let (session, duplicate) = session
            .read_entry(
                &entry(binary_oid, SnapshotIndexMode::Regular { executable: false }),
                &mut budget,
            )
            .await
            .unwrap();
        assert_eq!(
            duplicate,
            CandidateObjectIdentity::RegularFile {
                content_sha256: format!("{:x}", Sha256::digest(&binary)),
                size_bytes: binary.len() as u64,
                executable: false,
            }
        );
        assert_eq!(
            budget.hashed_bytes(),
            (binary.len() * 2 + symlink_bytes.len()) as u64
        );
        session.finish().await.unwrap();
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn budget_accepts_exact_remaining_bytes_and_rejects_one_over_before_body() {
        let repo = temp_repo("budget", false);
        let bytes = b"four";
        let oid = write_blob(&repo, bytes);
        let exact = SnapshotHashBudget::with_hashed_bytes_for_test(
            crate::workspace_snapshot::MAX_SNAPSHOT_CONTENT_BYTES - bytes.len() as u64,
        );
        let mut budget = exact;
        let (session, identity) = SnapshotIndexBlobSession::spawn(&repo)
            .await
            .unwrap()
            .read_entry(
                &entry(
                    oid.clone(),
                    SnapshotIndexMode::Regular { executable: false },
                ),
                &mut budget,
            )
            .await
            .unwrap();
        assert!(matches!(
            identity,
            CandidateObjectIdentity::RegularFile { size_bytes: 4, .. }
        ));
        assert_eq!(budget.remaining(), 0);
        session.finish().await.unwrap();

        let mut budget = SnapshotHashBudget::with_hashed_bytes_for_test(
            crate::workspace_snapshot::MAX_SNAPSHOT_CONTENT_BYTES - bytes.len() as u64 + 1,
        );
        let session = SnapshotIndexBlobSession::spawn(&repo).await.unwrap();
        assert!(matches!(
            session
                .read_entry(
                    &entry(oid, SnapshotIndexMode::Regular { executable: false }),
                    &mut budget
                )
                .await,
            Err(IndexBlobError::LimitExceeded("aggregate content bytes"))
        ));
        assert_eq!(budget.remaining(), 3);
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn missing_objects_fail_closed_without_charging_budget() {
        let repo = temp_repo("missing-object", false);
        let mut budget = SnapshotHashBudget::new();
        let session = SnapshotIndexBlobSession::spawn(&repo).await.unwrap();
        assert!(
            session
                .read_entry(
                    &entry(
                        "0".repeat(40),
                        SnapshotIndexMode::Regular { executable: false }
                    ),
                    &mut budget
                )
                .await
                .is_err()
        );
        assert_eq!(budget.hashed_bytes(), 0);
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn replace_refs_are_ignored_even_when_positive_control_uses_them() {
        let repo = temp_repo("replace", false);
        let original = b"original raw object";
        let replacement = b"replacement raw obj";
        assert_eq!(original.len(), replacement.len());
        let original_oid = write_blob(&repo, original);
        let replacement_oid = write_blob(&repo, replacement);
        let refname = format!("refs/replace/{original_oid}");
        git(&repo, &["update-ref", &refname, &replacement_oid]);
        assert_eq!(
            git(&repo, &["cat-file", "blob", &original_oid]),
            replacement
        );
        let mut budget = SnapshotHashBudget::new();
        let (session, identity) = SnapshotIndexBlobSession::spawn(&repo)
            .await
            .unwrap()
            .read_entry(
                &entry(
                    original_oid,
                    SnapshotIndexMode::Regular { executable: false },
                ),
                &mut budget,
            )
            .await
            .unwrap();
        assert_eq!(
            identity,
            CandidateObjectIdentity::RegularFile {
                content_sha256: format!("{:x}", Sha256::digest(original)),
                size_bytes: original.len() as u64,
                executable: false,
            }
        );
        session.finish().await.unwrap();
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn hostile_attributes_and_lfs_pointer_remain_raw_blob_bytes() {
        let repo = temp_repo("lfs-pointer", false);
        let pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nsize 12\n";
        std::fs::write(
            repo.join(".gitattributes"),
            "*.bin filter=hostile diff=hostile\n",
        )
        .unwrap();
        git(
            &repo,
            &[
                "config",
                "filter.hostile.smudge",
                "sh -c 'touch SHOULD_NOT_RUN'",
            ],
        );
        git(
            &repo,
            &[
                "config",
                "diff.hostile.textconv",
                "sh -c 'touch SHOULD_NOT_RUN'",
            ],
        );
        let oid = write_blob(&repo, pointer);
        let mut budget = SnapshotHashBudget::new();
        let (session, identity) = SnapshotIndexBlobSession::spawn(&repo)
            .await
            .unwrap()
            .read_entry(
                &entry(oid, SnapshotIndexMode::Regular { executable: false }),
                &mut budget,
            )
            .await
            .unwrap();
        assert_eq!(
            identity,
            CandidateObjectIdentity::RegularFile {
                content_sha256: format!("{:x}", Sha256::digest(pointer)),
                size_bytes: pointer.len() as u64,
                executable: false,
            }
        );
        assert!(!repo.join("SHOULD_NOT_RUN").exists());
        session.finish().await.unwrap();
        std::fs::remove_dir_all(repo).unwrap();
    }

    #[tokio::test]
    async fn sha256_repository_is_used_when_host_git_supports_it() {
        let repo = std::env::temp_dir().join(format!(
            "local-mcp-snapshot-2c-sha256-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&repo).unwrap();
        let init = std::process::Command::new(execution::host_git_path().unwrap())
            .args(["init", "--quiet", "--object-format=sha256"])
            .current_dir(&repo)
            .output()
            .unwrap();
        if !init.status.success() {
            eprintln!(
                "host Git lacks SHA-256 repository support: {}",
                String::from_utf8_lossy(&init.stderr)
            );
            std::fs::remove_dir_all(repo).unwrap();
            return;
        }
        let bytes = b"sha256 blob";
        let oid = write_blob(&repo, bytes);
        assert_eq!(oid.len(), 64);
        let mut budget = SnapshotHashBudget::new();
        let (session, identity) = SnapshotIndexBlobSession::spawn(&repo)
            .await
            .unwrap()
            .read_entry(
                &entry(oid, SnapshotIndexMode::Regular { executable: true }),
                &mut budget,
            )
            .await
            .unwrap();
        assert_eq!(
            identity,
            CandidateObjectIdentity::RegularFile {
                content_sha256: format!("{:x}", Sha256::digest(bytes)),
                size_bytes: bytes.len() as u64,
                executable: true,
            }
        );
        session.finish().await.unwrap();
        std::fs::remove_dir_all(repo).unwrap();
    }
}
