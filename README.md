# GoalLatch

**Host-controlled orchestration for local coding agents.**

GoalLatch is an independent, MIT-licensed MCP server for bounded local-machine
access and durable Goal/Task orchestration. It helps an MCP-capable coding agent
inspect a project, propose bounded work, and report verified results while the
host retains control of filesystem scope, sandboxing, approvals, and recovery.

GoalLatch is not an OpenAI product and is not affiliated with or endorsed by
OpenAI. It uses pinned OpenAI Codex components; their licenses and notices are
included in release archives. The repository and executable retain the
compatibility name `local-mcp`.

## Why GoalLatch

Local coding agents need access to project files and commands, but an agent's
request or saved task state should not silently grant that access. GoalLatch
keeps session permissions and execution policy host-owned, separates planning
from mutation, and verifies results instead of trusting a model's claim of
success.

- Durable, bounded Goal/Task orchestration and recovery.
- Host-controlled filesystem roots; MCP arguments cannot add permission roots.
- Sandboxed ordinary command execution on supported Linux and macOS platforms.
- Explicit approval for host-native unsandboxed execution.
- Writer proposals are applied by the host and checked by verification.
- Safety refusals are terminal; ambiguous side effects are reconciled before
  retry.

## Platform support

| Platform | Status | Important boundary |
| --- | --- | --- |
| Linux x86_64 | Supported when host requirements are met | Requires upstream Bubblewrap 0.12.0+ and the required namespaces/seccomp. |
| Linux aarch64 | Experimental | Sandbox closure is not validated for this architecture. |
| macOS x86_64 | Supported; native Intel release evidence | Seatbelt sandbox for ordinary commands. |
| macOS Apple Silicon | CI target; not locally release-closure validated | Do not infer local release validation from CI builds. |
| Windows x86_64 / ARM64 | Experimental | Windows has no Unix-equivalent process sandbox; command and mutation paths are host-native and approval-gated. |

Windows' current-user-only named-pipe DACL is an IPC boundary, not a process
sandbox. Do not treat YOLO as a harmless convenience: it suppresses approval
prompts for the lifetime of that session and can authorize host-native actions
with the service user's permissions. Use the normal ask/approval flow and a
least-privilege account, especially on Windows. See [SECURITY.md](SECURITY.md)
for the authoritative security model and [platform-specific install notes](docs/release/INSTALL.md).

## Quick Start

