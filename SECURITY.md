# Security policy

Please report security issues privately rather than opening a public issue with an
exploit or credential. If private vulnerability reporting is enabled for the canonical
repository, use its GitHub Security Advisory form. Otherwise use a private contact
channel provided by the repository owner; this file intentionally does not invent an
email address or claim a final repository owner. Include the affected version,
operating system, reproduction steps, and impact. Do not include real secrets.

## Supported versions

The current Public v1 line (`0.1.x`) is the supported release line. There are no
older public release lines documented at this time. The macOS release-closure claim
covers native Intel x86_64 validation only; Apple Silicon is not claimed as locally
validated and remains a remote-CI target. Windows remains experimental and
is not included in the macOS release-closure claim. Named-pipe current-user isolation
is now mechanically validated; remaining Windows runtime/sandbox boundaries still
require separate closure before experimental status can be removed.

## Security model

Session `permitted_directories` are the filesystem authority boundary. MCP arguments
cannot add roots; only the local approval UI command `/permission allow <directory>`
can do so. Existing targets and new-file parents are canonicalized, and symlink
escapes are rejected.

Unix command execution uses the Codex sandbox with a minimal environment and no
network for ordinary sandboxed calls. Windows support is experimental for Public v1:
it has no equivalent process sandbox, so host-native command and file mutation paths
are explicitly described and approval-gated. Windows named pipes reject remote clients
and are created with an explicit current-user-only DACL (owner and sole allow ACE are
the current Windows user SID; no Everyone, Anonymous, Authenticated Users, SYSTEM, or
Administrators grant). On Unix, GoalLatch-owned state directories are explicitly
restricted to mode 0700 and session/Goal JSON, atomic temporary files, and Goal lock
files to mode 0600. Unix approval sockets are 0600 inside a 0700 per-user directory.
Creation, open, load, and save paths establish or tighten owned state without relying
on a permissive process umask and fail closed when private state cannot be established.
Windows POSIX-mode claims do not apply; the named-pipe DACL remains the Windows IPC
boundary. `without_sandbox` always means host-native execution with network and mutation capability.

## Dependency advisory status (2026-09-25)

The locked upstream Codex dependency graph includes `quick-xml 0.38.4`
(RUSTSEC-2026-0194 and RUSTSEC-2026-0195) and `hickory-proto 0.25.2`
(RUSTSEC-2026-0118 and RUSTSEC-2026-0119). It also includes `lru 0.16.4`
(RUSTSEC-2026-0253, an unsound `LruCache::pop()` panic-safety issue fixed in
0.18.2) and unmaintained transitive packages: `derivative 2.2.0`
(RUSTSEC-2024-0388), `fxhash 0.2.1` (RUSTSEC-2025-0057), and `paste 1.0.15`
(RUSTSEC-2024-0436). The pinned Codex protocol manifest requires
`quick-xml 0.38.4`, and its Rama DNS dependency requires the Hickory 0.25
series, so those fixed versions are not selectable without an upstream pin or
manifest change. GoalLatch does not call Codex's XML hook-prompt parser, does not
configure the Codex-managed network proxy for sandboxed execution, and does not
enable Hickory DNSSEC validation features. These inherited findings remain in
the lockfile and should be reassessed with any Codex/Rama/Starlark dependency
migration; this status is not a claim that the upstream crates are generally safe
for other callers.

Managed worktrees are host-owned lifecycle state, not new authority. Four requirements are separate and none substitutes for another.

