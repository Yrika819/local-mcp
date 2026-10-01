//! Verifier-specific command authority for `VerificationSpec::CommandExit`.
//!
//! `COMMAND_EXIT` is pure observation only. A command the host cannot place in
//! a verifier-safe observation class is not executed, and the host does not
//! treat "not known to be a mutation" as permission: [`crate::fallback`]'
//! `SideEffectClass::Unknown` is never authority here, and neither is a caller
//! or model supplied `read_only_command` label.
//!
//! The only accepted argv shapes are the exact, machine-readable Git
//! observations below. There is deliberately no generic executable allowance:
//! `touch`, `rm`, shells, interpreters, project scripts, build and test
//! drivers, and unknown CLIs are all rejected. A product that needs verification
//! commands which legitimately write belongs in a separate explicit design such
//! as `BUILD_VERIFICATION` with its own bounded artifact scope, not here.

use std::path::Path;

/// The only command class a model-authored `COMMAND_EXIT` may reach.
///
/// This is an explicit positive result from a host-owned table, not the absence
/// of a mutation classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerifierCommandClass {
    /// The exact argv is a read-only observation the host can prove and that
    /// performs no index, ref, worktree, or remote write.
    PureObservation,
}

/// Why a proposed verification command was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VerifierCommandRejection {
    /// The argv carried no entries.
    Empty,
    /// The executable is a shell, an interpreter, or a script host.
    ScriptHost,
    /// The executable is anything other than host-listed Git.
    UnsupportedExecutable,
    /// The exact argv is not one of the host-approved observation shapes.
    UnlistedArgv,
}

impl VerifierCommandRejection {
    pub(crate) fn detail(self) -> &'static str {
        match self {
            Self::Empty => "verification command is empty",
            Self::ScriptHost => {
                "verification command uses a shell, interpreter, or script host, which is never host-approved observation authority"
            }
            Self::UnsupportedExecutable => {
                "verification executable is not a host-approved observation command; COMMAND_EXIT is pure observation only and admits exact read-only Git shapes"
            }
            Self::UnlistedArgv => {
                "verification command is not an approved pure-observation argv; COMMAND_EXIT is pure observation only and admits exact read-only Git shapes"
            }
        }
    }
}

/// Shells, interpreters, and script hosts that must never be reached through a
/// verification command, matched on the executable's file name.
const SCRIPT_HOSTS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "ksh",
    "dash",
    "fish",
    "csh",
    "tcsh",
    "ash",
    "busybox",
    "cmd",
    "cmd.exe",
    "powershell",
    "powershell.exe",
    "pwsh",
    "pwsh.exe",
    "env",
    "nohup",
    "setsid",
    "eval",
    "exec",
    "xargs",
    "timeout",
    "watch",
    "make",
    "just",
    "task",
    "node",
    "deno",
    "bun",
    "python",
    "python2",
    "python3",
    "py",
    "ruby",
    "perl",
    "php",
    "lua",
    "tclsh",
    "wish",
    "awk",
    "gawk",
    "mawk",
    "sed",
    "tee",
    "find",
    "xargs",
    "touch",
    "rm",
    "rmdir",
    "cp",
    "mv",
    "ln",
    "mkdir",
    "chmod",
    "chown",
    "dd",
    "truncate",
    "install",
    "cat",
    "less",
    "more",
    "man",
];

/// The exact read-only Git tails a model may propose, after an optional leading
/// `--no-optional-locks`.
///
/// Every entry is a complete match, so a global option, an extra argument, a
/// pathspec, or a shell never reaches execution.
///
/// `status` and `diff` are deliberately absent even in their bounded forms. Both
/// consult configuration the sandboxed environment still exposes (a
/// user-level `core.fsmonitor` is a program the host did not choose, and a
/// `.gitattributes` `textconv` or an external diff driver is another), and both
/// can write the index. The Verifier's own host-owned seam performs the real
/// status and staged-name observation with hooks, fsmonitor, external diff, and
/// optional index writes all neutralized; a model-authored `COMMAND_EXIT` must
/// not be able to reach a weaker version of the same question.
pub(crate) const OBSERVATION_TAILS: &[&[&str]] = &[
    &["rev-parse", "--show-toplevel"],
    &["rev-parse", "--git-dir"],
    &["rev-parse", "HEAD"],
    &["rev-parse", "--abbrev-ref", "HEAD"],
    &["rev-parse", "--is-inside-work-tree"],
    &["branch", "--show-current"],
    &["branch", "--list"],
    &["branch", "-l"],
    &["tag", "--list"],
    &["remote"],
    &["remote", "-v"],
    &["worktree", "list"],
    &["worktree", "list", "--porcelain", "-z"],
    &["symbolic-ref", "HEAD"],
    &["symbolic-ref", "-q", "HEAD"],
];

