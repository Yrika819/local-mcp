# Managed Worktrees V1 Phase 3 — Security / Recovery Closure Review

## Scope

- Review date: `2026-09-30`
- Scope kind: `commit range`
- Scope description: the Phase 3 security/recovery closure diff, `25948de..5b762b7` on
  `managed-worktrees/v1-phase3-creation-authority`
- Baseline: `25948de` (green, 11/11 on run 36700582297)
- Target: `5b762b7`
- Changed paths: `8`
- Requirements consulted: `docs/MANAGED_WORKTREES_V1_DESIGN.md` sections 5, 6, 7, 8, 9, 11, 20, 21, 23;
  `SECURITY.md`; `README.md`; `.agents/skills/goallatch-maintainer/SKILL.md`; the closure task contract
- Assumptions: Phase 4 remains out of scope and is not a finding; the pre-existing
  `phase0_tests` / `execution::tests` load-sensitive timing failures are not caused by this diff

## Why this is a new chain rather than a revision of the previous one

The prior chain (`rc-20260930-mwp3d94f18`) is left exactly as written. Its generation-1 report
recorded the Windows approval question as "effectively consistent with existing product behavior",
which **conflicts** with the frozen requirement in `docs/MANAGED_WORKTREES_V1_DESIGN.md` section 23:

> host-native mutation remains approval-gated under current Windows policy

That conclusion was reasonable against the evidence available then - the seam reused the host Git
identity and the frozen design's section 10 command block says nothing about approval - but it is
wrong. This chain records the now-confirmed gap, the product decisions taken, and the fixes. The
historical reports remain evidence of what was known when they were written and are not rewritten.

## Gate Snapshot

- **Recommendation**: `Discuss`
- **Completion**: `Complete within reviewed scope`
- **Why now**: both closure issues are implemented, locally verified, and confirmed by the Windows
  CI legs; the two remaining items are an open product question and a documented limitation
- **Must-review now** (full list below and in Complete Findings Index):
  1. `F1` the Windows approval gap this chain confirms, and the fix
  2. `F2` the durable lifetime retry decision, and the fix
  3. `F3` the legacy `PREPARED` migration that deliberately strands a Goal
- **Findings count**: `Block 0 | Major 0 | Minor 4 | Question 1`
- **Coverage confidence**: `high` for behavior and authority; `medium` for Windows runtime specifics
- **Biggest blind spot**: the approval await has no deadline, so an unanswered Windows prompt holds a
  `goal_run` open indefinitely. Deliberately unchanged; see `F4`.

## Complete Findings Index

| ID | Severity | Surface | Risk | Confidence | Origin | Verification | Disposition |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `F1` | `Major` (confirmed gap, now fixed) | Windows host-native managed mutation | a host-native Git mutation ran with no approval, against design section 23 | high | product decision + code review | code-path trace; `#[cfg(windows)]` policy test; Windows CI legs | Fixed in `c4124b8` |
| `F2` | `Major` (confirmed gap, now fixed) | creation retry budget | the bound was per-call, so restart/resume/repeat granted a fresh budget | high | product decision + code review | trace of the loop and the durable counter; restart test over a fresh `TaskStore` | Fixed in `c4124b8` |
| `F3` | `Intentional` | schema 4 -> 5 migration | a legacy `PREPARED` managed Goal migrates to a spent budget and stops | high | closure task contract | migration test against real durable bytes | Deliberate; directed by the task, recorded so it is not mistaken for a defect |
| `F4` | `Minor` (accepted) | approval await | an unanswered Windows prompt holds `goal_run` open with no deadline | medium | `/regression-review` | trace of `approvals::request` and the `goal_run` driver | Not changed; see rationale below |
| `F5` | `Minor` (fixed) | durable counter decode | an absent or document-supplied attempt count decoded as maximum authority | high | code review | three regression tests | Fixed in `e1d665c` |
| `F6` | `Minor` (fixed) | exhausted-budget reporting and prompt ordering | a spent budget lost the Git diagnostic and still prompted for approval | high | `/regression-review` | two regression tests | Fixed in `5b762b7` |
| `F7` | `Question` | legacy migration reporting | a migrated ambiguous record reports a specific consumed count it does not actually know | medium | `/regression-review` | migration code trace | Open for the product owner |

## Block

`None.`

## Major (both confirmed and fixed)

### F1 Major - Windows host-native managed mutation ran with no approval

User impact: on Windows, where the product documents that there is no process sandbox, a
`goal_run` could create a Git linked worktree, a local branch, and a ref on disk with no operator
consent, while every other host-native mutation in the product is approval-gated.

Review reason: the frozen design requires this gate explicitly, and the prior chain's contrary
conclusion was wrong.

Surface: `managed_worktree_create` reached from `goal_run`.

Fix: a narrow `ManagedCreationApprover` seam sits between reconciliation and attempt consumption. The
production implementation calls the existing `approvals::request`, so this is the same socket, the
same yolo semantics, and the same fail-closed handling of a missing channel or malformed reply that
`without_sandbox` and the staging mutation already use. It is not a second approval system. The
approval binds a host-owned description derived from the durable `PREPARED` intent - operation,
primary root, worktree target, branch ref, base commit, Goal identity - and after approval the
durable intent is re-read so a stale approval cannot authorize a different operation.

