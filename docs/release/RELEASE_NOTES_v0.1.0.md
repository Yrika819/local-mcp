# Local MCP v0.1.0 (Public v1)

Local MCP is an independent project. It is not affiliated with or endorsed by
OpenAI, and it is not an official release of
[upstream `nakasyou/local-mcp`](https://github.com/nakasyou/local-mcp), which it
continues under the MIT License with substantial rework of the orchestrator,
sandbox, and fallback boundaries.

## What this is

Local MCP exposes basic local-machine capabilities as MCP tools: file reads,
image reads, directory listings, sandboxed file writes, and sandboxed command
execution, plus explicitly approved unsandboxed execution. It deliberately
provides no web search and no general network-request tool.

On top of that it ships a **Goal / Task Orchestrator**: a durable, bounded way
to have an agent work through an objective while the host keeps control of
what happens. `goal_start` records an objective without running it, and
`goal_run` advances one Goal for a caller-supplied step budget. The split
between those three roles is the core of the design:

- **read-only** workers inspect and plan,
- the **writer** is the only role that mutates,
- the **verifier** re-checks the result against the request.

Goal state is session-scoped. It confers no filesystem, network, host-native,
Git, publication, or fallback authority of its own; effectful work still passes
through the same sandbox, approval, and verification boundaries as any other
call.

## Sandboxing and the permission model

Ordinary command execution is sandboxed on Linux and macOS, with no network
access:

- **Linux x86_64** uses upstream **Bubblewrap 0.12.0 or newer** with user,
  PID, and network namespaces plus seccomp, driven by a `codex-linux-sandbox`
  helper. **Linux aarch64 is experimental**: it builds natively in CI, but the
  repository's Linux sandbox evidence documents x86_64 and does not establish
  ARM64 sandbox closure. Both architectures' executables dynamically link to
  both architectures' executables dynamically link to the host GNU/Linux
  runtime. The measured highest required GLIBC symbols are `GLIBC_2.34` for
  x86_64 and `GLIBC_2.39` for aarch64; use a distribution providing at least
  that version. The release build also records each binary's `ldd`
  dependencies in its Actions job log.
- **macOS** uses the system **Seatbelt** (`sandbox-exec`); the release build
  records system library dependencies with `otool -L`. Native release-closure
  evidence is Intel x86_64 only; Apple Silicon is a remote CI target and is not
  claimed as locally release-closure validated.
- **Windows** has **no Unix-equivalent process sandbox** in this release. See
  the experimental note below.

`permitted_directories` are the filesystem authority boundary. MCP arguments
cannot add roots; only the local approval UI command `/permission allow
<directory>` can. Paths are canonicalized and symlink escapes are rejected.

`without_sandbox` means host-native execution with full user permissions and
network access. It always asks for approval first, and `/permissions yolo`
suppresses those prompts for the lifetime of one session only.

## Fallback safety

Codex fallback is enabled by default and can be disabled with
`LOCAL_MCP_CODEX_FALLBACK=off`. Two rules keep it from becoming a hole:

1. A nonzero exit code is not permission to launch Codex. Primary execution is
   classified first.
2. Executable fallback requires host-owned proof that the requested command
   actually started. On Linux and macOS the sandbox is a wrapper, and a
   wrapper starting or exiting nonzero says nothing about the command it
   launched, so the attempt fails closed to diagnose-only
   (`FALLBACK_DENIED_LIFECYCLE`).

The one automatic executable fallback is exact `git_stage_paths`, which
validates literal paths, repository scope, and a host-resolved Git identity,
and still requires explicit local approval.

## Supported platforms

| Platform | Status | Sandbox |
| --- | --- | --- |
| Linux x86_64 | Supported | Bubblewrap 0.12.0+, namespaces, seccomp |
| Linux aarch64 | **Experimental**; native build in CI, sandbox not closure-validated | Bubblewrap 0.12.0+, namespaces, seccomp |
| macOS x86_64 | Supported; native Intel release evidence | Seatbelt (`sandbox-exec`) |
| macOS Apple Silicon | Native build/test target in CI; not locally release-closure validated | Seatbelt (`sandbox-exec`) |
| Windows x86_64 | **Experimental** | None; host-native with approval |
| Windows aarch64 | **Experimental** | None; host-native with approval |

**Windows is experimental and is not covered by the Linux or macOS security
model.** It has no process sandbox equivalent, so command and file mutation
paths run host-native and are approval-gated. Named-pipe IPC is restricted to
the current user with an explicit DACL, but that is an IPC boundary and not a
substitute for a sandbox. Do not read successful macOS or Linux validation as
Windows security validation.

## Linux requirements

Linux needs upstream **Bubblewrap 0.12.0 or newer** on `PATH`. That release is
the first to fix [CVE-2026-87766](https://github.com/containers/bubblewrap/security/advisories/GHSA-pxhw-h44j-8pfx);
the helper rejects anything older or unparseable and exits 126 before starting
any requested command. Ubuntu's `0.11.1-1ubuntu0.2` backport was reverted in
[USN-8779-2](https://ubuntu.com/security/notices/USN-8779-2), so the check
rejects it too, by design.

The Linux archive contains **two** binaries and they must stay in the same
directory. `local-mcp` finds `codex-linux-sandbox` next to itself and refuses
to run a sandboxed command without it. See
[docs/linux_sandbox.md](../docs/linux_sandbox.md) for the full contract.

## Known limitations

- Windows is experimental and lacks a process sandbox.
- Linux aarch64 is an experimental native build target; sandbox behavior has
  not been release-closure validated on a host that can create the required
  namespaces.
- The Bubblewrap minimum-version gate can reject a distribution package whose
  upstream-reported version is below 0.12.0 even where a fix was backported.
- Windows remains experimental; its broader runtime dependency compatibility
  has not been established as part of this release.
- Hosted Linux images disagree about whether an unprivileged user can write
  `/proc/<pid>/uid_map`; where they cannot, the sandbox cannot be built at all.
  This is a kernel property, not a Bubblewrap property.
- Executable Codex fallback is diagnose-only on Linux and macOS.
- A same-user process can still race the final filesystem, Git-config, and
  executable checks in the staged-Git path. Those checks reduce but do not
  eliminate that TOCTOU window.
- Activity and approval messages can include your command arguments, paths, and
  file diff content. Do not put secrets in commands or files being edited.

## Known dependency advisories

The locked dependency graph inherits advisories from the pinned upstream Codex
revision, including `quick-xml 0.38.4` and `hickory-proto 0.25.2`, plus
`lru 0.16.4` (unsound) and several unmaintained transitive packages. They are
not selectable away without an upstream pin change, and Local MCP does not
exercise the affected code paths. **These remain known findings, not a
resolved set.** The current list, with dates and rationale, is in
[SECURITY.md](../SECURITY.md).

This release is not a claim of zero known vulnerabilities.

## Installation

Download the archive for your platform, verify it against `SHA256SUMS`, and
extract it. Full per-platform instructions are in
[docs/release/INSTALL.md](INSTALL.md).

```sh
sha256sum -c SHA256SUMS          # Linux
shasum -a 256 -c SHA256SUMS      # macOS
```

Source builds and the Nix flake remain supported:

```sh
nix run github:Yrika819/local-mcp
cargo build --release --locked
```

## License and attribution

MIT. See [LICENSE](../LICENSE) for the copyright notice and
[THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md) for dependency license
evidence. The original upstream copyright notice is preserved unchanged.
