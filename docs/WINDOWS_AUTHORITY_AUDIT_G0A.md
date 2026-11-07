# G0-A: Windows Authority Audit (External Agent Backend V1 — Local Campaign)

Status: **Draft — for Owner review**
Baseline: `main` @ `e6950fdc3bab72982285888a81cce822743fbba5`
Design reference: `docs/EXTERNAL_AGENT_BACKEND_V1_ARCHITECTURE.md` (PR #30, HEAD `ee231d10e5ed0483751e6fcbda2a09a3d78f97d0`, design doc read directly; PR #30 untouched)
Audit target: Dogfooding V1 observation — `execute`, `write_file`, and worktree
creation completed **without an interactive approval prompt**.

## 1. Executive summary

**No privilege bypass was found.** Every host-native mutation path in this
codebase requests approval before execution and fails closed on a missing
approval channel, a denied request, a malformed reply, or a transport
disconnect. The absence of an interactive prompt during Dogfooding V1 is
explained by the session approval daemon's **yolo** mode (an operator-owned,
stdin-local state that auto-allows each approval request), not by a server-side
skip. That classification is *spec-compliant*, with one visibility gap (F-01).

Findings: **HIGH 0**, **MEDIUM 1** (F-02), **LOW/INFO 4**.

Because no actual authority bypass is present, the campaign continued into
G0-B / E1-A / E2 per the stop-condition rules.

## 2. Approval architecture (two layers)

| Layer | Where | Who decides | Scope |
|---|---|---|---|
| L1 harness | MCP client (DeepAI Code / pi harness) | harness approval policy (ask / auto-accept / yolo) | whether the `execute`/`write_file` tool call reaches the GoalLatch server at all |
| L2 session | `local-mcp start <id>` permission UI over the per-session socket | operator (`allow`/`deny`, or session `yolo`) | whether GoalLatch runs the host-native operation |

The Dogfooding report ("no interactive approval appeared") cannot be attributed
to a single layer from inside the server: the server only observes L2. What the
server *does* guarantee is that **L2 must return `allow`** before any host-native
work starts. If neither layer surfaced a prompt to the operator, then both the
harness policy and the session UI auto-approved this session — the session UI
can only do that through `yolo`, which is exactly the state an operator sets
with `/permission yolo` and which "MCP callers cannot enable".

### 2.1 Approval call-site inventory (complete)

| Call site | Operation label | Gated action |
|---|---|---|
| `execution.rs` `spawn_sandboxed_command_inner` | `execute` / `start_command` (argv detail) | host-native primary command spawn |
| `execution.rs` fallback stage | `codex_fallback_v2_execute` | executable Codex fallback |
| `execution.rs` `codex_fallback` diagnose | `codex_fallback_v2_diagnose` | read-only diagnostic Codex spawn |
| `execution.rs` `without_sandbox` | `without_sandbox` | unrestricted host command |
| `mcp.rs` `write_file` (`#[cfg(windows)]`) | `write_file_host_native` | host file mutation |
| `managed_worktree_prepare.rs` `SessionManagedCreationApprover` | managed creation approval | `git worktree add` for MANAGED_WORKTREE goals |
| `agent.rs` | (Codex spawn gate) | Goal worker model spawn |
| `verifier_git_observation.rs` | verifier observation | verifier Git observation |

There is **no** host-native execution path that bypasses `approvals::request`
on Windows: `primary_execution_mode()` is `HostNative` on Windows and
`primary_execution_requires_approval(HostNative) == true`, so the request
happens before `tokio::spawn` of the process. On non-Windows the primary mode
is `Sandboxed` and the sandbox boundary substitutes for the prompt.

## 3. Fail-closed matrix (audited + pinned by tests)