Approval is platform policy: Linux and macOS gain no interactive step and keep their existing
sandbox, network, and path behavior. The decision is a parameter of the internal preparation entry
so the gate is testable on Unix CI, and a `#[cfg(windows)]` test proves production selects the
required value.

Verified by: 10 approval tests driven through the injected policy on every platform, plus
`test (windows-x64)` and both Windows compat legs.

### F2 Major - The creation retry bound was per-call, not durable

User impact: an operator could obtain more host Git creation invocations than the frozen design
permits by restarting the process, resuming the Goal, or simply running `goal_run` again, because the
only bound was a loop index inside a single call.

Review reason: design section 11 permits exactly one bounded retry, and design section 25.F requires
that worktree lifecycle recovery not reset low-level side-effect budgets.

Surface: `prepare_managed_workspace`, `ManagedWorktreeRecord`.

Fix: `creation_attempts_consumed` is durable, monotonic, bounded, and carried unchanged across every
lifecycle transition. It is consumed and persisted in a single mutation before `git worktree add`
runs, so a crash between consuming and spawning costs the attempt rather than refunding it, and a
failed persist can never let Git run. `GOAL_SCHEMA_VERSION` advanced 4 -> 5 because the field changes
durable retry authority. The per-call constant was deleted so nothing can read the old semantics
off it.

Verified by: 18 budget tests, including a restart simulated with a brand new `TaskStore` over the
same state root, and a crash simulated as a durable `PREPARED` intent with one attempt already
consumed.

## Intentional Changes

### F3 Intentional - A legacy `PREPARED` managed Goal is stranded by the migration

A schema-4 managed record in `PREPARED` has an unknown attempt history, so the migration gives it a
spent budget and it stops with `MANAGED_RETRY_EXHAUSTED` instead of creating a worktree. This is
deliberate and was directed: ambiguous legacy state must "fail closed rather than receive invented
authority", and must not be "defaulted to a fresh lifetime budget if that would permit additional Git
mutation". A `REQUESTED` record provably never invoked Git, so it keeps a full budget; every other
lifecycle is treated as ambiguous.

Recorded here so the behavior is not later mistaken for a regression. It is safe because the
lifecycle is untouched and the intent is retained, so explicit host recovery can still reconcile the
exact intended target.

## Watch

### F4 Minor - An unanswered approval prompt holds `goal_run` open

`approvals::request` blocks on the operator's reply, and the new await sits inside `goal_run`, so a
never-answered Windows prompt never returns. This was flagged by `/regression-review`.

Not changed, deliberately. Bounding it would make managed creation stricter than every other
approval-gated host mutation - `without_sandbox` and `write_file` are intentionally unbounded for
interactive operator review - and the closure was directed to preserve the existing approval
semantics. It is an extension of an existing property rather than a new class of defect, and no lock
is held across the await: `with_session_lock` is taken and released per operation, so the
concurrency CAS still fails closed.

If the product later wants a deadline, `src/agent.rs` holds the in-repo precedent for bounding the
same seam.

### F6 Minor - Fixed: spent-budget reporting and prompt ordering

Two user-visible defects this diff introduced, both fixed in `5b762b7`: the exhausted-budget block
lost the Git diagnostic that caused it, and approval was requested before the budget was checked, so
a spent workspace still prompted and reported a denial as the cause.

## Discuss

### F7 Question - A migrated ambiguous record reports a count it does not know

A legacy `PREPARED` record migrates to `MAX_LIFETIME_CREATION_ATTEMPTS`, and the block detail reports
that as a fact ("the lifetime creation budget is spent (2/2)"). The number is a fail-closed
placeholder, not a measurement.

It is left as-is because distinguishing a migrated placeholder from a genuinely spent budget would
need an additional durable marker, which is a schema change beyond this closure. The GoalBlocker
detail does state the budget is spent, and the lifecycle plus retained intent carry the real
recovery path. Open for the product owner: whether a separate durable "budget migrated as spent"
marker is worth adding.

## Coverage Ledger

