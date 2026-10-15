use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
#[cfg(windows)]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
#[cfg(windows)]
use tokio::time::{Instant, sleep};
use uuid::Uuid;

use crate::config::{self, Session};
#[cfg(unix)]
use crate::secure_fs;

trait SessionIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> SessionIo for T {}
type SessionStream = Box<dyn SessionIo>;

#[cfg(unix)]
#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "Approval IPC is reached through the process entrypoint, not the in-process test harness."
    )
)]
struct SessionListener(UnixListener);
#[cfg(windows)]
struct SessionListener {
    path: PathBuf,
    server: NamedPipeServer,
}

#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "Approval IPC is reached through the process entrypoint, not the in-process test harness."
    )
)]
impl SessionListener {
    async fn accept(&mut self) -> Result<SessionStream> {
        #[cfg(unix)]
        {
            return Ok(Box::new(self.0.accept().await?.0));
        }
        #[cfg(windows)]
        {
            // Keep the connecting instance in listener state while awaiting so
            // cancelling this future from `tokio::select!` cannot drop it.
            self.server.connect().await?;
            let server = std::mem::replace(&mut self.server, new_pipe(&self.path, false)?);
            Ok(Box::new(server))
        }
    }
}

#[cfg(windows)]
fn new_pipe(path: &Path, first: bool) -> Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options.first_pipe_instance(first);
    // Keep PIPE_REJECT_REMOTE_CLIENTS (Tokio default is already true).
    options.reject_remote_clients(true);
    // Supply an explicit current-user-only security descriptor. Tokio's default
    // `create` uses a null SECURITY_ATTRIBUTES pointer, which applies the Windows
    // default named-pipe DACL (broad principals such as Everyone/Anonymous).
    let security = crate::pipe_security::CurrentUserPipeSecurity::new()
        .context("failed to build current-user-only pipe security descriptor")?;
    let attrs = security.attributes();
    // SAFETY: `attrs` is a valid SECURITY_ATTRIBUTES whose lpSecurityDescriptor
    // points at an absolute descriptor owned by `security`, which outlives this call.
    // CreateNamedPipeW copies the descriptor into the object.
    unsafe { Ok(options.create_with_security_attributes_raw(path, &attrs as *const _ as *mut _)?) }
}

#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "Approval IPC is reached through the process entrypoint, not the in-process test harness."
    )
)]
async fn connect(path: &Path) -> Result<SessionStream> {
    #[cfg(unix)]
    {
        Ok(Box::new(UnixStream::connect(path).await?))
    }
    #[cfg(windows)]
    {
        const ERROR_PIPE_BUSY: i32 = 231;
        const RETRY_DELAY: Duration = Duration::from_millis(10);
        const RETRY_TIMEOUT: Duration = Duration::from_secs(5);

        let deadline = Instant::now() + RETRY_TIMEOUT;
        loop {
            match ClientOptions::new().open(path) {
                Ok(client) => return Ok(Box::new(client)),
                Err(error)
                    if error.raw_os_error() == Some(ERROR_PIPE_BUSY)
                        && Instant::now() < deadline =>
                {
                    sleep(RETRY_DELAY).await;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

#[cfg(unix)]
#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "Approval IPC is reached through the process entrypoint, not the in-process test harness."
    )
)]
fn bind_listener(path: &Path) -> Result<SessionListener> {
    Ok(SessionListener(UnixListener::bind(path)?))
}

#[cfg(unix)]
async fn bind_unix_session_listener(path: &Path) -> Result<SessionListener> {
    let state_dir = path.parent().context("session socket has no parent")?;
    secure_fs::ensure_private_directory(state_dir, None)?;
    remove_stale_socket(path).await?;
    let listener = bind_listener(path)?;
    secure_fs::secure_socket(path)?;
    Ok(listener)
}

#[cfg(windows)]
#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "Approval IPC is reached through the process entrypoint, not the in-process test harness."
    )
)]
fn bind_listener(path: &Path) -> Result<SessionListener> {
    Ok(SessionListener {
        path: path.to_owned(),
        server: new_pipe(path, true)?,
    })
}

#[derive(Serialize, Deserialize)]
pub struct Request {
    pub id: Uuid,
    pub operation: String,
    pub detail: String,
    pub cwd: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "Approval IPC is reached through the process entrypoint, not the in-process test harness."
    )
)]
enum Message {
    Approval {
        request: Request,
    },
    Activity {
        title: String,
        detail: Option<String>,
    },
}

/// One approval waiting for the operator's answer, with its enqueue time.
///
/// The enqueue time is what lets the session UI drop requests whose caller is
/// already gone: `request()` fails closed after `APPROVAL_RESPONSE_TIMEOUT`,
/// so any entry older than that has no live client. Answering it would grant
/// authority to nobody while consuming the operator's keystroke, and holding
/// it would head-block every newer approval behind a corpse.
struct PendingApproval {
    request: Request,
    stream: SessionStream,
    enqueued: std::time::Instant,
}

