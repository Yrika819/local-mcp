# Receiving Code Review Resolution

## Contents

1. [Report Contract](#report-contract)
2. [Scope](#scope)
3. [Disposition Summary](#disposition-summary)
4. [Finding Dispositions](#finding-dispositions)
5. [Test Gap Dispositions](#test-gap-dispositions)
6. [Question Disposition](#question-disposition)
7. [Fix-Introduced Findings](#fix-introduced-findings)
8. [Verification](#verification)
9. [Remaining Risk](#remaining-risk)
10. [Self-Check](#self-check)

## Report Contract

- Report type: `receiving-code-review`
- Resolution ID: `rr-20260930-mwp3b41d08`
- Source review report ID: `cr-20260930-mwp3a7c2e1`
- Source review report path: `tmp/reviews/2026-09-30-code-review-report-mwp3-gen0.md`
- Review chain ID: `rc-20260930-mwp3d94f18`
- Resolved at: `2026-09-30T08:55:00Z`
- Resolution path: `tmp/reviews/2026-09-30-code-review-resolution-mwp3.md`
- Git mutation during resolution: `commits 3f462f6, 4457d3c, 46b02a1 on managed-worktrees/v1-phase3-creation-authority`
- Authorization basis: the Phase 3 task contract authorizes a bounded post-implementation review and fixes to in-scope findings

## Scope

- Baseline reviewed: `ce3f354..a4bb321`
- Fix commits: `3f462f6`, `4457d3c`, `46b02a1`
- Generation-0 recommendation: `Changes requested`
- Generation-0 finding count: `Blocker 0 | Major 2 | Minor 9 | Question 1`, plus `1 Major / 2 Minor` test gaps
- All fixes are confined to the Phase 3 creation-authority surface; no Phase 4 routing, cleanup, or publication behavior was added

## Disposition Summary

| ID | Severity | Disposition | Fix commit |
| --- | --- | --- | --- |
| `F1` | Major | Fixed | `3f462f6` |
| `F2` | Major | Fixed, then re-fixed after the first attempt was found to be weaker and platform-broken | `3f462f6`, `4457d3c` |
| `F3` | Minor | Fixed by correcting the claim, not the code | `3f462f6`, `4457d3c` |
| `F4` | Minor | Fixed | `3f462f6` |
| `F5` | Minor | Fixed | `3f462f6` |
| `F6` | Minor | Fixed | `3f462f6` |
| `F7` | Minor | Fixed | `3f462f6` |
| `F8` | Minor | Fixed | `3f462f6` |
| `F9` | Minor | Fixed, then re-fixed after the first fix produced a canonical/non-canonical fixture | `3f462f6`, `46b02a1` |
| `F10` | Minor | Fixed | `3f462f6` |
| `F11` | Major | Fixed | `bb8c375` |
| `F12` | Major | Fixed | `4457d3c` |
| `F13` | Minor | Fixed | `4457d3c` |
| `F14` | Question | Left open for the product owner | - |
| `T1` | Major | Fixed | `4457d3c` |
| `T2` | Minor | Fixed | `4457d3c` |
| `T3` | Minor | Deferred with rationale | - |

## Finding Dispositions

### F1 Major - `goal_run` created a worktree for a cancelled managed Goal

**Accepted.** The coordinator reproduced it before fixing: a CANCELLING managed Goal reached `Active { head: ... }` with `creator.calls() == 1`.

The preparation seam ran before the runner's own control-state gate, so `goal_run` persisted `PREPARED`, executed `git worktree add --lock -b ...`, and committed `ACTIVE` for a Goal the existing runner contract returns from with zero steps.

Fixed in two layers. `prepare_managed_workspace_before_run` now applies `stop_for_state` first and returns the identical result the runner produced before, so no control-state or terminal Goal can reach preparation. `Goal::blocks_foreground_run` then enforces the same precondition inside `prepare_managed_workspace`, so the property holds for any future caller rather than only for the current one. The regression test failed before the fix and passes after.

### F2 Major - Creation resolved Git by bare name through `PATH`

**Accepted.** `Command::new("git")` on a *mutating* command, with only `GIT_*` scrubbed, was weaker than the identity rule the repository already applies to its only other mutating Git path.

The first fix attempt (`3f462f6`) hardened it in place but hand-rolled a second resolver. The generation-1 re-review of that fix found it diverged from the established resolver in five ways, one of which (`F12`) broke Windows outright. The final fix (`4457d3c`) removes the duplicate entirely: the seam calls `execution::host_git_path()`, so creation runs the same identity as `git_stage_paths` - same platform file names, same eager `PATH` validation, same skip-not-fail semantics, same UTF-8 requirement, same canonicalization, resolved once per process. An explicitly configured absolute executable is honored only if it is that identity.

Environment sanitization additionally removes the loader and Git-config-discovery variables (`LD_*`, `DYLD_*`, `XDG_CONFIG_HOME`, `HOME`, `APPDATA`).

### F3 Minor - The argv self-check could not detect a widened global-option head

**Accepted, and the finding's own premise was refined twice.** The first fix compared the whole argv, but built the expectation from the same constant the builder splices in, so the head comparison was `X ++ tail == X ++ tail` - a tautology. The re-review caught that the second version made a false claim.

Final state: the self-check keeps its genuine value - catching an inserted, removed, or reordered argument in the tail, which is what makes `-B`/`--force` impossible to express - and the comment now states plainly that it does **not** police the head, and names the literal expected-argv test that does. The fix corrects a documentation over-claim rather than pretending to add a mechanism.

### F4 Minor - The frozen retry predicate was bypassed

**Accepted.** The retry arm now routes on `Reconciliation::permits_bounded_retry()`. The guard arm is ordered first, immediately after `ActiveExact`, so no recovery-required classification can be shadowed; the re-review independently confirmed all thirteen non-retryable states still reach `MANAGED_RECOVERY_REQUIRED`.

### F5 Minor - Git's diagnostic was discarded and `stop_detail` was null

**Accepted.** `describe_creation_attempt` now records a bounded evidence string (400 chars of stderr, 800 overall) into the blocker detail, and `mcp::stop_detail` emits a `MANAGED_WORKSPACE_BLOCKED` object under the existing 8 KiB bound. The doubly prefixed stop-reason name was also corrected, since every managed code already starts with `MANAGED_`. The re-review confirmed the bound is applied twice, that nothing sensitive is carried, and that no in-repo consumer reads the stop-reason string.

### F6 Minor - The `plan_revision` guard ran after the durable commit

**Accepted.** Both `activate` and `block_lifecycle` now assert post-commit and consistently. The hard post-commit `Err` - which would have left durable `ACTIVE` state alongside an error return - is gone.

### F7 Minor - The overlap guard omitted the repository common directory

**Accepted.** The guard now rejects overlap in either direction against `repository_common_dir`, which covers design invariant 8 for a session whose cwd is itself a linked worktree. A test models exactly that host state, and the rejection is documented at the environment variable and in `SECURITY.md`.

### F8 Minor - Assertions that could not fail

**Accepted.** The unconditional `remotes.is_empty() || !porcelain.contains("push")` became a real state assertion plus a `git config --local --list` scan for `remote.`, `branch.`, `push`, and `url.`; the disjunctive spawn-failure assertion became an exact code, an exact call count equal to `MAX_CREATION_ATTEMPTS`, and a check that Git's own failure reached the evidence; the misnamed concurrency test was renamed to what its body does; the dead `Arc::strong_count` call was removed. A later `noncanonical` helper the first fix introduced was also found to be building a nonexistent path on any non-`/tmp` host and was replaced.

### F9 Minor - The fixture built a non-production-shaped session cwd

**Accepted, and the first fix was itself defective.** Canonicalizing only `session.cwd` desynchronized it from `primary` and `managed_root` on any host with a non-canonical temp dir. Branch CI proved it: macOS and Windows failed with `managed-worktree primary_root must remain the durable Goal.cwd`. `46b02a1` canonicalizes the fixture root once and derives every path from it, and the synthetic record test builds paths from that root instead of a hardcoded POSIX prefix.

Reproduced and verified locally: with `TMPDIR` pointed at a symlink to a real directory - exactly the macOS runner condition - the managed suite passes 149/149.

### F10 Minor - Operator and public surface disclosure

**Accepted.** `SECURITY.md` gained a paragraph covering the host-managed root, its authority prerequisite, the single host Git mutation, the bounded retry, the fact that a non-advanced Goal is not prepared, and that Windows classification is unchanged. The `goal_run` description discloses that a `MANAGED_WORKTREE` Goal creates a linked worktree and local branch. The re-review caught three over-claims in the first draft of that paragraph - calling the mode "isolation policy", calling the identity "validated", and calling it "exactly one" mutation while a retry exists - and all three were corrected.

### F11 Major - A `REQUESTED` workspace with no intent was rejected as corrupt

**Accepted** and fixed in `bb8c375`, before the review was commissioned but recorded here because it changed the reviewed baseline. `requires_creation_intent` was restricted to `Prepared`, matching design section 5's ordering and making an authority denial recoverable. Without this the entire Phase 3 flow was unimplementable: 33 tests failed at `goal_start`.

### F12 Major - Host Git resolution could not find Git on Windows

**Accepted.** Introduced by the `F2` fix and found by the generation-1 re-review. Resolved by reuse rather than repair, per `4457d3c`.

### F13 Minor - A blocked managed record reported a generic control-state code

**Accepted.** `MANAGED_GOAL_TERMINAL` is now separate from `MANAGED_GOAL_CONTROL_STATE`, and the control-state refusal moved to after the managed lifecycle classification so the more specific explicit-recovery code still wins. It remains before the authority gate, so no durable evidence is written for a Goal that will not proceed.

## Test Gap Dispositions

### T1 Major - the control-state test did not drive the production seam

**Accepted.** The first regression test called `prepare_managed_workspace` directly, which production never reaches in a control state because the seam returns first - so it proved a defence that production does not use, and the actually-fixed function was untested. `prepare_managed_workspace_before_run` is now exercised directly, asserting the runner's own stop reason, zero steps, and no worktree or branch.

### T2 Minor - revision-CAS serialization lost its named coverage

**Accepted.** The renamed test no longer claimed the property, so a stale-writer test was added that drives two `mutate_goal_snapshot` calls at the same revision and asserts the second is refused with a revision conflict while the lifecycle stays `PREPARED`.

### T3 Minor - retry budget durability across process restarts

**Deferred with rationale.** The budget is per-preparation, so a process killed mid-attempt grants a fresh budget on the next `goal_run`. Making it durable requires a new persisted field on `ManagedWorktreeRecord`, which changes the Phase 1 frozen record shape. The blast radius is bounded: every additional attempt must first be *proven* to have left no side effect, and any attempt that succeeds is adopted as `ACTIVE` so it is never repeated. Recorded as `MANAGED_CREATION_ATTEMPTS` per-preparation in the code and reported to the product owner rather than resolved unilaterally.

## Question Disposition

### F14 Question - Should the host-internal creation mutation be approval-gated?

**Left open for the product owner.** The frozen design specifies the command and host ownership of lifecycle but is silent on approval, and no existing Goal tool is approval-gated, so the current behavior is consistent with the existing product. This is a product-boundary decision, not a defect, and the coordinator did not want to invent approval semantics that the frozen design does not describe. `SECURITY.md` now states the boundary the code actually enforces, without claiming approval or sandboxing that does not exist.

Settlement criterion: an explicit product decision stating whether host-owned Goal lifecycle mutations are inside or outside the approval model.

## Fix-Introduced Findings

The generation-1 re-review of the first fix commit found defects that the fix itself introduced. Both are recorded because they changed the resolution:

| ID | Severity | Introduced by | Found by | Resolution |
| --- | --- | --- | --- | --- |
| `F12` | Major | `3f462f6` | `R3-N1` | Reuse the established resolver (`4457d3c`) |
| `F9` (recurrence) | Minor | `3f462f6` | branch CI `36687406442` | Canonicalize the whole fixture (`46b02a1`) |

`R3-N11` also showed the first `assert_exact_shape` rewrite kept the head comparison tautological, which is recorded under `F3` rather than as a new finding.

## Verification

| Check | Result |
| --- | --- |
| `cargo fmt --check` | clean |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | clean |
| `cargo test --locked --all-targets` | 836 passed, 0 failed |
| `cargo test --locked --all-targets managed_worktree` | 149 passed, 0 failed |
| Managed suite with `TMPDIR` pointed at a symlink (the macOS runner condition) | 149 passed, 0 failed |
| `git diff --check` | clean |
| Branch CI `36687406442` (the run these fixes address) | red on `test (windows-x64)`, `test (macos-arm64)`, `test (macos-intel)`; first causal error diagnosed as the fixture defect above |
| `a_managed_goal_in_a_control_state_does_not_reach_git` before the fix | FAILED with `Active { head: ... }`, proving `F1` |
| `a_failure_to_start_git_is_reconciled...` after the fix | asserts an exact block code, an exact call count, and that Git's failure reached the evidence |

Not fixed here, and deliberately so:

- `phase0_tests::mcp_contract::job_running_completion_and_completed_poll_are_frozen` failed on the macOS CI leg with "released background job must complete within the bounded poll". It is a pre-existing load-sensitive timing test in code this branch does not touch, it is green on `main` CI, and it passes locally on repeated runs. Changing it would be an unrelated fix to a test outside the authorized scope.
- `T3` and `F14` remain open by the rationales above.

## Remaining Risk

1. **Windows execution is still unverified for the fixed code.** `F12` was diagnosed from the platform list and the CI failure signature, then fixed by reuse rather than by a new platform guess. The re-run Windows legs are the authority.
2. **`T3`**: repeated process restarts can grant a fresh creation budget. Bounded by proven no-side-effect results; not durable.
3. **`F14`**: the approval boundary for host-internal lifecycle mutations is unconfirmed.
4. **Two pre-existing timing-sensitive test groups** (`phase0_tests::sandbox_contract`, `phase0_tests::mcp_contract` job polling) fail intermittently under CI load. Untouched by this branch, but they make a red CI run ambiguous unless the failing test names are read.
5. **The controlled prediction is retained, not proven:** the Phase 4 handoff must revisit whether the eligibility gate should re-run on `PREPARED` recovery, and whether the control-state gate ordering still holds once a managed Goal can reach non-`Planning` states.

## Self-Check

- `yes` Every generation-0 finding has an explicit disposition.
- `yes` Every disposition states the fix commit or the deferral rationale.
- `yes` Fix-introduced findings are recorded separately and were not presented as pre-existing.
- `yes` The untested-out-of-scope change is named explicitly rather than silently omitted.
- `yes` Verification distinguishes local evidence from CI evidence, and no green CI is claimed.
- `yes` Git mutations are confined to the authorized Phase 3 branch; `main` was not modified.
