# local-mcp

Local MCP is released under the MIT License. OpenAI Codex dependencies retain
their own upstream licenses and notices. Local MCP is an independent project and is
not affiliated with or endorsed by OpenAI.

This project is not a tool for bypassing OpenAI safety, policy, usage, or rate-limit decisions.
Safety and policy refusals are terminal: Local MCP does not reroute them to another
model, shell, retry, or host-native execution path.

`local-mcp` exposes basic local-machine capabilities as MCP tools: file reads,
image reads, directory listings, sandboxed file writes and commands, plus explicitly approved
unsandboxed command execution. It intentionally does not provide web search or a
dedicated network-request tool.

## Goal Orchestrator

Public `goal_*` tools provide a durable Goal / Task Orchestrator for bounded,
host-controlled work. `goal_start` records an objective without executing it;
`goal_status`, `goal_pause`, `goal_resume`, `goal_cancel`, and `goal_result` manage
or inspect durable state; and `goal_run` advances one Goal for a caller-provided
step budget. Goal state is session-scoped and does not grant filesystem, network,
host-native, Git, publication, or fallback authority. Effectful work continues to
use the existing Local MCP sandbox, approval, side-effect, verification, and
recovery boundaries. Codex processes used by the orchestrator are read-only.

## Platform support

| Platform | Status | Security boundary |
| --- | --- | --- |
| macOS | Supported for Public v1 (native Intel x86_64 validation) | Codex Seatbelt sandbox for ordinary commands; Unix path and approval checks |
| Linux | Supported for Public v1 when requirements below are met | Bubblewrap 0.12.0 or newer, user/PID/network namespaces, and seccomp; Unix path and approval checks |
| Windows | Experimental for Public v1; runtime/security validation deferred | Host-native command and file paths with approval; no Unix-equivalent process sandbox |

Windows is not covered by the macOS release verification described below. Do not treat
successful macOS or Linux validation as Windows security validation.

Commands use separate platform sandboxes: Bubblewrap plus seccomp through the
pinned Codex Linux helper on Linux, and Seatbelt (`sandbox-exec`) on macOS.
Ordinary commands have network access denied. Linux requires upstream Bubblewrap
0.12.0 or newer; the helper rejects older or unparseable versions before starting
the requested command. See [Linux sandbox support](docs/linux_sandbox.md) for
requirements and tested scope. The legacy Landlock path is not selected by Local
MCP and does not replace the full Bubblewrap filesystem/network contract.

## Usage

The primary Public v1 release artifact is the platform-built `local-mcp` binary.
On macOS, build it from source with the locked dependency graph:

```sh
cargo build --release --locked

# Run one persistent MCP server (for example through a tunnel):
local-mcp mcp

# In another terminal, start a session in the project directory:
cd ./some-project
local-mcp start

# Or choose a stable session ID (letters, numbers, "-", "_", and "."):
local-mcp start my-project

# Give the printed session ID to the agent in your prompt. The agent includes it
# in each local-mcp tool call.

# In the approvals UI, allow every unsandboxed call for the session:
/permissions yolo

# Manage the current session from the start screen:
/permission ask
/permission yolo
/permission allow ../another-project
/permission revoke ../another-project
/permission list
/permission status
```

With Nix, `curl` and `bash` are included in the runtime environment. On Linux,
provide upstream Bubblewrap 0.12.0 or newer on the host `PATH`; the flake does not
substitute the pinned Nixpkgs Bubblewrap package because it is older than the
security fix:

```sh
nix run github:nakasyou/local-mcp
nix develop
nix build
```

This release metadata does not promise a crates.io package or a prebuilt GitHub
download. Use the source build above or the optional Nix flake. On Linux, the
release build also includes the `codex-linux-sandbox` helper described below.