/// Read one session message with an explicit deadline.
///
/// A client that connects and then sends a partial frame, nothing, or
/// disconnects must surface as an error quickly — never as an indefinite
/// stall of the single accept/read sequence that every later connection waits
/// behind, and never as a dead session UI (callers drop the connection and
/// continue instead of propagating with `?`).
async fn read_session_message_with_timeout(
    stream: &mut SessionStream,
    timeout: Duration,
) -> Result<Message> {
    let mut line = String::new();
    let read = tokio::time::timeout(timeout, BufReader::new(&mut *stream).read_line(&mut line))
        .await
        .map_err(|_| anyhow::anyhow!("session message read timed out"))??;
    if read == 0 {
        anyhow::bail!("session connection closed before a complete message");
    }
    serde_json::from_str(&line).context("invalid session message")
}

/// Production reader: bounded by `APPROVAL_IO_TIMEOUT`, the machine-IPC
/// budget. Human latency is handled by the pending queue, not by this read.
async fn read_session_message(stream: &mut SessionStream) -> Result<Message> {
    read_session_message_with_timeout(stream, crate::resource_limits::APPROVAL_IO_TIMEOUT).await
}

/// Answer one pending approval on a best-effort basis.
///
/// The operator already made this decision (or yolo mode made it); a dead
/// stream means the caller is gone and there is nobody to grant anything to.
/// Failing here would propagate through `handle_input` and kill the whole
/// session UI, taking every later approval down with one corpse.
/// Errors are therefore swallowed after a bounded write attempt.
async fn answer(stream: &mut SessionStream, verdict: &[u8]) {
    let _ = tokio::time::timeout(
        crate::resource_limits::APPROVAL_IO_TIMEOUT,
        stream.write_all(verdict),
    )
    .await;
}

/// Drop queued approvals whose caller already timed out.
///
/// Entries older than `APPROVAL_RESPONSE_TIMEOUT` have no live client:
/// `request()` fails closed at exactly that age. Eviction grants nothing and
/// denies nothing — it only frees the operator from clearing corpses one
/// keystroke at a time so later approvals are processed correctly. Only the
/// operation name and id are echoed (both already operator-visible); the
/// detail is not repeated.
fn evict_expired(pending: &mut VecDeque<PendingApproval>) {
    evict_expired_at(pending, std::time::Instant::now());
}

/// Clock-injected eviction so tests never depend on machine uptime.
///
/// Asking a test to fabricate an 11-minute-old `Instant` via `checked_sub`
/// fails on machines booted minutes ago (fresh CI runners): the subtraction
/// underflows to `None` and the entry looks brand new. Passing `now` in keeps
/// the test deterministic on any uptime.
fn evict_expired_at(pending: &mut VecDeque<PendingApproval>, now: std::time::Instant) {
    let deadline = crate::resource_limits::APPROVAL_RESPONSE_TIMEOUT;
    while pending
        .front()
        .is_some_and(|entry| now.saturating_duration_since(entry.enqueued) > deadline)
    {
        if let Some(entry) = pending.pop_front() {
            eprintln!(
                "[expired] {} {} (caller timed out; no approval granted)",
                entry.request.operation, entry.request.id
            );
        }
    }
}

#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "Approval IPC is reached through the process entrypoint, not the in-process test harness."
    )
)]
pub async fn request(
    session_id: &str,
    operation: &str,
    detail: String,
    cwd: PathBuf,
) -> Result<bool> {
    let request = Request {
        id: Uuid::new_v4(),
        operation: operation.to_owned(),
        detail,
        cwd,
    };
    let path = config::socket_path(session_id)?;

    // Machine IPC steps are bounded by APPROVAL_IO_TIMEOUT: a dead pipe must
    // surface as an error quickly, never as an indefinite stall of the calling
    // dispatch (which holds its execution permit while waiting).
    let io_timeout = crate::resource_limits::APPROVAL_IO_TIMEOUT;
    let mut stream = tokio::time::timeout(io_timeout, connect(&path))
        .await
        .map_err(|_| anyhow::anyhow!("approval IPC connect timed out"))?
        .with_context(|| format!("session {session_id} is not running; run `local-mcp start`"))?;
    let payload = serde_json::to_vec(&Message::Approval { request })?;
    tokio::time::timeout(io_timeout, stream.write_all(&payload))
        .await
        .map_err(|_| anyhow::anyhow!("approval IPC write timed out"))??;
    tokio::time::timeout(io_timeout, stream.write_all(b"\n"))
        .await
        .map_err(|_| anyhow::anyhow!("approval IPC write timed out"))??;
    tokio::time::timeout(io_timeout, stream.shutdown())
        .await
        .map_err(|_| anyhow::anyhow!("approval IPC shutdown timed out"))??;

    // Human latency is generous (APPROVAL_RESPONSE_TIMEOUT) but still bounded:
    // when the session UI disappears, the wait must fail closed (error, never
    // allow) instead of leaking the dispatch and its permit forever.
    let mut response = String::new();
    tokio::time::timeout(
        crate::resource_limits::APPROVAL_RESPONSE_TIMEOUT,
        BufReader::new(stream).read_line(&mut response),
    )
    .await
    .map_err(|_| anyhow::anyhow!("approval response timed out"))??;
    match response.trim() {
        "allow" => Ok(true),
        "deny" => Ok(false),
        value => anyhow::bail!("invalid response from session: {value:?}"),
    }
}

