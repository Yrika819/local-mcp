//! Conservative full-argv Git side-effect classification.
//!
//! The previous rule read only `command[1]`, so `git symbolic-ref HEAD
//! refs/heads/x` and `git symbolic-ref --delete HEAD` were both treated as
//! side-effect free while `git branch --show-current` was treated as a mutation.
//! Every shape is now decided from the complete argv.
//!
//! The polarity is fail-closed. A shape is `SideEffectClass::None` only when a
//! table entry proves it is an observation, a mutation is only ever claimed when
//! the shape is provably effectful, and anything this module cannot place is
//! `SideEffectClass::Unknown` — never `None`.

use crate::fallback::SideEffectClass;

/// Classify a `git` invocation from its exact argv.
///
/// `command[0]` is assumed to be the Git executable; [`crate::fallback`]
/// normalizes the name and the Windows `.exe` spelling before calling here.
pub(crate) fn classify(command: &[String]) -> SideEffectClass {
    let Some(globals) = split_global_options(&command[1..]) else {
        return SideEffectClass::Unknown;
    };
    let subcommand = command[1 + globals.consumed].to_ascii_lowercase();
    let rest = &command[2 + globals.consumed..];
    let shape = match subcommand.as_str() {
        "branch" => branch(rest),
        "tag" => tag(rest),
        "worktree" => worktree(rest),
        "symbolic-ref" => symbolic_ref(rest),
        "remote" => remote(rest),
        "status" => status(rest, globals.no_optional_locks),
        "show-ref" => show_ref(rest),
        subcommand => match read_only_query(subcommand, rest) {
            Some(class) => class,
            None => local_or_remote_mutation(subcommand),
        },
    };
    // A global option that redirects or reconfigures Git changes which
    // repository, directory, or configuration the observed shape applies to, so
    // a read shape behind one is no longer provable from the argv alone. A
    // mutation claim is unaffected: over-claiming a mutation is always safe.
    if shape == SideEffectClass::None && globals.authority_shaped {
        return SideEffectClass::Unknown;
    }
    shape
}

/// Git global options, split by whether they can change which repository,
/// directory, or configuration an observed shape actually applies to.
///
/// `-C`/`--git-dir`/`--work-tree`/`--namespace`/`--super-prefix`/`--exec-path`
/// redirect the repository identity and `-c`/`--config-env` rewrite
/// configuration, so a model-authored invocation carrying any of them must not be
/// treated as an observation of the validated execution root.
fn global_option_kind(arg: &str) -> Option<GlobalKind> {
    // The long spellings accept both `--opt value` and `--opt=value`.
    if let Some((name, _)) = arg.split_once('=') {
        return match name {
            "--config-env" | "--namespace" | "--super-prefix" | "--git-dir" | "--work-tree"
            | "--exec-path" => Some(GlobalKind::Shaping),
            _ => None,
        };
    }
    // The short spellings accept both `-o value` and `-ovalue`.
    if let Some(name) = arg.strip_prefix('-')
        && !arg.starts_with("--")
        && name.chars().count() > 1
    {
        return match name.chars().next()? {
            'C' | 'c' => Some(GlobalKind::Shaping),
            _ => None,
        };
    }
    match arg {
        // Accepted and provably free of repository redirection.
        "--no-optional-locks"
        | "--paginate"
        | "--no-pager"
        | "-p"
        | "--literal-pathspecs"
        | "--glob-pathspecs"
        | "--noglob-pathspecs"
        | "--icase-pathspecs" => Some(GlobalKind::Inert),
        // Redirects Git without consuming a value.
        "--bare" | "--no-replace-objects" => Some(GlobalKind::Shaping),
        // Redirects Git and consumes a value argument.
        "-C" | "-c" | "--config-env" | "--namespace" | "--super-prefix" | "--git-dir"
        | "--work-tree" | "--exec-path" => Some(GlobalKind::ShapingValue),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GlobalKind {
    Inert,
    Shaping,
    ShapingValue,
}

struct GlobalOptions {
    /// How many argv entries after argv[0] were consumed as global options.
    consumed: usize,
    /// A global option redirected or reconfigured Git.
    authority_shaped: bool,
    /// `--no-optional-locks` disabled Git's optional index writes.
    no_optional_locks: bool,
}

/// Split Git's leading global options and locate the subcommand token.
///
/// Returns `None` when there is no subcommand or when a leading option cannot
/// be placed, which is itself an unsupported shape and so fails closed.
fn split_global_options(args: &[String]) -> Option<GlobalOptions> {
    let mut index = 0;
    let mut authority_shaped = false;
    let mut no_optional_locks = false;
    while index < args.len() {
        let arg = args[index].as_str();
        if !arg.starts_with('-') {
            break;
        }
        match global_option_kind(arg) {
            Some(GlobalKind::Inert) => {
                no_optional_locks |= arg == "--no-optional-locks";
                index += 1;
            }
            Some(GlobalKind::Shaping) => {
                authority_shaped = true;
                index += 1;
            }
            Some(GlobalKind::ShapingValue) => {
                authority_shaped = true;
                index += 1;
                // The detached `--opt value` and `-o value` spellings take the
                // value as the next argv entry. The attached forms (`--opt=v`,
                // `-ov`) do not, so a long option or a bare short option consumes
                // one more entry and nothing else does. Getting this wrong would
                // let the *value* be read as the subcommand.
                if !arg.contains('=') && (arg.starts_with("--") || arg.len() == 2) {
                    index += 1;
                }
            }
            None => return None,
        }
    }
    if index >= args.len() {
        return None;
    }
    Some(GlobalOptions {
        consumed: index,
        authority_shaped,
        no_optional_locks,
    })
}

/// Options and positional arguments left after the subcommand.
///
/// A `--` separator ends option parsing for the remainder, so a positional after
/// it is a pathspec or ref pattern and never an option.
struct Args<'a> {
    options: Vec<&'a str>,
    positionals: Vec<&'a str>,
}

fn split_args(args: &[String]) -> Args<'_> {
    let mut options = Vec::new();
    let mut positionals = Vec::new();
    let mut separated = false;
    for arg in args {
        if separated || !arg.starts_with('-') || arg == "-" {
            positionals.push(arg.as_str());
        } else if arg == "--" {
            separated = true;
        } else {
            options.push(arg.as_str());
        }
    }
    Args {
        options,
        positionals,
    }
}

