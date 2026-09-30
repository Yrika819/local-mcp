# User-Visible Regression Audit

## Scope

- Review date: `2026-09-30`
- Requested outcome: `review only`
- Continuation: `report only` (no implementation authority granted by this report)
- Scope reviewed: commit range `25948dee574bee79c0fe53fc3eb522db9ad9e54b..e1d665c01c861eaa9bcc04c2a4e1653246c8df72` on branch `managed-worktrees/v1-phase3-creation-authority`
- Baseline: `25948de` ("Managed Worktrees V1 Phase 3 - Creation authority", reported green, 11/11 CI)
- Completion: `Complete within reviewed scope`
- Assumptions:
  - The review was strictly read-only apart from this report. No `cargo build`/`cargo test`/`cargo clippy` was run, so **every conclusion below is static code-path evidence**, not runtime verification. "Baseline CI green" is a premise supplied in the request, not something this review observed.
  - The dev host is macOS. `managed_creation_requires_approval()` is a compile-time `cfg!(windows)` constant, so the Windows branch is analyzed by reading, never executed here.
  - No operator's real schema-4 Goal store was available; the migration was evaluated against synthetic documents constructed by the diff's own tests.
  - Working tree was clean at review start (`git status --short` empty); nothing was staged, so the range above is the entire change set.

### Diff inventory summary

| File | Change | Surface |
| --- | --- | --- |
| `src/managed_worktree_prepare.rs` | +330/-? - durable lifetime budget loop, Windows approval gate, zero-attempt adoption refusal | managed `goal_run` path |
| `src/managed_worktree.rs` | +102 - `MAX_LIFETIME_CREATION_ATTEMPTS`, `creation_attempts_consumed`, validation | durable model |
| `src/task_store.rs` | +96 - schema-5 constant rename, `decode_pre_durable_attempt_budget` migration | persisted data |
| `src/goal.rs` | +24 - `consume_managed_creation_attempt`, schema version bump | durable model |
| `src/goal_runner.rs` | +18 - `prepare_managed_workspace_before_run` is now `async`, wires the session approver | `goal_run` output |
| `src/mcp.rs` | +1/-1 - `goal_run` tool description | `tools/list` output |
| `SECURITY.md` | +7/-1 - rewritten as four numbered requirements | docs-only |
| `src/managed_worktree_creation_tests.rs` | +1102 - test-only | test-only |

## Gate Snapshot

- Recommendation: `Discuss`
- Completion: `Complete within reviewed scope`
- Why now: PRIMARY Goals are byte-compatible and the two new Windows stop reasons are deliberate, but three user-visible message/ordering defects in the managed recovery path should be resolved before the release that carries schema 5.
- Must-review now:
  1. `F1` `MANAGED_RETRY_EXHAUSTED` no longer carries the Git diagnostic that explains the failure.
  2. `F2` On Windows the operator is prompted to approve an invocation the host has already decided not to make, and a denial is reported instead of the real terminal cause.
  3. `F3` Upgrading a stored schema-4 `PREPARED` managed Goal can strand it, and the reported budget consumption is fabricated.
  - Full list is in `Complete Findings Index`.
- Findings count: `Block 0 | Discuss 3 | Watch 2 | Intentional 5`
- Coverage confidence: `high` for the managed creation/recovery path and PRIMARY parity (both read end to end); `medium` for real Windows operator experience (compile-time-gated branch, never executed here); `low` for the upgrade experience of any real operator store.
- Behavior graph coverage: `built for 6 surfaces` (PRIMARY `goal_run`, managed `goal_run` happy path, managed creation failure path, Windows approval gate, schema-5 migration, `tools/list` surface).
- Biggest blind spot: the Windows approval branch cannot be exercised on this host and is not exercised by the diff's own tests on a Unix CI leg, so the approval/budget ordering in `F2` is proven by reading only.

### PRIMARY path verdict (the highest-value question in this review)

**No observable `goal_run` output changed for a PRIMARY (non-managed) Goal.** Evidence, in order of strength:

- `src/goal_runner.rs:315` `prepare_managed_workspace_before_run` is now `async` only because of the approval `.await`. Its body is otherwise byte-identical to the baseline: same `store.load_goal`, same `stop_for_state` control-state gate, same `build_result(...)` with `steps_attempted = 0`.
- For a PRIMARY Goal the async chain `prepare_managed_workspace` -> `prepare_managed_workspace_with_policy` returns at the first statement (`workspace_mode().is_primary()` -> `NotManaged`). The only `.await` inside is `approver.approve(...)`, which sits behind `if requires_approval` and is therefore unreachable for PRIMARY. The future completes on its first poll without yielding, so `run_goal_with_authorities` receives identical inputs and emits identical output.
- `SessionManagedCreationApprover::new(session.id.clone())` is constructed unconditionally but is a `String` clone: no store read, no filesystem access, no IPC.
- `stop_reason_name` / `stop_detail` (`src/mcp.rs:926`) are untouched by this diff. For every non-managed stop reason `stop_detail` is still `Value::Null`, so no PRIMARY response gained, lost, or reshaped a field.
- No new `stop_reason` is reachable for PRIMARY: `MANAGED_CREATION_APPROVAL_DENIED` and `MANAGED_CREATION_APPROVAL_UNAVAILABLE` sit behind `if requires_approval`, which is false on macOS/Linux and unreachable for PRIMARY on any platform.
- `GOAL_SCHEMA_VERSION` 4 -> 5 does reach PRIMARY Goals, but only through `decode_pre_durable_attempt_budget` (`src/task_store.rs:433`), which for a document with no `managed_worktree` key only rewrites `schema_version` and re-decodes. `Goal` is `#[serde(deny_unknown_fields)]`, so nothing is silently dropped on the next rewrite, and `WorkspaceMode::Primary` still serializes away. The only durable delta for PRIMARY is the on-disk `schema_version` integer, which is not part of any MCP projection.