#[cfg(all(test, windows))]
async fn accept_test_approval_connection(
    listener: &mut SessionListener,
    ready_sender: tokio::sync::oneshot::Sender<std::result::Result<(), String>>,
) -> Result<SessionStream> {
    let accept = listener.accept();
    tokio::pin!(accept);
    let mut ready_sender = Some(ready_sender);
    std::future::poll_fn(|context| {
        let result = accept.as_mut().poll(context);
        if let Some(sender) = ready_sender.take() {
            let readiness = match &result {
                std::task::Poll::Ready(Err(error)) => Err(error.to_string()),
                std::task::Poll::Pending | std::task::Poll::Ready(Ok(_)) => Ok(()),
            };
            let _ = sender.send(readiness);
        }
        result
    })
    .await
}

#[cfg(all(test, windows))]
#[allow(
    dead_code,
    reason = "Windows-only test responder is selected by a cfg-gated integration path."
)]
pub(crate) async fn spawn_test_approval_responder(
    session_id: &str,
    expected_cwd: &Path,
) -> Result<(
    tokio::task::JoinHandle<Result<()>>,
    tokio::sync::oneshot::Sender<()>,
)> {
    let path = config::socket_path(session_id)?;
    let expected_cwd = std::fs::canonicalize(expected_cwd)?;
    let mut listener = bind_listener(&path)?;
    let (ready_sender, ready_receiver) = tokio::sync::oneshot::channel();
    let (probe_sender, probe_receiver) = tokio::sync::oneshot::channel();
    let (shutdown_sender, mut shutdown_receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut stream = accept_test_approval_connection(&mut listener, ready_sender).await?;
        let mut probe_ready = Some(probe_sender);
        loop {
            let mut line = String::new();
            let bytes_read = BufReader::new(&mut stream).read_line(&mut line).await?;
            if bytes_read == 0 {
                if let Some(sender) = probe_ready.take() {
                    stream = accept_test_approval_connection(&mut listener, sender).await?;
                } else {
                    stream = listener.accept().await?;
                }
                continue;
            }
            let message: serde_json::Value = serde_json::from_str(&line)?;
            match message["type"].as_str() {
                Some("activity") => {}
                Some("approval") => {
                    anyhow::ensure!(message["request"]["operation"] == "start_command");
                    let requested_cwd = message["request"]["cwd"]
                        .as_str()
                        .context("approval request omitted cwd")?;
                    anyhow::ensure!(
                        std::fs::canonicalize(requested_cwd)? == expected_cwd,
                        "Verifier command approval cwd did not match the managed candidate"
                    );
                    stream.write_all(b"allow\n").await?;
                }
                other => anyhow::bail!("unexpected test approval message type: {other:?}"),
            }
            stream = tokio::select! {
                _ = &mut shutdown_receiver => return Ok(()),
                accepted = listener.accept() => accepted?,
            };
        }
    });
    match ready_receiver.await {
        Ok(Ok(())) => {}
        Ok(Err(detail)) => {
            let _ = task.await;
            anyhow::bail!("test approval listener failed before readiness: {detail}")
        }
        Err(error) => {
            let _ = task.await;
            anyhow::bail!("test approval listener stopped before readiness: {error}")
        }
    }

    // Wait until the server accepts and drains a disposable connection, then
    // arms the replacement instance that will receive the real approval.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match ClientOptions::new().open(&path) {
            Ok(probe) => {
                drop(probe);
                break;
            }
            Err(error)
                if matches!(error.raw_os_error(), Some(2) | Some(231))
                    && Instant::now() < deadline =>
            {
                sleep(Duration::from_millis(10)).await;
            }
            Err(error) => {
                task.abort();
                anyhow::bail!("test approval pipe probe failed: {error}")
            }
        }
    }
    match probe_receiver.await {
        Ok(Ok(())) => Ok((task, shutdown_sender)),
        Ok(Err(detail)) => {
            let _ = task.await;
            anyhow::bail!("test approval listener failed after probe: {detail}")
        }
        Err(error) => {
            task.abort();
            anyhow::bail!("test approval pipe probe was not consumed: {error}")
        }
    }
}

#[cfg(all(test, not(windows)))]
#[allow(
    dead_code,
    reason = "Non-Windows test responder is a no-op for the shared integration fixture."
)]
pub(crate) async fn spawn_test_approval_responder(
    _session_id: &str,
    _expected_cwd: &Path,
) -> Result<(
    tokio::task::JoinHandle<Result<()>>,
    tokio::sync::oneshot::Sender<()>,
)> {
    let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _ = shutdown_receiver.await;
        Ok(())
    });
    Ok((task, shutdown_sender))
}