/// Decide whether a model-authored `COMMAND_EXIT` argv is a verifier-safe
/// pure observation.
///
/// The result is a positive classification from the host table. A shape that is
/// not listed, is listed ambiguously, uses an unsupported executable, or carries
/// an authority-shaping option is refused so the caller can refuse it before any
/// process is spawned.
///
/// This decides the argv only. It does not decide which Git binary runs: the
/// caller replaces `argv[0]` with the host-resolved Git identity, so a name that
/// merely *looks* like Git can never select the executable.
pub(crate) fn classify(
    command: &[String],
) -> Result<VerifierCommandClass, VerifierCommandRejection> {
    let Some(program) = command.first() else {
        return Err(VerifierCommandRejection::Empty);
    };
    let executable = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase();
    if SCRIPT_HOSTS.contains(&executable.as_str()) {
        return Err(VerifierCommandRejection::ScriptHost);
    }
    if !is_host_git_name(&executable) {
        return Err(VerifierCommandRejection::UnsupportedExecutable);
    }
    let tail = strip_inert_globals(&command[1..]);
    if OBSERVATION_TAILS.iter().any(|allowed| *allowed == tail) {
        Ok(VerifierCommandClass::PureObservation)
    } else {
        Err(VerifierCommandRejection::UnlistedArgv)
    }
}

/// Whether the executable names Git, including the Windows `.exe` spelling.
///
/// This is a readability and taxonomy check on the *proposal*, not the
/// executable that runs. The verifier substitutes the host-resolved Git identity
/// for `argv[0]` before execution, so a repository-planted `./git` or an
/// absolute `/tmp/x/git` never selects the binary; this check only decides which
/// rejection an unrecognised program name gets.
fn is_host_git_name(executable: &str) -> bool {
    executable == "git" || (cfg!(windows) && executable == "git.exe")
}

/// A human- and model-readable statement of the admitted shapes.
///
/// The planner prompt cannot embed a list that silently rots, so this is the
/// single description used both in the model's verification schema and in the
/// error a rejected plan receives.
pub(crate) const APPROVED_TAILS_HINT: &str = "The approved pure-observation shapes are exactly: git rev-parse --show-toplevel | git rev-parse --git-dir | git rev-parse HEAD | git rev-parse --abbrev-ref HEAD | git rev-parse --is-inside-work-tree | git branch --show-current | git branch --list | git branch -l | git tag --list | git remote | git remote -v | git worktree list | git worktree list --porcelain -z | git symbolic-ref HEAD | git symbolic-ref -q HEAD, each optionally prefixed by the single global --no-optional-locks. Use FILE_EXISTS, FILE_DIGEST, GIT_SCOPE, or NO_FORBIDDEN_CHANGES for anything else; a build, test, or script command is not a permitted pure observation.";