| Scenario | Behavior | Pinned by |
|---|---|---|
| Reply `allow` | proceeds | `request_fails_closed_on_invalid_and_deny_responses`, `approval_ipc_allow_deny_invalid_and_missing_are_distinct` |
| Reply `deny` | `Ok(false)` → `user denied {operation}` | same as above, `approval_denial_stops_host_native_primary_execution` |
| Reply invalid (`maybe`, `ALLOW`, `allow me`, `yes`, empty line) | `Err` → operation refused | `request_fails_closed_on_invalid_and_deny_responses`, **new** `request_allow_is_exact_after_trim` |
| Approval channel missing | `Err` ("session … is not running") → refused before spawn | `request_fails_closed_on_invalid_and_deny_responses`, **new** `execute_without_approval_channel_fails_closed` |
| Session disconnects mid-request (EOF, no reply) | `Err` → refused | **new** `request_fails_closed_when_session_disconnects_without_responding` |
| `yolo` smuggled through the pipe payload | ignored (stdin-local state only) | `yolo_cannot_be_enabled_by_pipe_messages`, `approval_pipe_payload_cannot_set_session_yolo_state` |
| cwd / write path outside permitted directories | refused by path authority before approval | **new** `execute_outside_permitted_directories_is_refused_before_approval`, **new** `write_file_path_authority_refuses_creation_outside_permitted_roots` |
| `write_file` without live approval (Windows) | fail closed, zero mutation | `write_file_still_requires_live_approval_on_windows` |

## 4. The two payload fields that look alarming

### 4.1 `side_effect_state: UNKNOWN` — a deliberate fail-safe, not a bug

`fallback::infer_side_effect_state` returns `Unknown` when the host cannot
**prove** the command's effect class. Consequences are conservative: the
fallback budget stays **locked** (`Budget::after_state`), so an UNKNOWN verdict
grants neither another attempt nor another side effect. A false
`ConfirmedPerformed` or `ConfirmedNotPerformed` would let an unreconciled effect
through; UNKNOWN cannot.

Observed during Dogfooding and re-verified against the classifier in this
audit: `git status --porcelain` reports `side_effect_class: UNKNOWN` because
plain `git status` may refresh the index stat cache; only
`git --no-optional-locks status …` is a provable observation
(`git_command_class::status`). Non-`git`/`gh` executables (`rustc --version`,
`cmd /c dir …`) are unclassified by design and therefore UNKNOWN. Pinned by
**new** tests `unclassified_host_commands_map_to_unknown_and_lock_the_budget`,
`git_status_is_an_observation_only_with_no_optional_locks`,
`classified_read_only_commands_are_confirmed_not_performed`.

### 4.2 `fallback_decision.operation_validated: false` — expected, misnamed

The flag is `policy.validated_git_stage_paths.is_some()`. It reports whether the
**structured `git_stage_paths` operation** was validated against the execution
root — a single-operation-type feature. For `read_only_command` and
`unstructured` operations it is `false` **by construction**, and the general
path-authority checks (`cwd(args, session)`, `validate_path_authority`) have
already run regardless. Reading `false` as "the operation was not validated" is
wrong. See F-03.

## 5. Findings

### F-01 (LOW) — Approval provenance is invisible to callers
Neither the tool result nor any log states whether `allow` came from the
session `yolo` auto-allow or an interactive decision. An operator auditing a
run cannot distinguish "a human approved each call" from "yolo allowed
everything". This is the reason Dogfooding V1's observation is ambiguous from
inside.
*Fix proposal (additive, non-breaking):* include an `approval` provenance object
(`{"mode":"yolo"|"interactive","request_id":…}`) in the execution payload and
surface the session mode in `session_info`. Owner decision required: this
changes the response schema additively.

### F-02 (MEDIUM) — Approval wait has no timeout
`approvals::request` writes the request and awaits `read_line` with no deadline.
A session UI that accepts the connection but never answers hangs the MCP call
indefinitely (availability only — nothing executes, so the security posture is
fail-closed). `connect()` bounds only the *busy-pipe* retry (5 s).
*Fix proposal:* wrap the response wait in a bounded timeout (e.g. 120 s,
configurable) that fails closed (`deny` semantics) on expiry, and record the
timeout in the activity log.

### F-03 (LOW) — `operation_validated` naming
Rename to `git_stage_paths_validated` (or document inline) so operators do not
read `false` as an integrity warning. Purely additive docs/rename; payload key
change would need Owner approval.

