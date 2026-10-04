# Resource Bounds V1 — Design

Status: frozen design, implemented on `hardening/resource-bounds-v1`.
Baseline: `eb4277173bc3235c4ba5ed3bc49330d1c409ab3e` (main, 11/11 green).

## Purpose

GoalLatch accepts untrusted input: MCP callers, model-proposed commands, command
output, and background jobs. Every one of those currently has an unbounded growth
path. This design bounds them, and freezes where each bound is enforced and what a
rejection means for authority.

## Non-goals (explicitly out of scope)

- Linux `PR_SET_PDEATHSIG`, watchdog processes, a persistent PID registry, or a
  post-crash orphan sweeper. Normal explicit shutdown cleanup **is** in scope;
  abnormal parent death is not.
- Managed Worktrees Phase 5, worktree cleanup, or parallel writers.
- `Goal::validate` optimization, Replanner request-window redesign, `goal_status`
  pagination, Writer ACL/xattr work, full existing-file CAS redesign.
- Widening Session, Task, or Writer authority. This design only narrows.

## Authority invariants preserved

Resource bounds are a *resource* policy, not an authority policy. Nothing here may:

- grant retry, fallback, or approval authority;
- convert a resource failure into a permission, safety, or tool-missing verdict;
- treat a resource rejection as proof that a command did not run.

A resource limit that has been reached is terminal for that attempt.

## 1. Central resource policy surface

All constants live in `src/resource_limits.rs`. They are the single source of
truth; production code never restates a bound as a literal. Unit is **bytes**
unless stated. Relationships between limits are asserted at compile time
(`const _: () = assert!(...)`), so an inconsistent set fails the build rather
than a test.

### 1.1 MCP transport

| Constant | Value | Derivation |
| --- | --- | --- |
| `MAX_MCP_REQUEST_FRAME_BYTES` | 8 MiB | Largest realistic request is a `write_file` (≤256 KiB) plus argv (≤512 KiB) plus a plan proposal (≤256 KiB) plus evidence (≤64 KiB) plus JSON overhead ≈1.1 MiB. 8 MiB is ≈7× margin and still cannot admit an accidental multi-megabyte blob. |
| `MAX_MCP_RESPONSE_FRAME_BYTES` | 16 MiB | Must exceed the largest method result **after JSON escaping**. See §1.7: a command result is escaped twice, so its frame is up to 7× its raw size. |

The request frame is the outer defense for every input-side bound below. Method
level bounds are the inner, early rejections that avoid expensive setup.

### 1.2 Command output capture

| Constant | Value | Derivation |
| --- | --- | --- |
| `MAX_COMMAND_STDOUT_BYTES` | 1536 KiB | Comfortably above real build/test diagnostics (the full `cargo test` suite for this repository is a few hundred KiB) while remaining bounded. |
| `MAX_COMMAND_STDERR_BYTES` | 256 KiB | Diagnostics. Trusted-Git and model/agent paths keep their own, stricter caps and are unchanged. |

These bound the **generic** path only. Trusted-Git and model/agent paths keep
their own, stricter, already-enforced caps and are **not** changed.

### 1.3 File and directory tools

| Constant | Value | Derivation |
| --- | --- | --- |
| `MAX_READ_FILE_BYTES` | 2 MiB | Larger than any ordinary source file; must fit its frame after escaping (§1.7). |
| `MAX_IMAGE_RAW_BYTES` | 8 MiB | Bounds the **raw** file before base64. base64 expands by 4/3 to 10.67 MiB, and the base64 alphabet needs no JSON escaping, so it stays inside the frame. |
| `MAX_WRITE_FILE_CONTENT_BYTES` | 256 KiB | Matches the Writer's existing `MAX_WRITE_CONTENT_BYTES`. |
| `MAX_WRITE_PREIMAGE_BYTES` | 2 MiB | Bounds the existing file read to build a preimage/diff. |
| `MAX_DIRECTORY_ENTRIES` | 20 000 | Entry-count bound. |
| `MAX_DIRECTORY_OUTPUT_BYTES` | 2 MiB | Aggregate rendered-listing byte bound. |

`MAX_DIRECTORY_ENTRIES` and `MAX_DIRECTORY_OUTPUT_BYTES` are deliberately
*independent*: 20 000 short names stay under the byte cap, while a smaller number
of very long names can breach it. Neither is derived from the other.

### 1.7 JSON escape expansion

