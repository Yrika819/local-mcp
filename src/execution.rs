use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::{approvals, atomic_publish_frame, config, fallback, sandbox, workspace_publish};

const FOREGROUND_TIMEOUT: Duration = Duration::from_secs(30);
const HOST_FOREGROUND_TIMEOUT: Duration = Duration::from_secs(20);
const CODEX_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct BackgroundExecution {
    pub(crate) rendered_command: String,
    pub(crate) handle: JoinHandle<Result<String>>,
    pub(crate) activity: &'static str,
}

pub(crate) enum ExecutionOutcome {
    Completed(String),
    Background(BackgroundExecution),
}

/// Publish a host-authorized Writer file change atomically, inside the sandbox.
///
/// On Unix this runs the host-owned `atomic-publish` helper **through**
/// `sandbox::run`, with the validated parent as the only writable root — exactly the
/// containment the previous `sh -c 'cat > "$1"'` implementation had. The boundary is
/// unchanged; only the mechanism is different, which is what makes the publication
/// atomic without granting any new authority.
///
/// On Windows there is no application sandbox (see `sandbox::build_sandbox_process`),
/// so the helper is executed directly and keeps the same structured, shell-free commit.
pub(crate) async fn publish_workspace_write(
    request: crate::writer::WriterWriteRequest<'_>,
) -> Result<sandbox::Output> {
    let helper = atomic_publish_helper()?;
    let parent = request.parent.to_path_buf();
    let frame = encode_publish_frame(&request)?;
    sandbox::run(
        &[helper],
        request.parent,
        std::slice::from_ref(&parent),
        Some(&frame),
    )
    .await
}

/// Resolve the host-owned publication helper next to this executable.
///
/// The path comes from the host's own executable directory and is never
/// model-influenced. Only that directory is searched in production. The
/// `deps`-parent fallback exists solely so `cargo test` can find the helper built
/// into the profile directory, and is compiled out otherwise; an unconditional
/// fallback would let a binary one level above the install prefix stand in for the
/// commit mechanism. When the helper is absent the publication is refused rather than
/// falling back to a non-atomic write.
fn atomic_publish_helper() -> Result<String> {
    let executable_dir = std::env::current_exe()
        .context("local-mcp executable has no path")?
        .parent()
        .context("local-mcp executable has no parent directory")?
        .to_path_buf();
    let name = if cfg!(windows) {
        "atomic-publish.exe"
    } else {
        "atomic-publish"
    };
    #[cfg_attr(
        not(test),
        allow(unused_mut, reason = "only the test build adds a fallback candidate")
    )]
    let mut candidates = vec![executable_dir.join(name)];
    #[cfg(test)]
    if executable_dir.file_name().is_some_and(|dir| dir == "deps")
        && let Some(profile_dir) = executable_dir.parent()
    {
        candidates.push(profile_dir.join(name));
    }
    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.to_string_lossy().into_owned());
        }
    }
    anyhow::bail!(
        "atomic publication helper is missing; looked in {}",
        candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Frame the request for the helper.
///
/// The frame is version-tagged and length-prefixed. The model supplies `content` only;
/// the parent, target, preimage and request identity are all host-derived, so this
/// function never encodes an authority-bearing value that came from the model.
fn encode_publish_frame(request: &crate::writer::WriterWriteRequest<'_>) -> Result<Vec<u8>> {
    let (kind, digest) = match request.expected_preimage {
        workspace_publish::ExpectedPreimage::Absent => (0u8, String::new()),
        workspace_publish::ExpectedPreimage::Sha256(sha256) => (1u8, sha256.clone()),
    };
    let parent = request.parent.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "writer parent path is not valid UTF-8: {}",
            request.parent.display()
        )
    })?;
    let target = request.path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "writer target path is not valid UTF-8: {}",
            request.path.display()
        )
    })?;
    Ok(atomic_publish_frame::encode(
        atomic_publish_frame::EncodedRequest {
            parent,
            target,
            preimage_kind: kind,
            preimage_digest: &digest,
            request_id: request.request_id,
            content: request.content.as_bytes(),
        },
    ))
}
pub(crate) async fn execute(args: &Value, session: &config::Session) -> Result<ExecutionOutcome> {
    let (rendered_command, mut handle) = spawn_sandboxed_command("execute", args, session).await?;

    match tokio::time::timeout(FOREGROUND_TIMEOUT, &mut handle).await {
        Ok(joined) => Ok(ExecutionOutcome::Completed(
            joined.context("command task failed")??,
        )),
        Err(_) => Ok(ExecutionOutcome::Background(BackgroundExecution {
            rendered_command,
            handle,
            activity: "Backgrounded",
        })),
    }
}

pub(crate) async fn start_command(
    args: &Value,
    session: &config::Session,
) -> Result<BackgroundExecution> {
    let (rendered_command, handle) =
        spawn_sandboxed_command("start_command", args, session).await?;
    Ok(BackgroundExecution {
        rendered_command,
        handle,
        activity: "Started",
    })
}

pub(crate) fn resolve_path(session_cwd: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        session_cwd.join(path)
    }
}

pub(crate) fn cwd(args: &Value, session: &config::Session) -> Result<PathBuf> {
    if let Some(requested) = args.get("cwd").and_then(Value::as_str) {
        // Bounded before it is resolved or authorized. Path authority itself is
        // unchanged: this only refuses a field far larger than any real path.
        anyhow::ensure!(
            requested.len() <= crate::resource_limits::MAX_EXECUTE_PATH_BYTES,
            "{}",
            crate::resource_limits::limit_error(
                crate::resource_limits::ResourceLimit::ExecutePathBytes,
                "no command was started",
            )
        );
    }
    let path = args
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .map(|path| resolve_path(&session.cwd, path))
        .unwrap_or_else(|| session.cwd.clone());
    config::validate_path_authority(session, &path, config::PathIntent::ExecutionCwd)
}

/// The `command` argument of an execute-style request, under frozen bounds.
///
/// The declared JSON Schema is documentation only — nothing validates against it
/// — so every bound is enforced here in host code. A tiny command name must not
/// be able to carry hundreds of megabytes of arguments.
pub(crate) fn required_command(args: &Value) -> Result<Vec<String>> {
    use crate::resource_limits::{
        MAX_EXECUTE_ARG_BYTES, MAX_EXECUTE_ARGV_ITEMS, MAX_EXECUTE_ARGV_TOTAL_BYTES, ResourceLimit,
    };
    let items = args
        .get("command")
        .and_then(Value::as_array)
        .context("missing command")?;
    anyhow::ensure!(
        items.len() <= MAX_EXECUTE_ARGV_ITEMS,
        "{}",
        crate::resource_limits::limit_error(
            ResourceLimit::ExecuteArgvItems,
            "no command was started",
        )
    );
    let mut command = Vec::with_capacity(items.len());
    let mut total = 0_usize;
    for item in items {
        let value = item.as_str().context("command entries must be strings")?;
        anyhow::ensure!(
            value.len() <= MAX_EXECUTE_ARG_BYTES,
            "{}",
            crate::resource_limits::limit_error(
                ResourceLimit::ExecuteArgBytes,
                "no command was started",
            )
        );
        total = total.saturating_add(value.len());
        anyhow::ensure!(
            total <= MAX_EXECUTE_ARGV_TOTAL_BYTES,
            "{}",
            crate::resource_limits::limit_error(
                ResourceLimit::ExecuteArgvTotalBytes,
                "no command was started",
            )
        );
        command.push(value.to_owned());
    }
    Ok(command)
}

#[derive(Clone)]
pub(crate) struct ExecutionPolicy {
    pub(crate) request_id: String,
    pub(crate) accepted_exit_codes: Vec<i32>,
    pub(crate) operation: Option<fallback::OperationIntent>,
    pub(crate) fallback_depth: u8,
    pub(crate) primary_execution_mode: fallback::PrimaryExecutionMode,
    validated_git_stage_paths: Option<GitStagePaths>,
}

impl ExecutionPolicy {
    pub(crate) fn new(
        request_id: String,
        accepted_exit_codes: Vec<i32>,
        operation: Option<fallback::OperationIntent>,
        fallback_depth: u8,
        primary_execution_mode: fallback::PrimaryExecutionMode,
    ) -> Self {
        Self {
            request_id,
            accepted_exit_codes,
            operation,
            fallback_depth,
            primary_execution_mode,
            validated_git_stage_paths: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct GitStagePaths {
    cwd: PathBuf,
    permitted_roots: Vec<PathBuf>,
    repository_root: PathBuf,
    git_dir: PathBuf,
    common_git_dir: PathBuf,
    trusted_executable: PathBuf,
    trusted_executable_arg: String,
    paths: Vec<String>,
}

impl GitStagePaths {
    async fn validate(
        intent: &fallback::OperationIntent,
        command: &[String],
        cwd: &Path,
        permitted_roots: &[PathBuf],
    ) -> Result<Self> {
        intent.validate()?;
        anyhow::ensure!(
            intent.kind == fallback::OperationType::GitStagePaths && intent.scope_matches(command),
            "Git staging scope is not exact"
        );
        let cwd =
            std::fs::canonicalize(cwd).context("cannot resolve Git staging working directory")?;
        let roots = canonical_permitted_roots(permitted_roots)?;
        for path in &intent.paths {
            validate_stage_target(&cwd, path)?;
        }
        let trusted_executable = host_git_path()?;
        let trusted_executable_arg = trusted_executable
            .to_str()
            .context("Git executable path is not valid UTF-8")?
            .to_owned();
        let requested_executable = command.first().context("Git command has no executable")?;
        validate_git_executable(requested_executable, &trusted_executable)?;
        let repository_root =
            trusted_git_query(&trusted_executable_arg, &cwd, "--show-toplevel").await?;
        let git_dir =
            trusted_git_query(&trusted_executable_arg, &cwd, "--absolute-git-dir").await?;
        let common_git_dir =
            trusted_git_query(&trusted_executable_arg, &cwd, "--git-common-dir").await?;
        anyhow::ensure!(
            path_is_within_roots(&repository_root, &roots)
                && path_is_within_roots(&git_dir, &roots)
                && path_is_within_roots(&common_git_dir, &roots),
            "Git repository is outside the permitted session roots"
        );
        anyhow::ensure!(
            !trusted_git_filter_configuration_present(&trusted_executable_arg, &cwd).await?,
            "Git repository has filter-driver configuration"
        );
        Ok(Self {
            cwd,
            permitted_roots: roots,
            repository_root,
            git_dir,
            common_git_dir,
            trusted_executable,
            trusted_executable_arg,
            paths: intent.paths.clone(),
        })
    }

    async fn revalidate(&self) -> Result<()> {
        let cwd = std::fs::canonicalize(&self.cwd)
            .context("cannot revalidate Git staging working directory")?;
        anyhow::ensure!(
            same_path_identity(&cwd, &self.cwd),
            "Git staging working directory changed"
        );
        let roots = canonical_permitted_roots(&self.permitted_roots)?;
        anyhow::ensure!(
            roots == self.permitted_roots,
            "Git staging permitted roots changed"
        );
        for path in &self.paths {
            validate_stage_target(&cwd, path)?;
        }
        let current_executable = resolve_host_git_path()?;
        anyhow::ensure!(
            same_path_identity(&current_executable, &self.trusted_executable),
            "Git executable identity changed"
        );
        validate_git_executable(&self.trusted_executable_arg, &self.trusted_executable)?;
        let repository_root =
            trusted_git_query(&self.trusted_executable_arg, &cwd, "--show-toplevel").await?;
        let git_dir =
            trusted_git_query(&self.trusted_executable_arg, &cwd, "--absolute-git-dir").await?;
        let common_git_dir =
            trusted_git_query(&self.trusted_executable_arg, &cwd, "--git-common-dir").await?;
        anyhow::ensure!(
            same_path_identity(&repository_root, &self.repository_root)
                && same_path_identity(&git_dir, &self.git_dir)
                && same_path_identity(&common_git_dir, &self.common_git_dir),
            "Git repository identity changed"
        );
        anyhow::ensure!(
            !trusted_git_filter_configuration_present(&self.trusted_executable_arg, &cwd).await?,
            "Git repository has filter-driver configuration"
        );
        Ok(())
    }

    fn approval_detail(
        &self,
        request_id: &str,
        operation: &fallback::OperationIntent,
        budget: &fallback::Budget,
    ) -> String {
        format!(
            "request_id={} mode=EXECUTE_AUTHORIZED_OPERATION operation={} repository_root={} git_dir={} common_git_dir={} executable={} cwd={} paths={:?} budget={}/{}",
            request_id,
            operation.kind.as_str(),
            self.repository_root.display(),
            self.git_dir.display(),
            self.common_git_dir.display(),
            self.trusted_executable.display(),
            self.cwd.display(),
            self.paths,
            budget.attempt_remaining,
            budget.side_effect_remaining,
        )
    }

    fn matches_intent(&self, intent: &fallback::OperationIntent) -> bool {
        intent.kind == fallback::OperationType::GitStagePaths && intent.paths == self.paths
    }

    fn index_snapshot_command(&self) -> Vec<String> {
        let mut command = trusted_git_command(&self.trusted_executable_arg, "ls-files");
        command.extend(["--stage".to_owned(), "-z".to_owned()]);
        command
    }

    fn stage_command(&self) -> Vec<String> {
        let mut command = trusted_git_command(&self.trusted_executable_arg, "add");
        command.push("--".to_owned());
        command.extend(self.paths.clone());
        command
    }
}

fn trusted_git_command(executable: &str, subcommand: &str) -> Vec<String> {
    vec![
        executable.to_owned(),
        "-c".to_owned(),
        format!("core.hooksPath={}", sandbox::null_device_path()),
        "-c".to_owned(),
        "core.fsmonitor=false".to_owned(),
        subcommand.to_owned(),
    ]
}

async fn trusted_git_filter_configuration_present(executable: &str, cwd: &Path) -> Result<bool> {
    let mut present = false;
    for scope in ["--local", "--worktree"] {
        let mut command = trusted_git_command(executable, "config");
        command.extend([
            scope.to_owned(),
            "--get-regexp".to_owned(),
            "^filter\\.".to_owned(),
        ]);
        let output = sandbox::run_unrestricted_clean_with_limits(
            &command,
            cwd,
            None,
            sandbox::TRUSTED_GIT_QUERY_TIMEOUT,
            sandbox::TRUSTED_GIT_STDOUT_LIMIT,
            sandbox::TRUSTED_GIT_STDERR_LIMIT,
        )
        .await
        .map_err(|error| error.error)?;
        match output.status {
            0 if !output.stdout.trim().is_empty() => present = true,
            0 | 1 => {}
            status => anyhow::bail!(
                "trusted Git filter query failed: scope={scope} status={status} stderr={}",
                output.stderr
            ),
        }
    }
    Ok(present)
}

async fn trusted_git_query(executable: &str, cwd: &Path, argument: &str) -> Result<PathBuf> {
    let mut command = trusted_git_command(executable, "rev-parse");
    command.push(argument.to_owned());
    let output = sandbox::run_unrestricted_clean_with_limits(
        &command,
        cwd,
        None,
        sandbox::TRUSTED_GIT_QUERY_TIMEOUT,
        sandbox::TRUSTED_GIT_STDOUT_LIMIT,
        sandbox::TRUSTED_GIT_STDERR_LIMIT,
    )
    .await
    .map_err(|error| error.error)?;
    anyhow::ensure!(
        output.status == 0,
        "trusted Git query failed: {} {}",
        argument,
        output.stderr
    );
    let value = output.stdout.strip_suffix('\n').unwrap_or(&output.stdout);
    let value = value.strip_suffix('\r').unwrap_or(value);
    anyhow::ensure!(
        !value.is_empty(),
        "trusted Git query returned no path: {argument}"
    );
    let value_path = Path::new(value);
    let value_path = if value_path.is_absolute() {
        value_path.to_path_buf()
    } else {
        cwd.join(value_path)
    };
    let path = std::fs::canonicalize(value_path)
        .with_context(|| format!("cannot resolve trusted Git query path: {argument}"))?;
    anyhow::ensure!(
        path.is_dir(),
        "trusted Git query path is not a directory: {argument}"
    );
    Ok(path)
}

fn canonical_permitted_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    roots
        .iter()
        .map(|root| {
            std::fs::canonicalize(root)
                .with_context(|| format!("cannot resolve permitted root {}", root.display()))
        })
        .collect()
}

fn path_is_within_roots(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

#[cfg(unix)]
fn is_forbidden_stage_component(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn is_forbidden_stage_component(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    metadata.file_attributes() & 0x0400 != 0
}

#[cfg(not(any(unix, windows)))]
fn is_forbidden_stage_component(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn validate_stage_target(cwd: &Path, relative: &str) -> Result<()> {
    let components = Path::new(relative).components().collect::<Vec<_>>();
    anyhow::ensure!(!components.is_empty(), "Git staging target is empty");
    let mut current = cwd.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(part) = component else {
            anyhow::bail!("Git staging target is not a normalized relative path");
        };
        current.push(part);
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                anyhow::ensure!(
                    index + 1 == components.len(),
                    "Git staging target has a missing intermediate component: {}",
                    current.display()
                );
                return Ok(());
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("cannot inspect Git staging target {}", current.display())
                });
            }
        };
        anyhow::ensure!(
            !is_forbidden_stage_component(&metadata),
            "Git staging target contains a symlink or reparse point: {}",
            current.display()
        );
        if index + 1 == components.len() {
            anyhow::ensure!(
                metadata.is_file(),
                "Git staging target is not a regular file: {}",
                current.display()
            );
        } else {
            anyhow::ensure!(
                metadata.is_dir(),
                "Git staging path component is not a directory: {}",
                current.display()
            );
        }
    }
    Ok(())
}