GoalLatch v0.1.1 is formally released. Download the matching archive from the
[v0.1.1 GitHub release](https://github.com/Yrika819/local-mcp/releases/tag/v0.1.1),
verify its checksum, and follow the platform steps in the
[Quick Start guide](docs/QUICK_START.md). The full archive contents and install
requirements are in [the installation guide](docs/release/INSTALL.md).

For a reproducible source build (requires Rust 1.96 as specified in
`Cargo.toml`):

```sh
git clone https://github.com/Yrika819/local-mcp.git
cd local-mcp
cargo build --release --locked
```

The executable is `target/release/local-mcp` on Unix and
`target/release/local-mcp.exe` on Windows. Building successfully is not a claim
of equivalent sandbox validation across operating systems.

## Connect an MCP client

GoalLatch exposes a **stdio MCP server**: run one persistent `local-mcp mcp`
process under the MCP client, and run `local-mcp start` separately in the
project directory to create a session and show its local permission UI. The
session ID printed by `start` must accompany every tool call. See
[Connecting an MCP client](docs/MCP_CLIENTS.md) for the protocol structure,
session lifecycle, tools, and client-specific caveats. Client configuration
formats vary; this repository does not claim a verified configuration for every
MCP client. MCP transport and direct tools do not require the separate Codex CLI
used by Goal model execution; see the [Quick Start](docs/QUICK_START.md) for
that prerequisite.

## First Goal

After the MCP client is connected, use the session ID from the local `start`
screen and the tools exposed by GoalLatch:

1. Call `session_info` and confirm the working directory and permitted roots.
2. Call `goal_start` with a small, read-only inspection objective.
3. Call `goal_status` to inspect the durable Goal.
4. Call `goal_run` with a bounded `max_steps` value.
5. Call `goal_status` and `goal_result` to inspect the actual state and result.

`goal_start` records a Goal; it does not itself run work. A Goal or model cannot
grant itself filesystem, network, host-native, Git, publication, or fallback
authority. Example procedures and recovery guidance are in
[the practical examples](docs/examples/README.md).

## Tools and runtime behavior

The MCP server provides file/image reads, directory listings, bounded writes,
sandboxed command execution, background-job controls, and Goal tools. It does
not provide web search or a general network-request tool. `execute` returns
foreground results within 30 seconds and returns a `job_id` for longer work;
`start_command` starts background work immediately. Poll or stop a retained job
with its job tools. Unsandboxed calls have a 20-second foreground limit before
being retained as jobs. Reads/listings fail rather than silently presenting
truncated content, and writes reject oversized content. See the detailed
[security and resource behavior](SECURITY.md).

## Security model at a glance

- `permitted_directories` are the filesystem authority boundary; MCP arguments
  cannot extend them.
- Ordinary command execution is sandboxed and network-denied on supported Linux
  and macOS configurations. Linux requires Bubblewrap 0.12.0 or newer.
- Windows is **Experimental** and has **no Unix-equivalent process sandbox**.
  Windows host-native execution and file mutation remain approval-gated.
- `without_sandbox` has host-user permissions and network capability. YOLO
  suppresses prompts for a session; it does not create a sandbox.
- Goal state is not authority. Writer changes pass through host-owned mutation
  and verification paths. Ambiguous side effects are not blindly retried.
- Activity and approval UI messages can contain caller-provided commands,
  paths, and file diffs; do not put secrets in those values.

Read [SECURITY.md](SECURITY.md) before enabling write or command access. Report
vulnerabilities privately as described there; do not publish exploits or
credentials in Issues.

## Limitations

- Windows remains experimental; no Unix-equivalent process sandbox is provided.
- Linux aarch64 is experimental; sandbox behavior is not release-closure
  validated there.
- macOS Apple Silicon is a CI target but not locally release-closure validated.
- Linux hosts need a compatible Bubblewrap version, namespaces, seccomp, and
  kernel support; distro backports below the upstream version gate can be
  rejected.
- Long-running mixed Goal/Job load and tail latency under high load are not
  fully verified. See the v0.1.1 [release notes](docs/release/RELEASE_NOTES_v0.1.1.md)
  and [known security findings](SECURITY.md).
- Release binaries are not code-signed/notarized. Check `SHA256SUMS` before
  running an archive.

## Documentation

- [Quick Start](docs/QUICK_START.md)
- [MCP client connection guide](docs/MCP_CLIENTS.md)
- [Hands-on examples](docs/examples/README.md)
- [Security policy and model](SECURITY.md)
- [Installation details](docs/release/INSTALL.md)
- [Linux sandbox requirements](docs/linux_sandbox.md)
- [v0.1.1 release notes](docs/release/RELEASE_NOTES_v0.1.1.md)
- [Public roadmap](docs/ROADMAP.md)
- [Contributing](CONTRIBUTING.md)

## Contributing

Bug reports, carefully scoped improvements, and documentation fixes are
welcome. Read [CONTRIBUTING.md](CONTRIBUTING.md) and the security boundaries
before opening a pull request. Do not include credentials, private data, or
unredacted logs in an issue. See the [security reporting instructions](SECURITY.md)
for private vulnerability reports.

## Upstream and attribution

GoalLatch originated from [nakasyou/local-mcp](https://github.com/nakasyou/local-mcp),
which is MIT-licensed. GoalLatch is an independent downstream continuation, not
an official successor to that project and not affiliated with or endorsed by
its maintainers. GoalLatch's substantial changes include its Goal/Task
orchestrator and sandbox and fallback boundaries; see [SECURITY.md](SECURITY.md)
and [CHANGELOG.md](CHANGELOG.md). The upstream MIT copyright notice remains in
[LICENSE](LICENSE). GoalLatch is not an OpenAI product, partner, or endorsed
project; pinned OpenAI Codex components keep their own licenses and notices,
listed in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) and included in
release archives.