`serde_json` renders a control byte as ` ```0 ` (6 bytes). A command result is
serialized **twice** — once by `render_output`, then again when `text_result`
embeds it as a string value — so the second pass re-escapes each backslash and a
raw byte costs up to **7**.

This factor is named (`JSON_ESCAPE_WORST_CASE`) and every text-returning bound is
asserted against it at compile time. A bound derived from raw byte counts alone
is wrong in a way that matters: `head -c 4194304 /dev/zero` is an ordinary
command that exits successfully, and its result serializes to a frame several
times larger than its own byte count. A test measures the real expansion so the
declared factor cannot drift into optimism.

### 1.4 Command arguments

| Constant | Value | Derivation |
| --- | --- | --- |
| `MAX_EXECUTE_ARGV_ITEMS` | 256 | Argument-count bound. |
| `MAX_EXECUTE_ARG_BYTES` | 64 KiB | Per-argument bound. |
| `MAX_EXECUTE_ARGV_TOTAL_BYTES` | 512 KiB | Aggregate argv bound. |
| `MAX_EXECUTE_PATH_BYTES` | 4 KiB | `cwd` field; matches `MAX_PATH_BYTES`/`MAX_MUTATION_PATH_BYTES` precedent. |

### 1.5 Background job registry

| Constant | Value | Derivation |
| --- | --- | --- |
| `MAX_BACKGROUND_JOBS_PER_SESSION` | 8 | One Session may not consume the whole registry. |
| `MAX_BACKGROUND_JOBS_GLOBAL` | 32 | Aggregate across all Sessions. |
| `FINISHED_JOB_TTL` | 15 min | Retention for a finished, unpolled result. Monotonic. |

Worst-case aggregate retained output is bounded by
`MAX_BACKGROUND_JOBS_GLOBAL × (MAX_COMMAND_STDOUT_BYTES + MAX_COMMAND_STDERR_BYTES)`
≈ 160 MiB, and in practice far less, because a job only retains what it captured.

### 1.6 Control-plane admission

| Constant | Value |
| --- | --- |
| `CONTROL_PLANE_PERMITS` | 16 |
| `EXECUTION_PERMITS` | 16 |

`CONTROL_PLANE_PERMITS + EXECUTION_PERMITS == MAX_CONCURRENT_REQUESTS` is asserted
at compile time, so total transport concurrency is **unchanged** at 32. Nothing is
raised.

## 2. Bounded process capture

`ProcessGroup::terminate_and_capture` is the single capture point for both generic
paths (`sandbox::run_tracked_with_path` and `sandbox::run_unrestricted_inner`).
Bounding it there fixes sandboxed and host-native execution together.

The reader is a streaming loop that checks the limit *before* extending the buffer,
so a full limit-sized result is accepted and limit+1 is rejected without ever
residing in memory. stdout and stderr are bounded **independently** and drained
concurrently, so two full pipes cannot deadlock each other.

This reuses the already-proven `capture_bounded` pattern from
`sandbox::run_bounded_git`: an 8 KiB scratch buffer, a `CaptureState` shared with
the waiter (too_large / failed / notify), and abort-on-drop capture tasks.

A deterministic overflow signal (`too_large`) is published by the reader, so
limit+1 is detected on read rather than by post-hoc length comparison.

**Capture is not shell-specific.** No production branch mentions `yes`, `cargo`,
`git`, or any other program. Flood tests use an injected reader and a test-owned
emitter.

## 3. Output overflow termination

On overflow the sequence is fixed:

1. reader observes `limit + 1`, sets `too_large`, stops reading;
2. host requests process-group termination via the existing `ProcessGroup`
   discipline (Unix: `kill(-pgid, SIGKILL)`; the leader is always signalled);
3. bounded cleanup waits for leader termination within a cleanup deadline and
   aborts both capture tasks so no reader task leaks;
4. lifecycle evidence is recorded (`command_started = true`);
5. a typed `ResourceLimitError` failure is returned.

Stopping capture alone would leave descendants running and the group alive, so
termination is not optional.

**stdin is written only after both readers exist**, and under a deadline. A child
that writes more than the pipe buffer before draining stdin would otherwise block
on its own write while the host blocks on `write_all`: a deadlock with no deadline
to break it, since the generic path deliberately has no command timeout.

The overflow failure carries the typed `ResourceLimitError` rather than wrapping
it, because `anyhow::Error::downcast_ref` searches its own context chain and not
an inner error's `source()` chain. Wrapping it would hide the marker from both
the fallback classifier and the transport's resource-limit code.

## 4. Side-effect semantics on overflow

**An output-limit failure is never proof that a command did not run.**

On overflow the host returns `RunError { command_started: true, .. }`. In
`execution::process_sandboxed_attempt_with_codex_override` that maps to
`CommandStart::Unproven` (the process that started was the sandbox wrapper, not
provably the requested command). Consequently `fallback::infer_side_effect_state`
cannot reach `ConfirmedNotPerformed` except through the existing, unchanged local
postcondition proof that applies only to exact `git_stage_paths`. For every other
command the state is `Unknown`, and `Budget::after_state` sets `locked = true` for
`Unknown`.

Net effect: **an output overflow after a possibly mutating command can never
restore retry budget.**

Independently of that, `FailureClass::ResourceLimit` is classified as terminal
before any text is inspected, and `decide` returns `FallbackAction::Block` with
`ReasonCode::NoFallbackResourceLimit` for it. An overflow is never re-derived
from caller-controlled output text, so a command cannot forge a permission
verdict by flooding bytes that contain "permission denied".

## 5. Background job lifecycle

Three states are distinguished explicitly:

- **Running** — a live `JoinHandle`; never expired by TTL and never evicted to
  admit another job.
- **Finished / Retained** — completed, result retained, pollable until TTL.
- **Expired / Evicted** — removed; a later poll returns a clear expired result.

Entry points: `store_job` (insert), `poll_job` (read + finished removal),
`stop_job` (remove + abort), and Session shutdown.

Admission is decided **under one lock** as `can_admit` followed by `insert`. The
decision is made by `store_job`, which receives an already-spawned handle: a
refused job is explicitly terminated before the error is returned, because
dropping a `JoinHandle` detaches the task rather than stopping the child, which
would leave a live process with no registry entry and no way to stop it.

`execute` deliberately performs **no** capacity pre-check: a command that finishes
inside the foreground window never creates a job, and refusing it because the
registry is full would couple the most-used tool to background-job occupancy.

If either the per-Session or the global ceiling is reached, creating a *new* job
fails with a deterministic resource-limit error. A running job is never evicted
to make room, and one Session can never evict another Session's job.

Output bounds apply **at capture time**, so a retained background result already
obeys `MAX_COMMAND_STDOUT_BYTES` / `MAX_COMMAND_STDERR_BYTES`. Becoming a
background job grants no additional storage. This holds for the foreground→background
transition too: the same capture path bounds output before and after backgrounding.

Every completion path — success, non-zero exit, timeout/background, overflow,
spawn failure, sandbox setup rejection, explicit stop, and join failure — releases
its registry entry per the lifecycle above.

## 6. GC policy

No maintenance thread. GC runs opportunistically on natural control-plane calls:
job insertion, job poll, job stop, and Session shutdown. Because the registry is
itself bounded by `MAX_BACKGROUND_JOBS_GLOBAL`, a GC scan is O(bounded) and cannot
grow with history.

GC removes only **finished** results older than `FINISHED_JOB_TTL`. It never
terminates a running process, and it never touches another Session's running job.
Elapsed time uses a monotonic clock (`tokio::time::Instant` in production, an
injectable source in tests) so wall-clock changes cannot corrupt expiry.

## 7. Job shutdown

Audit finding, and the reason this section differs from the obvious design:
**Resource Bounds V1 has no Session-teardown signal.** `local-mcp start` and
`local-mcp mcp` are separate processes; exiting the start UI does not notify the
MCP server, and the durable session record is not removed on exit. There is no
`local-mcp stop` subcommand. Inventing an IPC notification would be a protocol
change outside this scope.

What exists instead:

* `JobRegistry::release_session` is the scoped teardown operation: it terminates
  still-running jobs **for that Session only**, removes its retained finished
  results, and never touches another Session's jobs. It is exercised directly by
  tests.
* Explicit **server** shutdown (stdin EOF) terminates and drops every job the
  server holds. That is the shutdown the process actually has.

Retention in the absence of a Session signal is therefore bounded by the
per-Session ceiling, the global ceiling and the TTL, rather than by teardown.
Abnormal parent death is explicitly out of scope (§16 of the task, and below).

## 8. MCP request framing

The request loop reads with a **bounded incremental reader**, not
`BufReader::lines()`. `lines()` must accumulate the whole line before it can be
inspected, so an arbitrarily large line without a newline would allocate without
limit. The bounded reader:

- reads into a fixed scratch buffer and appends to a `Vec` capped at the limit;
- at `limit + 1` bytes it stops appending, discards the remainder of the frame,
  and reports a resource-limit parse error **without parsing JSON at all**;
- therefore an oversized *invalid* JSON frame costs no parse and no retention;
- protocol state stays deterministic: the oversized frame is consumed through its
  terminating newline, and the connection continues, so no partial request is ever
  dispatched.

A frame of exactly the limit is accepted. A frame of limit+1 is rejected. The
exact-limit frame must still be valid JSON to be dispatched.

This validation happens **before** the dispatch semaphore is acquired, so a huge
request cannot consume a worker permit merely to be discovered as too large. It
adds no new read queue: the read loop remains the single reader.

## 9. MCP response framing

Method-level bounds (§1.2, §1.3, §1.4) are enforced early enough that a global
response size is mechanically guaranteed, with JSON escape expansion accounted
for (§1.7). As defence in depth, `write_message` asserts the serialized frame is
within `MAX_MCP_RESPONSE_FRAME_BYTES` **before** any byte is written.

When a frame would exceed the cap it is **replaced**, not truncated and not
propagated: the writer substitutes a bounded resource-limit error frame and
continues. Failing the writer task instead would drop the response receiver, after
which every later response is discarded silently while the server keeps running
the client's commands — one oversized result would wedge the whole session. The
writer is therefore never allowed to end on a per-frame failure.

## 10. Error responses

Resource-limit errors are small and bounded. They state the resource, the limit,
and nothing else. They do **not** echo the offending request, the full path list,
or any captured stdout/stderr. A resource-limit error can never itself exceed the
response cap.

## 11. File semantics

`read_file` reads at most `MAX_READ_FILE_BYTES + 1` bytes and rejects when that
extra byte is present. It does not rely on metadata alone: a file that grows, or a
special file that misreports, is still caught. There is **no silent truncation**;
V1 is full-content-or-bounded-error.

`read_file` requires a **regular file**. Directories, FIFOs, devices, and sockets
are refused, which also removes the unbounded-stream/hang risk on a FIFO. Path
authority policy is unchanged — no root is widened.

`write_file` validates `content` length **before** any filesystem mutation, so an
oversized write performs zero mutation. The global request-frame cap remains the
outer defense.

`get_image` bounds raw bytes before base64 and before JSON serialization.

## 12. Command request semantics

argv item count, per-argument bytes, total argv bytes, and the `cwd` path field are
all bounded in host code. The declared JSON Schema is **not** enforced anywhere in
this codebase, so every bound is implemented as an explicit host check; schema
annotations are documentation only.

Command authority classification is unchanged. Bounds are additional to, never
substitutes for, authority and approval checks: a small request still requires the
same approvals, and Windows host-native approval behavior is untouched.

## 13. Control-plane saturation

Previously one 32-permit semaphore gated every request, and the **read loop itself**
awaited a permit. 32 concurrent long-running `execute` calls therefore blocked the
transport, including `poll_job` and `stop_job`.

Admission is now class-based over two bounded pools that sum to the same 32:

- **control plane** (reserved): `poll_job`, `stop_job`, `session_info`,
  `goal_status`, `goal_pause`, `goal_resume`, `goal_cancel`, `goal_result`,
  `codex_fallback`, plus protocol methods that do no work (`initialize`, `ping`,
  `tools/list`, notifications).
- **execution**: everything else, including `execute`, `start_command`,
  `goal_run`, `read_file`, `list_directory`, `get_image`, `write_file`.

Anything unrecognized defaults to the execution pool, so unknown or malformed
traffic cannot occupy reserved control capacity. Control-plane concurrency is
itself bounded; it is not made unlimited.

## 14. Error taxonomy

`FailureClass::ResourceLimit` is a distinct, terminal class. Output-too-large,
request-frame-too-large, file-too-large, directory-too-large, and job-capacity
failures are all resource failures and are **not** reported as sandbox permission
denied, tool missing, or an authority violation.

## 15. Compatibility

No protocol change: the transport remains newline-delimited JSON-RPC. No schema
migration. Behavior changes are intentional and user-visible:

- oversized resources now reject deterministically instead of growing without limit;
- finished, unpolled background jobs expire after the TTL;
- job creation can be rejected at configured capacity.

Unbounded behavior is not restored for compatibility. Existing specialized caps
(trusted Git, model/agent, Writer, Verifier, Replanner) are untouched.