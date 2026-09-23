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
Administrators grant). Unix uses 0600 sockets/0700 state directories.
`without_sandbox` always means host-native execution with network and mutation capability.

## Dependency advisory status (2026-09-23)

The locked upstream Codex dependency graph includes `quick-xml 0.38.4`
(RUSTSEC-2026-0194 and RUSTSEC-2026-0195) and `hickory-proto 0.25.2`
(RUSTSEC-2026-0118 and RUSTSEC-2026-0119). The pinned Codex protocol manifest
requires `quick-xml 0.38.4`, and its Rama DNS dependency requires the Hickory
0.25 series, so the fixed versions are not selectable without an upstream pin or
manifest change. Local MCP does not call Codex's XML hook-prompt parser, does not
configure the Codex-managed network proxy for sandboxed execution, and does not
enable Hickory DNSSEC validation features. These advisories remain in the lockfile
and should be reassessed with any Codex/Rama dependency migration; this status is
not a claim that the upstream crates are generally safe for other callers.

Codex fallback is public diagnose-only. Platform and safety refusals are terminal;
remote mutations with ambiguous side effects are not retried. Activity and approval messages can include caller-supplied command arguments, paths,
and file-diff content. Do not place secrets in commands or files being edited when
those messages are visible to the local approval UI; Local MCP does not promise
redaction of user-supplied values.

Maintainers should acknowledge a private report when practical, coordinate a fix or
mitigation, and publish release guidance only after the affected security boundary
has been verified. These are response goals, not a guaranteed service-level agreement.