/// Whether any option is one of the listed flags.
///
/// A long flag carrying an attached value (`--format=%(refname)`) is the same
/// flag, in both directions: the listedness check must accept it so the shape
/// can be placed, and the mutation check must match it so an attached value can
/// never turn a mutation into an observation. Over-matching a mutation is
/// always safe; under-matching one is not.
fn has_flag(args: &Args<'_>, flags: &[&str]) -> bool {
    args.options.iter().any(|option| {
        flags.iter().any(|flag| {
            *option == *flag
                || (flag.starts_with("--")
                    && option.starts_with(flag)
                    && option.as_bytes().get(flag.len()) == Some(&b'='))
        })
    })
}

/// Whether every option belongs to at least one of the supplied tables.
///
/// A long flag carrying an attached value is matched on its base name, so
/// `--format=%(refname)` is the listed `--format` and `--nonsense=x` is still
/// unlisted.
fn only_listed(args: &Args<'_>, tables: &[&[&str]]) -> bool {
    args.options.iter().all(|option| {
        let base = match option.split_once('=') {
            Some((name, _)) if name.starts_with("--") => name,
            _ => *option,
        };
        tables.iter().any(|table| table.contains(&base))
    })
}

/// `git branch` and `git tag` share a hazard: without an explicit listing
/// selector the first positional is a name to create, not a pattern to match.
///
/// `git branch --list main` filters, `git branch -q main` creates, and so does
/// `git branch --format='%(refname)' main` — `--format`, `--sort`, and
/// `--column` change how a listing is *rendered*, they do not put Git into
/// listing mode. A selector and a display option therefore have to be tracked
/// separately, or `git branch --format=x new-name` is read as a query while it
/// actually creates a ref.
///
/// Mutation flags are weighed first so a shape that both mutates and carries an
/// unlisted option is still claimed as a mutation; over-claiming a mutation is
/// always safe, and an unlisted option must never be able to hide one.
fn listing_command(
    args: &[String],
    mutation: &[&str],
    selector: &[&str],
    display: &[&str],
) -> SideEffectClass {
    let parsed = split_args(args);
    if has_flag(&parsed, mutation) {
        return SideEffectClass::LocalMutation;
    }
    if !only_listed(&parsed, &[mutation, selector, display]) {
        return SideEffectClass::Unknown;
    }
    if has_flag(&parsed, selector) || parsed.positionals.is_empty() {
        SideEffectClass::None
    } else {
        // A bare name creates a branch or tag; it is never an observation.
        SideEffectClass::LocalMutation
    }
}