## Complete Findings Index

| ID | Action | Surface | User-visible outcome | Confidence |
| --- | --- | --- | --- | --- |
| `F1` | `Discuss` | `goal_run` `stop_detail.detail` for `MANAGED_RETRY_EXHAUSTED`, and the durable `Goal.blockers` entry | The one terminal managed-creation message no longer says why the two attempts failed; the Git exit status and stderr are gone | high |
| `F2` | `Discuss` | Windows approval prompt for `managed_worktree_create` | Operator is asked to approve a mutation that cannot happen; denying yields `MANAGED_CREATION_APPROVAL_DENIED` instead of the real terminal cause | high |
| `F3` | `Discuss` | schema 4 -> 5 migration of a stored managed Goal | An upgrading operator's in-flight managed Goal can be stranded permanently, and the reported "2/2" consumption never happened | high |
| `F4` | `Watch` | `MANAGED_RECOVERY_REQUIRED` when an exact worktree exists with zero consumed attempts | A pre-existing exact worktree now burns the lifecycle to `BLOCKED` instead of yielding `ACTIVE` | medium |
| `F5` | `Watch` | operator-facing approval prompt text and detail for managed creation | Prompt says "Allow without sandbox?" for a Git mutation and omits which attempt is being authorized | high |
| `I1`..`I5` | `Intentional` | see `Intentional Changes` | - | - |

## Block

None.

## Discuss

### F1 Discuss - `MANAGED_RETRY_EXHAUSTED` no longer carries the Git diagnostic that explains the failure

User impact: When managed creation permanently stops, the operator sees only `the lifetime creation budget is spent (2/2) and no further host Git creation invocation is authorized`. The `git worktree add` exit status and stderr that actually caused it - for example "exit status 128: fatal: could not lock config file .git/config" - are no longer shown anywhere, in the `goal_run` response or in the durable blocker. A transient, host-recoverable failure and a permanent one are now indistinguishable from the only diagnostic the product emits for this terminal state.

Review reason: This is the sole error surface for the single terminal managed-creation outcome, and this diff is precisely the change that moved the block to a different point in the loop. Losing the evidence is a silent degradation of a message the baseline produced, and nothing in the test suite pins the string.

Surface: `goal_run` output (`stop_detail.detail`) and persisted `Goal.blockers`, for `MANAGED_WORKTREE` Goals only

Confidence: high (both the baseline and current construction sites were read; `attempt_evidence` is provably out of scope at the new block site)

Look here first:
- [managed_worktree_prepare.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L651)
- [managed_worktree_prepare.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L684)

Behavior delta:
- Before: the block was produced immediately after the failing attempt, so its detail was `"creation attempt {attempt} left no worktree or branch side effect and the retry budget is exhausted; {attempt_evidence}"` - for example `creation attempt 2 left no worktree or branch side effect and the retry budget is exhausted; git worktree add -b local-mcp/goal/<uuid> completed with exit status 128: fatal: ...`.
- After: the post-invocation match ends in `_ => continue`, so the loop re-enters at the top, the budget check at line 651 fires on the next iteration, and `attempt_evidence` - a `let` binding scoped to the previous iteration - is gone. The new detail names the budget and nothing else. The same string is what `block_lifecycle` persists as the durable blocker.

Evidence:
- `describe_creation_attempt` (`src/managed_worktree_prepare.rs:722`) exists specifically because "Git's own diagnostic is host-owned and is the only place the real reason a command failed appears". After this diff it is used at exactly one call site, the `MANAGED_RECOVERY_REQUIRED` arm at line 705.
- `after_two_consumed_attempts_no_third_git_invocation_occurs` asserts only `block.code`, not `block.detail`, so the loss is not covered by any test.
- Inference (not runtime-verified): because the durable blocker is written from the same `detail`, the evidence is not recoverable from `goal_result`/`goal_resume` afterwards either.

Reviewer action:
raise in review - carry the last attempt's evidence forward (bind it outside the loop iteration) so `MANAGED_RETRY_EXHAUSTED` keeps the Git diagnostic, and pin the detail in a test.