The session working directory is the directory where `local-mcp start` was run;
there is no separate persistent cwd setting. On Linux and macOS, sandboxed calls are
always allowed and have no network access. Windows host-native `execute` and
`start_command` calls require approval unless the session is in yolo mode. `without_sandbox`
runs with the service user's full host permissions and network access, so it asks
the approvals process before every call. `/permissions yolo` disables those
prompts only for the lifetime of that session; `/permissions ask`
turns prompts back on. The singular `/permission ...` spelling is also accepted.
Every tool takes a `session_id`. The agent can call `session_info` with the ID
from the prompt to confirm the working directory and sandbox roots. One
`local-mcp mcp` process can therefore serve multiple independently configured
sessions.
`get_image` returns PNG, JPEG, GIF, WebP, BMP, TIFF, and AVIF files as native MCP
image content. Relative image paths are resolved from the session working directory.

Each session uses its own local IPC endpoint: an explicitly permission-restricted
Unix domain socket on Unix, or a named pipe with an explicit current-user-only
Windows DACL. Both the MCP server and the start UI block on I/O, so idle operation
and pending approvals do not use polling timers.

The `start` screen also receives live activity from MCP calls. It shows file and
image reads, directory listings, file edits with unified diffs and line counts,
and command start/completion with output, in a compact Codex-style timeline.
`execute` returns its normal result for commands that finish within 30 seconds.
Longer commands continue in the background and return a `job_id`; use `poll_job`
to check for completion or `stop_job` to terminate them. `without_sandbox`
uses a 20-second foreground limit, then backgrounds long host-native
commands instead of holding one MCP tool call open indefinitely. Use
`start_command` when a primary command should run in the background
immediately without the 30-second foreground wait.

## Codex fallback policy V2

Codex fallback is enabled by default and can be disabled with
`LOCAL_MCP_CODEX_FALLBACK=off`. Policy V2 does **not** treat a nonzero exit
code as permission to launch Codex. Primary execution is classified first,
using explicit host lifecycle and execution-mode evidence plus optional structured
operation metadata. Executable `SANDBOX_PERMISSION` fallback requires proof that
the failed primary operation actually used the sandboxed path.

`execute` and `start_command` accept two additive Policy V2 fields:

- `accepted_exit_codes`: exit codes that have a defined normal/expected
  meaning for the probe. Exit `0` is always accepted.
- `operation`: an optional structured authority/scope/budget contract.
  Callers that do not provide it keep normal Local MCP behavior and do not
  gain executable fallback authority.

Host-side results preserve `exit_code`, `stdout`, and `stderr` and add a
request ID, primary execution mode, host/command lifecycle state, failure class,
side-effect class and state, fallback decision/reason code, fallback depth, budgets,
and verification
status.

The host records only evidence it can know truthfully:

- `host_reached=true` means the Local MCP host received and processed the tool
  request.
- `command_started` is set only after the sandbox process was successfully
  spawned.
- `command_finished` is set only after process completion was observed.

The host cannot infer that a request which never arrived was blocked by the
OpenAI platform. A known outer platform rejection must remain outside the host
and is terminal; Policy V2 never rewrites, splits, reroutes, or sends such an
operation to Codex.

### Failure classes and decisions

| Class / state | Automatic decision |
| --- | --- |
| `SUCCESS` | no fallback |
| `EXPECTED_STATE` | normal result; no fallback |
| `SEMANTIC_FAILURE` | return semantic result; no executable fallback |
| `SANDBOX_PERMISSION` | executable fallback only for a sandboxed primary when every structured authority/scope/state/budget rule passes |
| `HOST_ENVIRONMENT` | diagnose-only at most |
| `TOOL_MISSING` | diagnose-only at most |
| `UNKNOWN` | diagnose-only at most |
| `PLATFORM_SAFETY` | terminal block |
| remote mutation + `UNKNOWN` side effect | terminal block pending read-only reconciliation |
| remote mutation already performed | no retry; budget consumed |

The public `codex_fallback` tool is `DIAGNOSE_ONLY`. Codex runs with a
**read-only** sandbox and may only inspect and report. Public callers cannot
provide executable fallback authority, lifecycle evidence, or side-effect state.
Executable fallback is only an internal continuation of the host-observed
`execute`/`start_command` path.