/// Sends a one-way activity update to the `start` screen. Activity reporting is
/// deliberately best-effort: an MCP operation must not fail just because its UI
/// was closed between loading the session and completing the operation.
#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "Approval IPC is reached through the process entrypoint, not the in-process test harness."
    )
)]
pub async fn activity(session_id: &str, title: impl Into<String>, detail: Option<String>) {
    let Ok(path) = config::socket_path(session_id) else {
        return;
    };
    let Ok(mut stream) = connect(&path).await else {
        return;
    };
    let message = Message::Activity {
        title: title.into(),
        detail,
    };
    let Ok(bytes) = serde_json::to_vec(&message) else {
        return;
    };
    let _ = stream.write_all(&bytes).await;
    let _ = stream.write_all(b"\n").await;
    let _ = stream.shutdown().await;
}

#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "Approval IPC is reached through the process entrypoint, not the in-process test harness."
    )
)]
pub async fn start(session_id: Option<&str>) -> Result<()> {
    let mut session = config::create_session(&std::env::current_dir()?, session_id).await?;
    let path = config::socket_path(&session.id)?;
    #[cfg(unix)]
    let mut listener = bind_unix_session_listener(&path)
        .await
        .with_context(|| format!("failed to listen at {}", path.display()))?;
    #[cfg(windows)]
    let mut listener = {
        remove_stale_socket(&path).await?;
        bind_listener(&path).with_context(|| format!("failed to listen at {}", path.display()))?
    };
    eprintln!(
        "local-mcp session: {}\ncwd: {}\n\
         Give this session ID to the agent so it can include it in local-mcp tool calls.\n\
         Commands: /permission ask|yolo|allow <directory>|revoke <directory>|list|status\n\
         Press Ctrl-C to stop.",
        session.id,
        session.cwd.display()
    );

    let mut input = BufReader::new(tokio::io::stdin()).lines();
    let mut pending = VecDeque::<PendingApproval>::new();
    let mut yolo = false;
    loop {
        tokio::select! {
            connection = listener.accept() => {
                let mut stream = connection?;
                match read_session_message(&mut stream).await {
                    Err(error) => {
                        // Partial frame, silent client, disconnect, or garbage:
                        // drop this connection and keep serving. Propagating
                        // with `?` here killed the whole session UI on one bad
                        // client, taking every later approval down with it.
                        eprintln!("Ignoring session connection: {error:#}");
                        continue;
                    }
                    Ok(Message::Activity { title, detail }) => {
                        show_activity(&title, detail.as_deref())
                    }
                    Ok(Message::Approval { request }) if yolo => {
                        eprintln!("[yolo] allowing {}: {}", request.operation, request.detail);
                        answer(&mut stream, b"allow\n").await;
                    }
                    Ok(Message::Approval { request }) => {
                        evict_expired(&mut pending);
                        show_request(&request)?;
                        pending.push_back(PendingApproval {
                            request,
                            stream,
                            enqueued: std::time::Instant::now(),
                        });
                    }
                }
            }
            line = input.next_line() => {
                let Some(line) = line? else { anyhow::bail!("session input closed") };
                evict_expired(&mut pending);
                handle_input(line.trim(), &mut session, &mut yolo, &mut pending).await?;
            }
        }
    }
}

#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "Approval IPC is reached through the process entrypoint, not the in-process test harness."
    )
)]
fn show_activity(title: &str, detail: Option<&str>) {
    eprintln!("\n• {title}");
    if let Some(detail) = detail.filter(|value| !value.is_empty()) {
        for line in detail.lines() {
            eprintln!("  {line}");
        }
    }
}

#[cfg(unix)]
#[cfg_attr(
    test,
    allow(
        dead_code,
        reason = "Approval IPC is reached through the process entrypoint, not the in-process test harness."
    )
)]
async fn remove_stale_socket(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() || metadata.file_type().is_symlink() => {
            tokio::fs::remove_file(path)
                .await
                .context("failed to remove stale session socket")
        }
        Ok(_) => anyhow::bail!("unexpected non-socket path at {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("failed to inspect stale session socket"),
    }
}

#[cfg(windows)]
async fn remove_stale_socket(_path: &Path) -> Result<()> {
    Ok(())
}