/// Drop a leading `--no-optional-locks` and refuse anything else that appears
/// before the subcommand.
///
/// Only this one inert global option is tolerated. `-C`, `-c`, `--git-dir`,
/// `--work-tree`, `--config-env`, and `--exec-path` all redirect or reconfigure
/// Git, so a model can never aim a verification command at a repository outside
/// the host's validated execution root.
fn strip_inert_globals(args: &[String]) -> &[String] {
    match args.first() {
        Some(first) if first == "--no-optional-locks" => &args[1..],
        _ => args,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fallback::SideEffectClass;
    use crate::git_command_class;

    fn command_from(argv: &[&str]) -> Vec<String> {
        argv.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn every_approved_observation_is_also_a_side_effect_free_classification() {
        // The authority admits these tails with or without the inert global, but
        // `--no-optional-locks` is what makes the `status` observation provably
        // write-free, so the invariant is asserted on the inert-prefixed form.
        for tail in OBSERVATION_TAILS {
            let mut argv = vec!["git".to_owned(), "--no-optional-locks".to_owned()];
            argv.extend(tail.iter().map(|arg| (*arg).to_owned()));
            assert_eq!(
                git_command_class::classify(&argv),
                SideEffectClass::None,
                "{tail:?} is approved here but is not a side-effect-free argv"
            );
        }
    }

    #[test]
    fn the_approved_shapes_are_pure_observations() {
        for tail in OBSERVATION_TAILS {
            let mut argv = vec!["git".to_owned(), "--no-optional-locks".to_owned()];
            argv.extend(tail.iter().map(|arg| (*arg).to_owned()));
            assert_eq!(
                classify(&argv),
                Ok(VerifierCommandClass::PureObservation),
                "{tail:?}"
            );
            // The same tail without the inert global is admitted too; only the
            // bounded `status` form depends on the prefix to be write-free.
            let mut bare = vec!["git".to_owned()];
            bare.extend(tail.iter().map(|arg| (*arg).to_owned()));
            assert_eq!(
                classify(&bare),
                Ok(VerifierCommandClass::PureObservation),
                "{tail:?}"
            );
        }
    }

    #[test]
    fn generic_executables_are_never_verification_authority() {
        for argv in [
            vec!["/usr/bin/true"],
            vec!["/usr/bin/false"],
            vec!["true"],
            vec!["cargo", "test"],
            vec!["cargo", "build"],
            vec!["gradle", "test"],
            vec!["./gradlew", "test"],
            vec!["npm", "test"],
            vec!["unknown-cli", "--read-only"],
        ] {
            assert_eq!(
                classify(&command_from(&argv)),
                Err(VerifierCommandRejection::UnsupportedExecutable),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn shells_and_interpreters_are_refused_before_spawn() {
        for argv in [
            vec!["sh", "-c", "git status"],
            vec!["bash", "-lc", "git status"],
            vec!["zsh", "-c", "git status"],
            vec!["cmd", "/c", "git status"],
            vec!["cmd.exe", "/c", "git status"],
            vec!["powershell", "-Command", "git status"],
            vec!["pwsh", "-c", "git status"],
            vec!["/bin/sh", "-c", "git status"],
            vec!["python", "scripts/check.py"],
            vec!["python3", "-c", "print(1)"],
            vec!["node", "verify.js"],
            vec!["ruby", "verify.rb"],
            vec!["perl", "verify.pl"],
            vec!["make", "check"],
            vec!["touch", "file"],
            vec!["rm", "-rf", "dir"],
        ] {
            assert_eq!(
                classify(&command_from(&argv)),
                Err(VerifierCommandRejection::ScriptHost),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn mutating_and_ambiguous_git_shapes_are_refused_before_spawn() {
        for argv in [
            vec!["git"],
            vec!["git", "status"],
            vec!["git", "status", "--porcelain=v1"],
            vec!["git", "commit"],
            vec!["git", "add", "--", "src/a.rs"],
            vec!["git", "symbolic-ref", "HEAD", "refs/heads/x"],
            vec!["git", "symbolic-ref", "--delete", "HEAD"],
            vec!["git", "branch", "new-name"],
            vec!["git", "branch", "-D", "name"],
            vec!["git", "tag", "v1"],
            vec!["git", "tag", "-d", "v1"],
            vec!["git", "worktree", "add", "/tmp/x"],
            vec!["git", "worktree", "remove", "/tmp/x"],
            vec!["git", "worktree", "list", "--force"],
            vec!["git", "remote", "add", "origin", "url"],
            vec!["git", "remote", "set-url", "origin", "url"],
            vec!["git", "remote", "get-url", "origin"],
            vec!["git", "push", "origin", "main"],
            vec!["git", "fetch"],
            vec!["git", "config", "--local", "user.name", "x"],
            vec!["git", "rev-parse"],
            vec!["git", "rev-parse", "--show-toplevel", "extra"],
            vec!["git", "diff", "--cached", "--name-only"],
            vec!["git", "branch", "--list", "main"],
            vec!["git", "log", "--oneline"],
        ] {
            assert_eq!(
                classify(&command_from(&argv)),
                Err(VerifierCommandRejection::UnlistedArgv),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn authority_shaping_git_globals_are_refused_before_spawn() {
        for argv in [
            vec!["git", "-C", "/elsewhere", "rev-parse", "--show-toplevel"],
            vec!["git", "-C/elsewhere", "rev-parse", "--show-toplevel"],
            vec![
                "git",
                "-c",
                "core.fsmonitor=false",
                "rev-parse",
                "--show-toplevel",
            ],
            vec!["git", "--git-dir=/elsewhere/.git", "rev-parse", "HEAD"],
            vec!["git", "--work-tree=/elsewhere", "rev-parse", "HEAD"],
            vec![
                "git",
                "--config-env",
                "core.hooksPath=HOOKS",
                "rev-parse",
                "HEAD",
            ],
            vec!["git", "--exec-path=/tmp/bin", "rev-parse", "HEAD"],
            vec!["git", "--no-pager", "rev-parse", "HEAD"],
        ] {
            assert_eq!(
                classify(&command_from(&argv)),
                Err(VerifierCommandRejection::UnlistedArgv),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn an_empty_command_is_refused() {
        assert_eq!(classify(&[]), Err(VerifierCommandRejection::Empty));
    }

    #[test]
    fn the_model_cannot_promote_a_command_by_declaring_it_read_only() {
        // The authority reads argv only. There is no field on this type a
        // caller could set to widen the table, which is the structural reason a
        // `read_only_command` label can never become authority.
        for argv in [
            vec!["git", "branch", "new-name"],
            vec!["git", "commit", "-m", "x"],
            vec!["touch", "file"],
        ] {
            assert_ne!(
                classify(&command_from(&argv)),
                Ok(VerifierCommandClass::PureObservation),
                "{argv:?}"
            );
        }
    }
}