fn branch(args: &[String]) -> SideEffectClass {
    const MUTATION: &[&str] = &[
        "-d",
        "-D",
        "--delete",
        "-m",
        "-M",
        "--move",
        "-c",
        "-C",
        "--copy",
        "--edit-description",
        "-u",
        "--unset-upstream",
        "--track",
        "--force",
        "-f",
        "--set-upstream-to",
        "--copy-to",
        "--set-color",
    ];
    const SELECTOR: &[&str] = &[
        "--show-current",
        "--list",
        "-l",
        "--all",
        "-a",
        "--remotes",
        "-r",
        "--contains",
        "--no-contains",
        "--merged",
        "--no-merged",
        "--points-at",
    ];
    // `--format`, `--sort`, and `--column` render a listing; they do not select
    // listing mode, so a positional after them is still a branch to create.
    const DISPLAY: &[&str] = &[
        "--format",
        "--sort",
        "--column",
        "--no-column",
        "--verbose",
        "-v",
        "--quiet",
        "-q",
        "--abbrev",
        "--no-abbrev",
        "--ignore-case",
        "-i",
        "--color",
        "--no-color",
    ];
    listing_command(args, MUTATION, SELECTOR, DISPLAY)
}

fn tag(args: &[String]) -> SideEffectClass {
    const MUTATION: &[&str] = &[
        "-a",
        "--annotate",
        "-s",
        "--sign",
        "-u",
        "--local-user",
        "-f",
        "--force",
        "-d",
        "--delete",
        "--create-reflog",
        "-m",
        "--message",
        "--cleanup",
    ];
    const SELECTOR: &[&str] = &[
        "--list",
        "-l",
        "--contains",
        "--no-contains",
        "--merged",
        "--no-merged",
        "--points-at",
        "-n",
        "-v",
        "--verify",
    ];
    // `--format`, `--sort`, and `--column` render a listing; they do not select
    // listing mode, so a positional after them is still a tag to create.
    const DISPLAY: &[&str] = &[
        "--format",
        "--sort",
        "--column",
        "--no-column",
        "--ignore-case",
        "-i",
        "--color",
        "--no-color",
    ];
    listing_command(args, MUTATION, SELECTOR, DISPLAY)
}

fn worktree(args: &[String]) -> SideEffectClass {
    const READ: &[&str] = &["--porcelain", "-z"];
    const MUTATION: &[&str] = &["add", "remove", "move", "lock", "unlock", "prune", "repair"];
    let parsed = split_args(args);
    if !only_listed(&parsed, &[READ]) {
        return SideEffectClass::Unknown;
    }
    match parsed.positionals.first() {
        Some(verb) if MUTATION.contains(verb) => SideEffectClass::LocalMutation,
        Some(&"list") => SideEffectClass::None,
        _ => SideEffectClass::Unknown,
    }
}

fn symbolic_ref(args: &[String]) -> SideEffectClass {
    const READ: &[&str] = &["--quiet", "-q", "--short"];
    const MUTATION: &[&str] = &["--delete", "-d"];
    let parsed = split_args(args);
    if has_flag(&parsed, MUTATION) {
        return SideEffectClass::LocalMutation;
    }
    if !only_listed(&parsed, &[READ]) {
        return SideEffectClass::Unknown;
    }
    match parsed.positionals.len() {
        // `git symbolic-ref HEAD` names the current ref; the two-ref form
        // rewrites it and the `--delete` form removes it.
        1 => SideEffectClass::None,
        2 => SideEffectClass::LocalMutation,
        _ => SideEffectClass::Unknown,
    }
}

fn remote(args: &[String]) -> SideEffectClass {
    const READ: &[&str] = &[
        "-v",
        "--verbose",
        "--all",
        "-a",
        "--push",
        "--quiet",
        "-q",
        "--no-tags",
    ];
    const LOCAL_MUTATION: &[&str] = &[
        "add",
        "remove",
        "rm",
        "rename",
        "set-url",
        "set-head",
        "set-branches",
    ];
    // These two contact the remote, so they are remote mutations even though
    // their recorded effects land locally.
    const REMOTE_MUTATION: &[&str] = &["prune", "update"];
    const READ_VERB: &[&str] = &["get-url", "show"];
    let parsed = split_args(args);
    if !only_listed(&parsed, &[READ]) {
        return SideEffectClass::Unknown;
    }
    match parsed.positionals.first() {
        // `git remote` and `git remote -v` list configured remotes.
        None => SideEffectClass::None,
        Some(verb) if READ_VERB.contains(verb) => SideEffectClass::None,
        Some(verb) if LOCAL_MUTATION.contains(verb) => SideEffectClass::LocalMutation,
        Some(verb) if REMOTE_MUTATION.contains(verb) => SideEffectClass::RemoteMutation,
        Some(_) => SideEffectClass::Unknown,
    }
}

