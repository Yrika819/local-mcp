# Connecting an MCP client

GoalLatch implements an MCP server over **stdio**. The persistent server
process is session-independent; the session itself is created separately by
`local-mcp start` and selected by a `session_id` argument on each GoalLatch
tool call.

This documents the checked-in CLI and server shape. We do not currently claim
that a specific third-party MCP client's configuration has been tested, and
therefore do not provide client-specific JSON that may be stale or invented.
Use your chosen client's current documentation for its stdio-server setup.

## Process layout

1. Start the approval/session UI in the project directory:

   ```sh
   cd /path/to/project
   local-mcp start
   ```

   The UI shows the session ID, working directory, and permitted roots. Keep it
   open while using the session.

2. In the MCP client, register a local **stdio** server process whose command is
   the installed `local-mcp` executable and whose argument is `mcp`. The MCP
   server reads protocol messages from stdin and writes responses to stdout.
   Do not launch it as a shell command that prints banners or other text to
   stdout; protocol stdout must remain available to MCP framing.

3. Keep the server and `start` UI running. A single `local-mcp mcp` process
   serves multiple sessions, but each tool call is bound to the session ID
   supplied in its arguments.

On Windows the executable may be `local-mcp.exe`; configure its absolute path
if the MCP client does not inherit the expected `PATH`. Windows support is
Experimental and has no Unix-equivalent process sandbox. Linux clients also
need the installed `codex-linux-sandbox` sibling and host Bubblewrap
0.12.0+; all platforms need the `atomic-publish` sibling for file writes.
See [the install guide](release/INSTALL.md).

## Session ID and IPC

`local-mcp start [SESSION_ID]` creates/displays a session for the current
working directory. Without an argument the app generates an ID. An explicit ID
may contain only ASCII letters, digits, `-`, `_`, and `.`, must be at most 64
bytes, and cannot be `.` or `..`. Do not assume that a session ID alone is a
secret or permission grant. Each tool call must include the
session ID. Start another session in another project directory to create a
separate working-directory/permission context.

The app uses a session-local IPC endpoint between its UI and server: a
permission-restricted Unix domain socket on Unix, or a current-user-only named
pipe on Windows. This local IPC mechanism is internal to GoalLatch; an MCP
client connects to the **stdio** server, not directly to the UI IPC endpoint.

Call `session_info` first and verify the returned `cwd` and
`permitted_directories`. MCP arguments cannot add permission roots. Only the
local permission UI can update directory permissions.

## Tools and permissions

The server advertises local filesystem/command tools, background-job controls,
diagnostic fallback, and durable Goal tools. A minimal first verification is:

1. `session_info({"session_id":"<id>"})` — inspect the active session.
2. `list_directory({"session_id":"<id>","path":"."})` — read-only listing
   within the session's allowed roots.
3. `goal_start({"session_id":"<id>","title":"Inspect project","objective":"Read-only: inspect the project structure and summarize its language, test command, and any obvious missing documentation. Do not edit files or run commands."})`.
4. Use the returned Goal ID with `goal_status`, then `goal_run` with
   `max_steps` between 1 and 256, and inspect `goal_status`/`goal_result`.

Examples above show request shapes and proposed actions, not guaranteed outputs
or a claim that a third-party client was used. For a safer first run, use a
throwaway project with no secrets and read-only criteria. Goal state does not
create authority. Command execution, writing, Windows host-native operations,
and managed-worktree lifecycle retain their separate host permission and
approval requirements.

The MCP transport and direct read tools do not require Codex CLI. `goal_start`
only records a Goal; `goal_run` requires the external Codex CLI for model-backed
planning and work. GoalLatch requests local approval before each model process
and invokes Codex in read-only mode. See the
[Quick Start model prerequisites](QUICK_START.md#goal-model-execution-prerequisites)
for executable discovery, model selection, authentication, and availability
limitations. No Codex credentials are inspected or managed by GoalLatch.

## Client configuration status

No named MCP client configuration is published as verified here. MCP clients
vary in how they represent executable paths, arguments, environment variables,
and process lifetime. Follow the client vendor's current stdio-server guide,
then set the command to the actual `local-mcp` executable and pass the `mcp`
argument. If the client cannot start a local stdio process, compatibility has
not been established; do not assume network transport support.

## Troubleshooting

- **Server fails to launch:** run `local-mcp --version` and
  `local-mcp --help` directly; ensure the MCP client can resolve the absolute
  executable path.
- **No tools appear:** confirm the client starts `local-mcp mcp` as a stdio
  process and does not send non-protocol text to its stdout.
- **Session not found / tool rejects session:** keep `local-mcp start` open,
  copy the exact current session ID, and ensure each tool request includes it.
- **Path denied:** inspect `session_info`; use the local permission UI to
  explicitly grant only the required directory. Do not widen access by passing
  a different `cwd` or changing request arguments.
- **Linux sandbox unavailable:** check Bubblewrap 0.12.0+, namespaces, seccomp,
  and the supported host requirements in [Linux sandbox docs](linux_sandbox.md).
  Do not disable security controls to bypass a refusal.
- **Approval prompt:** review the exact command/path/diff before deciding.
  Avoid YOLO; on Windows it is especially consequential because execution is
  host-native.
- **Possible interrupted operation:** inspect its durable status/result and
  current filesystem state before retrying. Ambiguous side effects must not be
  assumed absent.