1. Workspace opt-in. A `goal_start` request may set `workspace_mode: MANAGED_WORKTREE`. It is worktree lifecycle policy only and cannot carry a worktree path, managed root, branch, ref, base commit, repository common directory, Git argv, or permission root. It is isolation policy, not consent: it does not by itself authorize a Git mutation, and it does not imply a Unix-equivalent sandbox on any platform.
2. Session filesystem authority. The managed root is host configuration or state (`LOCAL_MCP_MANAGED_WORKTREE_ROOT`, otherwise under the GoalLatch state directory) and the exact target is `<managed-root>/<session-id>/<goal-id>`, derived by the host; a root that overlaps the primary workspace or the repository common directory is rejected. That exact target must already be covered by the current Session `permitted_directories`. GoalLatch never appends it, and a durable Goal does not recreate a revoked authorization, so an operator must use `/permission allow` for a managed Goal to be prepared. Path authority is never approval, and approval is never path authority.
3. Windows host-native mutation approval. On Windows, where there is no process sandbox, the managed `git worktree add` is a host-native mutation and remains approval-gated under the existing Windows policy, through the same local approval system and yolo semantics used by `without_sandbox` and the staging mutation. Each invocation is approved separately; a yolo session authorizes each one under the existing policy. A denial, a missing session approval channel, or a malformed reply performs no Git invocation. This requirement is Windows policy only and adds no interactive step to Linux or macOS, which keep their existing sandbox, network, and path behavior.
4. Durable bounded retry and recovery. The managed workspace may spend at most two authorized host Git creation invocations over the entire lifetime of that creation sequence: the initial invocation, plus at most one retry after reconciliation mechanically proves no side effect. The consumed count is durable and monotonic. A process restart, Goal resume, repeated `goal_run`, transport failure, host restart, or a crash between consuming an attempt and spawning Git never replenishes it, and reaching `ACTIVE` or `BLOCKED` retains the count as evidence. No Git invocation happens at all when Session authority, eligibility, observation, or approval fails, and an attempt is consumed only once that attempt is durably persisted. Every other outcome - a branch or path conflict, a `HEAD` or common-directory mismatch, stale metadata, or an ambiguous observation - blocks for explicit host recovery and is never repaired with force, with `git worktree remove`, or with a prune. Creation never fetches, pulls, pushes, merges, rebases, force-pushes, deletes branches, or publishes. A `MANAGED_WORKTREE` Goal cannot be planned in this release line, so the linked worktree is created and reconciled but not used for Planner, writer, verifier, or worker execution, and a Goal the foreground runner would not advance is not prepared at all. Windows managed-worktree lifecycle does not change Windows' experimental sandbox classification.

Public MCP operation metadata describes requested intent only. Legacy caller fields
named `authorized` or `side_effect_state` are not advertised and are ignored as
authority when older requests are parsed. The only automatic executable fallback is
exact `git_stage_paths`: the host validates literal paths, repository scope, and a
host-resolved Git identity, then explicit local approval is required before the
host-built staging mutation. The staged Git snapshot is revalidated after approval,
after any agent preflight, and immediately before the primary sandboxed command. A
same-user process can still race the final filesystem, Git-config, and executable
checks because Git provides no shared atomic snapshot; these checks reduce but do
not eliminate that TOCTOU window. Read-only operation labels never enable
executable fallback, and public `codex_fallback` is diagnose-only. Platform and
safety refusals are terminal; remote mutations with ambiguous side effects are not retried.

Executable fallback additionally requires host-owned proof that the requested
command started. A sandbox wrapper starting, exiting nonzero, or printing a
setup-rejection message is not such proof: the wrapper's exit status cannot
distinguish a setup failure from the requested command's own exit value, and the
requested command controls its own output and exit value. On Linux and macOS the
sandbox is provided by such a wrapper, so a completed attempt leaves the
requested command's start unproven and fails closed to diagnose-only
(`FALLBACK_DENIED_LIFECYCLE`). Sandbox setup refusals decided by the host itself,
including the Bubblewrap minimum-version gate for CVE-2026-87766, are classified
from typed host-owned evidence and are terminal blocks (`PLATFORM_SAFETY` or
`SANDBOX_SETUP`) rather than as permission failures.
Activity and approval messages can include caller-supplied command arguments, paths,
and file-diff content. Do not place secrets in commands or files being edited when
those messages are visible to the local approval UI; GoalLatch does not promise
redaction of user-supplied values.

Maintainers should acknowledge a private report when practical, coordinate a fix or
mitigation, and publish release guidance only after the affected security boundary
has been verified. These are response goals, not a guaranteed service-level agreement.