/// `git status` is not unconditionally read-only: an ordinary status may refresh
/// and write index stat metadata. Only the bounded machine-readable form with
/// optional locks explicitly disabled is an observation here.
fn status(args: &[String], no_optional_locks: bool) -> SideEffectClass {
    // Options carrying an attached value are matched on their base name, so this
    // table lists `--untracked-files` and `--porcelain` rather than every
    // `--flag=value` spelling.
    const READ: &[&str] = &[
        "--porcelain",
        "-z",
        "--untracked-files",
        "-uno",
        "-uall",
        "-unormal",
        "--no-renames",
        "--renames",
        "--no-color",
        "--color",
        "--ignore-submodules",
        "--ignore-submodules=all",
        "--no-ahead-behind",
    ];
    if !no_optional_locks {
        return SideEffectClass::Unknown;
    }
    let parsed = split_args(args);
    if !only_listed(&parsed, &[READ]) {
        return SideEffectClass::Unknown;
    }
    if has_flag(&parsed, &["--porcelain", "-z"]) {
        SideEffectClass::None
    } else {
        SideEffectClass::Unknown
    }
}

fn show_ref(args: &[String]) -> SideEffectClass {
    // `-d` is `--dereference`, not a deletion: `git show-ref -d <ref>` prints the
    // ref and its annotated referents and leaves the ref in place. There is no
    // `git show-ref --delete`, so an invented spelling is simply an unlisted
    // option and fails closed as `Unknown`.
    const READ: &[&str] = &[
        "--verify",
        "--quiet",
        "-q",
        "--hash",
        "--head",
        "--heads",
        "--tags",
        "--dereference",
        "-d",
        "--abbrev",
    ];
    let parsed = split_args(args);
    if !only_listed(&parsed, &[READ]) {
        return SideEffectClass::Unknown;
    }
    SideEffectClass::None
}

/// Read-only query subcommands whose every supported option is a display or
/// scope restriction.
///
/// Each entry is a complete option allowlist: an option that is not listed makes
/// the whole invocation `Unknown`. This is the frozen fail-closed polarity — the
/// table is not an attempt to understand every Git read command.
const READ_ONLY_QUERIES: &[(&str, &[&str])] = &[
    (
        "rev-parse",
        &[
            "--show-toplevel",
            "--absolute-git-dir",
            "--git-dir",
            "--git-common-dir",
            "--git-path",
            "--is-inside-work-tree",
            "--is-bare-repository",
            "--show-prefix",
            "--show-cdup",
            "--show-superproject-working-tree",
            "--shared-index-path",
            "--path-format=absolute",
            "--abbrev-ref",
            "--verify",
            "--quiet",
            "-q",
        ],
    ),
    (
        "diff",
        &[
            "--cached",
            "--staged",
            "--name-only",
            "--name-status",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--raw",
            "--abbrev",
            "-z",
        ],
    ),
    (
        "log",
        &[
            "--oneline",
            "--max-count",
            "--format",
            "--pretty",
            "--no-color",
            "--name-only",
            "--name-status",
            "--graph",
            "--decorate",
            "--abbrev-commit",
            "-z",
        ],
    ),
    (
        "show",
        &[
            "--stat",
            "--name-only",
            "--name-status",
            "--oneline",
            "--format",
            "--no-color",
            "--no-patch",
            "-s",
        ],
    ),
    (
        "ls-files",
        &[
            "--stage",
            "-z",
            "--others",
            "--exclude-standard",
            "--full-name",
        ],
    ),
    ("cat-file", &["-t", "-s", "--batch-check"]),
    (
        "check-ignore",
        &["--quiet", "-q", "-z", "--stdin", "-n", "--non-matching"],
    ),
    ("describe", &["--tags", "--always", "--long"]),
    ("blame", &["--line-porcelain", "-l", "-L", "-w"]),
    ("shortlog", &[]),
    ("for-each-ref", &["--format", "--count", "--sort"]),
    ("name-rev", &["--name-only", "--all", "--stdin"]),
    (
        "diff-tree",
        &[
            "--no-commit-id",
            "--name-only",
            "--name-status",
            "-r",
            "-z",
            "--root",
        ],
    ),
    (
        "ls-remote",
        &["--heads", "--tags", "--refs", "--quiet", "-q"],
    ),
    ("version", &[]),
    ("help", &[]),
];