async fn handle_input(
    input: &str,
    session: &mut Session,
    yolo: &mut bool,
    pending: &mut VecDeque<PendingApproval>,
) -> Result<()> {
    match input {
        "/permissions yolo" | "/permission yolo" => {
            *yolo = true;
            eprintln!(
                "WARNING: Permissions yolo allows every approval-gated host-native call for this session only; restart resets it. MCP callers cannot enable yolo."
            );
            while let Some(mut entry) = pending.pop_front() {
                eprintln!(
                    "[yolo] allowing {}: {}",
                    entry.request.operation, entry.request.detail
                );
                answer(&mut entry.stream, b"allow\n").await;
            }
        }
        "/permissions ask" | "/permission ask" => {
            *yolo = false;
            eprintln!("Permissions: ask");
        }
        "y" | "Y" | "yes" | "YES" if !pending.is_empty() => {
            let mut entry = pending.pop_front().unwrap();
            answer(&mut entry.stream, b"allow\n").await;
            show_next(pending)?;
        }
        "n" | "N" | "no" | "NO" if !pending.is_empty() => {
            let mut entry = pending.pop_front().unwrap();
            answer(&mut entry.stream, b"deny\n").await;
            show_next(pending)?;
        }
        "/permission list" | "/permissions list" => show_permissions(session),
        "/permission status" | "/permissions status" => {
            eprintln!("Permissions: {}", if *yolo { "yolo" } else { "ask" });
            show_permissions(session);
        }
        command if permission_arg(command, "allow").is_some() => {
            let directory = config::canonical_directory(
                PathBuf::from(permission_arg(command, "allow").unwrap()).as_path(),
            )?;
            if !session.permitted_directories.contains(&directory) {
                session.permitted_directories.push(directory.clone());
                session.permitted_directories.sort();
                config::save_session(session).await?;
            }
            eprintln!("Allowed sandbox root: {}", directory.display());
        }
        command if permission_arg(command, "revoke").is_some() => {
            let directory = config::canonical_directory(
                PathBuf::from(permission_arg(command, "revoke").unwrap()).as_path(),
            )?;
            if directory == session.cwd {
                eprintln!("Cannot revoke the session cwd");
            } else {
                session
                    .permitted_directories
                    .retain(|item| item != &directory);
                config::save_session(session).await?;
                eprintln!("Revoked sandbox root: {}", directory.display());
            }
        }
        "/permission" | "/permissions" | "/permission help" | "/permissions help" => {
            eprintln!("/permission ask|yolo|allow <directory>|revoke <directory>|list|status");
        }
        "" => {}
        command if !pending.is_empty() => {
            let mut entry = pending.pop_front().unwrap();
            answer(&mut entry.stream, b"deny\n").await;
            eprintln!("Denied request (unrecognized response: {command})");
            show_next(pending)?;
        }
        command => eprintln!("Unknown command: {command}"),
    }
    Ok(())
}