### F2 Discuss - On Windows the operator is prompted to approve an invocation the host has already decided not to make

User impact: On Windows, once the durable budget is spent, every managed `goal_run` still shows the operator the `managed_worktree_create` approval prompt. Approving it produces `stop_reason = MANAGED_RETRY_EXHAUSTED`. Answering `n` produces `stop_reason = MANAGED_CREATION_APPROVAL_DENIED`, which names the operator's choice instead of the actual terminal condition. In a yolo session the console prints a third `[yolo] allowing managed_worktree_create: ...` line for a mutation that will never run. A first `goal_run` that exhausts the budget therefore shows three consent prompts for two Git invocations.

Review reason: Consent prompts must describe operations that can happen, and a stop reason must name the real cause. Both properties fail on the terminal failure path, and the combination is untested on the platform where it matters.

Surface: Windows operator console (`approvals::start`) and `goal_run` `stop_reason`, for `MANAGED_WORKTREE` Goals only

Confidence: high (loop order read directly; the ordering is not platform-conditional, only the gate is)

Look here first:
- [managed_worktree_prepare.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L592)
- [managed_worktree_prepare.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L651)

Behavior delta:
- Before: there was no approval step, so no prompt could precede an unavailable invocation. The only block on this path was `MANAGED_RETRY_EXHAUSTED`.
- After: Step 3 (approval, line 592) runs before Steps 4 and 5 (budget check at line 651, then consume). The third loop iteration - reached whenever attempt 2 proved no side effect, or whenever a migrated record arrives with a spent budget - reaches the prompt with `permits_creation_attempt() == false`.

Evidence:
- Iteration trace with `requires_approval == true`: it1 prompt 1 -> consume(1) -> create -> NoSideEffect -> continue; it2 prompt 2 -> consume(2) -> create -> NoSideEffect -> continue; it3 prompt 3 -> budget check fails -> `MANAGED_RETRY_EXHAUSTED`. Denying at prompt 3 returns `MANAGED_CREATION_APPROVAL_DENIED` through `refuse_managed_workspace`, which does not burn the lifecycle, so the Goal is reported as a refusal rather than as terminally blocked.
- Coverage gap: every test that combines approval with the budget (`a_denied_approval_never_invokes_git_and_costs_no_attempt`, `after_two_consumed_attempts_no_third_git_invocation_occurs`, `a_schema_four_prepared_record_migrates_fail_closed_with_no_fresh_retry_authority`) drives it through `fixture.prepare(...)`, which passes `managed_creation_requires_approval()` - `false` on the Unix CI leg. The approval-then-exhausted sequence is therefore never exercised by the diff's tests.

Reviewer action:
raise in review - move the `permits_creation_attempt()` check ahead of the approval step so the host never asks for consent it cannot honour, and add a test that drives `requires_approval = true` against a spent budget.

### F3 Discuss - Upgrading a stored schema-4 `PREPARED` managed Goal can strand it, and the reported budget consumption is fabricated

User impact: An operator who upgrades while a managed Goal is mid-creation (lifecycle `PREPARED`, no worktree and no branch on disk yet) finds that Goal permanently stops. After upgrade the record reports `creation_attempts_consumed = 2`, so preparation reaches the budget check, durably burns the lifecycle to `BLOCKED`, and reports `the lifetime creation budget is spent (2/2)`. Before the upgrade, that same Goal would have been created on the next `goal_run`. The reported number is also false: zero Git invocations were ever recorded for that Goal; the migration assigned the value, it did not measure one.

Review reason: This is the one place where the schema bump changes what happens to an existing user's durable state, and the operator-facing reason it gives is factually wrong. The fail-closed choice itself is defensible and design-consistent - see `I3` - but the upgrade consequence should be a stated release note, and the message should not assert a consumption that never happened.

Surface: Persisted Goal state and `goal_run` output after upgrading from a schema-4 store, for `MANAGED_WORKTREE` Goals only