fn read_only_query(subcommand: &str, args: &[String]) -> Option<SideEffectClass> {
    let (_, allowed) = READ_ONLY_QUERIES
        .iter()
        .find(|(name, _)| *name == subcommand)?;
    let parsed = split_args(args);
    if !only_listed(&parsed, &[allowed]) {
        return Some(SideEffectClass::Unknown);
    }
    Some(SideEffectClass::None)
}

/// Subcommands that are effectful regardless of their arguments.
///
/// This list only ever adds a claim of mutation. A subcommand missing from both
/// tables stays `Unknown`, and an unrecognised option can still downgrade a
/// listed mutation below to `Unknown` inside its own family classifier.
const LOCAL_MUTATIONS: &[&str] = &[
    "add",
    "apply",
    "checkout",
    "cherry-pick",
    "clean",
    "commit",
    "merge",
    "mv",
    "rebase",
    "revert",
    "rm",
    "restore",
    "reset",
    "stash",
    "switch",
    "update-ref",
    "gc",
    "prune",
    "repack",
    "notes",
    "replace",
    "am",
    "applymail",
    "filter-branch",
    "fast-import",
];
const REMOTE_MUTATIONS: &[&str] = &["push", "fetch", "pull", "fetch-pack", "send-pack", "clone"];

fn local_or_remote_mutation(subcommand: &str) -> SideEffectClass {
    if REMOTE_MUTATIONS.contains(&subcommand) {
        SideEffectClass::RemoteMutation
    } else if LOCAL_MUTATIONS.contains(&subcommand) {
        SideEffectClass::LocalMutation
    } else {
        SideEffectClass::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(args: &[&str]) -> SideEffectClass {
        let command = std::iter::once("git".to_owned())
            .chain(args.iter().map(|arg| (*arg).to_owned()))
            .collect::<Vec<_>>();
        classify(&command)
    }

    fn class_exe(args: &[&str]) -> SideEffectClass {
        let command = std::iter::once("git.exe".to_owned())
            .chain(args.iter().map(|arg| (*arg).to_owned()))
            .collect::<Vec<_>>();
        // The executable spelling is normalized by `fallback` before this module
        // is reached, so drive the same entry point the product uses.
        crate::fallback::infer_side_effect_class(&command, None)
    }

    #[test]
    fn the_windows_executable_spelling_classifies_identically() {
        for (argv, expected) in [
            (vec!["branch", "--show-current"], SideEffectClass::None),
            (vec!["branch", "new-name"], SideEffectClass::LocalMutation),
            (vec!["branch", "-D", "name"], SideEffectClass::LocalMutation),
            (vec!["tag", "--list"], SideEffectClass::None),
            (vec!["tag", "v1"], SideEffectClass::LocalMutation),
            (vec!["worktree", "list"], SideEffectClass::None),
            (
                vec!["worktree", "add", "/tmp/x"],
                SideEffectClass::LocalMutation,
            ),
            (vec!["symbolic-ref", "HEAD"], SideEffectClass::None),
            (
                vec!["symbolic-ref", "HEAD", "refs/heads/x"],
                SideEffectClass::LocalMutation,
            ),
            (
                vec!["symbolic-ref", "--delete", "HEAD"],
                SideEffectClass::LocalMutation,
            ),
            (vec!["remote"], SideEffectClass::None),
            (vec!["remote", "-v"], SideEffectClass::None),
            (vec!["remote", "get-url", "origin"], SideEffectClass::None),
            (
                vec!["remote", "add", "origin", "url"],
                SideEffectClass::LocalMutation,
            ),
            (vec!["status"], SideEffectClass::Unknown),
            (
                vec!["--no-optional-locks", "status", "-z"],
                SideEffectClass::None,
            ),
            (
                vec!["push", "origin", "main"],
                SideEffectClass::RemoteMutation,
            ),
            (vec!["nonsense"], SideEffectClass::Unknown),
        ] {
            assert_eq!(class(&argv), expected, "git {argv:?}");
            // `git.exe` is the Windows spelling of the same host executable. On a
            // Unix host it is a different program name and is not Git at all, so
            // it stays `Unknown` rather than borrowing Git's classification.
            assert_eq!(
                class_exe(&argv),
                if cfg!(windows) {
                    expected
                } else {
                    SideEffectClass::Unknown
                },
                "git.exe {argv:?}"
            );
        }
    }

    #[test]
    fn branch_read_shapes_are_observations_and_every_mutation_is_a_mutation() {
        for argv in [
            vec!["branch", "--show-current"],
            vec!["branch", "--list"],
            vec!["branch", "-l"],
            vec!["branch", "-a"],
            vec!["branch", "-r"],
            vec!["branch"],
            vec!["branch", "--list", "main"],
            vec!["branch", "--contains", "main"],
        ] {
            assert_eq!(class(&argv), SideEffectClass::None, "{argv:?}");
        }
        for argv in [
            vec!["branch", "new-name"],
            vec!["branch", "new-name", "main"],
            vec!["branch", "-D", "name"],
            vec!["branch", "-d", "name"],
            vec!["branch", "--delete", "name"],
            vec!["branch", "-m", "old", "new"],
            vec!["branch", "--move", "old", "new"],
            vec!["branch", "--set-upstream-to=origin/main", "main"],
            vec!["branch", "--edit-description", "main"],
        ] {
            assert_eq!(class(&argv), SideEffectClass::LocalMutation, "{argv:?}");
        }
    }

    #[test]
    fn a_display_flag_alone_does_not_turn_a_branch_name_into_a_pattern() {
        // `git branch -q main` creates a branch, and so does
        // `git branch --format=... main`: a rendering option is not a listing
        // mode selector. Only an explicit selector makes a positional a pattern.
        // Every mutation shape below was verified against git 2.50.1 and created
        // the named ref.
        for argv in [
            vec!["branch", "-q", "new-name"],
            vec!["branch", "-v", "new-name"],
            vec!["branch", "--abbrev", "new-name"],
            vec!["branch", "--format=%(refname)", "new-name"],
            vec!["branch", "--format=%(refname:short)", "new-name"],
            vec!["branch", "--sort=refname", "new-name"],
            vec!["branch", "--sort", "refname", "new-name"],
            vec!["branch", "--column", "new-name"],
            vec!["tag", "-i", "v1"],
            vec!["tag", "--format=%(refname)", "v1"],
            vec!["tag", "--sort", "refname", "v1"],
            vec!["tag", "--column", "v1"],
        ] {
            assert_eq!(class(&argv), SideEffectClass::LocalMutation, "{argv:?}");
        }
        // The same rendering options still observe when a real selector is
        // present, because there is then no name to create.
        for argv in [
            vec!["branch", "--list", "--format=%(refname)"],
            vec!["branch", "--format=%(refname)", "--list", "main"],
            vec!["tag", "--list", "--sort=refname"],
            vec!["tag", "-n", "v1"],
        ] {
            assert_eq!(class(&argv), SideEffectClass::None, "{argv:?}");
        }
    }

    #[test]
    fn the_detached_value_of_a_long_global_option_is_not_read_as_a_subcommand() {
        // `--git-dir /elsewhere/.git status` is a redirect followed by
        // `status`. The value must be consumed, so the subcommand is the last
        // entry and the redirect still fails the shape closed.
        for (argv, expected) in [
            (
                vec!["--git-dir", "/elsewhere/.git", "symbolic-ref", "HEAD"],
                SideEffectClass::Unknown,
            ),
            (
                vec!["--git-dir", "/elsewhere/.git", "commit"],
                SideEffectClass::LocalMutation,
            ),
            (
                vec!["-C", "/elsewhere", "symbolic-ref", "HEAD"],
                SideEffectClass::Unknown,
            ),
            (
                vec!["-c", "core.fsmonitor=false", "symbolic-ref", "HEAD"],
                SideEffectClass::Unknown,
            ),
            (
                vec!["--work-tree", "/elsewhere", "symbolic-ref", "HEAD"],
                SideEffectClass::Unknown,
            ),
            (
                vec![
                    "--config-env",
                    "core.hooksPath=HOOKS",
                    "symbolic-ref",
                    "HEAD",
                ],
                SideEffectClass::Unknown,
            ),
        ] {
            assert_eq!(class(&argv), expected, "{argv:?}");
        }
    }

    #[test]
    fn branch_and_tag_shapes_with_unknown_flags_fail_closed() {
        assert_eq!(class(&["branch", "--nonsense"]), SideEffectClass::Unknown);
        assert_eq!(
            class(&["branch", "--nonsense", "main"]),
            SideEffectClass::Unknown
        );
        assert_eq!(class(&["tag", "--nonsense"]), SideEffectClass::Unknown);
    }

    #[test]
    fn tag_read_and_write_shapes_are_separated() {
        for argv in [
            vec!["tag", "--list"],
            vec!["tag", "-l"],
            vec!["tag"],
            vec!["tag", "--list", "v*"],
            vec!["tag", "--contains", "HEAD"],
        ] {
            assert_eq!(class(&argv), SideEffectClass::None, "{argv:?}");
        }
        for argv in [
            vec!["tag", "v1"],
            vec!["tag", "-d", "v1"],
            vec!["tag", "--delete", "v1"],
            vec!["tag", "-f", "v1"],
            vec!["tag", "-a", "v1", "-m", "msg"],
            vec!["tag", "-s", "-u", "key", "v1"],
        ] {
            assert_eq!(class(&argv), SideEffectClass::LocalMutation, "{argv:?}");
        }
    }

    #[test]
    fn worktree_list_observes_and_every_lifecycle_verb_mutates() {
        assert_eq!(class(&["worktree", "list"]), SideEffectClass::None);
        assert_eq!(
            class(&["worktree", "list", "--porcelain", "-z"]),
            SideEffectClass::None
        );
        for verb in ["add", "remove", "move", "lock", "unlock", "prune", "repair"] {
            assert_eq!(
                class(&["worktree", verb]),
                SideEffectClass::LocalMutation,
                "{verb}"
            );
        }
        assert_eq!(class(&["worktree"]), SideEffectClass::Unknown);
        assert_eq!(
            class(&["worktree", "list", "--force"]),
            SideEffectClass::Unknown
        );
        assert_eq!(class(&["worktree", "nonsense"]), SideEffectClass::Unknown);
    }

    #[test]
    fn symbolic_ref_separates_the_read_form_from_both_write_forms() {
        assert_eq!(class(&["symbolic-ref", "HEAD"]), SideEffectClass::None);
        assert_eq!(
            class(&["symbolic-ref", "-q", "HEAD"]),
            SideEffectClass::None
        );
        assert_eq!(
            class(&["symbolic-ref", "--quiet", "HEAD"]),
            SideEffectClass::None
        );
        assert_eq!(
            class(&["symbolic-ref", "HEAD", "refs/heads/x"]),
            SideEffectClass::LocalMutation
        );
        assert_eq!(
            class(&["symbolic-ref", "--delete", "HEAD"]),
            SideEffectClass::LocalMutation
        );
        assert_eq!(
            class(&["symbolic-ref", "-d", "HEAD"]),
            SideEffectClass::LocalMutation
        );
        assert_eq!(class(&["symbolic-ref"]), SideEffectClass::Unknown);
        assert_eq!(
            class(&["symbolic-ref", "--nonsense", "HEAD"]),
            SideEffectClass::Unknown
        );
    }

    #[test]
    fn remote_reads_and_writes_are_separated() {
        for argv in [
            vec!["remote"],
            vec!["remote", "-v"],
            vec!["remote", "--verbose"],
            vec!["remote", "get-url", "origin"],
            vec!["remote", "get-url", "--all", "origin"],
        ] {
            assert_eq!(class(&argv), SideEffectClass::None, "{argv:?}");
        }
        for argv in [
            vec!["remote", "add", "origin", "url"],
            vec!["remote", "set-url", "origin", "url"],
            vec!["remote", "remove", "origin"],
            vec!["remote", "rename", "origin", "upstream"],
            vec!["remote", "set-head", "origin", "-a"],
        ] {
            assert_eq!(class(&argv), SideEffectClass::LocalMutation, "{argv:?}");
        }
        assert_eq!(
            class(&["remote", "update"]),
            SideEffectClass::RemoteMutation
        );
        assert_eq!(
            class(&["remote", "prune", "origin"]),
            SideEffectClass::RemoteMutation
        );
        assert_eq!(class(&["remote", "nonsense"]), SideEffectClass::Unknown);
    }

    #[test]
    fn plain_status_is_not_an_observation_but_the_bounded_host_form_is() {
        assert_eq!(class(&["status"]), SideEffectClass::Unknown);
        assert_eq!(
            class(&["status", "--porcelain=v1"]),
            SideEffectClass::Unknown
        );
        assert_eq!(
            class(&[
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all"
            ]),
            SideEffectClass::None
        );
        assert_eq!(
            class(&["--no-optional-locks", "status", "-z"]),
            SideEffectClass::None
        );
        assert_eq!(
            class(&["--no-optional-locks", "status", "--nonsense"]),
            SideEffectClass::Unknown
        );
        // A redirecting global option removes the proof even for the bounded form.
        assert_eq!(
            class(&["-C", "/elsewhere", "--no-optional-locks", "status", "-z"]),
            SideEffectClass::Unknown
        );
    }

    #[test]
    fn show_ref_reads_verify_and_dereferences_but_never_deletes() {
        assert_eq!(
            class(&["show-ref", "--verify", "refs/heads/absent"]),
            SideEffectClass::None
        );
        assert_eq!(class(&["show-ref"]), SideEffectClass::None);
        // `-d` is `--dereference`. Verified against git 2.50.1: `show-ref -d
        // refs/heads/<b>` printed the OID and the branch survived, so this is a
        // read and classifying it as a mutation was simply wrong.
        assert_eq!(
            class(&["show-ref", "-d", "refs/heads/x"]),
            SideEffectClass::None
        );
        assert_eq!(
            class(&["show-ref", "--dereference", "--verify", "refs/heads/x"]),
            SideEffectClass::None
        );
        // `git show-ref` has no `--delete`; an invented spelling is an unlisted
        // option and fails closed.
        assert_eq!(
            class(&["show-ref", "--delete", "refs/heads/x"]),
            SideEffectClass::Unknown
        );
    }

    #[test]
    fn authority_shaping_global_options_are_never_observations() {
        for global in [
            vec!["-C", "/elsewhere", "status"],
            vec!["-C/elsewhere", "status"],
            vec!["-c", "core.fsmonitor=false", "status"],
            vec!["--git-dir=/elsewhere/.git", "status"],
            vec!["--git-dir", "/elsewhere/.git", "status"],
            vec!["--work-tree=/elsewhere", "status"],
            vec!["--config-env", "core.hooksPath=HOOKS", "status"],
            vec!["--exec-path=/tmp/bin", "status"],
            vec!["--namespace=other", "status"],
            vec!["--bare", "rev-parse", "HEAD"],
        ] {
            assert_eq!(class(&global), SideEffectClass::Unknown, "{global:?}");
        }
        // A mutating shape is still a mutating shape behind a redirect.
        assert_eq!(
            class(&["-C", "/elsewhere", "commit"]),
            SideEffectClass::LocalMutation
        );
    }

    #[test]
    fn supported_inert_global_options_still_reach_the_subcommand() {
        assert_eq!(
            class(&[
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all"
            ]),
            SideEffectClass::None
        );
        assert_eq!(
            class(&["--literal-pathspecs", "rev-parse", "HEAD"]),
            SideEffectClass::None
        );
    }

    #[test]
    fn unplaceable_or_missing_shapes_fail_closed() {
        assert_eq!(class(&[]), SideEffectClass::Unknown);
        assert_eq!(class(&["--no-optional-locks"]), SideEffectClass::Unknown);
        assert_eq!(class(&["--nonsense", "status"]), SideEffectClass::Unknown);
        assert_eq!(class(&["nonsense-subcommand"]), SideEffectClass::Unknown);
        assert_eq!(class(&["bisect", "start"]), SideEffectClass::Unknown);
        assert_eq!(class(&["--"]), SideEffectClass::Unknown);
    }

    #[test]
    fn query_subcommands_only_accept_their_listed_options() {
        assert_eq!(class(&["rev-parse", "HEAD"]), SideEffectClass::None);
        assert_eq!(
            class(&["rev-parse", "--show-toplevel"]),
            SideEffectClass::None
        );
        assert_eq!(
            class(&["diff", "--cached", "--name-only", "-z"]),
            SideEffectClass::None
        );
        assert_eq!(
            class(&["diff", "--cached", "--ext-diff"]),
            SideEffectClass::Unknown
        );
        assert_eq!(class(&["show", "--nonsense"]), SideEffectClass::Unknown);
    }

    #[test]
    fn effectful_subcommands_are_mutations_regardless_of_arguments() {
        assert_eq!(
            class(&["add", "--", "src/a.rs"]),
            SideEffectClass::LocalMutation
        );
        assert_eq!(class(&["commit"]), SideEffectClass::LocalMutation);
        assert_eq!(
            class(&["push", "origin", "main"]),
            SideEffectClass::RemoteMutation
        );
        assert_eq!(class(&["fetch"]), SideEffectClass::RemoteMutation);
        assert_eq!(
            class(&["reset", "--hard", "HEAD"]),
            SideEffectClass::LocalMutation
        );
    }

    #[test]
    fn no_ambiguous_shape_is_downgraded_to_an_observation() {
        for argv in [
            vec!["sneaky", "status"],
            vec!["-c", "core.fsmonitor=touch /tmp/x", "status"],
            vec!["status", "--porcelain=v1", "--ahead-behind"],
            vec!["remote", "get-url", "--push", "--all", "-x", "origin"],
            vec!["symbolic-ref", "--short", "HEAD", "refs/heads/x", "extra"],
        ] {
            assert_ne!(class(&argv), SideEffectClass::None, "{argv:?}");
        }
    }
}
