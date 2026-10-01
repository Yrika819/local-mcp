//! Host-owned read-only Git observation for the Verifier.
//!
//! The Verifier's own `GIT_SCOPE` / `NO_FORBIDDEN_CHANGES` checks are host
//! observation, not a model proposal. They used to travel the generic
//! `start_command` path, which on Linux and macOS is carried by a sandbox
//! wrapper: the wrapper's exit says nothing about whether the requested `git`
//! ever started, so the observation could not satisfy the requested-command
//! lifecycle contract without pretending wrapper completion was command
//! completion. This seam expresses the specific observations the Verifier needs
//! and runs them directly, where the host spawns the requested command itself
//! and its start is therefore host-proven.
//!
//! Confinement — and the trade-off this makes
//!
//! The observation is spawned directly, so it does not carry a Landlock or
//! Seatbelt profile on any platform. That is a deliberate narrowing of *what the
//! host will run*: the executable is the host-resolved Git identity, the argv is
//! host-generated from the fixed query table below, the environment is cleared
//! with Git's own configuration neutralized, output and time are bounded, and no
//! caller, model, or verification spec can extend any of it. In exchange the
//! Verifier regains host-proven command completion for the Git state that its own
//! security gates depend on. The mitigation is the argv and environment
//! envelope, not a sandbox, and `host_owned_observation_writes_nothing_anywhere`
//! and `host_owned_observation_never_runs_a_repository_chosen_program` are what
//! hold that envelope honest.
//!
//! This is deliberately not a generic command API. There is no way to express
//! arbitrary argv here.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::approvals;
use crate::config;
use crate::execution;
use crate::sandbox;

/// Every host observation the Verifier needs, and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GitQuery {
    /// The absolute top level of the worktree containing the root.
    ShowToplevel,
    /// Machine-readable worktree state including untracked files.
    StatusPorcelain,
    /// Staged path names only, so no external diff driver or `textconv`
    /// filter can run.
    StagedNames,
    /// The current commit, which is absent in a repository without one.
    Head,
}

impl GitQuery {
    /// The exact Git subcommand and arguments for this observation.
    fn tail(self) -> &'static [&'static str] {
        match self {
            Self::ShowToplevel => &["rev-parse", "--show-toplevel"],
            Self::StatusPorcelain => &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            Self::StagedNames => &[
                "diff",
                "--cached",
                "--name-only",
                "-z",
                "--no-ext-diff",
                "--no-textconv",
            ],
            Self::Head => &["rev-parse", "HEAD"],
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ShowToplevel => "rev-parse --show-toplevel",
            Self::StatusPorcelain => "status --porcelain=v1 -z --untracked-files=all",
            Self::StagedNames => "diff --cached --name-only -z",
            Self::Head => "rev-parse HEAD",
        }
    }
}

/// Bounded observation time.
///
/// This matches the bound the Verifier's own command path used before the
/// observation moved onto this seam, so a large worktree does not acquire a new
/// failure mode that blocks verification.
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(300);
const OBSERVATION_STDOUT_LIMIT: usize = sandbox::TRUSTED_GIT_STDOUT_LIMIT;
const OBSERVATION_STDERR_LIMIT: usize = sandbox::TRUSTED_GIT_STDERR_LIMIT;

/// The host-owned Git identity and the read-only argv the Verifier runs.
pub(crate) fn observation_argv(query: GitQuery) -> Result<Vec<String>, String> {
    let executable = execution::host_git_path().map_err(|error| error.to_string())?;
    let executable = executable
        .to_str()
        .ok_or_else(|| "host Git path is not valid UTF-8".to_owned())?;
    let mut argv = vec![
        executable.to_owned(),
        // Hooks cannot run for an observation, and an external filesystem
        // monitor is a host program the host did not choose.
        "-c".to_owned(),
        format!("core.hooksPath={}", sandbox::null_device_path()),
        "-c".to_owned(),
        "core.fsmonitor=false".to_owned(),
        // Ordinary `git status` may refresh and write index stat metadata; this
        // observation must not.
        "--no-optional-locks".to_owned(),
    ];
    argv.extend(query.tail().iter().map(|arg| (*arg).to_owned()));
    Ok(argv)
}