struct HostExecutionAuthority {
    _private: (),
}

impl HostExecutionAuthority {
    fn granted() -> Self {
        Self { _private: () }
    }
}

struct TrustedGitFailure {
    error: anyhow::Error,
    output: Option<sandbox::Output>,
}

impl std::fmt::Debug for TrustedGitFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TrustedGitFailure")
            .field("error", &self.error.to_string())
            .field("has_output", &self.output.is_some())
            .finish()
    }
}

impl std::fmt::Display for TrustedGitFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for TrustedGitFailure {}

fn trusted_git_failure(error: anyhow::Error, output: Option<sandbox::Output>) -> anyhow::Error {
    anyhow::Error::new(TrustedGitFailure { error, output })
}

static HOST_GIT: OnceLock<Result<PathBuf, String>> = OnceLock::new();

/// The host Git executable, resolved and validated once and cached.
///
/// Exposed so the Managed Worktrees creation seam runs the same host Git
/// identity as the staging mutation instead of re-deriving a weaker one.
pub(crate) fn host_git_path() -> Result<PathBuf> {
    HOST_GIT
        .get_or_init(|| resolve_host_git_path().map_err(|error| format!("{error:#}")))
        .clone()
        .map_err(|error| anyhow::anyhow!(error))
}

fn resolve_host_git_path() -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    resolve_git_from_path(&path)
}

fn resolve_git_from_path(path: &std::ffi::OsStr) -> Result<PathBuf> {
    anyhow::ensure!(!path.is_empty(), "PATH is empty");
    let entries = std::env::split_paths(path).collect::<Vec<_>>();
    for entry in &entries {
        anyhow::ensure!(
            !entry.as_os_str().is_empty(),
            "PATH contains an empty entry"
        );
        anyhow::ensure!(entry.is_absolute(), "PATH contains a relative entry");
        anyhow::ensure!(entry.to_str().is_some(), "PATH entry is not valid UTF-8");
    }
    for entry in entries {
        for name in git_file_names() {
            let candidate = entry.join(name);
            let Ok(metadata) = std::fs::metadata(&candidate) else {
                continue;
            };
            if !metadata.is_file() || !has_execute_permission(&metadata) {
                continue;
            }
            let canonical = std::fs::canonicalize(&candidate).with_context(|| {
                format!("cannot resolve Git executable {}", candidate.display())
            })?;
            anyhow::ensure!(
                canonical.to_str().is_some(),
                "Git executable path is not valid UTF-8"
            );
            return Ok(canonical);
        }
    }
    anyhow::bail!("Git executable not found in PATH")
}

#[cfg(unix)]
fn has_execute_permission(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn has_execute_permission(_metadata: &std::fs::Metadata) -> bool {
    true
}

fn git_file_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["git.exe", "git"]
    }
    #[cfg(not(windows))]
    {
        &["git"]
    }
}

fn validate_git_executable(requested: &str, trusted: &Path) -> Result<()> {
    if is_bare_git_executable(requested) {
        let path = std::env::var_os("PATH").context("PATH is not set")?;
        let resolved = resolve_git_from_path(&path)?;
        anyhow::ensure!(
            same_path_identity(&resolved, trusted),
            "Git executable does not match the host Git identity"
        );
        return Ok(());
    }
    let requested = Path::new(requested);
    anyhow::ensure!(
        requested.is_absolute(),
        "Git executable must be bare git or an absolute path"
    );
    let canonical = std::fs::canonicalize(requested)
        .with_context(|| format!("cannot resolve Git executable {}", requested.display()))?;
    anyhow::ensure!(
        std::fs::metadata(&canonical)
            .context("cannot inspect Git executable")?
            .is_file(),
        "Git executable is not a regular file"
    );
    anyhow::ensure!(
        same_path_identity(&canonical, trusted),
        "Git executable does not match the host Git identity"
    );
    Ok(())
}

fn same_path_identity(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_str()
            .zip(right.to_str())
            .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn is_bare_git_executable(value: &str) -> bool {
    #[cfg(windows)]
    {
        value.eq_ignore_ascii_case("git") || value.eq_ignore_ascii_case("git.exe")
    }
    #[cfg(not(windows))]
    {
        value == "git"
    }
}

pub(crate) fn primary_execution_mode() -> fallback::PrimaryExecutionMode {
    #[cfg(windows)]
    {
        fallback::PrimaryExecutionMode::HostNative
    }
    #[cfg(not(windows))]
    {
        fallback::PrimaryExecutionMode::Sandboxed
    }
}

pub(crate) fn primary_execution_requires_approval(mode: fallback::PrimaryExecutionMode) -> bool {
    mode == fallback::PrimaryExecutionMode::HostNative
}

pub(crate) fn ensure_primary_execution_authorized(
    mode: fallback::PrimaryExecutionMode,
    approved: bool,
    operation: &str,
) -> Result<()> {
    if primary_execution_requires_approval(mode) && !approved {
        anyhow::bail!("user denied {operation}");
    }
    Ok(())
}

pub(crate) fn execution_policy(args: &Value) -> Result<ExecutionPolicy> {
    let fallback_depth = args
        .get("fallback_depth")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    anyhow::ensure!(
        fallback_depth <= fallback::max_depth() as u64,
        "fallback_depth exceeds configured maximum"
    );
    Ok(ExecutionPolicy::new(
        Uuid::new_v4().to_string(),
        fallback::accepted_exit_codes(args.get("accepted_exit_codes"))?,
        fallback::operation_from_value(args.get("operation"))?,
        fallback_depth as u8,
        primary_execution_mode(),
    ))
}

async fn spawn_sandboxed_command(
    operation: &str,
    args: &Value,
    session: &config::Session,
) -> Result<(String, JoinHandle<Result<String>>)> {
    spawn_sandboxed_command_inner(operation, args, session, || -> Result<()> { Ok(()) }).await
}

#[cfg(all(test, unix))]
async fn spawn_sandboxed_command_with_pre_primary_failure<F>(
    operation: &str,
    args: &Value,
    session: &config::Session,
    before_primary: F,
) -> Result<(String, JoinHandle<Result<String>>)>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    spawn_sandboxed_command_inner(operation, args, session, before_primary).await
}