| Area | Surface | Result | Evidence |
| --- | --- | --- | --- |
| Windows host-native approval | `managed_worktree_prepare`, `approvals` | `Finding F1`, `Finding F4` | injected-policy tests on every platform; `#[cfg(windows)]` policy test; Windows CI legs |
| Approval is not path authority | same | `Reviewed - no user-visible regression found` | `approval_does_not_add_a_permitted_directory`, `session_authority_alone_does_not_replace_approval` |
| Approval is not the opt-in | `goal_start` | `Reviewed - no user-visible regression found` | `the_workspace_mode_opt_in_is_not_approval` |
| Durable retry budget | `managed_worktree_prepare`, `managed_worktree` | `Finding F2` | 18 tests incl. restart and crash; durable round-trip through `TaskStore` |
| Budget accounting integrity | `managed_worktree` | `Finding F5` | 3 regression tests for absent / supplied / out-of-range counts |
| Schema migration | `task_store` | `Finding F3`, `Finding F7` | 6 migration tests against real durable bytes at the real store path |
| PRIMARY (`workspace_mode` omitted) | `goal_api`, `planner`, `mcp` | `Reviewed - no user-visible regression found` | 4 regression tests; the `async` seam returns `NotManaged` before any await, so no primary Goal can reach the approval gate |
| `goal_run` output shape | `mcp`, `goal_runner` | `Reviewed - no user-visible regression found` | primary-path tests unchanged; no new stop reason or `stop_detail` shape reachable for primary |
| `ACTIVE` still cannot plan | `planner`, `replanner` | `Reviewed - no user-visible regression found` | `ensure_workspace_is_plannable` untouched by this diff; existing tests still pass |
| Approval prompt UX | `approvals` | `Reviewed - no user-visible regression found` | the `"Allow without sandbox?"` string is pre-existing and shared by all operations; this diff adds operation and detail lines, making it more specific |
| Unix sandbox / network / path rules | `execution`, `sandbox`, `HostWorktreeCreator` | `Not user-visible` | the approval gate is `cfg!(windows)`-scoped; no Unix path changed |
| Windows experimental status | `SECURITY.md` | `Reviewed - no user-visible regression found` | classification preserved verbatim |

## Evidence Appendix

### Verification commands

- `cargo fmt --check` -> clean
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> clean
- `cargo test --locked --all-targets managed_worktree` -> 182 passed
- `cargo test --locked --all-targets` -> 868 passed; the only failures seen were
  `phase0_tests::mcp_contract::job_running_completion_and_completed_poll_are_frozen` and
  `execution::tests` staging preflight, both load-sensitive and in files this diff does not touch -
  each passes on repeated isolated runs (`execution::tests` 33/33 three times; the poll test 1/1 three
  times)
- `git diff --check` -> clean
- branch CI on the closure commits: `test (windows-x64)`, `compat (windows-2022)`, and
  `compat (windows-arm64)` are the legs this closure is about

### Dismissed candidates

| Candidate | Decision | Reason |
| --- | --- | --- |
| Concurrent `goal_run` double-spends the budget via a stale snapshot | dismissed | the check that authorizes is `consume_creation_attempt` running inside `mutate_goal_snapshot` against the goal re-loaded under the session OS lock; a racing run gets `RevisionConflict` and blocks |
| A persistence failure between consume and spawn could still spawn Git | dismissed | `commit_goal_unlocked` fsyncs file and parent directory, and every failure propagates before `from_intent` and `create` |
| An approval denial leaves the lifecycle `PREPARED`, so a later run retries | dismissed | correct: no Git ran, no attempt was consumed, and the next run re-asks approval |
| Nested Tokio runtime from the new `async` prepare | dismissed | `run_goal_foreground` already runs under a current-thread runtime inside `spawn_blocking`; the await happens on that runtime |
| Approval prompt wording is wrong for a Git mutation | dismissed | pre-existing generic string shared with the staging mutation; not introduced here |

### Blind spots

- The Windows approval path is exercised by injected fakes plus CI compilation and the existing
  `#[cfg(windows)]` policy assertion. No automated run drives a real interactive approval UI, and
  none should.
- The migration's fail-closed handling of a legacy `PREPARED` record is verified against synthetic
  durable documents, not against a Goal actually created by a released schema-4 binary.
- `F4`'s operational impact under a real operator workflow was not measured.

## Regression Review

`/regression-review` was run on `25948de..e1d665c` and produced
`tmp/reviews/2026-09-30-user-visible-regression-report-a077187f.md` (recommendation `Discuss`;
Block 0, Discuss 3, Watch 2, Intentional 5).

Its three `Discuss` items were adjudicated independently:

- *the exhausted budget lost the Git diagnostic* -> real, my regression, **fixed** (`F6`).
- *approval preceded the budget check* -> real, my regression, **fixed** (`F6`).
- *schema 4 -> 5 can strand an in-flight managed Goal* -> **Intentional** (`F3`); it is the directed
  fail-closed behavior, not a regression.

Its `Watch` items were adjudicated: the prompt wording is pre-existing and shared, and the
zero-attempt refusal burning the lifecycle is a new guard on a genuine conflict, not a change to
previously working behavior.

Its headline positive was confirmed directly: `PRIMARY` Goals are byte-compatible, because the
`async` seam returns `NotManaged` before any await, so a primary Goal can never reach the approval
gate, and the schema bump only rewrites an on-disk integer that no MCP projection exposes.

## Receiving Handoff

- Handoff status: `Ready for receiving-code-review` (all in-scope findings already fixed under the
  existing authorization)
- Actionable finding IDs: `None`
- Accepted-with-rationale finding IDs: `F4`, `F7`
- Open question IDs: `F7`
- Highest-risk verification to repeat: the three Windows legs of any future matrix
- Chain rule: further work requires an explicit request