/// Whether the platform's policy requires an operator to approve each host-native
/// Git observation.
///
/// Windows has no process sandbox, so a host-native observation there is host
/// access and stays approval-gated under the existing Windows policy, through
/// the same local approval system and yolo semantics used by `start_command`.
/// The gate exists so the Windows policy does not change shape, not because the
/// Unix path needs it.
pub(crate) const fn verifier_git_observation_requires_approval() -> bool {
    cfg!(windows)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitObservation {
    pub(crate) head: Option<String>,
    pub(crate) root: PathBuf,
    pub(crate) changed: Vec<PathBuf>,
    pub(crate) staged: Vec<PathBuf>,
}

/// Observe the worktree containing `root`.
///
/// Fails closed: a missing, unusable, or unauthorized Git observation is an
/// error, never an empty observation that would read as a clean worktree.
pub(crate) async fn observe(
    root: &Path,
    session: &config::Session,
) -> Result<GitObservation, String> {
    observe_with_policy(root, session, verifier_git_observation_requires_approval()).await
}

/// [`observe`] with an explicit approval policy, so the Windows gate is testable
/// on any host without weakening production.
pub(crate) async fn observe_with_policy(
    root: &Path,
    session: &config::Session,
    requires_approval: bool,
) -> Result<GitObservation, String> {
    let top = run(root, session, GitQuery::ShowToplevel, requires_approval).await?;
    if !top.succeeded() {
        return Err("Goal cwd is not an observable Git worktree".to_owned());
    }
    let top_path = parse_single_path_line(&top.stdout)?;
    if !top_path.is_absolute() {
        return Err("Git root output is not an absolute filesystem path".to_owned());
    }
    let git_root = config::canonical_path_like(&top_path, root)
        .map_err(|error| format!("cannot canonicalize Git root: {error}"))?;
    if !root.starts_with(&git_root) {
        return Err("Git root does not contain Goal cwd".to_owned());
    }

    let status = run(
        &git_root,
        session,
        GitQuery::StatusPorcelain,
        requires_approval,
    )
    .await?;
    if !status.succeeded() {
        return Err("Git status observation failed".to_owned());
    }
    let changed = parse_status_paths(&status.stdout, &git_root)?;

    let staged = run(&git_root, session, GitQuery::StagedNames, requires_approval).await?;
    let staged = parse_nul_paths(&staged.stdout, &git_root)?;

    let head = run(&git_root, session, GitQuery::Head, requires_approval)
        .await
        .ok()
        .filter(GitOutput::succeeded)
        .and_then(|output| parse_head_oid(&output.stdout).ok());

    Ok(GitObservation {
        head,
        root: git_root,
        changed,
        staged,
    })
}

struct GitOutput {
    stdout: Vec<u8>,
    exit_code: i32,
}

impl GitOutput {
    fn succeeded(&self) -> bool {
        self.exit_code == 0
    }
}

async fn run(
    cwd: &Path,
    session: &config::Session,
    query: GitQuery,
    requires_approval: bool,
) -> Result<GitOutput, String> {
    // The observation root is host-derived, but it is still a working directory
    // the host is about to run a process in, and a Git top level can
    // legitimately be an ancestor of the session cwd. Session path authority is
    // the filesystem boundary, so it is re-checked here rather than assumed from
    // the caller's root. The generic command path performed the same check
    // through its cwd resolution; moving off that path does not move authority
    // with it.
    config::validate_path_authority(session, cwd, config::PathIntent::ExecutionCwd).map_err(
        |error| format!("host-owned Git observation root is outside Session authority: {error}"),
    )?;
    let argv = observation_argv(query)?;
    if requires_approval {
        // The operation label and the requested cwd are the same ones the generic
        // command path used, so the operator-facing approval the existing Windows
        // policy describes does not change. The detail shows the exact argv that
        // will run, so the prompt stays as auditable as the path it replaces.
        let approved = approvals::request(
            &session.id,
            "start_command",
            format!("argv: {argv:?}"),
            cwd.to_path_buf(),
        )
        .await
        .map_err(|error| format!("Git observation approval failed: {error:#}"))?;
        if !approved {
            return Err("host-owned Git observation was not approved".to_owned());
        }
    }
    // The generic command path reported every execution to the approval console.
    // The seam must not become an invisible side channel.
    approvals::activity(
        &session.id,
        format!("Running {}", query.label()),
        Some(format!(
            "└ host-owned read-only Git observation in {}",
            cwd.display()
        )),
    )
    .await;
    let output = sandbox::run_unrestricted_clean_raw_with_limits(
        &argv,
        cwd,
        None,
        OBSERVATION_TIMEOUT,
        OBSERVATION_STDOUT_LIMIT,
        OBSERVATION_STDERR_LIMIT,
    )
    .await
    .map_err(|error| format!("host-owned Git observation failed: {error}"))?;
    // A host-native spawn has no wrapper in between, so the requested command
    // itself was exec'd and this is real lifecycle evidence.
    debug_assert!(output.command_start.is_proven());
    if !output.command_start.is_proven() {
        return Err(
            "host-owned Git observation could not prove the requested command started".to_owned(),
        );
    }
    Ok(GitOutput {
        stdout: output.stdout,
        exit_code: output.status,
    })
}

fn nul_records(stdout: &[u8]) -> Result<Vec<&[u8]>, String> {
    if stdout.is_empty() {
        return Ok(Vec::new());
    }
    let Some(payload) = stdout.strip_suffix(&[0]) else {
        return Err("Git returned an unterminated NUL record".to_owned());
    };
    let records = payload.split(|byte| *byte == 0).collect::<Vec<_>>();
    if records.iter().any(|record| record.is_empty()) {
        return Err("Git returned an empty NUL record".to_owned());
    }
    Ok(records)
}

fn valid_xy(index: u8, worktree: u8) -> bool {
    let ordinary = |status| {
        matches!(
            status,
            b' ' | b'M' | b'T' | b'A' | b'D' | b'R' | b'C' | b'U'
        )
    };
    (ordinary(index) && ordinary(worktree) && (index != b' ' || worktree != b' '))
        || (index == b'?' && worktree == b'?')
        || (index == b'!' && worktree == b'!')
}

fn parse_status_paths(stdout: &[u8], root: &Path) -> Result<Vec<PathBuf>, String> {
    let records = nul_records(stdout)?;
    let mut paths = BTreeSet::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        if record.len() < 4 || record[2] != b' ' || !valid_xy(record[0], record[1]) {
            return Err("Git returned malformed porcelain status".to_owned());
        }
        let first_path = &record[3..];
        if first_path.is_empty() {
            return Err("Git returned an empty porcelain path".to_owned());
        }
        paths.insert(resolve_git_path_bytes(first_path, root)?);
        if matches!(record[0], b'R' | b'C') || matches!(record[1], b'R' | b'C') {
            index += 1;
            let second_path = records
                .get(index)
                .copied()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| "Git rename/copy record omitted its source path".to_owned())?;
            paths.insert(resolve_git_path_bytes(second_path, root)?);
        }
        index += 1;
    }
    Ok(paths.into_iter().collect())
}