### F-04 (LOW) — Reply matching is exact after trim
`"ALLOW"` / `"allow me"` fail closed while `" allow "` allows. Strictness is the
safe direction; pinned by `request_allow_is_exact_after_trim` so a future
"helpful" case-insensitive change is caught in review.

### F-05 (INFO) — YOLO resets only on daemon restart
Session yolo state is stdin-local to `local-mcp start` and resets on restart;
MCP callers cannot set or clear it (tested). No action needed; documented here
because External Backend V1 must not assume it can read or change this state.

### F-06 (LOW) — `git_stage_paths` structured validation is unusable from nested worktree cwds
Observed live in this campaign: staging files whose argv paths are relative to a
nested worktree cwd while `operation.paths` are relative to the session root
reproduces `Git staging scope is not exact` (and the absolute-path spelling is
rejected as "not an exact safe relative path"). Callers therefore fall back to
`unstructured` operations, which are still approval-gated but skip the
structured stage-path validation entirely — the hardening feature silently
loses adoption.
*Fix proposal:* resolve `operation.paths` and the command's path arguments
against the same base (the command `cwd`) before comparison, or document the
exact required spelling in the tool schema.

## 6. Live evidence collected in this campaign (session `localmcp`)

* `git worktree add` (unstructured, host-native) completed without an
  interactive prompt and returned `side_effect_state: UNKNOWN`,
  `operation_validated: false` — consistent with sections 4.1/4.2 and with L2
  auto-approval.
* `git status --porcelain` (read-only intent, unproven shape) → `UNKNOWN` +
  `budget_locked: true`. `git rev-parse HEAD`, `git worktree list`,
  `git ls-remote`, `gh pr view/list` → `CONFIRMED_NOT_PERFORMED`.
* All writes and command executions in this campaign went through the
  GoalLatch MCP session; every one of them required and obtained an L2 `allow`
  response (otherwise the fail-closed paths above would have errored).
* The harness layer (L1) approval behavior is outside this codebase's
  visibility; Owner should confirm which policy ran there during Dogfooding.

## 7. Required test matrix (campaign requirement → evidence)

| Required case | Status | Test |
|---|---|---|
| 承認あり (approval granted) | covered | `request_fails_closed_on_invalid_and_deny_responses` (allow arm) |
| 承認拒否 (denied) | covered | `approval_denial_stops_host_native_primary_execution`, `request_fails_closed_on_invalid_and_deny_responses` (deny arm) |
| 承認チャネル不在 (channel absent) | covered | **new** `execute_without_approval_channel_fails_closed` |
| 不正な承認応答 (invalid reply) | covered | **new** `request_allow_is_exact_after_trim` |
| セッション切断 (disconnect) | **new** | `request_fails_closed_when_session_disconnects_without_responding` |
| YOLO整合 (yolo consistency) | covered | `yolo_cannot_be_enabled_by_pipe_messages`, `approval_pipe_payload_cannot_set_session_yolo_state` |
| 許可ディレクトリ外 (outside permitted roots) | **new** | `execute_outside_permitted_directories_is_refused_before_approval`, `write_file_path_authority_refuses_creation_outside_permitted_roots` |

No security control was disabled to obtain any of these results; all tests use
mock approval sockets and isolated temp fixtures.

## 8. Residual risks & Owner decisions

1. **Owner decision (F-01):** approve the additive approval-provenance field, or
   accept the current ambiguity.
2. **Owner decision (F-02):** approve the bounded approval wait (timeout ⇒
   fail-closed deny).
3. **Operational:** confirm whether the Dogfooding session ran `/permission
   yolo`; if it did not, L1 (harness) auto-accept is the remaining explanation
   and the harness policy should be reviewed there.
4. **Version drift:** the live "GoalLatch Windows v2" server matches this
   baseline's tool schema and classifier behavior (the UNKNOWN classifications
   reproduce main's `git_command_class` rules exactly). Any future audit should
   re-pin the running binary's commit.

## 9. Changes in this PR

* `docs/WINDOWS_AUTHORITY_AUDIT_G0A.md` (this file)
* `src/windows_authority_audit_tests.rs` (new regression module)
* `src/approvals.rs` (two new fail-closed tests in the existing test module)
* `src/main.rs` (register the new test module)
