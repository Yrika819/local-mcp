# local-mcp

`local-mcp` exposes basic local-machine capabilities as MCP tools: file reads,
image reads, directory listings, sandboxed file writes and commands, plus explicitly approved
unsandboxed command execution. It intentionally does not provide web search or a
dedicated network-request tool.

Commands are isolated with OpenAI Codex's `codex-rs/sandboxing`: Landlock and the
Linux sandbox helper on Linux, and Seatbelt (`sandbox-exec`) on macOS. Network
access is denied for ordinary commands.

## Usage

```sh
cargo build --release

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

With Nix, `curl` and `bash` are included in the runtime environment. Linux builds
also include `bwrap`:

```sh
nix run github:OWNER/local-mcp
nix develop
nix build
```

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
Unix domain socket on Unix, or a named pipe using Windows' default security
descriptor. Both the MCP server and the start UI block on I/O, so idle operation
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

The explicit `codex_fallback` tool has two modes:

- `DIAGNOSE_ONLY`: Codex runs with a **read-only** sandbox and may only inspect
  and report.
- `EXECUTE_AUTHORIZED_OPERATION`: available only for a `SANDBOXED` primary
  execution and an explicitly authorized, structured, allowlisted operation with
  confirmed host execution evidence,
  `CONFIRMED_NOT_PERFORMED` state, remaining budget, exact scope, and
  `fallback_depth == 0`.

Policy V2 deliberately does not give Codex arbitrary mutation authority. For
executable fallback, Codex performs a read-only preflight; Local MCP then
constructs the exact host-native command from the structured operation and
performs independent read-only postcondition verification. This prevents the
model from broadening `git add -- pathA pathB` into `git add -A`, changing a
normal push into a force push, or inventing additional mutations.

The initial automatic execute allowlist is intentionally small:

- exact structured `read_only_command`;
- exact-path `git_stage_paths` using canonical `git add -- <paths...>` scope.

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
install or copy both into the same directory, and ensure `bwrap` (bubblewrap) is
available in `PATH`. On macOS, only `local-mcp` is needed; sandboxed commands use
the system `/usr/bin/sandbox-exec`. Windows uses named-pipe IPC and direct argv
execution; it does not currently provide the filesystem/network sandbox enforced
by Linux and macOS. Consequently, `execute` and `start_command` require approval
on Windows unless the session is in yolo mode, while `write_file` writes directly
to the requested host path. Because these command operations are host-native,
permission-like failures on Windows do not qualify as `SANDBOX_PERMISSION` and
cannot enter executable sandbox fallback. The Windows named pipe uses the
[default security descriptor](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights),
which grants full control to LocalSystem, administrators, and the creator owner,
and read access to Everyone and anonymous users; unlike Unix, `local-mcp` does not
install an explicit per-user ACL. Windows builds use the MSVC Rust target and
require Visual Studio Build Tools with the "Desktop development with C++"
workload. Build from a Developer PowerShell with
`cargo build --locked --release`.