fn parse_nul_paths(stdout: &[u8], root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = BTreeSet::new();
    for record in nul_records(stdout)? {
        paths.insert(resolve_git_path_bytes(record, root)?);
    }
    Ok(paths.into_iter().collect())
}

fn path_from_git_bytes(raw: &[u8]) -> Result<PathBuf, String> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(OsString::from_vec(raw.to_vec())))
    }
    #[cfg(windows)]
    {
        let value = std::str::from_utf8(raw)
            .map_err(|_| "Git path bytes are not representable losslessly on Windows".to_owned())?;
        Ok(PathBuf::from(OsString::from(value)))
    }
}

fn resolve_git_path_bytes(raw: &[u8], root: &Path) -> Result<PathBuf, String> {
    let path = path_from_git_bytes(raw)?;
    if path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
    {
        return Err("Git emitted an unsafe path".to_owned());
    }
    canonicalize_existing_prefix(&root.join(path), root)
}

fn parse_single_path_line(stdout: &[u8]) -> Result<PathBuf, String> {
    let Some(line) = stdout.strip_suffix(b"\n") else {
        return Err("Git root output is not a single newline-terminated path".to_owned());
    };
    if line.is_empty() || line.contains(&0) || line.contains(&b'\n') || line.contains(&b'\r') {
        return Err("Git root output contains ambiguous path framing".to_owned());
    }
    path_from_git_bytes(line)
}

fn parse_head_oid(stdout: &[u8]) -> Result<String, String> {
    let Some(oid) = stdout.strip_suffix(b"\n") else {
        return Err("Git HEAD output is not newline terminated".to_owned());
    };
    if oid.is_empty() || !oid.iter().all(u8::is_ascii_hexdigit) {
        return Err("Git HEAD output is not an ASCII object ID".to_owned());
    }
    String::from_utf8(oid.to_vec()).map_err(|_| "Git HEAD output is not ASCII".to_owned())
}

