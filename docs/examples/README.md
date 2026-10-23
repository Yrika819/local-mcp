# Practical examples

These are guided scenarios using the checked-in GoalLatch tools and the MCP
stdio transport. They are not captured transcripts from a third-party client.
Tool results depend on the project and configured model. Use a disposable
project, inspect every permission prompt, and do not include secrets in project
files or commands.

## Example A: Read-only project inspection

**Purpose:** Ask a Goal to summarize a small Rust project's layout and test
command without changing it.

**Prerequisites:** GoalLatch v0.1.1, a running `local-mcp start` session in a
small Rust project, and a connected stdio MCP client. Start in a clean
throwaway repository if possible.

**Steps:**

1. Call `session_info` with the UI's session ID. Check `cwd` and
   `permitted_directories`.
2. Call `list_directory` on `.` and `read_file` on `Cargo.toml` (and
   `README.md` if present). These are direct, read-only inspection tools.
3. Create a Goal with `goal_start`, e.g. title `Inspect Rust project`, and
   objective: `Read-only inspection only. Identify the project structure, Rust
   package name, and documented test command. Do not edit files or run
   commands. Cite only facts visible in the files you read.`
4. Record the returned Goal ID. Call `goal_status`, then call `goal_run` with
   that ID and a small `max_steps` such as `4`.
5. Inspect `goal_status` and `goal_result`; verify any claims yourself by
   reading the named files. Do not treat a nonterminal status as success.

**Expected result:** A durable Goal record and a bounded attempt to produce a
summary. The terminal state and content are not predetermined; the model may
need more steps or may report a blocker.

**How to verify:** Confirm that no file changed (`git status --short` in the
test project), and compare the summary with `Cargo.toml`/README. Inspect the
Goal status/result rather than assuming `goal_run` means completion.

**Caution:** `goal_start` alone does not execute. A model claim is not an
independent verification. Keep the objective read-only and avoid enabling
unsandboxed commands.

**If it fails:** Check the session ID, allowed roots, `goal_status`, and any
reported platform/sandbox refusal. Do not retry an ambiguous operation before
checking the repository state.

## Example B: Verified change in a disposable project

**Purpose:** Exercise the supported writer/verifier lifecycle on a disposable
fixture, while checking the mutation and verification evidence yourself.

**Prerequisites:** GoalLatch installed and connected, an isolated scratch
project inside the session's allowed directory, and the normal approval UI.
Use an empty git repository or a newly created temporary directory; never point
this exercise at production data. On Linux, meet the Bubblewrap 0.12.0+
host requirements. Windows is Experimental and has no process sandbox.

**Steps:**

1. Create a scratch directory and a tiny project with a single text file,
   e.g. `README.md` containing `# Sample\n` (the exact setup command is
   platform-specific; create it manually if unsure).
2. Start `local-mcp start` from that directory, then verify the session with
   `session_info`.
3. Start a Goal with explicit, narrow criteria: add one sentence to the
   fixture README; do not touch any other path; verify the exact new sentence
   with a read-only check. Use `workspace_mode: "PRIMARY"` unless you have
   deliberately configured and authorized a managed worktree root. Managed
   worktree setup has its own permission/approval prerequisites and is not
   needed for this basic example.
4. Review each approval request, including the exact path and proposed diff.
   Call `goal_run` with a small step budget and inspect `goal_status` and
   `goal_result`.
5. Independently inspect the file and run the project's relevant verifier or
   test command yourself. Compare the result to the requested sentence and
   inspect `git diff -- README.md`.

**Expected result:** If the installed platform support, session authority,
planner/writer path and verifier all permit the work, the requested change may
be applied and checked. This guide does not assert that any particular run
will succeed or that a write is safe merely because a Goal was started.

**How to verify:** Confirm only the intended file changed, compare its diff to
the requested content, and run the exact verifier/test command. Check Goal
status/result for a terminal verified outcome; do not infer success from model
text alone.

**Caution:** Do not use YOLO for this example. `without_sandbox` has the
service user's full permissions and network capability. Windows mutation is
host-native; Windows is Experimental. Activity/approval UI can show paths and
diffs, so do not use secrets.

**If it fails:** Stop and inspect the actual file and Git status before
re-running. Treat safety/platform refusals as terminal. If an operation's side
effect is ambiguous, reconcile the filesystem and Goal evidence first; never
blindly retry.

## Example C: Durable Goal recovery

**Purpose:** Observe a Goal across an interrupted client call and use the
explicit status, resume, run, and result tools without inventing success.

**Prerequisites:** A test session in a disposable project, a connected
stdio MCP client, and a non-sensitive bounded objective. Do not terminate the
host process while it is performing an operation whose side effects you have
not inspected.

**Steps:**

1. Call `goal_start` with a small objective and save the returned Goal ID.
2. Call `goal_run` with a small budget (for example `1`) and capture the actual
   response. If the Goal is still nonterminal, call `goal_status`.
3. Simulate a client-side disconnect only after the tool call has returned or
   after you have established that no effectful operation is in flight. Leave
   the GoalLatch host/session state intact.
4. Reconnect to the same server/session and call `goal_status` with the saved
   Goal ID. Call `goal_resume` for that Goal ID to reconcile/recover durable
   task state; this tool does not itself claim task completion.
5. If status says the Goal is resumable, call `goal_run` again with a bounded
   step count. Inspect `goal_status` and `goal_result` for the actual terminal
   result.

**Expected result:** The durable record can be queried by the same session and
Goal ID; depending on the interrupted step, it may be resumable, terminal, or
blocked pending explicit recovery. No specific terminal state is guaranteed.

**How to verify:** Confirm the same Goal ID and session are used, compare
status before and after `goal_resume`, inspect the result, and independently
check any claimed filesystem effects. Only a terminal result with matching
verification evidence counts as completion.

**Caution:** Goal persistence does not grant permission or prove an operation
completed. Do not change session ID, delete state, or retry a mutation to force
progress. On restart, keep the same configured state and session context. The
exact recovery behavior depends on the operation and persisted evidence.

**If it fails:** Preserve returned errors and state; redact session IDs, local
paths, and private data before sharing. If state or side effects are ambiguous,
stop and ask a maintainer rather than repeating an effectful call.