Confidence: high for the mechanism (migration code and validation both read; the diff's own test asserts exactly this block); medium for how many real operators are in that state

Look here first:
- [task_store.rs](/Users/yuta/local-mcp-connector-parity/src/task_store.rs#L433)
- [managed_worktree.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree.rs#L661)

Behavior delta:
- Before (schema 4, per-call loop): a `PREPARED` Goal with no side effect re-entered `for attempt in 1..=MAX_CREATION_ATTEMPTS` with a fresh budget and would invoke `git worktree add` on the next `goal_run`.
- After (schema 5, durable budget): the migration sets `creation_attempts_consumed = MAX_LIFETIME_CREATION_ATTEMPTS` for every lifecycle other than `REQUESTED`, so the same Goal blocks before any invocation. `REQUESTED` records keep a full budget, and a `PREPARED` record whose worktree *does* reconcile exactly still activates without spending anything - so the stranded set is precisely "mid-flight, no side effect", which is the state the design calls genuinely ambiguous.

Evidence:
- `a_schema_four_prepared_record_migrates_fail_closed_with_no_fresh_retry_authority` constructs a real schema-4 document, migrates it, and asserts `MANAGED_RETRY_EXHAUSTED` with `creator.calls() == 0`. The behavior is intended and covered; the concern is the message and the undocumented upgrade consequence.
- No lost history: `decode_pre_durable_attempt_budget` rewrites `schema_version` and inserts exactly one field, then re-decodes through `#[serde(deny_unknown_fields)]`. Terminal Goal history, blockers, tasks, and verification records are untouched. A `PRIMARY` schema-4 Goal takes the no-`managed_worktree` branch and changes semantics not at all, satisfying design section 9.
- No new stuck state beyond managed: `MANAGED_RECOVERY_REQUIRED` and the lifecycle `BLOCKED` terminal state already existed at baseline; what is new is that the migration can route an *in-flight* Goal into it.

Reviewer action:
confirm intent and request a release note - the fail-closed derivation is defensible, but the `(2/2)` figure should be phrased as "no retry authority was granted for a legacy in-flight record" rather than as a measured consumption, and the upgrade impact on in-flight managed Goals should be documented.

## Watch

### F4 Watch - A pre-existing exact worktree now burns the lifecycle instead of yielding `ACTIVE`

User impact: If an exact matching worktree already exists at the host-derived target while the record has consumed zero attempts - for example an operator pre-created `<managed-root>/<session>/<goal>` with the expected branch and base before the first `goal_run` - preparation now reports `MANAGED_RECOVERY_REQUIRED` and durably moves the lifecycle to `BLOCKED`. Before, the same situation activated the workspace (`MANAGED_WORKSPACE_ACTIVE_NOT_PLANNABLE`, lifecycle `ACTIVE`). The operator's obvious remedy, deleting the stray worktree and re-running, is no longer available through `goal_run`, because the lifecycle was burned and the record simultaneously asserts the exact side effect is not the Goal's.

Review reason: The refusal itself is correct and tested ("never adopt what this lifecycle did not create"), and the case is narrow. The collateral is that a refusal whose remediation is a single `git worktree remove` becomes permanent.

Surface: `goal_run` `stop_reason` and durable managed lifecycle, for `MANAGED_WORKTREE` Goals only

Confidence: medium (the code path is certain; the frequency of the triggering state is unknown)

Look here first:
- [managed_worktree_prepare.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L563)

Behavior delta:
- Before: `Reconciliation::ActiveExact` on the pre-invocation path called `activate(...)` unconditionally.
- After: it first requires `creation_attempts_consumed() != 0`, otherwise `block_lifecycle(... "MANAGED_RECOVERY_REQUIRED" ...)`, which burns the lifecycle. The same check was added to the `ACTIVE` reconciliation path at line 445, where it is effectively unreachable in production because `to_active` only follows `PREPARED`.

Evidence:
- `an_exact_worktree_with_no_consumed_attempt_is_not_adopted_as_ours` plants the exact worktree out of band and asserts the block. The intent is explicit and reasonable.
- The neighbouring refusals in this file (`MANAGED_SESSION_AUTHORITY`, `MANAGED_OBSERVATION_UNAVAILABLE`, and now approval denial/unavailability) all use `refuse_managed_workspace`, which records a deduplicated blocker and preserves the lifecycle precisely so a later `goal_run` can proceed. This new block is the one refusal that does not.

Reviewer action:
approve with caveat - consider `refuse_managed_workspace` (lifecycle preserved, durable blocker recorded) instead of `block_lifecycle` here, so removing the foreign worktree and re-running remains a viable recovery; at minimum say so in the detail string.

### F5 Watch - The managed approval prompt reuses the sandbox-bypass wording and omits attempt context

User impact: A Windows operator approving a `git worktree add` reads `Allow without sandbox? [y/N]`, which describes a different operation. The detail line correctly binds the mutation (`operation=managed_worktree_create goal=... primary_root=... worktree_root=... branch=... base_commit=...`) but does not indicate that this is the second of two authorized invocations, so an operator consenting to the retry has no signal that a prior `git worktree add` already ran.

Review reason: The wrong wording is pre-existing shared behavior for every approval-gated operation, so it is not a regression introduced here, but it now applies to a Git mutation an operator is likely to see for the first time. The missing attempt context is new surface with no cost to add.

Surface: Windows operator console, `local-mcp start`, for `MANAGED_WORKTREE` Goals only

Confidence: high

Look here first:
- [approvals.rs](/Users/yuta/local-mcp-connector-parity/src/approvals.rs#L452)
- [managed_worktree_prepare.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L288)

Behavior delta:
- Before: no managed-creation approval existed. `show_request` already hardcoded `Allow without sandbox?` for `without_sandbox`, `git_stage_paths`, and `codex_fallback_v2_execute`.
- After: a fourth operation name, `managed_worktree_create`, reuses the same hardcoded prompt line.

Evidence:
- Verified positives for this surface, all by reading: the detail string binds the exact durable target, branch ref, and base commit; the `cwd` passed to `approvals::request` is the primary root, matching what Git will run in; denial performs no Git invocation and consumes no attempt (`a_denied_approval_never_invokes_git_and_costs_no_attempt`), and the only durable residue is a code-deduplicated blocker, so repeated denials do not grow the Goal; an unavailable or malformed channel is treated identically (`an_unavailable_approval_channel_never_invokes_git`); yolo remains coherent because `approvals::start` auto-allows each request under the pre-existing session-global flag, and the pipe payload cannot set that flag; and no approval IPC await happens while the per-session OS lock is held, so the operator's `/permission` handling cannot deadlock.

Reviewer action:
approve with caveat - add the attempt number (`attempt=2/2`) to the approval detail; the prompt wording itself is a separate, pre-existing item.

## Intentional Changes

- `I1` Managed worktree creation is bounded by a **durable lifetime** budget of two authorized Git invocations, persisted as `ManagedWorktreeRecord.creation_attempts_consumed`, instead of a per-call loop counter. A restart, resume, repeated `goal_run`, or crash no longer replenishes it, and reaching `ACTIVE`/`BLOCKED` keeps the count as evidence. This is the headline product change of the phase and is stated in `SECURITY.md` requirement 4. Consequence worth stating in release notes: a transient Git failure that previously self-healed on the next `goal_run` now permanently blocks that managed Goal. - [SECURITY.md](/Users/yuta/local-mcp-connector-parity/SECURITY.md), [managed_worktree.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree.rs#L417)
- `I2` On **Windows only**, the host-native `git worktree add` is approval-gated through the existing local approval system, adding the stop reasons `MANAGED_CREATION_APPROVAL_DENIED` and `MANAGED_CREATION_APPROVAL_UNAVAILABLE` and the approval operation `managed_worktree_create`. Design section 23 ("host-native mutation remains approval-gated under current Windows policy"). Linux and macOS gain no interactive step; `managed_creation_requires_approval()` is `cfg!(windows)` and the gate is never consulted off Windows. A client with an exhaustive `stop_reason` switch will see two new values, but only for opt-in managed Goals on Windows. - [SECURITY.md](/Users/yuta/local-mcp-connector-parity/SECURITY.md), [mcp.rs](/Users/yuta/local-mcp-connector-parity/src/mcp.rs#L241)
- `I3` The schema 4 -> 5 migration derives the missing counter from the lifecycle alone: `REQUESTED` starts at 0, every other lifecycle migrates to a spent budget and fails closed. Never from a branch name, path, revision, or Git observation; a schema-4 document that carries the field is rejected. - [task_store.rs](/Users/yuta/local-mcp-connector-parity/src/task_store.rs#L433)
- `I4` The `goal_run` MCP tool description now states the Windows approval requirement and the durable attempt limit. Visible to clients in `tools/list`. - [mcp.rs](/Users/yuta/local-mcp-connector-parity/src/mcp.rs#L241)
- `I5` `SECURITY.md` restates managed-worktree security as four numbered requirements (opt-in, session path authority, Windows approval, durable bounded retry) and states the real authority boundary. Docs-only. - [SECURITY.md](/Users/yuta/local-mcp-connector-parity/SECURITY.md)

## Coverage Ledger

| Surface / path | Touched files or entry points | Status | Result | Evidence |
| --- | --- | --- | --- | --- |
| `goal_run` output for PRIMARY Goals (all stop reasons) | [goal_runner.rs](/Users/yuta/local-mcp-connector-parity/src/goal_runner.rs#L302) | Reviewed - no user-visible regression found | Async seam added but never yields for PRIMARY; `stop_detail` stays `null`; no new code reachable | Direct path trace; `the_foreground_run_seam_refuses_a_control_state_before_any_authority_call` pins the control-state seam |
| `goal_run` output for managed Goals, happy path | [managed_worktree_prepare.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L502) | Reviewed - no user-visible regression found | Same `ACTIVE` -> `MANAGED_WORKSPACE_ACTIVE_NOT_PLANNABLE` outcome; now with one attempt durably consumed | Direct path trace; `the_first_invocation_consumes_the_attempt_durably_before_git`, `adopting_an_exact_side_effect_does_not_consume_budget` |
| `goal_run` output for managed Goals, failure/retry path | [managed_worktree_prepare.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L651) | Finding F1, F2, F3 | Terminal message lost its Git diagnostic; approval precedes the budget check; migrated records report a fabricated count | Direct path trace, baseline source read for comparison |
| Windows approval prompt (operator console) | [approvals.rs](/Users/yuta/local-mcp-connector-parity/src/approvals.rs#L444), [managed_worktree_prepare.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L305) | Intentional I2; Finding F2, F5 | Gate is correct and denial is side-effect free; prompt wording and ordering are the gaps | Direct path trace of `approvals::request` / `start` / yolo; 8 dedicated tests |
| `tools/list` output | [mcp.rs](/Users/yuta/local-mcp-connector-parity/src/mcp.rs#L241) | Intentional I4 | `goal_run` description updated; no schema change | Direct read |
| `goal_start` with `MANAGED_WORKTREE` | [goal_api.rs](/Users/yuta/local-mcp-connector-parity/src/goal_api.rs#L465) | Reviewed - no user-visible regression found | Untouched code path; record is now created with `creation_attempts_consumed: 0` | Direct read; `a_fresh_request_starts_with_the_full_lifetime_budget` |
| `goal_start` with `PRIMARY` (default) | [goal_api.rs](/Users/yuta/local-mcp-connector-parity/src/goal_api.rs#L465) | Reviewed - no user-visible regression found | Untouched; `WorkspaceMode::Primary` still serializes away | Direct read |
| Durable store migration: schema 1/2/3 -> 5 | [task_store.rs](/Users/yuta/local-mcp-connector-parity/src/task_store.rs#L380) | Reviewed - no user-visible regression found | Sibling migrations rewritten to the current version; PRIMARY semantics unchanged | Direct read; `a_schema_four_primary_goal_migrates_without_managed_authority` |
| Durable store migration: schema 4 -> 5, managed record | [task_store.rs](/Users/yuta/local-mcp-connector-parity/src/task_store.rs#L433) | Finding F3; Intentional I3 | Fail-closed derivation, no invented authority, no lost history; upgrade consequence undocumented | Direct read + 4 dedicated tests |
| Durable store: current-schema document missing the counter | [managed_worktree.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree.rs#L417) | Reviewed - no user-visible regression found | Deliberately not `serde(default)`: a missing field is rejected rather than read as zero | `a_current_schema_document_missing_the_attempt_field_is_rejected` |
| Durable store: out-of-range counter | [managed_worktree.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree.rs#L651) | Reviewed - no user-visible regression found | Rejected at validate, never clamped or wrapped | `an_out_of_range_attempt_count_fails_closed` |
| Durable store: counter across lifecycle transitions | [managed_worktree.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree.rs#L758) | Reviewed - no user-visible regression found | Carried unchanged into `ACTIVE` and `BLOCKED`; no refund path exists | Direct read |
| Adoption of an exact side effect | [managed_worktree_prepare.rs](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L563) | Finding F4 | Refusal is correct; collateral is a burned lifecycle | Direct path trace + `an_exact_worktree_with_no_consumed_attempt_is_not_adopted_as_ours` |
| `goal_resume` interaction with the new blockers | [goal.rs](/Users/yuta/local-mcp-connector-parity/src/goal.rs#L3113) | Reviewed - no user-visible regression found | `goal_resume` clears blockers but never advances the managed lifecycle, so it cannot accidentally re-grant an attempt; unchanged from baseline | Direct read |
| Approval IPC runtime correctness | [mcp.rs](/Users/yuta/local-mcp-connector-parity/src/mcp.rs#L839) | Reviewed - no user-visible regression found | The await runs on a dedicated `new_current_thread().enable_all()` runtime inside `spawn_blocking`; no nested runtime, no lock held across the await | Direct read of the caller and `approvals::request` |
| `SECURITY.md` | `SECURITY.md` | Intentional I5 | Docs-only; no code behavior | Direct read |
| `README.md` | not touched | Not user-visible | No managed-worktree section exists in `README.md`, so the schema bump and the Windows gate introduce no README/behavior contradiction | `grep` for `managed worktree` / `MANAGED_WORKTREE` / `retry budget` returns nothing relevant |
| `docs/MANAGED_WORKTREES_V1_DESIGN.md` | not touched | Reviewed - no user-visible regression found | Frozen contracts still hold: section 11 makes bounded retry permissive ("may be allowed"), section 23 mandates the Windows gate, section 9 requires non-managed Goals to keep their semantics | Direct read of sections 9, 11, 23, 26 |
| `src/managed_worktree_creation_tests.rs` | test-only | Not user-visible | No production path depends on it | - |

## Evidence Appendix

### Behavior Graph Deltas

| ID | Surface | Baseline path | After-change path | Delta | Ledger / finding link |
| --- | --- | --- | --- | --- | --- |
| `B1` | `goal_run`, PRIMARY Goal | Entry `goal_run` -> Input durable Goal (PRIMARY) -> Guards control-state gate -> Transform `prepare_managed_workspace` -> Output unchanged runner result | Same, with the preparation seam made `async` | No changed guard, transform, or output. The added future never reaches its only `.await` for PRIMARY, so it completes on first poll | Ledger row 1 |
| `B2` | `goal_run`, managed Goal happy path | Entry `goal_run` -> Input managed record (REQUESTED) -> Guards session authority -> eligibility -> durable PREPARED intent -> pre-reconcile -> Git -> post-reconcile -> Output `MANAGED_WORKSPACE_ACTIVE_NOT_PLANNABLE` | Same, plus Step 3 approval (Windows) and Steps 4/5 durable counter consume before Git | Changed input to Git: an attempt is durably consumed before each spawn. Changed output on Windows only: an approval round trip precedes it | Ledger rows 2, 4 |
| `B3` | `goal_run`, managed creation failure | Entry `goal_run` -> Input PREPARED record -> Guards pre-reconcile NoSideEffect -> Git fails -> post-reconcile NoSideEffect -> Output `MANAGED_RETRY_EXHAUSTED` carrying `attempt_evidence` | Entry -> Input PREPARED record -> Guards pre-reconcile -> **approval (Windows)** -> **budget check** -> consume -> Git fails -> post-reconcile -> loop -> budget check -> Output `MANAGED_RETRY_EXHAUSTED` without `attempt_evidence` | Moved guard (budget check now gates re-entry, not loop continuation) and lost evidence; added an approval guard ahead of the budget guard | F1, F2 |
| `B4` | Windows approval gate | Entry `goal_run` managed -> no host mutation happens without operator consent -> Output n/a | Entry -> guards session authority, eligibility, pre-reconcile -> **approval** -> durable consume -> Git -> Output `ACTIVE` or a block | New guard on Windows only. Binds goal/primary_root/worktree_root/branch/base; re-reads the durable intent after approval | I2, F2, F5 |
| `B5` | schema 5 migration | Entry `load_goal` -> Input schema-4 document -> Guards store_format + version -> Transform decode at v4 -> Output Goal | Entry -> Input schema-4 document -> Guards store_format + version -> Transform `decode_pre_durable_attempt_budget` (insert counter from lifecycle, rewrite version) -> validate -> Output Goal at v5 | New transform step; inserts exactly one field and rejects a document that already carries it | I3, F3 |
| `B6` | `tools/list` discovery | Entry MCP `tools/list` -> Output tool descriptions | Same | `goal_run` description text changed; no schema change | I4 |

### Diff Inventory

| File or area | Classification | User-visible path considered |
| --- | --- | --- |
| `src/managed_worktree_prepare.rs` | surface | managed `goal_run`, Windows operator console |
| `src/managed_worktree.rs` | dependency (durable model) | persisted state, validation refusals |
| `src/task_store.rs` | dependency (persistence) | upgrade path for stored Goals |
| `src/goal.rs` | dependency (durable model) | schema version, consume operation |
| `src/goal_runner.rs` | surface | `goal_run` output for every Goal |
| `src/mcp.rs` | surface | `tools/list` output |
| `SECURITY.md` | docs-only | none directly; authoritative contract for intent |
| `src/managed_worktree_creation_tests.rs` | test-only | none |

### Candidate Sweep Log

| Candidate | Decision | Reason |
| --- | --- | --- |
| `ManagedWorkspaceError::ApprovalUnavailable` becomes a new user-facing error string | dismissed | The approver's `Err` is converted into the `MANAGED_CREATION_APPROVAL_UNAVAILABLE` block at line 597, so it never reaches the `MANAGED_WORKSPACE_EVALUATION_FAILED` path. No independent surface. |
| Post-invocation reconciliation lost its `!permits_bounded_retry()` guard, so ambiguous results might now retry | dismissed | Verified false: the guard is intact at [managed_worktree_prepare.rs:698](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L698). Only the two trailing arms were rewritten. |
| `ManagedCreationApproval::matches` omits `primary_root`, so a stale approval could run Git elsewhere | dismissed (not user-visible) | `primary_root` is set once in `requested()` and validated to equal `Goal.cwd` ([goal.rs:3516](/Users/yuta/local-mcp-connector-parity/src/goal.rs#L3516)); no mutation path exists, so the omission is unreachable. Defense-in-depth only. |
| The post-approval re-read can produce `MANAGED_RECOVERY_REQUIRED` and burn the lifecycle | dismissed (unreachable) | `worktree_root`, `branch_ref`, and `base_commit` are frozen at creation and have no setter, so `matches` cannot become false during the approval window. |
| The durable budget makes the creation loop unbounded | dismissed | Every iteration either returns or calls `consume_creation_attempt`, which refuses at the bound; `validate` caps the stored value, so the third iteration always takes the budget branch. |
| Schema-5 migration could silently drop unknown fields from a stored schema-4 Goal | dismissed | `Goal` and `ManagedWorktreeRecord` are both `#[serde(deny_unknown_fields)]`; an unexpected field is a loud `CorruptGoal`, not data loss. |
| The schema bump changes `goal_run` or `goal_result` output for PRIMARY Goals | dismissed | No MCP projection serializes `schema_version` or the managed record; verified in `src/mcp.rs` and `src/goal_api.rs`. |
| A PRIMARY schema-4 Goal is converted into a managed Goal by the migration | dismissed | The no-`managed_worktree` branch only rewrites `schema_version`; `a_schema_four_primary_goal_migrates_without_managed_authority` covers it. |
| Approval IPC awaited while the per-session lock is held could deadlock the operator console | dismissed | Each `persist` / `load_goal` acquires and releases the lock; the `await` at line 594 happens between them. |

### Verification Commands

- `git --no-pager diff --stat 25948dee..HEAD` - 8 files, +1613/-68.
- `git --no-pager diff 25948dee..HEAD -- src/goal.rs src/goal_runner.rs src/mcp.rs SECURITY.md` - inspected in full.
- `git --no-pager diff 25948dee..HEAD -- src/managed_worktree.rs src/task_store.rs` - inspected in full.
- `git --no-pager diff 25948dee..HEAD -- src/managed_worktree_prepare.rs` - inspected in full, plus a line-by-line read of both the baseline and current creation loop.
- `git --no-pager show 25948dee:src/managed_worktree_prepare.rs` - baseline creation loop read for exact output comparison.
- `grep` / read of `src/approvals.rs`, `src/mcp.rs`, `src/goal_api.rs`, `src/goal_runner.rs`, `src/managed_worktree.rs`, `src/task_store.rs`, `docs/MANAGED_WORKTREES_V1_DESIGN.md`, `SECURITY.md`, `README.md`, `.agents/skills/goallatch-maintainer/SKILL.md`.
- **Not run** (read-only scope): `cargo build`, `cargo test`, `cargo clippy`, `cargo fmt --check`. No claim in this report rests on a green build.

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `F1` | output | [managed_worktree_prepare.rs:656](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L656) | New `MANAGED_RETRY_EXHAUSTED` detail, without the Git evidence |
| `F1` | behavior | [managed_worktree_prepare.rs:684](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L684) | `attempt_evidence` is now scoped to an iteration that already ended |
| `F2` | guard | [managed_worktree_prepare.rs:592](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L592) | Approval runs before the budget guard |
| `F2` | guard | [managed_worktree_prepare.rs:651](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L651) | The budget guard that should run first |
| `F3` | input | [task_store.rs:433](/Users/yuta/local-mcp-connector-parity/src/task_store.rs#L433) | Migration assigns the spent budget to legacy non-`REQUESTED` records |
| `F3` | invariant | [managed_worktree.rs:661](/Users/yuta/local-mcp-connector-parity/src/managed_worktree.rs#L661) | `REQUESTED` must have zero consumed, which is why the migration branches on lifecycle |
| `F4` | behavior | [managed_worktree_prepare.rs:563](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L563) | Zero-attempt `ActiveExact` now blocks the lifecycle instead of activating |
| `F4` | contrast | [managed_worktree_prepare.rs:842](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L842) | `refuse_managed_workspace`, the lifecycle-preserving refusal used elsewhere |
| `F5` | output | [approvals.rs:452](/Users/yuta/local-mcp-connector-parity/src/approvals.rs#L452) | Hardcoded "Allow without sandbox?" prompt line |
| `F5` | behavior | [managed_worktree_prepare.rs:288](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L288) | The approval detail an operator actually reads |
| PRIMARY | entry | [goal_runner.rs:302](/Users/yuta/local-mcp-connector-parity/src/goal_runner.rs#L302) | The only production change on the PRIMARY path |
| PRIMARY | transform | [managed_worktree_prepare.rs:377](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L377) | `NotManaged` return that keeps PRIMARY free of store writes and Git |

### Blind Spots

| Area | Risk introduced by the blind spot | What would resolve it |
| --- | --- | --- |
| No build or test execution (read-only scope) | A compile error or a failing pre-existing test would surface as a `Discuss` that this review could not see | `cargo test --locked --all-targets` and `cargo clippy --locked --all-targets --all-features -- -D warnings` on a scratch checkout |
| The Windows approval branch was never executed | `F2`'s ordering and the real operator console transcript are proven by reading only | Run the managed creation suite on the Windows CI leg, plus one manual `goal_run` against a spent budget with `local-mcp start` attached |
| No real schema-4 operator store | `F3`'s blast radius is inferred from the migration derivation and a synthetic document, not from observed data | Query the store migration against a snapshot of a real schema-4 session with an in-flight managed Goal |
| The operator-facing effect of losing the Git diagnostic was not observed end to end | `F1`'s severity is judged from code, not from an operator session | A manual reproduction of two failed `git worktree add` attempts and inspection of the resulting `goal_run` response and blocker list |
| `README.md` was not updated for the schema bump or the Windows gate | `README.md` has no managed-worktree section today, so nothing became contradictory, but the public-behavior surface is silent on both | Confirm with the owner that `SECURITY.md` is the intended public surface for these two changes |

### Report Self-Check

- `yes` Every touched user-visible or unknown-impact surface appears in `Coverage Ledger`.
- `yes` Every finding in an action section appears in `Complete Findings Index`.
- `yes` Every `Finding F#` ledger row has a matching card.
- `yes` Every `Not covered` row has a reason and next verification step. (No row is marked `Not covered`; the gaps are recorded as blind spots with concrete next steps.)
- `yes` Every user-visible or unknown-impact surface has graph or direct path evidence, or is explicitly marked as not covered.
- `yes` Recommendation follows the mapping rules from the skill: no `Block`, three unresolved `Discuss` findings, complete coverage of the reviewed surfaces, therefore `Discuss`.

### Continuation

Report only. No implementation authority was requested or granted by this review; F1-F5 require a receiving workflow before any source change.