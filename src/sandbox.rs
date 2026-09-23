use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
#[cfg(not(windows))]
use codex_protocol::models::PermissionProfile;
#[cfg(not(windows))]
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

fn absolute(path: &Path) -> Result<AbsolutePathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    AbsolutePathBuf::from_absolute_path(path).map_err(|error| anyhow::anyhow!(error))
}

#[derive(Debug)]
pub struct RunError {
    pub error: anyhow::Error,
    pub command_started: bool,
    pub command_finished: bool,
}

impl RunError {
    fn new(error: anyhow::Error, command_started: bool, command_finished: bool) -> Self {
        Self {
            error,
            command_started,
            command_finished,
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for RunError {}

fn sandbox_process(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    stdin_present: bool,
) -> Result<(PathBuf, Command)> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    let roots = writable_roots
        .iter()
        .map(|path| absolute(path))
        .collect::<Result<Vec<_>>>()?;
    #[cfg(all(test, target_os = "linux"))]
    let network_policy =
        if std::env::var("LOCAL_MCP_TEST_ALLOW_LINUX_NETWORK").as_deref() == Ok("1") {
            // GitHub-hosted Linux runners allow the bubblewrap filesystem/user
            // namespaces used here but deny RTM_NEWADDR while bwrap initializes an
            // isolated loopback device. Keep production restricted; only CI tests
            // opt out of the network namespace so the filesystem sandbox contract
            // remains executable in that environment.
            NetworkSandboxPolicy::Enabled
        } else {
            NetworkSandboxPolicy::Restricted
        };
    #[cfg(not(any(windows, all(test, target_os = "linux"))))]
    let network_policy = NetworkSandboxPolicy::Restricted;
    #[cfg(not(windows))]
    let permissions = PermissionProfile::workspace_write_with(&roots, network_policy, true, true)
        .materialize_project_roots_with_workspace_roots(&[absolute(&cwd)?]);
    #[cfg(windows)]
    let _ = roots;

    #[cfg(target_os = "linux")]
    let mut process = {
        let args =
            codex_sandboxing::landlock::create_linux_sandbox_command_args_for_permission_profile(
                command.to_vec(),
                &cwd,
                &permissions,
                &cwd,
                false,
                false,
            );
        let executable_dir = std::env::current_exe()?
            .parent()
            .context("local-mcp executable has no parent directory")?
            .to_owned();
        let executable = executable_dir.join("codex-linux-sandbox");
        #[cfg(test)]
        let executable = if !executable.is_file()
            && executable_dir.file_name().and_then(|name| name.to_str()) == Some("deps")
        {
            executable_dir
                .parent()
                .map(|debug_dir| debug_dir.join("codex-linux-sandbox"))
                .filter(|test_helper| test_helper.is_file())
                .unwrap_or(executable)
        } else {
            executable
        };
        anyhow::ensure!(
            executable.is_file(),
            "sandbox helper is missing: {}",
            executable.display()
        );
        let mut process = Command::new(executable);
        process.args(args);
        process
    };

    #[cfg(target_os = "macos")]
    let mut process = {
        use codex_sandboxing::seatbelt::CreateSeatbeltCommandArgsParams;
        use codex_sandboxing::seatbelt::MACOS_PATH_TO_SEATBELT_EXECUTABLE;
        use codex_sandboxing::seatbelt::create_seatbelt_command_args;

        let (file_system_policy, network_policy) = permissions.to_runtime_permissions();
        let args = create_seatbelt_command_args(CreateSeatbeltCommandArgsParams {
            command: command.to_vec(),
            file_system_sandbox_policy: &file_system_policy,
            network_sandbox_policy: network_policy,
            sandbox_policy_cwd: &cwd,
            enforce_managed_network: false,
            network: None,
            extra_allow_unix_sockets: &[],
        });
        let mut process = Command::new(MACOS_PATH_TO_SEATBELT_EXECUTABLE);
        process.args(args);
        process
    };

    #[cfg(windows)]
    let mut process = {
        // Windows has no equivalent of Landlock/Seatbelt in this application.
        // Preserve argv execution and the restricted environment so the
        // feature remains usable, while documenting that this is not a
        // filesystem/network sandbox.
        let mut process = Command::new(&command[0]);
        process.args(&command[1..]);
        process
    };

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let mut process = { anyhow::bail!("sandboxed execution is unsupported on this platform") };

    process
        .kill_on_drop(true)
        .current_dir(&cwd)
        .env_clear()
        .envs(safe_environment())
        .stdin(if stdin_present {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    Ok((cwd, process))
}

pub async fn run_tracked(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> std::result::Result<Output, RunError> {
    let (_, mut process) = sandbox_process(command, cwd, writable_roots, stdin.is_some())
        .map_err(|error| RunError::new(error, false, false))?;
    let mut child = process
        .spawn()
        .context("failed to start primary command")
        .map_err(|error| RunError::new(error, false, false))?;

    if let Some(bytes) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        child_stdin
            .write_all(bytes)
            .await
            .map_err(|error| RunError::new(error.into(), true, false))?;
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|error| RunError::new(error.into(), true, false))?;
    Ok(Output {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(unix)]
pub async fn run(
    command: &[String],
    cwd: &Path,
    writable_roots: &[PathBuf],
    stdin: Option<&[u8]>,
) -> Result<Output> {
    run_tracked(command, cwd, writable_roots, stdin)
        .await
        .map_err(|error| error.error)
}

pub async fn run_unrestricted(
    command: &[String],
    cwd: &Path,
    stdin: Option<&[u8]>,
) -> Result<Output> {
    anyhow::ensure!(!command.is_empty(), "command must not be empty");
    let cwd = std::fs::canonicalize(cwd)
        .with_context(|| format!("cannot resolve cwd {}", cwd.display()))?;
    let mut process = Command::new(&command[0]);
    process
        .kill_on_drop(true)
        .args(&command[1..])
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = process
        .spawn()
        .context("failed to start unsandboxed command")?;
    if let Some(bytes) = stdin
        && let Some(mut child_stdin) = child.stdin.take()
    {
        child_stdin.write_all(bytes).await?;
    }
    let output = child.wait_with_output().await?;
    Ok(Output {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn safe_environment() -> HashMap<String, String> {
    [
        "PATH",
        "LANG",
        "LC_ALL",
        "TERM",
        "TMPDIR",
        "TEMP",
        "TMP",
        "SystemRoot",
    ]
    .into_iter()
    .filter_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| (name.to_owned(), value))
    })
    .collect()
}

#[cfg(test)]
mod unrestricted_tests {
    use super::*;

    #[tokio::test]
    async fn host_native_execution_preserves_exit_and_output() -> Result<()> {
        let cwd = std::env::temp_dir();
        #[cfg(unix)]
        let command = vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf stdout; printf stderr >&2; exit 0".into(),
        ];
        #[cfg(windows)]
        let command = vec![
            "powershell.exe".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            "[Console]::Out.Write('stdout'); [Console]::Error.Write('stderr'); exit 0".into(),
        ];
        let output = run_unrestricted(&command, &cwd, None).await?;
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout, "stdout");
        assert_eq!(output.stderr, "stderr");
        Ok(())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn test_directory() -> PathBuf {
        std::env::temp_dir().join(format!("local-mcp-sandbox-test-{}", Uuid::new_v4()))
    }

    #[tokio::test]
    async fn seatbelt_allows_workspace_writes_and_denies_other_writes() -> Result<()> {
        // Nix's macOS build sandbox does not allow a nested Seatbelt profile.
        if std::env::var_os("NIX_BUILD_TOP").is_some() {
            return Ok(());
        }
        let root = test_directory();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&outside)?;

        let allowed = run(
            &["/usr/bin/touch".into(), "allowed".into()],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_eq!(allowed.status, 0, "{}", allowed.stderr);
        assert!(workspace.join("allowed").is_file());

        let denied_path = outside.join("denied");
        let denied = run(
            &[
                "/usr/bin/touch".into(),
                denied_path.to_string_lossy().into_owned(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(denied.status, 0);
        assert!(!denied_path.exists());

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn seatbelt_denies_network_access() -> Result<()> {
        if std::env::var_os("NIX_BUILD_TOP").is_some() {
            return Ok(());
        }
        let workspace = test_directory();
        std::fs::create_dir_all(&workspace)?;
        let output = run(
            &[
                "/usr/bin/curl".into(),
                "--fail".into(),
                "--max-time".into(),
                "2".into(),
                "https://example.com".into(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(output.status, 0);

        std::fs::remove_dir_all(workspace)?;
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;
    use uuid::Uuid;

    fn test_directory() -> PathBuf {
        std::env::temp_dir().join(format!("local-mcp-linux-sandbox-{}", Uuid::new_v4()))
    }

    async fn run_shell(cwd: &Path, script: &str) -> Result<Output> {
        run(
            &["/bin/sh".into(), "-c".into(), script.into()],
            cwd,
            &[],
            None,
        )
        .await
    }

    #[tokio::test]
    async fn bubblewrap_confines_filesystem_writes_to_workspace() -> Result<()> {
        let root = test_directory();
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&outside)?;
        let existing_outside = outside.join("sentinel.txt");
        std::fs::write(&existing_outside, "untouched")?;

        let allowed = run_shell(
            &workspace,
            "printf readable > existing.txt && mkdir nested && printf new > nested/new.txt",
        )
        .await?;
        assert_eq!(allowed.status, 0, "{}", allowed.stderr);
        assert_eq!(
            std::fs::read_to_string(workspace.join("existing.txt"))?,
            "readable"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("nested/new.txt"))?,
            "new"
        );

        let absolute_escape = run_shell(
            &workspace,
            &format!(
                "printf escaped > '{}'",
                outside.join("absolute.txt").display()
            ),
        )
        .await?;
        assert_ne!(absolute_escape.status, 0, "{}", absolute_escape.stderr);

        let traversal_escape =
            run_shell(&workspace, "printf escaped > ../outside/traversal.txt").await?;
        assert_ne!(traversal_escape.status, 0, "{}", traversal_escape.stderr);

        std::os::unix::fs::symlink(&outside, workspace.join("intermediate-link"))?;
        std::os::unix::fs::symlink(&existing_outside, workspace.join("final-link"))?;
        std::os::unix::fs::symlink(
            outside.join("dangling-target.txt"),
            workspace.join("dangling-link"),
        )?;
        for path in [
            "intermediate-link/intermediate.txt",
            "final-link",
            "dangling-link",
        ] {
            let escaped = run_shell(&workspace, &format!("printf escaped > '{path}'")).await?;
            assert_ne!(escaped.status, 0, "path {path}: {}", escaped.stderr);
        }

        assert_eq!(std::fs::read_to_string(existing_outside)?, "untouched");
        assert!(!outside.join("absolute.txt").exists());
        assert!(!outside.join("traversal.txt").exists());
        assert!(!outside.join("intermediate.txt").exists());
        assert!(!outside.join("dangling-target.txt").exists());

        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "run on native Linux with LOCAL_MCP_TEST_ALLOW_LINUX_NETWORK unset"]
    async fn bubblewrap_restricted_network_blocks_ip_sockets_and_exposes_only_loopback()
    -> Result<()> {
        anyhow::ensure!(
            std::env::var("LOCAL_MCP_TEST_ALLOW_LINUX_NETWORK").as_deref() != Ok("1"),
            "network isolation test requires the production restricted-network policy"
        );

        let workspace = test_directory();
        std::fs::create_dir_all(&workspace)?;

        let ip_socket = run_shell(
            &workspace,
            "/usr/bin/python3 -c 'import socket; socket.socket(socket.AF_INET, socket.SOCK_STREAM)'",
        )
        .await?;
        assert_ne!(ip_socket.status, 0, "IPv4 socket creation was allowed");
        assert!(
            ip_socket.stderr.contains("Operation not permitted"),
            "expected seccomp EPERM for IP socket creation, got: {}",
            ip_socket.stderr
        );

        let socket_families = run_shell(
            &workspace,
            r#"/usr/bin/python3 -c 'import errno, socket
n=0
for family in (socket.AF_INET, socket.AF_INET6):
 for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
  try: socket.socket(family, kind)
  except OSError as error:
   assert error.errno == errno.EPERM, error
   n += 1
  else: raise SystemExit("IP socket creation unexpectedly succeeded")
assert n == 4
socket.socketpair()
'"#,
        )
        .await?;
        assert_eq!(socket_families.status, 0, "{}", socket_families.stderr);

        let proxy_environment = run_shell(
            &workspace,
            r#"/usr/bin/python3 -c 'import os
names={"http_proxy", "https_proxy", "all_proxy", "no_proxy"}
present=[name for name in os.environ if name.lower() in names]
assert not present, present
'"#,
        )
        .await?;
        assert_eq!(proxy_environment.status, 0, "{}", proxy_environment.stderr);

        let interfaces = run(
            &["/bin/cat".into(), "/proc/net/dev".into()],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_eq!(interfaces.status, 0, "{}", interfaces.stderr);
        let visible_interfaces: Vec<_> = interfaces
            .stdout
            .lines()
            .skip(2)
            .filter_map(|line| line.split_once(':').map(|(name, _)| name.trim()))
            .collect();
        assert_eq!(visible_interfaces, ["lo"]);

        let external_network = run(
            &[
                "/usr/bin/curl".into(),
                "--noproxy".into(),
                "*".into(),
                "--connect-timeout".into(),
                "2".into(),
                "--max-time".into(),
                "3".into(),
                "--silent".into(),
                "--show-error".into(),
                "https://1.1.1.1".into(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(external_network.status, 0, "external network was reachable");

        let dns = run(
            &[
                "/usr/bin/getent".into(),
                "hosts".into(),
                "example.com".into(),
            ],
            &workspace,
            &[],
            None,
        )
        .await?;
        assert_ne!(dns.status, 0, "external DNS resolution succeeded");

        std::fs::remove_dir_all(workspace)?;
        Ok(())
    }
}
