use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(windows)]
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
    let mut stream = connect(&path)
        .await
        .with_context(|| format!("session {session_id} is not running; run `local-mcp start`"))?;
    stream
        .write_all(&serde_json::to_vec(&Message::Approval { request })?)
        .await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;

    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
    match response.trim() {
        "allow" => Ok(true),
        "deny" => Ok(false),
        value => anyhow::bail!("invalid response from session: {value:?}"),
    }
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
    let mut pending = VecDeque::<(Request, SessionStream)>::new();
    let mut yolo = false;
    loop {
        tokio::select! {
            connection = listener.accept() => {
                let mut stream = connection?;
                let mut line = String::new();
                BufReader::new(&mut stream).read_line(&mut line).await?;
                let message: Message = serde_json::from_str(&line).context("invalid session message")?;
                match message {
                    Message::Activity { title, detail } => show_activity(&title, detail.as_deref()),
                    Message::Approval { request } if yolo => {
                        eprintln!("[yolo] allowing {}: {}", request.operation, request.detail);
                        stream.write_all(b"allow\n").await?;
                    }
                    Message::Approval { request } => {
                        show_request(&request)?;
                        pending.push_back((request, stream));
                    }
                }
            }
            line = input.next_line() => {
                let Some(line) = line? else { anyhow::bail!("session input closed") };
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
    pending: &mut VecDeque<(Request, SessionStream)>,
) -> Result<()> {
    match input {
        "/permissions yolo" | "/permission yolo" => {
            *yolo = true;
            eprintln!(
                "WARNING: Permissions yolo allows every approval-gated host-native call for this session only; restart resets it. MCP callers cannot enable yolo."
            );
            while let Some((request, mut stream)) = pending.pop_front() {
                eprintln!("[yolo] allowing {}: {}", request.operation, request.detail);
                stream.write_all(b"allow\n").await?;
            }
        }
        "/permissions ask" | "/permission ask" => {
            *yolo = false;
            eprintln!("Permissions: ask");
        }
        "y" | "Y" | "yes" | "YES" if !pending.is_empty() => {
            let (_, mut stream) = pending.pop_front().unwrap();
            stream.write_all(b"allow\n").await?;
            show_next(pending)?;
        }
        "n" | "N" | "no" | "NO" if !pending.is_empty() => {
            let (_, mut stream) = pending.pop_front().unwrap();
            stream.write_all(b"deny\n").await?;
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
            let (_, mut stream) = pending.pop_front().unwrap();
            stream.write_all(b"deny\n").await?;
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

fn show_next(pending: &VecDeque<(Request, SessionStream)>) -> Result<()> {
    if let Some((request, _)) = pending.front() {
        show_request(request)?;
    }
    Ok(())
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