async fn spawn_sandboxed_command_inner<F>(
    operation: &str,
    args: &Value,
    session: &config::Session,
    before_primary: F,
) -> Result<(String, JoinHandle<Result<String>>)>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    let command = required_command(args)?;
    let cwd = cwd(args, session)?;
    let mut policy = execution_policy(args)?;
    if let Some(operation) = policy.operation.as_ref()
        && operation.kind == fallback::OperationType::GitStagePaths
    {
        policy.validated_git_stage_paths = Some(
            GitStagePaths::validate(operation, &command, &cwd, &session.permitted_directories)
                .await?,
        );
    }
    if primary_execution_requires_approval(policy.primary_execution_mode) {
        let detail = policy
            .validated_git_stage_paths
            .as_ref()
            .and_then(|stage| {
                policy.operation.as_ref().map(|intent| {
                    let budget = fallback::Budget::from_operation(Some(intent));
                    stage.approval_detail(&policy.request_id, intent, &budget)
                })
            })
            .unwrap_or_else(|| format!("argv: {command:?}"));
        let approved = approvals::request(&session.id, operation, detail, cwd.clone()).await?;
        ensure_primary_execution_authorized(policy.primary_execution_mode, approved, operation)?;
        if let Some(stage) = policy.validated_git_stage_paths.as_ref() {
            stage.revalidate().await?;
        }
    }
    // The caller-provided cwd was validated against the persisted roots above.
    // It must never become a new sandbox root.
    let roots = session.permitted_directories.clone();

    let pre_index_snapshot =
        capture_index_snapshot_if_needed(policy.validated_git_stage_paths.as_ref(), &cwd).await;

    let rendered_command = render_command(&command);
    approvals::activity(
        &session.id,
        format!("Running {rendered_command}"),
        Some(format!("└ request_id={}", policy.request_id)),
    )
    .await;
    if let Some(stage) = policy.validated_git_stage_paths.as_ref() {
        stage.revalidate().await?;
    }
    let session_id = session.id.clone();
    let task_command = rendered_command.clone();
    let handle = tokio::spawn(async move {
        let preflight = async {
            before_primary()?;
            if let Some(stage) = policy.validated_git_stage_paths.as_ref() {
                stage.revalidate().await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = preflight {
            let result = Err(error);
            report_command_finished(session_id, &task_command, &result).await;
            return result;
        }
        let attempt = sandbox::run_tracked(&command, &cwd, &roots, None).await;
        let result = process_sandboxed_attempt(
            &session_id,
            &command,
            &cwd,
            &policy,
            pre_index_snapshot,
            attempt,
        )
        .await;
        report_command_finished(session_id, &task_command, &result).await;
        result
    });
    Ok((rendered_command, handle))
}

pub(crate) async fn process_sandboxed_attempt(
    session_id: &str,
    command: &[String],
    cwd: &Path,
    policy: &ExecutionPolicy,
    pre_index_snapshot: Option<String>,
    attempt: std::result::Result<sandbox::Output, sandbox::RunError>,
) -> Result<String> {
    process_sandboxed_attempt_with_codex_override(
        session_id,
        command,
        cwd,
        policy,
        pre_index_snapshot,
        attempt,
        None,
    )
    .await
}

#[cfg(all(test, unix))]
async fn process_sandboxed_attempt_with_test_codex(
    session_id: &str,
    command: &[String],
    cwd: &Path,
    policy: &ExecutionPolicy,
    pre_index_snapshot: Option<String>,
    attempt: std::result::Result<sandbox::Output, sandbox::RunError>,
    codex_command: Vec<String>,
) -> Result<String> {
    process_sandboxed_attempt_with_codex_override(
        session_id,
        command,
        cwd,
        policy,
        pre_index_snapshot,
        attempt,
        Some(codex_command),
    )
    .await
}

async fn process_sandboxed_attempt_with_codex_override(
    session_id: &str,
    command: &[String],
    cwd: &Path,
    policy: &ExecutionPolicy,
    pre_index_snapshot: Option<String>,
    attempt: std::result::Result<sandbox::Output, sandbox::RunError>,
    codex_command_override: Option<Vec<String>>,
) -> Result<String> {
    // The sandboxed attempt reports two separate lifecycles: the lifecycle of the
    // process that carried the sandbox, and the lifecycle of the requested
    // command. Only the latter grants fallback authority, and only host-owned
    // evidence may establish it. A sandbox helper that rejected setup and exited
    // is a helper lifecycle, not a command lifecycle.
    let (lifecycle, exit_code, stdout, stderr, execution_error, setup_rejection, resource_limit) =
        match attempt {
            Ok(output) => (
                fallback::LifecycleEvidence::completed_with_start_proof(output.command_start),
                Some(output.status),
                output.stdout,
                output.stderr,
                None,
                None,
                None,
            ),
            Err(error) => (
                fallback::LifecycleEvidence {
                    host_reached: true,
                    // The executed process starting is not evidence that the requested
                    // command started: where a wrapper is used the wrapper is the
                    // process. Only a process that provably never started proves the
                    // requested command never started.
                    command_start: if error.command_started {
                        sandbox::CommandStart::Unproven
                    } else {
                        sandbox::CommandStart::Refuted
                    },
                    command_finished: false,
                    process_finished: error.command_finished,
                },
                None,
                String::new(),
                String::new(),
                Some(format!("{:#}", error.error)),
                error.setup_rejection,
                // Preserved as a typed marker. An output-limit failure must be
                // classified as a terminal resource failure, never re-derived from
                // the very output the host just discarded.
                error
                    .error
                    .downcast_ref::<crate::resource_limits::ResourceLimitError>()
                    .map(|resource_error| resource_error.limit()),
            ),
        };

    let side_effect_class = fallback::infer_side_effect_class(command, policy.operation.as_ref());
    let classification = fallback::classify(fallback::ClassificationInput {
        command,
        accepted_exit_codes: &policy.accepted_exit_codes,
        primary_execution_mode: policy.primary_execution_mode,
        lifecycle,
        exit_code,
        stdout: &stdout,
        stderr: &stderr,
        execution_error: execution_error.as_deref(),
        side_effect_class,
        authoritative_platform_safety: false,
        authoritative_setup_rejection: setup_rejection,
        authoritative_resource_limit: resource_limit,
    });

    let local_not_performed_proof = if let Some(stage) = policy.validated_git_stage_paths.as_ref()
        && classification.failure_class != fallback::FailureClass::Success
    {
        match (
            pre_index_snapshot.as_ref(),
            capture_git_index_snapshot(stage, cwd).await.as_ref(),
        ) {
            (Some(before), Some(after)) => Some(before == after),
            _ => None,
        }
    } else {
        None
    };

    let side_effect_state = fallback::infer_side_effect_state(
        side_effect_class,
        lifecycle,
        classification.failure_class,
        local_not_performed_proof,
    );
    let budget_before = fallback::Budget::from_operation(policy.operation.as_ref());
    let budget_after_primary = budget_before.after_state(side_effect_state);
    let scope_valid = policy.validated_git_stage_paths.is_some()
        && policy
            .operation
            .as_ref()
            .is_some_and(|operation| operation.scope_matches(command));
    let decision = fallback::decide(fallback::DecisionInput {
        failure_class: classification.failure_class,
        safety_signal: classification.safety_signal,
        primary_execution_mode: policy.primary_execution_mode,
        lifecycle,
        operation: policy.operation.as_ref(),
        side_effect_class,
        side_effect_state,
        fallback_depth: policy.fallback_depth,
        max_depth: fallback::max_depth(),
        budget: budget_after_primary,
        operation_validated: policy.validated_git_stage_paths.is_some(),
        scope_valid,
        automatic_enabled: fallback::automatic_enabled(),
        auto_execute_enabled: fallback::auto_execute_enabled(),
    });

    emit_fallback_trace(
        session_id,
        policy,
        lifecycle,
        exit_code,
        classification.failure_class,
        side_effect_class,
        side_effect_state,
        &decision,
        budget_before,
        budget_after_primary,
        None,
    )
    .await;

    if decision.action == fallback::FallbackAction::Execute {
        let operation = policy
            .operation
            .as_ref()
            .context("execute fallback missing operation intent")?;
        let stage = policy
            .validated_git_stage_paths
            .as_ref()
            .context("execute fallback missing validated Git stage paths")?;
        match execute_authorized_operation_fallback_with_codex(
            FallbackExecutionRequest {
                session_id,
                policy,
                operation,
                stage,
                pre_index_snapshot: pre_index_snapshot.as_deref(),
                budget_before_fallback: budget_after_primary,
            },
            codex_command_override,
        )
        .await
        {
            Ok((fallback_output, budget_after_fallback, verification_passed)) => {
                emit_fallback_trace(
                    session_id,
                    policy,
                    lifecycle,
                    Some(fallback_output.status),
                    classification.failure_class,
                    side_effect_class,
                    fallback::SideEffectState::ConfirmedPerformed,
                    &decision,
                    budget_before,
                    budget_after_fallback,
                    Some(verification_passed),
                )
                .await;
                return Ok(execution_payload(
                    policy,
                    lifecycle,
                    Some(fallback_output.status),
                    &fallback_output.stdout,
                    &fallback_output.stderr,
                    None,
                    classification.failure_class,
                    side_effect_class,
                    fallback::SideEffectState::ConfirmedPerformed,
                    &decision,
                    budget_after_fallback,
                    Some(verification_passed),
                    None,
                ));
            }
            Err(fallback_error) => {
                let trusted_failure = fallback_error.downcast_ref::<TrustedGitFailure>();
                let mut failure_decision = decision.clone();
                let failure_budget = if trusted_failure.is_some() {
                    let budget = budget_after_primary.after_started_mutation_failure();
                    failure_decision.side_effect_state = fallback::SideEffectState::Unknown;
                    failure_decision.attempt_budget_remaining = budget.attempt_remaining;
                    failure_decision.side_effect_budget_remaining = budget.side_effect_remaining;
                    failure_decision.budget_locked = budget.locked;
                    budget
                } else {
                    budget_after_primary
                };
                let failure_output = trusted_failure.and_then(|failure| failure.output.as_ref());
                let failure_exit_code = failure_output.map(|output| output.status).or(exit_code);
                let failure_stdout = failure_output
                    .map(|output| output.stdout.as_str())
                    .unwrap_or(stdout.as_str());
                let failure_stderr = failure_output
                    .map(|output| output.stderr.as_str())
                    .unwrap_or(stderr.as_str());
                let failure_state = if trusted_failure.is_some() {
                    fallback::SideEffectState::Unknown
                } else {
                    side_effect_state
                };
                emit_fallback_trace(
                    session_id,
                    policy,
                    lifecycle,
                    failure_exit_code,
                    classification.failure_class,
                    side_effect_class,
                    failure_state,
                    &failure_decision,
                    budget_before,
                    failure_budget,
                    Some(false),
                )
                .await;
                let payload = execution_payload(
                    policy,
                    lifecycle,
                    failure_exit_code,
                    failure_stdout,
                    failure_stderr,
                    execution_error.as_deref(),
                    classification.failure_class,
                    side_effect_class,
                    failure_state,
                    &failure_decision,
                    failure_budget,
                    Some(false),
                    Some(&format!("{fallback_error:#}")),
                );
                return Err(payload_error(payload, resource_limit));
            }
        }
    }

    let payload = execution_payload(
        policy,
        lifecycle,
        exit_code,
        &stdout,
        &stderr,
        execution_error.as_deref(),
        classification.failure_class,
        side_effect_class,
        side_effect_state,
        &decision,
        budget_after_primary,
        None,
        None,
    );
    if matches!(
        classification.failure_class,
        fallback::FailureClass::Success | fallback::FailureClass::ExpectedState
    ) {
        Ok(payload)
    } else {
        Err(payload_error(payload, resource_limit))
    }
}

/// Build the failure an unsuccessful attempt returns.
///
/// The payload is a rendered JSON diagnostic and the transport error taxonomy
/// must still see it as a *resource* failure rather than a generic server error.
/// `anyhow::bail!(payload)` would take only the string and drop the type, so the
/// typed marker is re-attached here as the error's source; `downcast_ref` finds
/// it through the context chain.
fn payload_error(
    payload: String,
    resource_limit: Option<crate::resource_limits::ResourceLimit>,
) -> anyhow::Error {
    let error = anyhow::anyhow!(payload);
    match resource_limit {
        Some(limit) => anyhow::Error::new(crate::resource_limits::ResourceLimitError::new(
            limit,
            "the attempt failed and its output was discarded",
        ))
        .context(error),
        None => error,
    }
}

async fn capture_index_snapshot_if_needed(
    stage: Option<&GitStagePaths>,
    cwd: &Path,
) -> Option<String> {
    let stage = stage?;
    capture_git_index_snapshot(stage, cwd).await
}

async fn capture_git_index_snapshot(stage: &GitStagePaths, cwd: &Path) -> Option<String> {
    let output = sandbox::run_unrestricted_clean_with_limits(
        &stage.index_snapshot_command(),
        cwd,
        None,
        sandbox::TRUSTED_GIT_SNAPSHOT_TIMEOUT,
        sandbox::TRUSTED_GIT_STDOUT_LIMIT,
        sandbox::TRUSTED_GIT_STDERR_LIMIT,
    )
    .await
    .ok()?;
    (output.status == 0).then_some(output.stdout)
}

struct FallbackExecutionRequest<'a> {
    session_id: &'a str,
    policy: &'a ExecutionPolicy,
    operation: &'a fallback::OperationIntent,
    stage: &'a GitStagePaths,
    pre_index_snapshot: Option<&'a str>,
    budget_before_fallback: fallback::Budget,
}

#[cfg(all(test, unix))]
#[allow(clippy::too_many_arguments)]
async fn execute_authorized_operation_fallback_with_test_codex(
    session_id: &str,
    _cwd: &Path,
    policy: &ExecutionPolicy,
    operation: &fallback::OperationIntent,
    stage: &GitStagePaths,
    pre_index_snapshot: Option<&str>,
    budget_before_fallback: fallback::Budget,
    codex_command: Vec<String>,
) -> Result<(sandbox::Output, fallback::Budget, bool)> {
    execute_authorized_operation_fallback_with_codex(
        FallbackExecutionRequest {
            session_id,
            policy,
            operation,
            stage,
            pre_index_snapshot,
            budget_before_fallback,
        },
        Some(codex_command),
    )
    .await
}

async fn execute_authorized_operation_fallback_with_codex(
    request: FallbackExecutionRequest<'_>,
    codex_command_override: Option<Vec<String>>,
) -> Result<(sandbox::Output, fallback::Budget, bool)> {
    let FallbackExecutionRequest {
        session_id,
        policy,
        operation,
        stage,
        pre_index_snapshot,
        budget_before_fallback,
    } = request;
    anyhow::ensure!(
        policy.fallback_depth == 0,
        "fallback recursion is not allowed"
    );
    anyhow::ensure!(
        operation.auto_execute_allowlisted() && stage.matches_intent(operation),
        "operation is not executable-fallback allowlisted"
    );
    anyhow::ensure!(
        policy.validated_git_stage_paths.as_ref() == Some(stage),
        "validated Git stage paths are unavailable"
    );
    anyhow::ensure!(
        !budget_before_fallback.locked
            && budget_before_fallback.attempt_remaining > 0
            && budget_before_fallback.side_effect_remaining > 0,
        "fallback budget is exhausted or locked"
    );
    anyhow::ensure!(
        pre_index_snapshot.is_some(),
        "missing pre-fallback Git index snapshot"
    );

    if !approvals::request(
        session_id,
        "codex_fallback_v2_execute",
        stage.approval_detail(&policy.request_id, operation, &budget_before_fallback),
        stage.cwd.clone(),
    )
    .await?
    {
        anyhow::bail!("user denied executable Codex fallback");
    }
    stage.revalidate().await?;

    let codex_command = match codex_command_override {
        Some(command) => command,
        None => fallback::codex_read_only_command(&stage.cwd, fallback::Effort::Low)?,
    };
    let preflight_prompt =
        fallback::execute_preflight_prompt(&stage.cwd, &policy.request_id, operation);
    let codex_output = crate::agent::run_bounded_host_process(
        &codex_command,
        preflight_prompt.as_bytes(),
        &stage.cwd,
        CODEX_PREFLIGHT_TIMEOUT,
    )
    .await
    .map_err(|error| anyhow::anyhow!("Codex read-only preflight failed: {error}"))?;
    anyhow::ensure!(
        codex_output.status == 0,
        "Codex read-only preflight failed: exit={} stderr={}",
        codex_output.status,
        codex_output.stderr
    );
    stage.revalidate().await?;

    let authority = HostExecutionAuthority::granted();
    let (output, verification_passed) =
        execute_exact_git_stage(authority, stage, &stage.cwd, pre_index_snapshot).await?;

    anyhow::ensure!(
        verification_passed,
        "post-fallback verification did not confirm the exact authorized postcondition"
    );

    let budget_after = budget_before_fallback.consume_fallback();
    Ok((output, budget_after, true))
}

async fn execute_exact_git_stage(
    _authority: HostExecutionAuthority,
    stage: &GitStagePaths,
    cwd: &Path,
    pre_index_snapshot: Option<&str>,
) -> Result<(sandbox::Output, bool)> {
    let output = match sandbox::run_unrestricted_clean_with_limits(
        &stage.stage_command(),
        cwd,
        None,
        sandbox::TRUSTED_GIT_STAGE_TIMEOUT,
        sandbox::TRUSTED_GIT_STDOUT_LIMIT,
        sandbox::TRUSTED_GIT_STDERR_LIMIT,
    )
    .await
    {
        Ok(output) => output,
        Err(error) if error.command_started => {
            return Err(trusted_git_failure(
                anyhow::anyhow!("{:#}", error.error),
                None,
            ));
        }
        Err(error) => return Err(anyhow::anyhow!("{:#}", error.error)),
    };
    if output.status != 0 {
        return Err(trusted_git_failure(
            anyhow::anyhow!("trusted Git staging exited with status {}", output.status),
            Some(output),
        ));
    }
    let Some(before) = pre_index_snapshot else {
        return Err(trusted_git_failure(
            anyhow::anyhow!("missing pre-fallback Git index snapshot"),
            Some(output),
        ));
    };
    let Some(after) = capture_git_index_snapshot(stage, cwd).await else {
        return Err(trusted_git_failure(
            anyhow::anyhow!("failed to capture post-fallback Git index snapshot"),
            Some(output),
        ));
    };
    if !fallback::verify_index_change_scope(before, &after, &stage.paths) {
        return Err(trusted_git_failure(
            anyhow::anyhow!("post-fallback verification did not confirm the exact postcondition"),
            Some(output),
        ));
    }
    Ok((output, true))
}