Policy V2 deliberately does not give Codex arbitrary mutation authority. For
executable fallback, Codex performs a read-only preflight; Local MCP then
constructs the exact host-native command from the structured operation and
performs independent read-only postcondition verification. This prevents the
model from broadening `git add -- pathA pathB` into `git add -A`, changing a
normal push into a force push, or inventing additional mutations.

The initial automatic execute allowlist is intentionally small:

- exact-path `git_stage_paths` using canonical `git add -- <paths...>` scope.

`read_only_command` is intentionally not in the executable fallback allowlist:
a caller-supplied read-only label cannot grant host-native execution.

Pathspecs that can broaden staging (`.`, parent traversal, globs, Git pathspec
magic, `.git` internals, and absolute paths) are rejected before mutation.
Other local mutation types may be represented for classification/budget
tracking but are not auto-executed.

Remote mutations such as `git push`, GitHub workflow dispatch, API writes, or
publication are never automatically re-executed through Codex. Once a remote
request may have been transmitted, side-effect state is `UNKNOWN`; retry is
blocked until an authorized read-only reconciliation proves whether the
operation happened.

Side-effect states are:

- `CONFIRMED_NOT_PERFORMED`;
- `CONFIRMED_PERFORMED`;
- `UNKNOWN`.

An operation may supply `attempt_budget_remaining` and
`side_effect_budget_remaining`. The primary attempt and fallback share these
budgets. Proven non-execution leaves budget available; confirmed execution
consumes it; unknown state locks it. A fallback is never a free extra
mutation.

Executable fallback always requires independent post-verification. For
`git_stage_paths`, Local MCP snapshots `git ls-files --stage -z` before the
sandbox attempt, proves the failed attempt did not change the index before
fallback, executes only the structured exact-path staging command, then
compares the index again and rejects any changed path outside the allowlist.
Codex prose is never accepted as postcondition evidence.

Useful configuration:

- `LOCAL_MCP_CODEX_FALLBACK=off` disables fallback.
- `LOCAL_MCP_CODEX_FALLBACK_AUTO_EXECUTE=off` keeps classification/diagnosis
  behavior but disables automatic executable fallback.
- `LOCAL_MCP_CODEX_FALLBACK_MODEL` selects the Codex model.
- `LOCAL_MCP_CODEX_CLI_PATH` overrides the Codex executable.
- `LOCAL_MCP_CODEX_FALLBACK_MAX_DEPTH` may reduce the maximum depth; Policy V2
  hard-caps it at `1`.

Safety and policy refusals are not fallback opportunities. The implementation
does not use alternate command spellings, `without_sandbox`, Codex, or another
route to evade platform, tool, host, or sandbox safety controls.

On Linux, the build produces `local-mcp` and its sibling `codex-linux-sandbox`;
install or copy both into the same directory, and ensure upstream Bubblewrap
0.12.0 or newer is available in `PATH`. Older versions fail closed before the
requested command starts. On macOS, only `local-mcp` is needed; sandboxed commands use
the system `/usr/bin/sandbox-exec`. Windows support is **experimental for Public v1**: it uses named-pipe IPC and direct argv
execution, and it does not currently provide the filesystem/network sandbox enforced
by Linux and macOS. The named pipe rejects remote clients and is created with an
explicit current-user-only DACL (owner and sole allow ACE are the current Windows
user SID; no Everyone, Anonymous, Authenticated Users, SYSTEM, or Administrators
grant). Consequently, `execute` and `start_command` require approval
on Windows unless the session is in yolo mode, while `write_file` performs a host-native mutation only after the persisted
session permitted-root check and explicit approval. It must not be mistaken for the Unix sandbox. Because these command operations are host-native,
permission-like failures on Windows do not qualify as `SANDBOX_PERMISSION` and
cannot enter executable sandbox fallback. Unlike Unix's 0600 socket mode bit, the
Windows ACL is installed at pipe creation via `CreateNamedPipeW` security
attributes (not a post-create default descriptor). Windows builds use the MSVC Rust target and
require Visual Studio Build Tools with the "Desktop development with C++"
workload. Build from a Developer PowerShell with
`cargo build --locked --release`.