fn permission_arg<'a>(command: &'a str, action: &str) -> Option<&'a str> {
    ["/permission", "/permissions"]
        .into_iter()
        .find_map(|prefix| {
            command
                .strip_prefix(&format!("{prefix} {action} "))
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
}

fn show_permissions(session: &Session) {
    eprintln!("Sandbox roots:");
    for path in &session.permitted_directories {
        eprintln!("  {}", path.display());
    }
}

fn show_request(request: &Request) -> Result<()> {
    eprintln!(
        "\n[{}] {}\ncwd: {}\n{}",
        request.id,
        request.operation,
        request.cwd.display(),
        request.detail
    );
    eprint!("Allow without sandbox? [y/N] ");
    std::io::stderr().flush()?;
    Ok(())
}

fn show_next(pending: &VecDeque<PendingApproval>) -> Result<()> {
    if let Some(entry) = pending.front() {
        show_request(&entry.request)?;
    }
    Ok(())
}

#[cfg(test)]
mod approval_stability_tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    fn test_session() -> Session {
        Session {
            id: format!("stability-{}", Uuid::new_v4()),
            cwd: std::env::temp_dir(),
            permitted_directories: vec![std::env::temp_dir()],
        }
    }

    fn approval_request(operation: &str) -> Request {
        Request {
            id: Uuid::new_v4(),
            operation: operation.to_owned(),
            detail: "stability test".to_owned(),
            cwd: std::env::temp_dir(),
        }
    }

    fn pending_entry(stream: SessionStream) -> PendingApproval {
        pending_entry_at(stream, std::time::Instant::now())
    }

    fn pending_entry_at(stream: SessionStream, enqueued: std::time::Instant) -> PendingApproval {
        PendingApproval {
            request: approval_request("without_sandbox"),
            stream,
            enqueued,
        }
    }

    fn approval_frame(operation: &str) -> Vec<u8> {
        let request = approval_request(operation);
        let mut bytes = serde_json::to_vec(&Message::Approval { request }).unwrap();
        bytes.push(b'\n');
        bytes
    }

    #[tokio::test]
    async fn session_reader_accepts_a_valid_approval_frame() -> Result<()> {
        let (mut client, server) = tokio::io::duplex(4096);
        client.write_all(&approval_frame("execute")).await?;
        let mut stream: SessionStream = Box::new(server);
        let message = tokio::time::timeout(
            Duration::from_secs(5),
            read_session_message_with_timeout(&mut stream, Duration::from_secs(2)),
        )
        .await
        .expect("valid frame must not hang")?;
        assert!(
            matches!(message, Message::Approval { .. }),
            "valid approval frame must parse"
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_reader_rejects_garbage_without_hanging_or_killing_the_ui() -> Result<()> {
        let (mut client, server) = tokio::io::duplex(4096);
        client.write_all(b"not json\n").await?;
        let mut stream: SessionStream = Box::new(server);
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            read_session_message_with_timeout(&mut stream, Duration::from_secs(2)),
        )
        .await
        .expect("garbage must not hang the reader");
        assert!(
            outcome.is_err(),
            "garbage must be an error the accept loop drops instead of a `?` that kills the UI"
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_reader_times_out_on_a_partial_frame() -> Result<()> {
        // A client that connects and sends bytes without a newline held the
        // old accept/read sequence forever, head-blocking every later
        // connection behind it.
        let (mut client, server) = tokio::io::duplex(4096);
        client.write_all(b"{\"type\":\"approval\"").await?;
        let mut stream: SessionStream = Box::new(server);
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            read_session_message_with_timeout(&mut stream, Duration::from_millis(200)),
        )
        .await
        .expect("partial frame must not hang the test");
        assert!(
            outcome.is_err(),
            "partial frame must time out instead of blocking later connections"
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_reader_fails_closed_on_disconnect() -> Result<()> {
        let (client, server) = tokio::io::duplex(4096);
        drop(client);
        let mut stream: SessionStream = Box::new(server);
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            read_session_message_with_timeout(&mut stream, Duration::from_secs(2)),
        )
        .await
        .expect("disconnect must not hang the reader");
        assert!(
            outcome.is_err(),
            "disconnect before a complete message must be an error"
        );
        Ok(())
    }

    #[tokio::test]
    async fn answering_a_dead_approval_does_not_kill_the_session_ui() -> Result<()> {
        // The caller timed out and is gone; the operator's answer must still
        // succeed (best-effort) so the UI survives for later approvals. The
        // old `write_all(...).await?` returned Err here and `start()` exited.
        let (dead_client, dead_server) = tokio::io::duplex(64);
        drop(dead_client);
        let mut session = test_session();
        let mut yolo = false;
        let mut pending = VecDeque::from([pending_entry(Box::new(dead_server))]);
        let outcome = tokio::time::timeout(
            Duration::from_secs(15),
            handle_input("y", &mut session, &mut yolo, &mut pending),
        )
        .await
        .expect("dead-stream answer must not hang");
        assert!(
            outcome.is_ok(),
            "answering a stale approval must not kill the UI"
        );
        assert!(pending.is_empty(), "answered entry must be popped");
        Ok(())
    }

    #[tokio::test]
    async fn expired_approvals_are_evicted_and_later_ones_are_answered() -> Result<()> {
        // A request older than APPROVAL_RESPONSE_TIMEOUT has no live caller
        // (request() fails closed at exactly that age). It must be evicted so
        // the operator's next keystroke reaches a live approval.
        // Fresh entries must survive the real clock: eviction only applies
        // past APPROVAL_RESPONSE_TIMEOUT, never to live approvals.
        let mut fresh = VecDeque::from([
            pending_entry(Box::new(tokio::io::duplex(64).1)),
            pending_entry(Box::new(tokio::io::duplex(64).1)),
        ]);
        evict_expired(&mut fresh);
        assert_eq!(fresh.len(), 2, "live approvals must not be evicted");
        drop(fresh);
        // Age the queue deterministically with forward-only clock math (safe
        // on any machine uptime): at the eviction point the stale entry is 11
        // minutes old while the live one is 30 seconds old.
        let t0 = std::time::Instant::now();
        let (stale_client, stale_server) = tokio::io::duplex(64);
        drop(stale_client);
        let (mut live_client, live_server) = tokio::io::duplex(64);
        let stale_at = t0;
        let live_at = t0
            .checked_add(Duration::from_secs(10 * 60 + 30))
            .expect("test clock must advance");
        let now = t0
            .checked_add(Duration::from_secs(11 * 60))
            .expect("test clock must advance");
        let mut pending = VecDeque::from([
            pending_entry_at(Box::new(stale_server), stale_at),
            pending_entry_at(Box::new(live_server), live_at),
        ]);
        evict_expired_at(&mut pending, now);
        assert_eq!(pending.len(), 1, "only the live approval must remain");
        let mut session = test_session();
        let mut yolo = false;
        tokio::time::timeout(
            Duration::from_secs(15),
            handle_input("y", &mut session, &mut yolo, &mut pending),
        )
        .await
        .expect("live answer must not hang")?;
        let mut line = String::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::io::BufReader::new(&mut live_client).read_line(&mut line),
        )
        .await
        .expect("live client must be answered promptly")?;
        assert_eq!(line, "allow\n", "live approval must receive the answer");
        assert!(pending.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn yolo_drain_survives_dead_streams() -> Result<()> {
        let (dead_client, dead_server) = tokio::io::duplex(64);
        drop(dead_client);
        let mut session = test_session();
        let mut yolo = false;
        let mut pending = VecDeque::from([pending_entry(Box::new(dead_server))]);
        let outcome = tokio::time::timeout(
            Duration::from_secs(20),
            handle_input("/permission yolo", &mut session, &mut yolo, &mut pending),
        )
        .await
        .expect("yolo drain must not hang");
        assert!(
            outcome.is_ok(),
            "yolo drain over dead streams must not kill the UI"
        );
        assert!(yolo);
        assert!(pending.is_empty());
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn approval_ipc_allow_deny_invalid_and_missing_are_distinct() -> Result<()> {
        let id = format!("ipc-approval-{}", Uuid::new_v4());
        let path = config::socket_path(&id)?;
        let cwd = std::env::temp_dir().join(format!("local-mcp-approval-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&cwd)?;

        let mut listener = bind_unix_session_listener(&path).await?;
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut line)
                .await
                .unwrap();
            assert!(serde_json::from_str::<Message>(&line).is_ok());
            stream.write_all(b"maybe\n").await.unwrap();
        });
        assert!(
            request(&id, "test-op", "detail".into(), cwd.clone())
                .await
                .is_err()
        );
        server.await?;

        let mut listener = bind_unix_session_listener(&path).await?;
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut line)
                .await
                .unwrap();
            stream.write_all(b"deny\n").await.unwrap();
        });
        assert!(!request(&id, "test-op", "detail".into(), cwd.clone()).await?);
        server.await?;

        let mut listener = bind_unix_session_listener(&path).await?;
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut line)
                .await
                .unwrap();
            stream.write_all(b"allow\n").await.unwrap();
        });
        assert!(request(&id, "test-op", "detail".into(), cwd.clone()).await?);
        server.await?;

        let missing_id = format!("ipc-missing-{}", Uuid::new_v4());
        assert!(
            request(&missing_id, "test-op", "detail".into(), cwd.clone())
                .await
                .is_err()
        );
        remove_stale_socket(&path.to_path_buf()).await?;
        std::fs::remove_dir_all(cwd)?;
        Ok(())
    }

    #[test]
    fn approval_pipe_payload_cannot_set_session_yolo_state() {
        let approval = r#"{"type":"approval","request":{"id":"00000000-0000-4000-8000-000000000001","operation":"execute","detail":"argv","cwd":"/tmp"},"yolo":true}"#;
        let parsed: Message = serde_json::from_str(approval).unwrap();
        assert!(matches!(parsed, Message::Approval { .. }));
        let activity = r#"{"type":"activity","title":"t","yolo":true}"#;
        let parsed: Message = serde_json::from_str(activity).unwrap();
        assert!(matches!(parsed, Message::Activity { .. }));
    }

    #[tokio::test]
    async fn stale_regular_file_is_not_removed() -> Result<()> {
        let id = format!("ipc-regular-{}", Uuid::new_v4());
        let path = config::socket_path(&id)?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, b"keep")?;
        assert!(remove_stale_socket(&path).await.is_err());
        assert_eq!(std::fs::read(&path)?, b"keep");
        std::fs::remove_file(path)?;
        Ok(())
    }

    #[tokio::test]
    async fn session_socket_uses_private_modes_and_unlinks_stale_symlinks() -> Result<()> {
        let id = format!("ipc-{}", Uuid::new_v4());
        let path = config::socket_path(&id)?;
        let outside = std::env::temp_dir().join(format!("local-mcp-ipc-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&outside)?;
        let sentinel = outside.join("sentinel");
        std::fs::write(&sentinel, "unchanged")?;

        let state_dir = path.parent().context("session socket has no parent")?;
        tokio::fs::create_dir_all(state_dir).await?;
        std::os::unix::fs::symlink(&sentinel, &path)?;
        let listener = bind_unix_session_listener(&path).await?;

        let directory = std::fs::metadata(state_dir)?;
        assert_eq!(directory.permissions().mode() & 0o777, 0o700);
        assert_eq!(directory.uid(), unsafe { libc::geteuid() });
        let socket = std::fs::symlink_metadata(&path)?;
        assert!(socket.file_type().is_socket());
        assert_eq!(socket.permissions().mode() & 0o777, 0o600);
        assert_eq!(socket.uid(), unsafe { libc::geteuid() });
        assert_eq!(std::fs::read_to_string(&sentinel)?, "unchanged");

        drop(listener);
        remove_stale_socket(&path).await?;
        assert!(std::fs::symlink_metadata(&path).is_err());
        assert_eq!(std::fs::read_to_string(&sentinel)?, "unchanged");
        std::fs::remove_dir_all(outside)?;
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use tokio::time::timeout;

    fn pipe_path() -> PathBuf {
        PathBuf::from(format!(r"\\.\pipe\local-mcp-test-{}", Uuid::new_v4()))
    }

    #[tokio::test]
    async fn cancelling_accept_preserves_the_pipe_server() -> Result<()> {
        let path = pipe_path();
        let mut listener = bind_listener(&path)?;

        assert!(
            timeout(Duration::from_millis(10), listener.accept())
                .await
                .is_err()
        );
        let _client = ClientOptions::new().open(&path)?;
        let _server = timeout(Duration::from_secs(1), listener.accept()).await??;

        Ok(())
    }

    #[tokio::test]
    async fn connecting_retries_while_all_pipe_instances_are_busy() -> Result<()> {
        let path = pipe_path();
        let mut listener = bind_listener(&path)?;
        let _first_client = ClientOptions::new().open(&path)?;

        let client_path = path.clone();
        let waiting_client = tokio::spawn(async move { connect(&client_path).await });
        sleep(Duration::from_millis(50)).await;
        let _first_server = listener.accept().await?;
        let _second_client = timeout(Duration::from_secs(1), waiting_client).await???;

        Ok(())
    }

    #[tokio::test]
    async fn first_pipe_instance_rejects_double_bind() -> Result<()> {
        let path = pipe_path();
        let _listener = bind_listener(&path)?;
        assert!(
            bind_listener(&path).is_err(),
            "second first_pipe_instance bind must fail while the session owns the pipe"
        );
        Ok(())
    }

    #[tokio::test]
    async fn named_pipe_current_user_only_acl() -> Result<()> {
        use std::os::windows::io::AsRawHandle;

        let path = pipe_path();
        let listener = bind_listener(&path)?;
        let handle = listener.server.as_raw_handle() as *mut _;
        let (owner, aces) = crate::pipe_security::inspect_handle_security(handle)?;
        let user = crate::pipe_security::current_user_sid()?;
        crate::pipe_security::assert_current_user_only(&owner, &aces, &user)?;

        // Everyone and Anonymous must not appear as any ACE principal.
        for ace in &aces {
            assert!(
                ace.sid_string != "S-1-1-0",
                "Everyone must not be granted pipe access"
            );
            assert!(
                ace.sid_string != "S-1-5-7",
                "Anonymous must not be granted pipe access"
            );
            assert!(
                !crate::pipe_security::is_broad_principal(&ace.sid_string),
                "broad principal {} must not be granted pipe access",
                ace.sid_string
            );
        }

        // Mechanical evidence (categories only; no personal username).
        let owner_category = if owner == user {
            "current-user"
        } else {
            "other"
        };
        eprintln!(
            "ACL_EVIDENCE owner={owner_category} ace_count={}",
            aces.len()
        );
        for (index, ace) in aces.iter().enumerate() {
            let principal = if ace.sid_string == user {
                "current-user"
            } else if crate::pipe_security::is_broad_principal(&ace.sid_string) {
                "broad-forbidden"
            } else if ace.sid_string.starts_with("S-1-5-21-") {
                "unrelated-account"
            } else {
                "other-well-known"
            };
            eprintln!(
                "ACL_EVIDENCE ace[{index}] principal={principal} allowed={} mask={:#x}",
                ace.allowed, ace.mask
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn named_pipe_same_user_can_connect() -> Result<()> {
        let path = pipe_path();
        let mut listener = bind_listener(&path)?;
        let client = ClientOptions::new().open(&path)?;
        let _server = timeout(Duration::from_secs(1), listener.accept()).await??;
        drop(client);
        Ok(())
    }

    #[tokio::test]
    async fn request_fails_closed_on_invalid_and_deny_responses() -> Result<()> {
        let id = format!("approval-fail-{}", Uuid::new_v4());
        let path = config::socket_path(&id)?;
        let cwd = std::env::temp_dir().join(format!("local-mcp-approval-fail-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&cwd)?;

        // Invalid response must be an error, never an implicit allow.
        let mut listener = bind_listener(&path)?;
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut line)
                .await
                .unwrap();
            stream.write_all(b"maybe\n").await.unwrap();
        });
        let invalid = request(&id, "test-op", "detail".into(), cwd.clone()).await;
        assert!(
            invalid.is_err(),
            "invalid approval response must fail closed"
        );
        server.await.unwrap();

        // Deny is an explicit false, not an error and not an allow.
        let mut listener = bind_listener(&path)?;
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut line)
                .await
                .unwrap();
            stream.write_all(b"deny\n").await.unwrap();
        });
        let denied = request(&id, "test-op", "detail".into(), cwd.clone()).await?;
        assert!(!denied, "deny must return false");
        server.await.unwrap();

        // Allow is the only true success path.
        let mut listener = bind_listener(&path)?;
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut line)
                .await
                .unwrap();
            stream.write_all(b"allow\n").await.unwrap();
        });
        let allowed = request(&id, "test-op", "detail".into(), cwd.clone()).await?;
        assert!(allowed, "allow must return true");
        server.await.unwrap();

        // Missing session pipe must fail closed before any host-native work.
        let missing_id = format!("approval-missing-{}", Uuid::new_v4());
        let missing = request(&missing_id, "test-op", "detail".into(), cwd.clone()).await;
        assert!(missing.is_err());

        let _ = std::fs::remove_dir_all(&cwd);
        Ok(())
    }

    #[test]
    fn yolo_cannot_be_enabled_by_pipe_messages() {
        // Approval/Activity are the only IPC variants. yolo is stdin-local state
        // in `start()` and is not deserializable from the pipe protocol.
        let approval = r#"{"type":"approval","request":{"id":"7418eda5-fd07-4e00-ace5-c1ece2f68a02","operation":"execute","detail":"argv","cwd":"C:\\tmp"},"yolo":true}"#;
        let parsed: Message = serde_json::from_str(approval).unwrap();
        assert!(matches!(parsed, Message::Approval { .. }));
        let activity = r#"{"type":"activity","title":"t","yolo":true}"#;
        let parsed: Message = serde_json::from_str(activity).unwrap();
        assert!(matches!(parsed, Message::Activity { .. }));
    }
}