/// Render the host-owned requested-command start proof for audit payloads.
///
/// `command_started` in the payload is exactly `PROVEN`; `REFUTED` and
/// `UNPROVEN` are both reported as `command_started: false` so absence of proof
/// can never be read as proof of absence.
fn command_start_proof(command_start: sandbox::CommandStart) -> &'static str {
    match command_start {
        sandbox::CommandStart::Proven => "PROVEN",
        sandbox::CommandStart::Refuted => "REFUTED",
        sandbox::CommandStart::Unproven => "UNPROVEN",
    }
}

#[allow(clippy::too_many_arguments)]
fn execution_payload(
    policy: &ExecutionPolicy,
    lifecycle: fallback::LifecycleEvidence,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    execution_error: Option<&str>,
    failure_class: fallback::FailureClass,
    side_effect_class: fallback::SideEffectClass,
    side_effect_state: fallback::SideEffectState,
    decision: &fallback::FallbackDecision,
    budget_after: fallback::Budget,
    verification_passed: Option<bool>,
    fallback_error: Option<&str>,
) -> String {
    json!({
        "request_id": policy.request_id,
        "operation_type": policy.operation.as_ref().map(|operation| operation.kind.as_str()).unwrap_or("unstructured"),
        "primary_execution_mode": policy.primary_execution_mode.as_str(),
        "host_reached": lifecycle.host_reached,
        "command_started": lifecycle.command_started(),
        "command_start_proof": command_start_proof(lifecycle.command_start),
        "command_finished": lifecycle.command_finished,
        "sandbox_process_finished": lifecycle.process_finished,
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "execution_error": execution_error,
        "failure_class": failure_class.as_str(),
        "side_effect_class": side_effect_class.as_str(),
        "side_effect_state": side_effect_state.as_str(),
        "fallback_decision": {
            "action": decision.action.as_str(),
            "reason_code": decision.reason_code.as_str(),
            "operation_validated": decision.operation_validated,
            "side_effect_state": decision.side_effect_state.as_str(),
            "attempt_budget_remaining": decision.attempt_budget_remaining,
            "side_effect_budget_remaining": decision.side_effect_budget_remaining,
            "budget_locked": decision.budget_locked,
            "verification_required": decision.verification_required,
        },
        "fallback_mode": decision.mode.map(|mode| mode.as_str()),
        "fallback_depth": policy.fallback_depth,
        "remaining_attempt_budget": budget_after.attempt_remaining,
        "remaining_side_effect_budget": budget_after.side_effect_remaining,
        "budget_locked": budget_after.locked,
        "verification": {
            "required": decision.verification_required,
            "passed": verification_passed,
        },
        "fallback_error": fallback_error,
    })
    .to_string()
}

#[allow(clippy::too_many_arguments)]
async fn emit_fallback_trace(
    session_id: &str,
    policy: &ExecutionPolicy,
    lifecycle: fallback::LifecycleEvidence,
    exit_code: Option<i32>,
    failure_class: fallback::FailureClass,
    side_effect_class: fallback::SideEffectClass,
    side_effect_state: fallback::SideEffectState,
    decision: &fallback::FallbackDecision,
    budget_before: fallback::Budget,
    budget_after: fallback::Budget,
    verification_passed: Option<bool>,
) {
    let trace = json!({
        "request_id": policy.request_id,
        "operation_type": policy.operation.as_ref().map(|operation| operation.kind.as_str()).unwrap_or("unstructured"),
        "primary_execution_mode": policy.primary_execution_mode.as_str(),
        "host_reached": lifecycle.host_reached,
        "command_started": lifecycle.command_started(),
        "command_start_proof": command_start_proof(lifecycle.command_start),
        "command_finished": lifecycle.command_finished,
        "sandbox_process_finished": lifecycle.process_finished,
        "exit_code": exit_code,
        "failure_class": failure_class.as_str(),
        "side_effect_class": side_effect_class.as_str(),
        "side_effect_state": side_effect_state.as_str(),
        "fallback_action": decision.action.as_str(),
        "reason_code": decision.reason_code.as_str(),
        "fallback_depth": policy.fallback_depth,
        "budget_before": {
            "attempt": budget_before.attempt_remaining,
            "side_effect": budget_before.side_effect_remaining,
            "locked": budget_before.locked,
        },
        "budget_after": {
            "attempt": budget_after.attempt_remaining,
            "side_effect": budget_after.side_effect_remaining,
            "locked": budget_after.locked,
        },
        "verification_passed": verification_passed,
    });
    approvals::activity(
        session_id,
        format!("Fallback V2 {}", policy.request_id),
        Some(format!("└ {}", trace)),
    )
    .await;
}

pub(crate) async fn codex_fallback(
    args: &Value,
    session: &config::Session,
) -> Result<BackgroundExecution> {
    let task = args
        .get("task")
        .and_then(Value::as_str)
        .context("missing task")?;
    anyhow::ensure!(task.len() <= 128 * 1024, "task is too large");
    let blocker = args
        .get("blocker")
        .and_then(Value::as_str)
        .context("missing blocker")?;
    anyhow::ensure!(blocker.len() <= 64 * 1024, "blocker is too large");
    let cwd = cwd(args, session)?;
    let request_id = Uuid::new_v4().to_string();
    let mode = args
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("DIAGNOSE_ONLY");

    let explicit_failure_class = args
        .get("failure_class")
        .and_then(Value::as_str)
        .map(parse_failure_class)
        .transpose()?;

    let safety_probe = fallback::classify(fallback::ClassificationInput {
        command: &[],
        accepted_exit_codes: &[0],
        primary_execution_mode: primary_execution_mode(),
        lifecycle: fallback::LifecycleEvidence::not_started(),
        exit_code: None,
        stdout: "",
        stderr: "",
        execution_error: Some(blocker),
        side_effect_class: fallback::SideEffectClass::Unknown,
        authoritative_platform_safety: explicit_failure_class
            == Some(fallback::FailureClass::PlatformSafety),
        authoritative_setup_rejection: None,
        authoritative_resource_limit: None,
    });

    if safety_probe.safety_signal
        || explicit_failure_class == Some(fallback::FailureClass::PlatformSafety)
    {
        anyhow::bail!("terminal platform/safety classification: Codex fallback is not permitted");
    }

    // Public MCP callers may request diagnosis only. Executable fallback is an
    // internal continuation of the host-observed execute/start_command path;
    // accepting caller-supplied lifecycle, failure, or side-effect facts here
    // would turn assertions into authority.
    anyhow::ensure!(
        mode == "DIAGNOSE_ONLY",
        "EXECUTE_AUTHORIZED_OPERATION is not available through the public MCP surface"
    );

    let failure_class = explicit_failure_class.unwrap_or(safety_probe.failure_class);
    let requires_code_change = args
        .get("requires_code_change")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let effort = if requires_code_change {
        fallback::Effort::Medium
    } else {
        fallback::Effort::Low
    };
    if !approvals::request(
        &session.id,
        "codex_fallback_v2_diagnose",
        format!(
            "request_id={} mode=DIAGNOSE_ONLY class={} model={} effort={}",
            request_id,
            failure_class.as_str(),
            fallback::model(),
            effort.as_str()
        ),
        cwd.clone(),
    )
    .await?
    {
        anyhow::bail!("user denied diagnostic Codex fallback");
    }

    let command = fallback::codex_read_only_command(&cwd, effort)?;
    let prompt = fallback::diagnose_prompt(&cwd, &request_id, failure_class, blocker);
    let session_id = session.id.clone();
    let label = format!(
        "Codex fallback diagnose {}/{}",
        fallback::model(),
        effort.as_str()
    );
    let task_label = label.clone();
    let handle = tokio::spawn(async move {
        let result = sandbox::run_unrestricted(&command, &cwd, Some(prompt.as_bytes()))
            .await
            .and_then(render_output);
        report_command_finished(session_id, &task_label, &result).await;
        result
    });
    Ok(BackgroundExecution {
        rendered_command: label,
        handle,
        activity: "Started",
    })
}

fn parse_failure_class(value: &str) -> Result<fallback::FailureClass> {
    serde_json::from_value(Value::String(value.to_owned()))
        .with_context(|| format!("invalid failure_class: {value}"))
}

pub(crate) async fn without_sandbox(
    args: &Value,
    session: &config::Session,
) -> Result<ExecutionOutcome> {
    let command = required_command(args)?;
    let cwd = cwd(args, session)?;
    if !approvals::request(
        &session.id,
        "without_sandbox",
        format!(
            "mode=HOST_NATIVE sandboxed=false network=true mutation_capable=true argv={command:?}"
        ),
        cwd.clone(),
    )
    .await?
    {
        anyhow::bail!("user denied without_sandbox")
    }

    let rendered_command = render_command(&command);
    approvals::activity(&session.id, format!("Running {rendered_command}"), None).await;

    let session_id = session.id.clone();
    let task_command = rendered_command.clone();
    let handle = tokio::spawn(async move {
        let result = sandbox::run_unrestricted(&command, &cwd, None)
            .await
            .and_then(render_output);
        report_command_finished(session_id, &task_command, &result).await;
        result
    });

    let mut handle = handle;
    match tokio::time::timeout(HOST_FOREGROUND_TIMEOUT, &mut handle).await {
        Ok(joined) => Ok(ExecutionOutcome::Completed(
            joined.context("command task failed")??,
        )),
        Err(_) => Ok(ExecutionOutcome::Background(BackgroundExecution {
            rendered_command,
            handle,
            activity: "Backgrounded host command",
        })),
    }
}