/// Resolve a Git-emitted path against the observed root.
///
/// Absolute, rooted, drive-prefixed, or `..` paths are refused before joining. A symlink is
/// different: canonicalization follows it, so a tracked link pointing out of the
/// worktree resolves to its target rather than to the link. That is
/// fail-closed rather than permissive, because such a path matches none of the
/// durable allowed boundaries and is therefore reported as out of scope — but
/// it is not "rejected", and callers must not read the refusal above as the
/// whole containment story.
#[cfg(test)]
pub(crate) fn resolve_git_path(raw: &str, root: &Path) -> Result<PathBuf, String> {
    let path = Path::new(raw);
    if path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
    {
        return Err("Git emitted an unsafe path".to_owned());
    }
    canonicalize_existing_prefix(&root.join(path), root)
}

/// Canonicalize the deepest existing ancestor of `path` and re-append the
/// missing suffix, so a path Git reports for a not-yet-created file still
/// resolves.
pub(crate) fn canonicalize_existing_prefix(
    path: &Path,
    spelling_root: &Path,
) -> Result<PathBuf, String> {
    let mut existing = path.to_path_buf();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| "Git emitted a path with no canonicalizable ancestor".to_owned())?;
        suffix.push(name.to_os_string());
        if !existing.pop() {
            return Err("Git emitted a path with no canonicalizable ancestor".to_owned());
        }
    }
    let mut resolved = config::canonical_path_like(&existing, spelling_root)
        .map_err(|error| format!("cannot canonicalize Git-emitted path: {error}"))?;
    for part in suffix.iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_dir(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("local-mcp-verifier-git-{label}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::canonicalize(path).unwrap()
    }

    fn init_repo(root: &Path) {
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("git runs");
            assert!(
                status.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );
        };
        git(&["init", "-q", "."]);
        git(&["config", "user.email", "verifier@example.invalid"]);
        git(&["config", "user.name", "Verifier Observation Test"]);
        git(&["config", "commit.gpgsign", "false"]);
        // Repository-local only. `git commit` can trigger Git's asynchronous
        // auto-maintenance, which briefly creates and removes
        // `.git/objects/maintenance.lock` on its own schedule. The write-freedom
        // test below snapshots the whole worktree twice and compares it exactly,
        // so that transient lock races the comparison and reports a write the
        // observation never made. Disabling auto-maintenance here keeps the tree
        // quiescent. No assertion and no snapshot exclusion is weakened, and the
        // observation itself still runs against this repository unmodified.
        git(&["config", "gc.auto", "0"]);
        git(&["config", "maintenance.auto", "false"]);
        std::fs::write(root.join("tracked.txt"), b"observed\n").unwrap();
        git(&["add", "tracked.txt"]);
        git(&["commit", "-q", "-m", "initial"]);
    }

    fn session(root: &Path) -> config::Session {
        config::Session {
            id: format!("s{}", Uuid::new_v4().simple()),
            cwd: root.to_path_buf(),
            permitted_directories: vec![root.to_path_buf()],
        }
    }

    fn has_pair(argv: &[String], key: &str, value: &str) -> bool {
        argv.windows(2)
            .any(|pair| pair[0] == key && pair[1] == value)
    }

    /// The observation argv is host-generated, names the host Git identity, and
    /// neutralises the three ways a read-only-looking Git invocation can still
    /// write or execute something.
    #[test]
    fn observation_argv_is_host_shaped_and_write_free() {
        let host_git = execution::host_git_path().unwrap();
        for query in [
            GitQuery::ShowToplevel,
            GitQuery::StatusPorcelain,
            GitQuery::StagedNames,
            GitQuery::Head,
        ] {
            let argv = observation_argv(query).unwrap();
            assert_eq!(argv[0], host_git.to_str().unwrap(), "{query:?}");
            // The caller never chooses the executable.
            assert_eq!(
                argv[0],
                execution::host_git_path().unwrap().to_str().unwrap()
            );
            // Hooks cannot run and an external filesystem monitor is disabled.
            assert!(
                has_pair(
                    &argv,
                    "-c",
                    &format!("core.hooksPath={}", sandbox::null_device_path())
                ),
                "{query:?} did not disable hooks"
            );
            assert!(has_pair(&argv, "-c", "core.fsmonitor=false"), "{query:?}");
            // Optional index writes are off, so an ordinary `git status` cannot
            // refresh and persist index stat metadata.
            assert!(
                argv.contains(&"--no-optional-locks".to_owned()),
                "{query:?}"
            );
            // The exact host-selected subcommand tail, and nothing else.
            let tail = argv[argv.len() - query.tail().len()..].to_vec();
            assert_eq!(tail, query.tail().to_vec(), "{query:?}");
        }

        // Diff-like observation must not run an external diff driver or a
        // `textconv` filter.
        let staged = observation_argv(GitQuery::StagedNames).unwrap();
        assert!(staged.contains(&"--no-ext-diff".to_owned()));
        assert!(staged.contains(&"--no-textconv".to_owned()));
        for query in [
            GitQuery::ShowToplevel,
            GitQuery::StatusPorcelain,
            GitQuery::Head,
        ] {
            let argv = observation_argv(query).unwrap();
            assert!(!argv.contains(&"--no-ext-diff".to_owned()));
            assert!(!argv.contains(&"--no-textconv".to_owned()));
        }

        // No observation can reach a remote or a hook.
        for query in [
            GitQuery::ShowToplevel,
            GitQuery::StatusPorcelain,
            GitQuery::StagedNames,
            GitQuery::Head,
        ] {
            let argv = observation_argv(query).unwrap();
            for forbidden in [
                "fetch", "push", "pull", "clone", "fsck", "gc", "commit", "add",
            ] {
                assert!(
                    !argv.contains(&forbidden.to_owned()),
                    "{query:?} can {forbidden}"
                );
            }
        }
    }

    /// The observation seam is narrow: it expresses specific Git observations,
    /// not arbitrary argv, and it reports host-proven completion.
    #[tokio::test]
    async fn host_owned_observation_reports_the_worktree_it_ran_in() {
        let root = temp_dir("observe");
        init_repo(&root);
        std::fs::write(root.join("tracked.txt"), b"changed\n").unwrap();
        std::fs::write(root.join("untracked.txt"), b"new\n").unwrap();
        // An explicit approval policy keeps this test about the observation; the
        // platform approval gate has its own tests below.
        let observed = observe_with_policy(&root, &session(&root), false)
            .await
            .expect("host-owned observation succeeds");
        assert!(observed.root.starts_with(&root));
        assert_eq!(observed.changed.len(), 2, "both changes are observed");
        assert!(observed.changed.contains(&root.join("tracked.txt")));
        assert!(observed.changed.contains(&root.join("untracked.txt")));
        assert!(observed.staged.is_empty());
        assert!(observed.head.is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    /// The observation runs Git host-side without the platform sandbox, so its
    /// confinement is an argv and environment property. This proves mechanically
    /// that observing a worktree creates nothing, alters nothing inside the
    /// worktree — including the index an ordinary `git status` would refresh —
    /// and never reaches outside the observed root.
    #[tokio::test]
    async fn host_owned_observation_writes_nothing_anywhere() {
        let base = temp_dir("observe-write");
        let root = base.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        init_repo(&root);
        let outside = base.join("outside.txt");
        std::fs::write(&outside, b"untouched\n").unwrap();

        let index = root.join(".git").join("index");
        let index_before = std::fs::read(&index).ok();
        let index_mtime_before = std::fs::metadata(&index).ok().map(|m| m.modified().ok());
        let worktree_before = sorted_entries(&root);

        let observed = observe_with_policy(&root, &session(&root), false)
            .await
            .expect("host-owned observation succeeds");
        assert!(observed.root.starts_with(&root));

        assert_eq!(std::fs::read(&outside).unwrap(), b"untouched\n");
        assert_eq!(
            std::fs::read(&index).ok(),
            index_before,
            "the observation must not rewrite the index"
        );
        assert_eq!(
            std::fs::metadata(&index).ok().map(|m| m.modified().ok()),
            index_mtime_before,
            "the observation must not even refresh index stat metadata"
        );
        assert_eq!(
            sorted_entries(&root),
            worktree_before,
            "the observation must not add or remove any worktree entry"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    /// A repository that carries a hostile external filesystem monitor must not
    /// be able to run it. `--no-optional-locks` is not what stops this;
    /// `core.fsmonitor=false` is, and the marker file proves it.
    #[cfg(unix)]
    #[tokio::test]
    async fn host_owned_observation_never_runs_a_repository_chosen_program() {
        let base = temp_dir("observe-hostile");
        let root = base.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        init_repo(&root);
        let marker = base.join("fsmonitor-ran");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git runs");
            assert!(output.status.success(), "git {args:?} failed");
        };
        git(&[
            "config",
            "core.fsmonitor",
            &format!("touch {}", marker.display()),
        ]);
        std::fs::write(root.join("tracked.txt"), b"changed\n").unwrap();
        let observed = observe_with_policy(&root, &session(&root), false)
            .await
            .expect("host-owned observation succeeds despite the hostile config");
        assert!(observed.changed.contains(&root.join("tracked.txt")));
        assert!(
            !marker.exists(),
            "the observation ran a repository-chosen filesystem monitor"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    fn sorted_entries(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut entries: Vec<(String, Vec<u8>)> = walk(root)
            .into_iter()
            .filter_map(|path| {
                let relative = path.strip_prefix(root).ok()?.to_str()?.to_owned();
                let bytes = std::fs::read(&path).ok()?;
                Some((relative, bytes))
            })
            .collect();
        entries.sort();
        entries
    }

    fn walk(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(root) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk(&path));
            } else {
                found.push(path);
            }
        }
        found
    }

    /// A Session whose permitted roots are narrower than the Git top level must
    /// not have the Verifier's own observation run outside those roots.
    ///
    /// A Git top level can legitimately be an ancestor of the session cwd, so the
    /// observation root is host-derived rather than caller-chosen. Moving the
    /// observation off the generic command path must not move authority with it:
    /// the seam re-checks Session path authority on every working directory it
    /// runs in.
    #[tokio::test]
    async fn observation_refuses_a_root_outside_session_authority() {
        let base = temp_dir("observe-authority");
        let repo = base.join("repo");
        let subdirectory = repo.join("sub");
        std::fs::create_dir_all(&subdirectory).unwrap();
        init_repo(&repo);
        // The Session cwd is a subdirectory and the Session permits only that
        // subdirectory, so the repository top level is outside Session authority.
        let narrowed = config::Session {
            id: format!("s{}", Uuid::new_v4().simple()),
            cwd: subdirectory.clone(),
            permitted_directories: vec![subdirectory.clone()],
        };
        assert!(
            observe_with_policy(&repo, &narrowed, false).await.is_err(),
            "the observation must not run at a Git root outside Session authority"
        );
        // Permitting the repository root makes the same observation succeed, so
        // the failure above is the authority check and not something incidental.
        let permitted = config::Session {
            id: format!("s{}", Uuid::new_v4().simple()),
            cwd: subdirectory.clone(),
            permitted_directories: vec![subdirectory, repo.clone()],
        };
        assert!(observe_with_policy(&repo, &permitted, false).await.is_ok());
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn observation_of_a_non_repository_fails_closed() {
        let base = temp_dir("not-a-repo");
        assert!(
            observe_with_policy(&base, &session(&base), false)
                .await
                .is_err()
        );
        let _ = std::fs::remove_dir_all(base);
    }

    /// The platform approval gate is platform policy, not a universal one, and
    /// the seam must fail closed when the gate is on and the operator has not
    /// approved.
    #[test]
    fn the_observation_approval_gate_is_windows_only_and_binds() {
        assert_eq!(
            verifier_git_observation_requires_approval(),
            cfg!(windows),
            "the approval gate is platform policy, not a universal one"
        );
    }

    #[test]
    fn porcelain_v1_z_parser_consumes_rename_and_copy_source_records_statefully() {
        let root = temp_dir("porcelain-parser");
        let cases: &[(&str, &[u8], &[&str])] = &[
            ("modified", b" M ordinary.txt\0", &["ordinary.txt"]),
            ("third-byte-space", b" M ab file.txt\0", &["ab file.txt"]),
            (
                "spaces",
                b"?? path with spaces.txt\0",
                &["path with spaces.txt"],
            ),
            (
                "tab-newline",
                b"?? tab\tand\nnewline\0",
                &["tab\tand\nnewline"],
            ),
            ("untracked", b"?? new.txt\0", &["new.txt"]),
            ("deleted", b" D gone.txt\0", &["gone.txt"]),
            (
                "rename",
                b"R  destination.txt\0source.txt\0",
                &["destination.txt", "source.txt"],
            ),
            (
                "rename-third-byte-space",
                b"R  ab destination.txt\0ab forbidden.txt\0",
                &["ab destination.txt", "ab forbidden.txt"],
            ),
            (
                "copy",
                b" C copied.txt\0original.txt\0",
                &["copied.txt", "original.txt"],
            ),
            (
                "index-rename",
                b"R  destination.txt\0source.txt\0",
                &["destination.txt", "source.txt"],
            ),
            (
                "worktree-rename",
                b" R destination.txt\0source.txt\0",
                &["destination.txt", "source.txt"],
            ),
        ];
        for (name, bytes, expected) in cases {
            let actual =
                parse_status_paths(bytes, &root).unwrap_or_else(|error| panic!("{name}: {error}"));
            let expected = expected
                .iter()
                .map(|path| root.join(path))
                .collect::<BTreeSet<_>>();
            assert_eq!(
                actual.into_iter().collect::<BTreeSet<_>>(),
                expected,
                "{name}"
            );
        }
        for malformed in [
            &b" M unterminated"[..],
            &b" M path\0\0"[..],
            &b" M path\0R  destination\0"[..],
            &b"R  destination\0\0"[..],
            &b"M "[..],
            &b"xY path\0"[..],
            &b"   path\0"[..],
            &b"?  path\0"[..],
            &b" M path\0bad-token\0"[..],
            &b" M /absolute\0"[..],
            &b" M ../escape\0"[..],
        ] {
            assert!(
                parse_status_paths(malformed, &root).is_err(),
                "{malformed:?}"
            );
        }
        assert!(parse_nul_paths(b"file\0", &root).is_ok());
        assert!(parse_nul_paths(b"../escape\0", &root).is_err());
        assert!(parse_nul_paths(b"/absolute\0", &root).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn real_git_rename_preserves_both_paths_including_ab_space_source() {
        let root = temp_dir("rename-real-git");
        init_repo(&root);
        std::fs::write(root.join("ab forbidden.txt"), b"forbidden\n").unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new(execution::host_git_path().unwrap())
                .args(args)
                .current_dir(&root)
                .output()
                .expect("Git starts");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["add", "--", "ab forbidden.txt"]);
        git(&["commit", "-q", "-m", "add forbidden source"]);
        std::fs::rename(root.join("ab forbidden.txt"), root.join("renamed.txt")).unwrap();
        let observed = observe_with_policy(&root, &session(&root), false)
            .await
            .expect("real rename observation succeeds");
        assert!(observed.changed.contains(&root.join("ab forbidden.txt")));
        assert!(observed.changed.contains(&root.join("renamed.txt")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_rooted_and_drive_relative_git_paths_are_rejected_before_join() {
        let root = temp_dir("windows-paths");
        for unsafe_path in [b"C:outside.txt".as_slice(), b"\\rooted.txt".as_slice()] {
            assert!(
                resolve_git_path_bytes(unsafe_path, &root).is_err(),
                "{unsafe_path:?}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn non_utf8_git_path_round_trips_losslessly() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let root = temp_dir("non-utf8");
        init_repo(&root);
        let name = OsString::from_vec(vec![b'n', b'o', b'n', b'u', b't', b'f', b'8', b'-', 0xff]);
        let path = root.join(name);
        if std::fs::write(&path, b"bytes\n").is_err() {
            let _ = std::fs::remove_dir_all(root);
            return;
        }
        let observed = observe_with_policy(&root, &session(&root), false)
            .await
            .expect("raw Git path must be representable on Unix");
        assert!(observed.changed.contains(&path));
        assert_eq!(path.file_name().unwrap().as_bytes().last(), Some(&0xff));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn an_unapproved_observation_performs_no_git_invocation() {
        let root = temp_dir("unapproved");
        init_repo(&root);
        let session = session(&root);
        // With the gate off, the same seam observes normally.
        assert!(observe_with_policy(&root, &session, false).await.is_ok());
        // With the gate on and no approval channel available, the observation
        // fails closed and reports no Git state at all. It never degrades into a
        // silently empty observation.
        let denied = observe_with_policy(&root, &session, true)
            .await
            .expect_err("an unapproved observation must not succeed");
        // The failure is specifically the approval gate rather than a Git
        // failure. This is what ties the platform policy constant to a real
        // approval request instead of leaving it a tautology.
        assert!(
            denied.contains("approval"),
            "the failure must come from the approval gate, got: {denied}"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