pub(crate) fn render_command(command: &[String]) -> String {
    command
        .iter()
        .map(|arg| shell_word(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

async fn report_command_finished(session_id: String, command: &str, result: &Result<String>) {
    let detail = match result {
        Ok(text) => command_summary(text),
        Err(error) => Some(format!("└ Error: {error:#}")),
    };
    approvals::activity(&session_id, format!("Ran {command}"), detail).await;
}

pub(crate) fn shell_word(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:=+".contains(c))
    {
        value.to_owned()
    } else {
        format!("{:?}", value)
    }
}

/// A short activity-timeline digest of a command result.
///
/// The start UI echoes this for every completed command, so it is bounded
/// independently of the capture limit: a flood that fits inside the capture
/// bounds must not turn into an unbounded approval-UI render.
fn command_summary(text: &str) -> Option<String> {
    use crate::resource_limits::MAX_COMMAND_SUMMARY_BYTES;
    let value: Value = serde_json::from_str(text).ok()?;
    let stdout = value
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end();
    let stderr = value
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end();
    let output = if stdout.is_empty() { stderr } else { stdout };
    if output.is_empty() {
        return None;
    }
    let mut rendered = String::new();
    let mut truncated = false;
    for line in output.lines() {
        let entry = format!("└ {line}\n");
        if rendered.len().saturating_add(entry.len()) > MAX_COMMAND_SUMMARY_BYTES {
            truncated = true;
            break;
        }
        rendered.push_str(&entry);
    }
    if truncated {
        rendered.push_str("└ … output truncated for the activity timeline\n");
    }
    Some(rendered)
}

pub(crate) fn render_output(output: sandbox::Output) -> Result<String> {
    let text = json!({"exit_code":output.status,"stdout":output.stdout,"stderr":output.stderr})
        .to_string();
    if output.status == 0 {
        Ok(text)
    } else {
        anyhow::bail!(text)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    const TEST_APPROVAL_TIMEOUT: Duration = Duration::from_secs(15);

    struct FakeGit {
        root: PathBuf,
    }

    impl FakeGit {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "local-mcp-pre-approval-fake-git-{}",
                Uuid::new_v4()
            ));
            std::fs::create_dir(&root).unwrap();
            let fixture = Self { root };
            let git = fixture.git_path();
            std::fs::write(
                &git,
                "#!/bin/sh\nif [ \"$1\" = \"ls-files\" ]; then\n  : > \"$0.sentinel\"\nfi\nexit 0\n",
            )
            .unwrap();
            let mut permissions = std::fs::metadata(&git).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&git, permissions).unwrap();
            std::fs::write(fixture.root.join("safe.txt"), "").unwrap();
            fixture
        }

        fn git_path(&self) -> PathBuf {
            self.root.join("git")
        }

        fn sentinel_path(&self) -> PathBuf {
            self.root.join("git.sentinel")
        }

        fn session(&self) -> config::Session {
            config::Session {
                id: format!("pre-approval-fake-git-{}", Uuid::new_v4()),
                cwd: self.root.clone(),
                permitted_directories: vec![self.root.clone()],
            }
        }

        fn args(&self, executable: String, authorized: bool) -> Value {
            json!({
                "command": [executable, "add", "--", "safe.txt"],
                "operation": {
                    "type": "git_stage_paths",
                    "authorized": authorized,
                    "paths": ["safe.txt"]
                }
            })
        }
    }

    impl Drop for FakeGit {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    async fn run_test_git(git: &Path, cwd: &Path, args: &[String]) -> sandbox::Output {
        let mut command = vec![git.to_str().unwrap().to_owned()];
        command.extend(args.iter().cloned());
        sandbox::run_unrestricted_clean_with_limits(
            &command,
            cwd,
            None,
            sandbox::TRUSTED_GIT_STAGE_TIMEOUT,
            sandbox::TRUSTED_GIT_STDOUT_LIMIT,
            sandbox::TRUSTED_GIT_STDERR_LIMIT,
        )
        .await
        .unwrap()
    }

    async fn initialize_test_repo(root: &Path) -> PathBuf {
        std::fs::create_dir_all(root).unwrap();
        let git = host_git_path().unwrap();
        let init = run_test_git(&git, root, &["init".to_owned(), "--quiet".to_owned()]).await;
        assert_eq!(init.status, 0, "{}", init.stderr);
        let hooks = root.join("disabled-hooks");
        let attributes = root.join("empty-attributes");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(&attributes, "").unwrap();
        for args in [
            vec![
                "config".to_owned(),
                "--local".to_owned(),
                "core.hooksPath".to_owned(),
                hooks.to_string_lossy().into_owned(),
            ],
            vec![
                "config".to_owned(),
                "--local".to_owned(),
                "core.fsmonitor".to_owned(),
                "false".to_owned(),
            ],
            vec![
                "config".to_owned(),
                "--local".to_owned(),
                "core.attributesFile".to_owned(),
                attributes.to_string_lossy().into_owned(),
            ],
        ] {
            let output = run_test_git(&git, root, &args).await;
            assert_eq!(output.status, 0, "{}", output.stderr);
        }
        let filters = run_test_git(
            &git,
            root,
            &[
                "config".to_owned(),
                "--local".to_owned(),
                "--get-regexp".to_owned(),
                "^filter\\.".to_owned(),
            ],
        )
        .await;
        if filters.status == 0 {
            for line in filters.stdout.lines() {
                if let Some((key, _)) = line.split_once(' ') {
                    let _ = run_test_git(
                        &git,
                        root,
                        &[
                            "config".to_owned(),
                            "--local".to_owned(),
                            "--unset-all".to_owned(),
                            key.to_owned(),
                        ],
                    )
                    .await;
                }
            }
        }
        git
    }

    struct StageFixture {
        root: PathBuf,
        session: config::Session,
        operation: fallback::OperationIntent,
        stage: GitStagePaths,
        before: String,
        staged_path: PathBuf,
        staged_bytes: String,
        bystander_path: PathBuf,
        bystander_bytes: String,
        codex: PathBuf,
        sentinel: PathBuf,
    }

    impl StageFixture {
        async fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("local-mcp-stage-approval-{}", Uuid::new_v4()));
            initialize_test_repo(&root).await;
            let staged_path = root.join("staged.txt");
            let staged_bytes = "stage me".to_owned();
            let bystander_path = root.join("bystander.txt");
            let bystander_bytes = "leave me alone".to_owned();
            std::fs::write(&staged_path, &staged_bytes).unwrap();
            std::fs::write(&bystander_path, &bystander_bytes).unwrap();
            let operation = fallback::OperationIntent {
                kind: fallback::OperationType::GitStagePaths,
                operation_id: Some("stage-approval".to_owned()),
                paths: vec!["staged.txt".to_owned()],
                argv: vec![],
                source: None,
                destination: None,
                target: None,
                create_only: false,
                force: false,
                attempt_budget_remaining: 1,
                side_effect_budget_remaining: 1,
            };
            let command = vec![
                "git".to_owned(),
                "add".to_owned(),
                "--".to_owned(),
                "staged.txt".to_owned(),
            ];
            let stage =
                GitStagePaths::validate(&operation, &command, &root, std::slice::from_ref(&root))
                    .await
                    .unwrap();
            let before = capture_git_index_snapshot(&stage, &root)
                .await
                .expect("fixture index snapshot");
            let codex = root.join("codex");
            // Models the real read-only preflight contract rather than only its
            // exit value. The launcher delivers the preflight prompt on stdin
            // and treats a direct child which exits while that writer is still
            // unfinished as a timeout, so a fixture that validated argv and
            // exited without reading stdin raced the launcher's own contract.
            // Draining the prompt first, and only then creating the sentinel,
            // makes the preflight's success depend on the contract being met.
            std::fs::write(
                &codex,
                "#!/bin/sh\nmode=\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"-s\" ] && [ \"$2\" = \"read-only\" ]; then\n    mode=read-only\n  fi\n  shift\ndone\n/bin/cat > /dev/null\nif [ \"$mode\" = read-only ]; then\n  : > \"$0.sentinel\"\n  exit 0\nfi\nexit 17\n",
            )
            .unwrap();
            let mut permissions = std::fs::metadata(&codex).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&codex, permissions).unwrap();
            let session = config::Session {
                id: format!("finding-a-approval-{}", Uuid::new_v4()),
                cwd: root.clone(),
                permitted_directories: vec![root.clone()],
            };
            Self {
                root,
                session,
                operation,
                stage,
                before,
                staged_path,
                staged_bytes,
                bystander_path,
                bystander_bytes,
                sentinel: codex.with_file_name("codex.sentinel"),
                codex,
            }
        }

        fn policy(&self) -> ExecutionPolicy {
            let mut policy = ExecutionPolicy::new(
                "stage-approval-test".to_owned(),
                vec![0],
                Some(self.operation.clone()),
                0,
                fallback::PrimaryExecutionMode::Sandboxed,
            );
            policy.validated_git_stage_paths = Some(self.stage.clone());
            policy
        }

        async fn index_snapshot(&self) -> String {
            capture_git_index_snapshot(&self.stage, &self.root)
                .await
                .expect("index snapshot")
        }
    }

    impl Drop for StageFixture {
        fn drop(&mut self) {
            if let Ok(path) = config::socket_path(&self.session.id) {
                let _ = std::fs::remove_file(path);
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    async fn bind_test_approval_socket(session_id: &str) -> tokio::net::UnixListener {
        let path = config::socket_path(session_id).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        tokio::net::UnixListener::bind(path).unwrap()
    }

    async fn serve_test_approval(
        listener: tokio::net::UnixListener,
        response: &'static str,
        expected_executable: String,
        expected_path: String,
    ) {
        loop {
            let (mut stream, _) = tokio::time::timeout(TEST_APPROVAL_TIMEOUT, listener.accept())
                .await
                .expect("approval server accept timed out")
                .expect("approval server accept failed");
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            let read = tokio::time::timeout(TEST_APPROVAL_TIMEOUT, reader.read_line(&mut line))
                .await
                .expect("approval server read timed out")
                .expect("approval server read failed");
            if read == 0 || line.trim().is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(line.trim()).expect("approval message JSON");
            if message["type"] != "approval" {
                continue;
            }
            assert!(line.contains(&expected_executable));
            assert!(line.contains(&expected_path));
            tokio::time::timeout(TEST_APPROVAL_TIMEOUT, stream.write_all(response.as_bytes()))
                .await
                .expect("approval server write timed out")
                .expect("approval server write failed");
            tokio::time::timeout(TEST_APPROVAL_TIMEOUT, stream.flush())
                .await
                .expect("approval server flush timed out")
                .expect("approval server flush failed");
            return;
        }
    }

    async fn serve_test_approval_with_stage_detail(
        listener: tokio::net::UnixListener,
        response: &'static str,
        stage: GitStagePaths,
        cwd: PathBuf,
    ) {
        loop {
            let (mut stream, _) = tokio::time::timeout(TEST_APPROVAL_TIMEOUT, listener.accept())
                .await
                .expect("approval server accept timed out")
                .expect("approval server accept failed");
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            let read = tokio::time::timeout(TEST_APPROVAL_TIMEOUT, reader.read_line(&mut line))
                .await
                .expect("approval server read timed out")
                .expect("approval server read failed");
            if read == 0 || line.trim().is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(line.trim()).expect("approval message JSON");
            if message["type"] != "approval" {
                continue;
            }
            for expected in [
                stage.repository_root.display().to_string(),
                stage.git_dir.display().to_string(),
                stage.common_git_dir.display().to_string(),
                stage.trusted_executable.display().to_string(),
                cwd.display().to_string(),
                stage.paths[0].clone(),
            ] {
                assert!(
                    line.contains(&expected),
                    "missing approval detail: {expected}"
                );
            }
            tokio::time::timeout(TEST_APPROVAL_TIMEOUT, stream.write_all(response.as_bytes()))
                .await
                .expect("approval server write timed out")
                .expect("approval server write failed");
            tokio::time::timeout(TEST_APPROVAL_TIMEOUT, stream.flush())
                .await
                .expect("approval server flush timed out")
                .expect("approval server flush failed");
            return;
        }
    }

    async fn run_test_stage_without_approval(fixture: &StageFixture) -> Result<()> {
        let policy = fixture.policy();
        let budget = fallback::Budget::from_operation(Some(&fixture.operation));
        tokio::time::timeout(
            TEST_APPROVAL_TIMEOUT,
            execute_authorized_operation_fallback_with_test_codex(
                &fixture.session.id,
                &fixture.root,
                &policy,
                &fixture.operation,
                &fixture.stage,
                Some(&fixture.before),
                budget,
                vec![
                    fixture.codex.to_string_lossy().into_owned(),
                    "-s".to_owned(),
                    "read-only".to_owned(),
                ],
            ),
        )
        .await
        .expect("stage execution timed out")
        .map(|_| ())
    }

    async fn run_test_stage_with_approval(
        fixture: &StageFixture,
        response: &'static str,
    ) -> Result<()> {
        let listener = bind_test_approval_socket(&fixture.session.id).await;
        let expected_executable = fixture.stage.trusted_executable.display().to_string();
        let expected_path = fixture.stage.paths[0].clone();
        let server = tokio::spawn(serve_test_approval(
            listener,
            response,
            expected_executable,
            expected_path,
        ));
        let result = tokio::time::timeout(
            TEST_APPROVAL_TIMEOUT,
            run_test_stage_without_approval(fixture),
        )
        .await
        .expect("stage execution timed out");
        tokio::time::timeout(TEST_APPROVAL_TIMEOUT, server)
            .await
            .expect("approval server join timed out")
            .expect("approval server task failed");
        result
    }

    async fn assert_pre_approval_git_is_not_run(
        fixture: &FakeGit,
        executable: String,
        authorized: bool,
    ) {
        let args = fixture.args(executable.clone(), authorized);
        let session = fixture.session();
        let result = spawn_sandboxed_command("execute", &args, &session).await;
        assert!(result.is_err());
        assert!(
            !fixture.sentinel_path().exists(),
            "caller-selected executable {executable} ran before validation rejection"
        );
    }

    #[test]
    fn bare_git_must_match_the_cached_host_identity() {
        let host_git = host_git_path().unwrap();
        assert!(validate_git_executable("git", &host_git).is_ok());

        let root = std::env::temp_dir().join(format!("local-mcp-git-mismatch-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let fake_git = root.join("git");
        std::fs::write(&fake_git, "not the host git").unwrap();
        assert!(validate_git_executable("git", &fake_git).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_non_utf8_host_git_paths_before_argv_construction() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let root = std::env::temp_dir().join(format!("local-mcp-git-encoding-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let invalid_dir = root.join(OsString::from_vec(vec![b'g', 0xff]));
        let mut path_bytes = invalid_dir.as_os_str().as_bytes().to_vec();
        path_bytes.push(b':');
        let path = OsString::from_vec(path_bytes);
        let error = resolve_git_from_path(path.as_os_str()).unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn host_git_resolution_rejects_invalid_path_entries_and_caches_identity() {
        assert!(resolve_git_from_path(std::ffi::OsStr::new("")).is_err());
        assert!(resolve_git_from_path(std::ffi::OsStr::new("relative/path")).is_err());
        let first = host_git_path().unwrap();
        let second = host_git_path().unwrap();
        assert_eq!(first, second);
        assert!(first.is_absolute());
    }

    #[cfg(unix)]
    #[test]
    fn host_git_resolution_skips_non_executable_git_candidates() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("local-mcp-git-exec-{}", Uuid::new_v4()));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let first_git = first.join("git");
        let second_git = second.join("git");
        std::fs::write(&first_git, "not executable").unwrap();
        std::fs::write(&second_git, "executable").unwrap();
        std::fs::set_permissions(&second_git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([first, second]).unwrap();
        assert_eq!(
            resolve_git_from_path(path.as_os_str()).unwrap(),
            std::fs::canonicalize(second_git).unwrap()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn host_git_resolution_rejects_relative_entries_after_a_valid_entry() {
        let host_git = host_git_path().unwrap();
        let valid_entry = host_git.parent().unwrap();
        let path = std::env::join_paths([valid_entry, Path::new("relative")]).unwrap();
        assert!(resolve_git_from_path(path.as_os_str()).is_err());
    }

    #[tokio::test]
    async fn started_trusted_git_failure_is_not_a_verified_success() {
        let root = std::env::temp_dir().join(format!("local-mcp-failed-git-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        // The test-owned Git records the fact that it started before it fails,
        // so the "started" precondition this scenario depends on is established
        // by the process itself rather than by the scheduler having a spare
        // moment to start it.
        let executable = root.join("git");
        std::fs::write(&executable, "#!/bin/sh\n: > \"$0.sentinel\"\nexit 17\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let stage = GitStagePaths {
            cwd: root.clone(),
            permitted_roots: vec![root.clone()],
            repository_root: root.clone(),
            git_dir: root.clone(),
            common_git_dir: root.clone(),
            trusted_executable: executable.clone(),
            trusted_executable_arg: executable.to_str().unwrap().to_owned(),
            paths: vec!["safe.txt".to_owned()],
        };
        let result =
            execute_exact_git_stage(HostExecutionAuthority::granted(), &stage, &root, Some(""))
                .await;
        let error = result.expect_err("a failed staging attempt must not be a verified success");
        // The stage command provably started: the test-owned Git wrote its
        // sentinel as its first action.
        assert!(root.join("git.sentinel").exists());
        // A stage command that started and then failed leaves the mutation
        // ambiguous, which is the only condition the payload maps to UNKNOWN.
        assert!(
            error.downcast_ref::<TrustedGitFailure>().is_some(),
            "a trusted Git failure after start must be reported as an ambiguous mutation"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn unstarted_trusted_git_failure_is_not_an_ambiguous_mutation() {
        let root = std::env::temp_dir().join(format!("local-mcp-absent-git-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        // The trusted executable cannot be started at all. This is a different
        // lifecycle from a started command that then failed: the mutation was
        // never attempted, so claiming an ambiguous side effect would lock the
        // budgets for something that provably did not happen.
        let absent = root.join("git");
        let stage = GitStagePaths {
            cwd: root.clone(),
            permitted_roots: vec![root.clone()],
            repository_root: root.clone(),
            git_dir: root.clone(),
            common_git_dir: root.clone(),
            trusted_executable: absent.clone(),
            trusted_executable_arg: absent.to_string_lossy().into_owned(),
            paths: vec!["safe.txt".to_owned()],
        };
        let result =
            execute_exact_git_stage(HostExecutionAuthority::granted(), &stage, &root, Some(""))
                .await;
        let error = result.expect_err("an absent trusted Git must fail");
        assert!(
            error.downcast_ref::<TrustedGitFailure>().is_none(),
            "a trusted Git that never started must not be reported as an ambiguous mutation"
        );
        assert!(!absent.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn started_trusted_git_failure_payload_is_unknown_locked_and_not_successful() {
        let fixture = StageFixture::new().await;
        let index_lock = fixture.root.join(".git/index.lock");
        std::fs::create_dir(&index_lock).unwrap();
        let policy = fixture.policy();
        let listener = bind_test_approval_socket(&fixture.session.id).await;
        let expected_executable = fixture.stage.trusted_executable_arg.clone();
        let expected_path = "staged.txt".to_owned();
        let server = tokio::spawn(serve_test_approval(
            listener,
            "allow\n",
            expected_executable,
            expected_path,
        ));
        let command = vec![
            "git".to_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "staged.txt".to_owned(),
        ];
        let execution = tokio::time::timeout(
            TEST_APPROVAL_TIMEOUT,
            process_sandboxed_attempt_with_test_codex(
                &fixture.session.id,
                &command,
                &fixture.root,
                &policy,
                Some(String::new()),
                Ok(sandbox::Output {
                    status: 128,
                    stdout: String::new(),
                    stderr: "fatal: Unable to create '.git/index.lock': Operation not permitted"
                        .to_owned(),
                    // These tests model a sandboxed `git add` that provably
                    // started and then failed on a sandbox write restriction.
                    command_start: sandbox::CommandStart::Proven,
                }),
                vec![
                    fixture.codex.to_string_lossy().into_owned(),
                    "-s".to_owned(),
                    "read-only".to_owned(),
                ],
            ),
        );
        let server_wait = tokio::time::timeout(TEST_APPROVAL_TIMEOUT, server);
        let (result, server_result) = tokio::join!(execution, server_wait);
        server_result
            .expect("approval server join timed out")
            .expect("approval server task failed");
        let result = result.expect("fallback process timed out");
        let error = result.unwrap_err();
        let payload: Value = serde_json::from_str(&error.to_string()).unwrap();
        // Host-owned proof that the stage command started, captured by the host
        // from the trusted Git process itself. A stage command that never started
        // cannot produce this text, so this scenario never depends on the
        // scheduler having found room to start Git for the UNKNOWN verdict below
        // to be the right one.
        assert!(
            payload["stderr"]
                .as_str()
                .is_some_and(|stderr| stderr.contains("index.lock")),
            "the trusted Git stage command must have started and reported its own failure: {error}"
        );
        assert_eq!(payload["side_effect_state"], "UNKNOWN");
        assert_eq!(payload["budget_locked"], true);
        assert_eq!(payload["remaining_attempt_budget"], 0);
        assert_eq!(payload["remaining_side_effect_budget"], 0);
        assert_eq!(payload["verification"]["passed"], false);
        assert_eq!(payload["exit_code"], 128);
        assert_eq!(payload["fallback_decision"]["side_effect_state"], "UNKNOWN");
        assert!(index_lock.is_dir());
    }

    #[tokio::test]
    async fn codex_preflight_failure_remains_pre_mutation_state() {
        let fixture = StageFixture::new().await;
        let policy = fixture.policy();
        let listener = bind_test_approval_socket(&fixture.session.id).await;
        let expected_executable = fixture.stage.trusted_executable_arg.clone();
        let expected_path = "staged.txt".to_owned();
        let server = tokio::spawn(serve_test_approval(
            listener,
            "allow\n",
            expected_executable,
            expected_path,
        ));
        let command = vec![
            "git".to_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "staged.txt".to_owned(),
        ];
        let execution = tokio::time::timeout(
            TEST_APPROVAL_TIMEOUT,
            process_sandboxed_attempt_with_test_codex(
                &fixture.session.id,
                &command,
                &fixture.root,
                &policy,
                Some(fixture.before.clone()),
                Ok(sandbox::Output {
                    status: 128,
                    stdout: String::new(),
                    stderr: "fatal: Unable to create '.git/index.lock': Operation not permitted"
                        .to_owned(),
                    // These tests model a sandboxed `git add` that provably
                    // started and then failed on a sandbox write restriction.
                    command_start: sandbox::CommandStart::Proven,
                }),
                vec![
                    fixture.codex.to_string_lossy().into_owned(),
                    "-s".to_owned(),
                    "mutating".to_owned(),
                ],
            ),
        );
        let server_wait = tokio::time::timeout(TEST_APPROVAL_TIMEOUT, server);
        let (result, server_result) = tokio::join!(execution, server_wait);
        server_result
            .expect("approval server join timed out")
            .expect("approval server task failed");
        let error = result.expect("fallback process timed out").unwrap_err();
        let payload: Value = serde_json::from_str(&error.to_string()).unwrap();
        assert_eq!(payload["side_effect_state"], "CONFIRMED_NOT_PERFORMED");
        assert_eq!(payload["budget_locked"], false);
        assert_eq!(payload["remaining_attempt_budget"], 1);
        assert_eq!(payload["remaining_side_effect_budget"], 1);
        assert_eq!(payload["exit_code"], 128);
        assert!(!fixture.sentinel.exists());
    }

    /// Create a fake `bwrap` on a private `PATH` that reports `version`.
    ///
    /// This emulates an unsupported system Bubblewrap without depending on the
    /// version installed on the host, and never touches the real
    /// `/usr/bin/bwrap`.
    #[cfg(target_os = "linux")]
    fn write_fixture_bwrap(root: &Path, version: &str) -> PathBuf {
        let bin = root.join("fixture-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let bwrap = bin.join("bwrap");
        std::fs::write(
            &bwrap,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then\n  printf 'bubblewrap {version}\\n'\n  exit 0\nfi\nprintf 'fixture bwrap must not be launched for --version-only detection\\n' >&2\nexit 97\n"
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&bwrap).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&bwrap, permissions).unwrap();
        bin.into_os_string().into()
    }

    /// A Bubblewrap version below the supported minimum must fail sandbox setup
    /// in the host, before the sandbox helper or the requested command run, and
    /// must never authorise an executable host fallback.
    ///
    /// Regression for the Security V2.1 sandbox lifecycle boundary: the helper
    /// already refused to start the requested command, but the shared lifecycle
    /// model reported the *helper* as the *command*, so the exact GitStagePaths
    /// host fallback executed and mutated the real Git index.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn unsupported_bubblewrap_never_authorises_executable_host_fallback() {
        let fixture = StageFixture::new().await;
        let path = write_fixture_bwrap(&fixture.root, "0.11.1");
        let sentinel = fixture.root.join("requested-command-ran");
        let command = vec![
            "git".to_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "staged.txt".to_owned(),
        ];
        // The requested command must not be able to start at all. Use a command
        // that would create a sentinel so "command started" is observable.
        let attempted = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!("printf started > '{}'", sentinel.display()),
        ];

        let attempt = sandbox::run_tracked_with_path(
            &attempted,
            &fixture.root,
            &[],
            None,
            Some(path.as_os_str()),
        )
        .await;

        // The requested command never started.
        assert!(
            !sentinel.exists(),
            "requested command started despite unsupported Bubblewrap"
        );

        // The reported lifecycle must not claim the requested command started.
        let error = match attempt {
            Ok(_) => panic!(
                "unsupported Bubblewrap must be a host-owned setup failure, not a completed run"
            ),
            Err(error) => error,
        };
        assert!(
            !error.command_started,
            "requested command start must be refuted, not assumed from helper start"
        );
        assert!(!error.command_finished);

        // And no executable fallback may follow from that attempt.
        let policy = fixture.policy();
        let error = process_sandboxed_attempt_with_test_codex(
            &fixture.session.id,
            &command,
            &fixture.root,
            &policy,
            Some(fixture.before.clone()),
            Err(error),
            vec![
                fixture.codex.to_string_lossy().into_owned(),
                "-s".to_owned(),
                "read-only".to_owned(),
            ],
        )
        .await
        .expect_err("unsupported Bubblewrap must not report a successful execution");
        let payload: Value = serde_json::from_str(&error.to_string()).unwrap();
        assert_eq!(payload["command_started"], false);
        assert_ne!(payload["fallback_decision"]["action"], "EXECUTE");
        assert_ne!(payload["fallback_mode"], "EXECUTE_AUTHORIZED_OPERATION");
        assert!(
            !fixture.sentinel.exists(),
            "Codex preflight must not run for an unsupported Bubblewrap"
        );
        assert_eq!(
            fixture.index_snapshot().await,
            fixture.before,
            "unsupported Bubblewrap must not mutate the Git index"
        );
    }

    /// A model-authored requested command must not be able to manufacture a
    /// sandbox setup rejection, and with it an executable host fallback.
    ///
    /// The requested command fully controls its own stdout, stderr and exit
    /// value, so it can print exactly what the helper prints and exit exactly
    /// what the helper exits. Authority comes only from host-owned evidence.
    #[cfg(unix)]
    #[tokio::test]
    async fn requested_command_cannot_forge_setup_rejection_into_host_fallback() {
        let fixture = StageFixture::new().await;
        let policy = fixture.policy();
        let command = vec![
            "git".to_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "staged.txt".to_owned(),
        ];
        // Exactly the text and status the Linux helper uses to refuse an
        // unsupported Bubblewrap, produced by the requested command itself.
        let attempt = Ok(sandbox::Output {
            status: 126,
            stdout: String::new(),
            stderr: "Linux sandbox setup rejected: bubblewrap 0.11.1 is unsupported; install \
                     upstream bubblewrap 0.12.0 or newer (the first release fixing \
                     CVE-2026-87766)\nfatal: Unable to create '.git/index.lock': Operation not \
                     permitted"
                .to_owned(),
            // The realistic wrapper outcome: the executed process finished, the
            // requested command's start is not host-proven.
            command_start: sandbox::CommandStart::Unproven,
        });

        let error = process_sandboxed_attempt_with_test_codex(
            &fixture.session.id,
            &command,
            &fixture.root,
            &policy,
            Some(fixture.before.clone()),
            attempt,
            vec![
                fixture.codex.to_string_lossy().into_owned(),
                "-s".to_owned(),
                "read-only".to_owned(),
            ],
        )
        .await
        .expect_err("a forged setup rejection must not be reported as success");
        let payload: Value = serde_json::from_str(&error.to_string()).unwrap();
        assert_eq!(payload["command_started"], false);
        assert_eq!(payload["command_start_proof"], "UNPROVEN");
        assert_ne!(payload["failure_class"], "PLATFORM_SAFETY");
        assert_ne!(payload["failure_class"], "SANDBOX_SETUP");
        assert_ne!(payload["fallback_decision"]["action"], "EXECUTE");
        assert_eq!(
            payload["fallback_decision"]["reason_code"],
            "FALLBACK_DENIED_LIFECYCLE"
        );
        assert!(
            !fixture.sentinel.exists(),
            "forged output must not reach the Codex preflight"
        );
        assert_eq!(
            fixture.index_snapshot().await,
            fixture.before,
            "forged output must not reach host Git"
        );
    }

    #[tokio::test]
    async fn missing_local_approval_causes_no_index_or_codex_side_effect() {
        let fixture = StageFixture::new().await;
        let result = run_test_stage_without_approval(&fixture).await;
        assert!(result.is_err());
        assert_eq!(fixture.index_snapshot().await, fixture.before);
        assert!(!fixture.sentinel.exists());
    }

    #[tokio::test]
    async fn denied_local_approval_causes_no_index_or_codex_side_effect() {
        let fixture = StageFixture::new().await;
        let result = run_test_stage_with_approval(&fixture, "deny\n").await;
        assert!(result.is_err());
        assert_eq!(fixture.index_snapshot().await, fixture.before);
        assert!(!fixture.sentinel.exists());
    }

    #[tokio::test]
    async fn malformed_local_approval_causes_no_index_or_codex_side_effect() {
        let fixture = StageFixture::new().await;
        let result = run_test_stage_with_approval(&fixture, "maybe\n").await;
        assert!(result.is_err());
        assert_eq!(fixture.index_snapshot().await, fixture.before);
        assert!(!fixture.sentinel.exists());
    }

    #[tokio::test]
    async fn allowed_local_approval_runs_read_only_preflight_and_verified_staging() {
        let fixture = StageFixture::new().await;
        run_test_stage_with_approval(&fixture, "allow\n")
            .await
            .unwrap();
        assert!(fixture.sentinel.exists());
        let after = fixture.index_snapshot().await;
        assert_ne!(after, fixture.before);
        assert!(after.contains("staged.txt"));
        assert!(!after.contains("bystander.txt"));
        assert_eq!(
            std::fs::read_to_string(&fixture.staged_path).unwrap(),
            fixture.staged_bytes
        );
        assert_eq!(
            std::fs::read_to_string(&fixture.bystander_path).unwrap(),
            fixture.bystander_bytes
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_preflight_state_change_is_rejected_before_staging() {
        let fixture = StageFixture::new().await;
        let marker = fixture.root.join("filter.marker");
        let git = fixture.stage.trusted_executable.display().to_string();
        let marker_text = marker.display().to_string();
        std::fs::write(
            &fixture.codex,
            format!(
                "#!/bin/sh\n/bin/cat > /dev/null\n'{git}' config --local filter.marker.clean \"touch '{marker_text}'\"\nprintf '%s\\n' '*.txt filter=marker' > .gitattributes\nexit 0\n"
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fixture.codex).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fixture.codex, permissions).unwrap();
        let listener = bind_test_approval_socket(&fixture.session.id).await;
        let server = tokio::spawn(serve_test_approval_with_stage_detail(
            listener,
            "allow\n",
            fixture.stage.clone(),
            fixture.root.clone(),
        ));
        let policy = fixture.policy();
        let command = vec![
            "git".to_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "staged.txt".to_owned(),
        ];
        let execution = tokio::time::timeout(
            TEST_APPROVAL_TIMEOUT,
            process_sandboxed_attempt_with_test_codex(
                &fixture.session.id,
                &command,
                &fixture.root,
                &policy,
                Some(fixture.before.clone()),
                Ok(sandbox::Output {
                    status: 128,
                    stdout: String::new(),
                    stderr: "fatal: Unable to create '.git/index.lock': Operation not permitted"
                        .to_owned(),
                    // These tests model a sandboxed `git add` that provably
                    // started and then failed on a sandbox write restriction.
                    command_start: sandbox::CommandStart::Proven,
                }),
                vec![
                    fixture.codex.to_string_lossy().into_owned(),
                    "-s".to_owned(),
                    "read-only".to_owned(),
                ],
            ),
        );
        let server_wait = tokio::time::timeout(TEST_APPROVAL_TIMEOUT, server);
        let (result, server_result) = tokio::join!(execution, server_wait);
        server_result
            .expect("approval server join timed out")
            .expect("approval server task failed");
        let error = result
            .expect("stage execution timed out")
            .expect_err("state change must block staging");
        let payload: Value = serde_json::from_str(&error.to_string()).unwrap();
        assert_eq!(payload["side_effect_state"], "CONFIRMED_NOT_PERFORMED");
        assert_eq!(payload["budget_locked"], false);
        assert_eq!(fixture.index_snapshot().await, fixture.before);
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn approval_wait_target_substitution_is_rejected_before_codex() {
        let fixture = StageFixture::new().await;
        let outside = fixture.root.join("outside-target");
        std::fs::write(&outside, "outside").unwrap();
        let target = fixture.staged_path.clone();
        let session_id = fixture.session.id.clone();
        let stage = fixture.stage.clone();
        let listener = bind_test_approval_socket(&session_id).await;
        let server = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(TEST_APPROVAL_TIMEOUT, listener.accept())
                .await
                .expect("approval server accept timed out")
                .expect("approval server accept failed");
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            let read = tokio::time::timeout(TEST_APPROVAL_TIMEOUT, reader.read_line(&mut line))
                .await
                .expect("approval server read timed out")
                .expect("approval server read failed");
            assert!(read > 0);
            assert!(line.contains(&stage.trusted_executable.display().to_string()));
            assert!(line.contains("staged.txt"));
            std::fs::remove_file(&target).unwrap();
            std::os::unix::fs::symlink(&outside, &target).unwrap();
            tokio::time::timeout(TEST_APPROVAL_TIMEOUT, stream.write_all(b"allow\n"))
                .await
                .expect("approval server write timed out")
                .expect("approval server write failed");
            tokio::time::timeout(TEST_APPROVAL_TIMEOUT, stream.flush())
                .await
                .expect("approval server flush timed out")
                .expect("approval server flush failed");
        });
        let result = tokio::time::timeout(
            TEST_APPROVAL_TIMEOUT,
            run_test_stage_without_approval(&fixture),
        )
        .await
        .expect("stage execution timed out");
        tokio::time::timeout(TEST_APPROVAL_TIMEOUT, server)
            .await
            .expect("approval server join timed out")
            .expect("approval server task failed");
        assert!(result.is_err());
        assert_eq!(fixture.index_snapshot().await, fixture.before);
        assert!(!fixture.sentinel.exists());
        assert!(
            std::fs::symlink_metadata(&fixture.staged_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn absolute_and_relative_fake_git_cannot_validate_as_fallback_identity() {
        let fixture = FakeGit::new();
        let absolute = [
            fixture.git_path().to_string_lossy().into_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "safe.txt".to_owned(),
        ];
        let relative = [
            "./git".to_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "safe.txt".to_owned(),
        ];

        let trusted = host_git_path().unwrap();
        assert!(validate_git_executable(&absolute[0], &trusted).is_err());
        assert!(validate_git_executable(&relative[0], &trusted).is_err());
    }

    #[tokio::test]
    async fn repository_scope_rejects_a_permitted_subdirectory() {
        let root = std::env::temp_dir().join(format!("local-mcp-repo-scope-{}", Uuid::new_v4()));
        let subdirectory = root.join("nested");
        std::fs::create_dir_all(&subdirectory).unwrap();
        initialize_test_repo(&root).await;
        std::fs::write(subdirectory.join("safe.txt"), "safe").unwrap();
        let operation = fallback::OperationIntent {
            kind: fallback::OperationType::GitStagePaths,
            operation_id: None,
            paths: vec!["safe.txt".to_owned()],
            argv: vec![],
            source: None,
            destination: None,
            target: None,
            create_only: false,
            force: false,
            attempt_budget_remaining: 1,
            side_effect_budget_remaining: 1,
        };
        let command = vec![
            "git".to_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "safe.txt".to_owned(),
        ];
        assert!(
            GitStagePaths::validate(
                &operation,
                &command,
                &subdirectory,
                std::slice::from_ref(&subdirectory),
            )
            .await
            .is_err()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn linked_worktree_common_gitdir_outside_permitted_roots_is_rejected() {
        let base = std::env::temp_dir().join(format!("local-mcp-common-git-{}", Uuid::new_v4()));
        let repository = base.join("repository");
        let worktree = base.join("worktree");
        let git = initialize_test_repo(&repository).await;
        let tracked = repository.join("tracked.txt");
        std::fs::write(&tracked, "tracked").unwrap();
        let add = run_test_git(
            &git,
            &repository,
            &["add".to_owned(), "tracked.txt".to_owned()],
        )
        .await;
        assert_eq!(add.status, 0, "{}", add.stderr);
        let commit = run_test_git(
            &git,
            &repository,
            &[
                "-c".to_owned(),
                "user.name=Finding A".to_owned(),
                "-c".to_owned(),
                "user.email=finding-a@example.invalid".to_owned(),
                "commit".to_owned(),
                "--quiet".to_owned(),
                "-m".to_owned(),
                "init".to_owned(),
            ],
        )
        .await;
        assert_eq!(commit.status, 0, "{}", commit.stderr);
        let add_worktree = run_test_git(
            &git,
            &repository,
            &[
                "worktree".to_owned(),
                "add".to_owned(),
                "--quiet".to_owned(),
                "--detach".to_owned(),
                worktree.to_string_lossy().into_owned(),
                "HEAD".to_owned(),
            ],
        )
        .await;
        assert_eq!(add_worktree.status, 0, "{}", add_worktree.stderr);
        let worktree_git_dir = run_test_git(
            &git,
            &worktree,
            &["rev-parse".to_owned(), "--absolute-git-dir".to_owned()],
        )
        .await;
        assert_eq!(worktree_git_dir.status, 0, "{}", worktree_git_dir.stderr);
        let worktree_git_dir = std::fs::canonicalize(worktree_git_dir.stdout.trim()).unwrap();
        std::fs::write(worktree.join("safe.txt"), "safe").unwrap();
        let operation = fallback::OperationIntent {
            kind: fallback::OperationType::GitStagePaths,
            operation_id: None,
            paths: vec!["safe.txt".to_owned()],
            argv: vec![],
            source: None,
            destination: None,
            target: None,
            create_only: false,
            force: false,
            attempt_budget_remaining: 1,
            side_effect_budget_remaining: 1,
        };
        let command = vec![
            "git".to_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "safe.txt".to_owned(),
        ];
        let roots = [worktree.clone(), worktree_git_dir];
        assert!(
            GitStagePaths::validate(&operation, &command, &worktree, &roots)
                .await
                .is_err()
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn linked_worktree_filter_configuration_is_rejected_without_running_filter() {
        let base =
            std::env::temp_dir().join(format!("local-mcp-worktree-filter-{}", Uuid::new_v4()));
        let repository = base.join("repository");
        let worktree = base.join("worktree");
        let git = initialize_test_repo(&repository).await;
        let tracked = repository.join("tracked.txt");
        std::fs::write(&tracked, "tracked").unwrap();
        let add = run_test_git(
            &git,
            &repository,
            &["add".to_owned(), "tracked.txt".to_owned()],
        )
        .await;
        assert_eq!(add.status, 0, "{}", add.stderr);
        let commit = run_test_git(
            &git,
            &repository,
            &[
                "-c".to_owned(),
                "user.name=Finding A".to_owned(),
                "-c".to_owned(),
                "user.email=finding-a@example.invalid".to_owned(),
                "commit".to_owned(),
                "--quiet".to_owned(),
                "-m".to_owned(),
                "init".to_owned(),
            ],
        )
        .await;
        assert_eq!(commit.status, 0, "{}", commit.stderr);
        let enable_worktree_config = run_test_git(
            &git,
            &repository,
            &[
                "config".to_owned(),
                "extensions.worktreeConfig".to_owned(),
                "true".to_owned(),
            ],
        )
        .await;
        assert_eq!(
            enable_worktree_config.status, 0,
            "{}",
            enable_worktree_config.stderr
        );
        let add_worktree = run_test_git(
            &git,
            &repository,
            &[
                "worktree".to_owned(),
                "add".to_owned(),
                "--quiet".to_owned(),
                "--detach".to_owned(),
                worktree.to_string_lossy().into_owned(),
                "HEAD".to_owned(),
            ],
        )
        .await;
        assert_eq!(add_worktree.status, 0, "{}", add_worktree.stderr);
        let marker = worktree.join("filter.marker");
        let configure = run_test_git(
            &git,
            &worktree,
            &[
                "config".to_owned(),
                "--worktree".to_owned(),
                "filter.marker.clean".to_owned(),
                format!("touch '{}'", marker.display()),
            ],
        )
        .await;
        assert_eq!(configure.status, 0, "{}", configure.stderr);
        std::fs::write(worktree.join(".gitattributes"), "*.txt filter=marker\n").unwrap();
        std::fs::write(worktree.join("safe.txt"), "safe").unwrap();
        let worktree_git_dir = run_test_git(
            &git,
            &worktree,
            &["rev-parse".to_owned(), "--absolute-git-dir".to_owned()],
        )
        .await;
        assert_eq!(worktree_git_dir.status, 0, "{}", worktree_git_dir.stderr);
        let worktree_git_dir = std::fs::canonicalize(worktree_git_dir.stdout.trim()).unwrap();
        let common_git_dir = std::fs::canonicalize(repository.join(".git")).unwrap();
        let operation = fallback::OperationIntent {
            kind: fallback::OperationType::GitStagePaths,
            operation_id: None,
            paths: vec!["safe.txt".to_owned()],
            argv: vec![],
            source: None,
            destination: None,
            target: None,
            create_only: false,
            force: false,
            attempt_budget_remaining: 1,
            side_effect_budget_remaining: 1,
        };
        let command = vec![
            "git".to_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "safe.txt".to_owned(),
        ];
        let result = GitStagePaths::validate(
            &operation,
            &command,
            &worktree,
            &[worktree.clone(), worktree_git_dir, common_git_dir],
        )
        .await;
        assert!(result.is_err());
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn primary_sandbox_revalidates_after_pre_primary_mutation() {
        let fixture = StageFixture::new().await;
        let outside = fixture.root.join("outside-target");
        std::fs::write(&outside, "outside").unwrap();
        let target = fixture.staged_path.clone();
        let args = json!({
            "command": ["git", "add", "--", "staged.txt"],
            "operation": {
                "type": "git_stage_paths",
                "paths": ["staged.txt"]
            }
        });
        let before = fixture.index_snapshot().await;
        let (_rendered_command, handle) = spawn_sandboxed_command_with_pre_primary_failure(
            "execute",
            &args,
            &fixture.session,
            move || {
                std::fs::remove_file(&target)?;
                std::os::unix::fs::symlink(&outside, &target)?;
                Ok::<(), anyhow::Error>(())
            },
        )
        .await
        .expect("primary task must spawn before the inner mutation seam runs");
        let result = handle
            .await
            .expect("primary task must not panic")
            .expect_err("inner Git revalidation must reject the mutation");
        assert!(result.to_string().contains("symlink or reparse point"));
        assert_eq!(fixture.index_snapshot().await, before);
        assert!(!fixture.sentinel.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn spawn_rejects_linked_worktree_filter_before_primary_git_add() {
        let base = std::env::temp_dir().join(format!(
            "local-mcp-spawn-worktree-filter-{}",
            Uuid::new_v4()
        ));
        let repository = base.join("repository");
        let worktree = base.join("worktree");
        let git = initialize_test_repo(&repository).await;
        let tracked = repository.join("tracked.txt");
        std::fs::write(&tracked, "tracked").unwrap();
        let add = run_test_git(
            &git,
            &repository,
            &["add".to_owned(), "tracked.txt".to_owned()],
        )
        .await;
        assert_eq!(add.status, 0, "{}", add.stderr);
        let commit = run_test_git(
            &git,
            &repository,
            &[
                "-c".to_owned(),
                "user.name=Finding A".to_owned(),
                "-c".to_owned(),
                "user.email=finding-a@example.invalid".to_owned(),
                "commit".to_owned(),
                "--quiet".to_owned(),
                "-m".to_owned(),
                "init".to_owned(),
            ],
        )
        .await;
        assert_eq!(commit.status, 0, "{}", commit.stderr);
        let enable_worktree_config = run_test_git(
            &git,
            &repository,
            &[
                "config".to_owned(),
                "extensions.worktreeConfig".to_owned(),
                "true".to_owned(),
            ],
        )
        .await;
        assert_eq!(
            enable_worktree_config.status, 0,
            "{}",
            enable_worktree_config.stderr
        );
        let add_worktree = run_test_git(
            &git,
            &repository,
            &[
                "worktree".to_owned(),
                "add".to_owned(),
                "--quiet".to_owned(),
                "--detach".to_owned(),
                worktree.to_string_lossy().into_owned(),
                "HEAD".to_owned(),
            ],
        )
        .await;
        assert_eq!(add_worktree.status, 0, "{}", add_worktree.stderr);
        let marker = worktree.join("filter.marker");
        let configure = run_test_git(
            &git,
            &worktree,
            &[
                "config".to_owned(),
                "--worktree".to_owned(),
                "filter.marker.clean".to_owned(),
                format!("touch '{}'", marker.display()),
            ],
        )
        .await;
        assert_eq!(configure.status, 0, "{}", configure.stderr);
        std::fs::write(worktree.join(".gitattributes"), "*.txt filter=marker\n").unwrap();
        std::fs::write(worktree.join("safe.txt"), "safe").unwrap();
        let worktree_git_dir = run_test_git(
            &git,
            &worktree,
            &["rev-parse".to_owned(), "--absolute-git-dir".to_owned()],
        )
        .await;
        assert_eq!(worktree_git_dir.status, 0, "{}", worktree_git_dir.stderr);
        let worktree_git_dir = std::fs::canonicalize(worktree_git_dir.stdout.trim()).unwrap();
        let common_git_dir = std::fs::canonicalize(repository.join(".git")).unwrap();
        let before = run_test_git(
            &git,
            &worktree,
            &[
                "diff".to_owned(),
                "--cached".to_owned(),
                "--name-only".to_owned(),
            ],
        )
        .await
        .stdout;
        let session = config::Session {
            id: format!("spawn-worktree-filter-{}", Uuid::new_v4()),
            cwd: worktree.clone(),
            permitted_directories: vec![worktree.clone(), worktree_git_dir, common_git_dir],
        };
        let args = json!({
            "command": ["git", "add", "--", "safe.txt"],
            "operation": {
                "type": "git_stage_paths",
                "paths": ["safe.txt"]
            }
        });
        let result = spawn_sandboxed_command("execute", &args, &session).await;
        if let Ok((_, handle)) = result {
            let _ = handle.await;
            panic!("linked worktree filter validation must fail before primary Git");
        }
        let after = run_test_git(
            &git,
            &worktree,
            &[
                "diff".to_owned(),
                "--cached".to_owned(),
                "--name-only".to_owned(),
            ],
        )
        .await
        .stdout;
        assert_eq!(before, after);
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repository_filter_configuration_is_rejected_without_running_filter() {
        let root = std::env::temp_dir().join(format!("local-mcp-filter-{}", Uuid::new_v4()));
        let git = initialize_test_repo(&root).await;
        let marker = root.join("filter.marker");
        let filter = format!("touch '{}'", marker.display());
        let configure = run_test_git(
            &git,
            &root,
            &[
                "config".to_owned(),
                "--local".to_owned(),
                "filter.marker.clean".to_owned(),
                filter,
            ],
        )
        .await;
        assert_eq!(configure.status, 0, "{}", configure.stderr);
        std::fs::write(root.join(".gitattributes"), "*.txt filter=marker\n").unwrap();
        std::fs::write(root.join("safe.txt"), "safe").unwrap();
        let operation = fallback::OperationIntent {
            kind: fallback::OperationType::GitStagePaths,
            operation_id: None,
            paths: vec!["safe.txt".to_owned()],
            argv: vec![],
            source: None,
            destination: None,
            target: None,
            create_only: false,
            force: false,
            attempt_budget_remaining: 1,
            side_effect_budget_remaining: 1,
        };
        let command = vec![
            "git".to_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "safe.txt".to_owned(),
        ];
        assert!(
            GitStagePaths::validate(&operation, &command, &root, std::slice::from_ref(&root))
                .await
                .is_err()
        );
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn trusted_git_commands_disable_external_fsmonitor() {
        let root = std::env::temp_dir().join(format!("local-mcp-fsmonitor-{}", Uuid::new_v4()));
        let git = initialize_test_repo(&root).await;
        let marker = root.join("fsmonitor.marker");
        let monitor = format!("touch '{}'", marker.display());
        let configure = run_test_git(
            &git,
            &root,
            &[
                "config".to_owned(),
                "--local".to_owned(),
                "core.fsmonitor".to_owned(),
                monitor,
            ],
        )
        .await;
        assert_eq!(configure.status, 0, "{}", configure.stderr);
        std::fs::write(root.join("safe.txt"), "safe").unwrap();
        let operation = fallback::OperationIntent {
            kind: fallback::OperationType::GitStagePaths,
            operation_id: None,
            paths: vec!["safe.txt".to_owned()],
            argv: vec![],
            source: None,
            destination: None,
            target: None,
            create_only: false,
            force: false,
            attempt_budget_remaining: 1,
            side_effect_budget_remaining: 1,
        };
        let command = vec![
            "git".to_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "safe.txt".to_owned(),
        ];
        let stage =
            GitStagePaths::validate(&operation, &command, &root, std::slice::from_ref(&root))
                .await
                .unwrap();
        let before = capture_git_index_snapshot(&stage, &root).await.unwrap();
        for trusted_command in [stage.index_snapshot_command(), stage.stage_command()] {
            assert!(
                trusted_command
                    .windows(2)
                    .any(|window| { window[0] == "-c" && window[1] == "core.hooksPath=/dev/null" })
            );
            assert!(
                trusted_command
                    .windows(2)
                    .any(|window| { window[0] == "-c" && window[1] == "core.fsmonitor=false" })
            );
        }
        execute_exact_git_stage(
            HostExecutionAuthority::granted(),
            &stage,
            &root,
            Some(&before),
        )
        .await
        .unwrap();
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn linked_gitdir_outside_permitted_root_is_rejected() {
        let base = std::env::temp_dir().join(format!("local-mcp-linked-git-{}", Uuid::new_v4()));
        let repository = base.join("repository");
        let git_dir = base.join("git-dir");
        let subdirectory = repository.join("nested");
        std::fs::create_dir_all(&subdirectory).unwrap();
        initialize_test_repo(&repository).await;
        std::fs::rename(repository.join(".git"), &git_dir).unwrap();
        std::fs::write(
            repository.join(".git"),
            format!("gitdir: {}\n", git_dir.to_str().unwrap()),
        )
        .unwrap();
        std::fs::write(subdirectory.join("safe.txt"), "safe").unwrap();
        let operation = fallback::OperationIntent {
            kind: fallback::OperationType::GitStagePaths,
            operation_id: None,
            paths: vec!["safe.txt".to_owned()],
            argv: vec![],
            source: None,
            destination: None,
            target: None,
            create_only: false,
            force: false,
            attempt_budget_remaining: 1,
            side_effect_budget_remaining: 1,
        };
        let command = vec![
            "git".to_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "safe.txt".to_owned(),
        ];
        assert!(
            GitStagePaths::validate(
                &operation,
                &command,
                &subdirectory,
                std::slice::from_ref(&subdirectory),
            )
            .await
            .is_err()
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn directory_target_is_rejected_before_index_snapshot_or_staging() {
        let root =
            std::env::temp_dir().join(format!("local-mcp-directory-target-{}", Uuid::new_v4()));
        let git = initialize_test_repo(&root).await;
        let directory = root.join("target");
        std::fs::create_dir_all(&directory).unwrap();
        let descendant = directory.join("child.txt");
        std::fs::write(&descendant, "unchanged").unwrap();
        let operation = fallback::OperationIntent {
            kind: fallback::OperationType::GitStagePaths,
            operation_id: None,
            paths: vec!["target".to_owned()],
            argv: vec![],
            source: None,
            destination: None,
            target: None,
            create_only: false,
            force: false,
            attempt_budget_remaining: 1,
            side_effect_budget_remaining: 1,
        };
        let command = vec![
            "git".to_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "target".to_owned(),
        ];
        let before = run_test_git(
            &git,
            &root,
            &[
                "diff".to_owned(),
                "--cached".to_owned(),
                "--name-only".to_owned(),
            ],
        )
        .await
        .stdout;
        assert!(
            GitStagePaths::validate(&operation, &command, &root, std::slice::from_ref(&root))
                .await
                .is_err()
        );
        let after = run_test_git(
            &git,
            &root,
            &[
                "diff".to_owned(),
                "--cached".to_owned(),
                "--name-only".to_owned(),
            ],
        )
        .await
        .stdout;
        assert_eq!(before, after);
        assert_eq!(std::fs::read_to_string(&descendant).unwrap(), "unchanged");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_components_are_rejected_without_following_them() {
        let base =
            std::env::temp_dir().join(format!("local-mcp-symlink-target-{}", Uuid::new_v4()));
        let root = base.join("repository");
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let git = initialize_test_repo(&root).await;
        std::fs::write(outside.join("child.txt"), "outside").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        std::os::unix::fs::symlink(outside.join("child.txt"), root.join("alias.txt")).unwrap();
        for path in ["link/child.txt", "alias.txt"] {
            let operation = fallback::OperationIntent {
                kind: fallback::OperationType::GitStagePaths,
                operation_id: None,
                paths: vec![path.to_owned()],
                argv: vec![],
                source: None,
                destination: None,
                target: None,
                create_only: false,
                force: false,
                attempt_budget_remaining: 1,
                side_effect_budget_remaining: 1,
            };
            let command = vec![
                "git".to_owned(),
                "add".to_owned(),
                "--".to_owned(),
                path.to_owned(),
            ];
            assert!(
                GitStagePaths::validate(&operation, &command, &root, std::slice::from_ref(&root),)
                    .await
                    .is_err(),
                "{path}"
            );
        }
        assert!(git.is_absolute());
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn host_resolved_git_snapshot_works_in_temp_repo() {
        let root = std::env::temp_dir().join(format!("local-mcp-host-git-{}", Uuid::new_v4()));
        initialize_test_repo(&root).await;

        let operation = fallback::OperationIntent {
            kind: fallback::OperationType::GitStagePaths,
            operation_id: None,
            paths: vec!["safe.txt".to_owned()],
            argv: vec![],
            source: None,
            destination: None,
            target: None,
            create_only: false,
            force: false,
            attempt_budget_remaining: 1,
            side_effect_budget_remaining: 1,
        };
        let command = vec![
            "git".to_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "safe.txt".to_owned(),
        ];
        let stage =
            GitStagePaths::validate(&operation, &command, &root, std::slice::from_ref(&root))
                .await
                .unwrap();
        let snapshot = capture_git_index_snapshot(&stage, &root)
            .await
            .expect("host-resolved Git snapshot should succeed");
        assert!(snapshot.is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn exact_git_argv_uses_the_trusted_host_executable() {
        let root = std::env::temp_dir().join(format!("local-mcp-git-argv-{}", Uuid::new_v4()));
        let host_git = initialize_test_repo(&root).await;
        let alias = root.join("git");
        std::os::unix::fs::symlink(&host_git, &alias).unwrap();
        let operation = fallback::OperationIntent {
            kind: fallback::OperationType::GitStagePaths,
            operation_id: None,
            paths: vec!["safe.txt".to_owned()],
            argv: vec![],
            source: None,
            destination: None,
            target: None,
            create_only: false,
            force: false,
            attempt_budget_remaining: 1,
            side_effect_budget_remaining: 1,
        };
        let command = vec![
            alias.to_string_lossy().into_owned(),
            "add".to_owned(),
            "--".to_owned(),
            "safe.txt".to_owned(),
        ];
        let stage =
            GitStagePaths::validate(&operation, &command, &root, std::slice::from_ref(&root))
                .await
                .unwrap();
        let exact = stage.stage_command();
        assert_eq!(exact[0], host_git.to_str().unwrap());
        assert_ne!(exact[0], alias.to_string_lossy().as_ref());
        assert_eq!(
            &exact[1..5],
            &[
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false"
            ]
        );
        assert_eq!(&exact[5..], &["add", "--", "safe.txt"]);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn absolute_fake_git_is_not_run_for_unauthorized_pre_approval_snapshot() {
        let fixture = FakeGit::new();
        assert_pre_approval_git_is_not_run(
            &fixture,
            fixture.git_path().to_string_lossy().into_owned(),
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn absolute_fake_git_is_not_run_for_authorized_pre_approval_snapshot() {
        let fixture = FakeGit::new();
        assert_pre_approval_git_is_not_run(
            &fixture,
            fixture.git_path().to_string_lossy().into_owned(),
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn relative_fake_git_is_not_run_for_unauthorized_pre_approval_snapshot() {
        let fixture = FakeGit::new();
        assert_pre_approval_git_is_not_run(&fixture, "./git".to_owned(), false).await;
    }

    #[tokio::test]
    async fn relative_fake_git_is_not_run_for_authorized_pre_approval_snapshot() {
        let fixture = FakeGit::new();
        assert_pre_approval_git_is_not_run(&fixture, "./git".to_owned(), true).await;
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::path::Path;

    use uuid::Uuid;

    use super::validate_stage_target;

    fn create_junction(link: &Path, target: &Path) {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status()
            .expect("failed to spawn mklink /J");
        assert!(status.success(), "mklink /J failed for {}", link.display());
    }

    #[test]
    fn validate_stage_target_rejects_intermediate_junction_to_outside() {
        let base =
            std::env::temp_dir().join(format!("local-mcp-stage-junction-{}", Uuid::new_v4()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        let junction = root.join("junction");
        create_junction(&junction, &outside);

        assert!(validate_stage_target(&root, "junction/secret.txt").is_err());

        let regular_directory = root.join("regular");
        std::fs::create_dir_all(&regular_directory).unwrap();
        std::fs::write(regular_directory.join("file.txt"), "regular").unwrap();
        assert!(validate_stage_target(&root, "regular/file.txt").is_ok());
        assert!(validate_stage_target(&root, "missing.txt").is_ok());

        std::fs::remove_dir_all(base).unwrap();
    }
}
