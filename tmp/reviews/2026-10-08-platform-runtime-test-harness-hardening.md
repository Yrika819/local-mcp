# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261008-7bc91a`
- Review chain ID: `rc-20261008-7bc91a`
- Review generation: `0`
- Review trigger: `initial`
- Parent review report ID: `None`
- Parent review report path: `None`
- Parent resolution ID: `None`
- Parent resolution path: `None`
- Generated at: `2026-10-07T22:34:00Z`
- Report path: `tmp/reviews/2026-10-08-platform-runtime-test-harness-hardening.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:6a2786d62dcef4bdb9f568c93305984b442d91b7624e11b3f1a0925fe52ee161`

This report covers the test-harness hardening only. It does not declare Failure A fixed: its status remains `NOT REPRODUCED`.

## Scope

- Review date: `2026-10-08`
- Scope kind: `working tree`
- Scope description: Final review of the four uncommitted process-test changes on `hardening/platform-runtime-parallel-stability-v1` against baseline `d5b848f1a24b5310a1876b39975cef1ac670ea4c`. The scope is B's FIFO/readiness witness, A's test-only readiness and failure diagnostics, test-only process evidence, and the process-group stress fixture's external-kill cleanup boundary.
- Scope mode: `full frozen scope`
- Baseline: `d5b848f1a24b5310a1876b39975cef1ac670ea4c`
- Target: working tree at `d5b848f1a24b5310a1876b39975cef1ac670ea4c` plus four uncommitted source diffs
- Changed paths: `4` source paths; this report is a separate tracked-review-class artifact
- Diff size: `440 additions / 50 deletions`
- Completion: `Complete within reviewed scope`
- Requirements consulted: Current user instructions §§2–11 and the preceding Failure A/B diagnostic contract; relevant process runner, process-group, and test code.
- Prior resolution consulted: `None`
- Assumptions: Failure A's historical root cause is not established. macOS runtime evidence does not prove Windows or Linux process semantics. The final full-suite run requires `target/debug/atomic-publish` to be built first.
- Excluded as unrelated: Managed Worktrees Phase 5 Slice 1 files and contract; production runtime changes; merge to main.

## Review Orchestration

- Assessment subagent: `Coordinator assessment - the cohesive change spans distinct FIFO lifetime proof, timeout/readiness ordering, and process ownership diagnostics; independent bounded reviewers improve adversarial coverage.`
- Orchestration decision: `Parallel specialists`
- Decision confidence: `high`
- Decision rationale: B's filesystem-witness protocol and A's process-group/test-only instrumentation have independent failure modes, so two read-only specialist passes were proportionate. Coordinator independently checked both candidates against the code and actual validation results.
- Coordinator override: `None`
- Context or tool limits: macOS host only; no Windows/Linux runtime claims are made. GitHub CI is reported separately from this pre-commit review.

### Risk Dimensions

- FIFO EOF must not be accepted as readiness when no writer has ever opened the FIFO.
- A readiness handshake must prove the intended descendant reached the writer-open point before a cleanup verdict.
- Test-only evidence must not change production signal, deadline, or cleanup behavior.
- Test-owned process cleanup must not rely on PID liveness or stale process-group identifiers.
- External test-harness termination bypasses Rust destructors and can leave fixture residue.

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `R1` | B lifetime witness and fixture startup | `src/phase0_sandbox_tests.rs` B helper/test | FIFO semantics, ready-marker ordering, stdio inheritance, PGID cleanup, zombie/PID reuse | Complete; one Minor candidate |
| `R2` | A diagnostics, process semantics, and external-kill boundary | `src/platform_runtime_tests.rs`, `src/process_blocking.rs`, `src/process_group_stress_tests.rs` | `cfg(test)` isolation, timeout semantics, ownership proof, failure-only inspection, Drop boundary | Complete; one Minor candidate |

### Synthesis Statement

The coordinator re-read the relevant source paths, compared the working-tree diff to the baseline, and checked both specialist candidates. The FIFO writer/ready marker ordering and test-only process evidence are consistent with the intended test contract. Two Minor findings are retained as non-blocking follow-ups: B lacks an independent watchdog around its spawned runner join, and the external-kill comment overstates that a PGID is durably recorded. No Major or Critical finding remains. Failure A remains `NOT REPRODUCED`, not resolved.

## Review Snapshot

- Recommendation: `Pass with caveat`
- Completion: `Complete within reviewed scope`
- Why now: The test-only change is bounded and the requested macOS focused/full-suite evidence is green, with two non-blocking test-maintenance caveats and Failure A still explicitly unresolved.
- Must-review now: `F1` B runner-task watchdog; `F2` external-kill identity wording
  1. `F1` A regression in the bounded runner could leave the test awaiting its task indefinitely.
  2. `F2` The comment's “recorded test-owned group identity” is not durably recorded across an external kill.
- Findings count: `Blocker 0 | Major 0 | Minor 2 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 0`
- Coverage confidence: `high` for the macOS tested path; `medium` for cross-platform interpretation because native Windows/Linux runs are delegated to CI.
- Biggest blind spot: Failure A's historical FIFO-pending failure remains unexplained and was not reproduced in this verification set.

## Complete Findings Index

| ID | Severity | Surface | Review risk | Confidence | Origin | Verification | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `F1` | `Minor` | B bounded-runner test | A runner future regression could hang this focused test instead of yielding a bounded failure. | `high` | `R1` | Static async control-flow trace; final focused and full-suite runs | `behavior; entry=bounded descendant cleanup test; contract=runner completion remains bounded and test reports failure; effect=focused test hangs when runner task fails to return` | `ifp-sha256:e7acfe994e1211a91f9d42fcce5145a86b16688df9eb891f4262aa6b316a0ff8` | `kind:public-contract; strength:authoritative; evidence:user validation contract requires bounded process-test execution; src/sandbox.rs exposes a bounded runner with deadline and cleanup grace` |
| `F2` | `Minor` | External harness-kill cleanup documentation | The comment implies a durable PGID record exists although the guards hold identity only in memory. | `high` | `R2` | Static trace of `FixtureRoot`, `UnrelatedGroup`, and `MeasuredGroup` ownership; prior orphan evidence | `behavior; entry=external test-harness termination cleanup guidance; contract=only claim durable identities that survive Drop; effect=operator cannot safely identify orphan process group after harness kill` | `ifp-sha256:b4833581406f3b73a33708a248745783de62ebbbdb3e466922c2025524db1d47` | `kind:owner-decision; strength:authoritative; evidence:current user §4 distinguishes Drop cleanup from external termination and requires residuals to be classified without weakening ownership proof` |

## Blocker

None.

## Major

None.

## Minor

### F1 Minor - Bound the spawned runner join in B

Impact: Test availability and bounded diagnostic behavior.
Review reason: `bounded_clean_runner_kills_descendant_retaining_pipes` waits for readiness with a timeout, but then awaits the runner `JoinHandle` without an independent outer timeout. A future regression that prevents the runner future from completing could hang this test rather than fail it.
Surface: B test in `src/phase0_sandbox_tests.rs`.
Issue key: `behavior; entry=bounded descendant cleanup test; contract=runner completion remains bounded and test reports failure; effect=focused test hangs when runner task fails to return`
Issue fingerprint: `ifp-sha256:e7acfe994e1211a91f9d42fcce5145a86b16688df9eb891f4262aa6b316a0ff8`
Expected basis: `kind:public-contract; strength:authoritative; evidence:user validation contract requires bounded process-test execution; src/sandbox.rs exposes a bounded runner with deadline and cleanup grace`
Confidence: `high`
Origin: `R1-C1`
Coordinator verification: The task is awaited after the readiness timeout and no watchdog wraps that await. The production runner currently returns within its documented run/cleanup bounds in all executed matrices; this finding concerns protection against a future regression.

Look here first:
- [`B runner join`](../../src/phase0_sandbox_tests.rs#L301)
- [`bounded runner deadline`](../../src/sandbox.rs#L703)

Failure mode:
- Expected: A broken or stuck runner task produces a bounded, attributable test failure.
- Current: Readiness polling is bounded, but the subsequent task join has no test-level bound.

Evidence:
- The static path at `src/phase0_sandbox_tests.rs:301-313` awaits the spawned task directly.
- Final tests passed; no hang was observed.

Assumptions and limits:
- The runner implementation is currently deadline-bounded; the test watchdog would protect against a future regression in that contract.

Reviewer action:
`defer a narrow watchdog follow-up; do not weaken FIFO EOF assertion`

### F2 Minor - Make external-kill identity guidance accurate

Impact: Safe operational cleanup after a test executable is externally killed.
Review reason: Rust `Drop` guards keep process-group identifiers in memory. They cannot durably record those identifiers after SIGKILL or machine failure, so the comment's cleanup instruction overstates available evidence.
Surface: External-kill residual comment in `src/process_group_stress_tests.rs`.
Issue key: `behavior; entry=external test-harness termination cleanup guidance; contract=only claim durable identities that survive Drop; effect=operator cannot safely identify orphan process group after harness kill`
Issue fingerprint: `ifp-sha256:b4833581406f3b73a33708a248745783de62ebbbdb3e466922c2025524db1d47`
Expected basis: `kind:owner-decision; strength:authoritative; evidence:current user §4 distinguishes Drop cleanup from external termination and requires residuals to be classified without weakening ownership proof`
Confidence: `high`
Origin: `R2-C1`
Coordinator verification: `FixtureRoot`, `UnrelatedGroup`, and `MeasuredGroup` keep ownership in process memory. The code does not persist PGIDs for recovery after hard termination.

Look here first:
- [`external-kill comment`](../../src/process_group_stress_tests.rs#L36)

Failure mode:
- Expected: Documentation states that cleanup after external termination requires a fresh mechanical ownership proof, or that no durable identifier is available.
- Current: The comment says cleanup can use a “recorded test-owned group identity,” but no such durable record is written.

Evidence:
- The previously confirmed `r12-0` through `r12-7` fixture residue was attributable by PID/PPID/PGID, cwd UUID root, open script, and process command after the harness had terminated. Those observations are post-event evidence, not a durable record emitted by the `Drop` guards.

Assumptions and limits:
- No test-owned process was live at final audit. No unrelated process was signalled.

Reviewer action:
`defer wording correction to a small follow-up; retain mechanical per-process ownership checks`

## Questions

None.

## Test Gaps

None.

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | B readiness/lifetime witness | `src/phase0_sandbox_tests.rs` | `R1` | `dependency trace; runtime verified` | `Finding F1` | FIFO EOF is cleanup-only; ready marker follows successful child spawn/pre-exec writer byte | Consider runner join watchdog follow-up; no PID or zombie liveness verdict remains in this target test. |
| `A2` | A readiness-before-verdict and diagnostics | `src/platform_runtime_tests.rs` | `R2` | `contract trace; runtime verified` | `Reviewed - no issue found` | A's descendant emits FIFO marker and separate pipe handshake before `spawn_ready` returns; diagnostic process inspection is failure-only | Keep A status `NOT REPRODUCED`; do not infer root-cause closure. |
| `A3` | Test-only process evidence | `src/process_blocking.rs` | `R2` | `dependency trace; runtime verified` | `Reviewed - no issue found` | Evidence struct/thread-local and capture calls are `cfg(all(test, unix))` / `cfg(test)`; non-test `cargo check` passes | No production cleanup/deadline semantic change found. |
| `A4` | External test-harness termination boundary | `src/process_group_stress_tests.rs` | `R2` | `contract trace` | `Finding F2` | Drop runs on normal return/unwind/owned-value drop, not SIGKILL/tool hard-timeout/machine crash | Correct wording in a narrow follow-up; no claim that this is a production containment defect. |
| `A5` | Fixture lifetime and cross-platform claims | B helper, A helper, process-group stress fixtures | `Coordinator` | `contract trace; runtime verified` | `Reviewed - no issue found` | B/A use `sleep 30`; process stress uses `sleep 300` so fixtures outlive tested run bounds; no fixture lifetime change made | Revisit shortening long-lived stress fixtures separately if external hard-kill residue reduction is desired. |

## Subagent Candidate Adjudication

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `R1-C1` | `R1` | `accepted` | `F1` | Direct code trace confirms the readiness timeout does not bound the later `JoinHandle` await. | A future stuck runner can hang this focused test; impact is non-blocking because current runner and repeated suites complete. |
| `R2-C1` | `R2` | `accepted` | `F2` | Direct ownership trace confirms PGIDs live in guards and are not written to durable storage. | Existing wording is operationally inaccurate after external termination, but no process is signalled by that comment itself. |

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `src/phase0_sandbox_tests.rs` | `test-only` | B readiness, FIFO writer ownership, inherited pipes, cleanup verdict, fixture lifetime |
| `src/platform_runtime_tests.rs` | `test-only` | A readiness sequencing, FIFO result, failure-only diagnostics |
| `src/process_blocking.rs` | `test-only` | `cfg(test)` thread-local evidence and unchanged signal/deadline behavior |
| `src/process_group_stress_tests.rs` | `docs-only` | Drop lifecycle, external hard-kill residual, fixture maximum lifetime |
| `tmp/reviews/2026-10-08-platform-runtime-test-harness-hardening.md` | `review report` | Review-chain artifact; tracked review reports exist in the repository, so this new report is included in the explicit commit path. |

### Verification Commands

- `cargo fmt --all -- --check` -> passed on the final source tree.
- `CARGO_TARGET_DIR=target cargo clippy --locked --all-targets --all-features -- -D warnings` -> passed.
- `git diff --check` -> passed.
- `CARGO_TARGET_DIR=target cargo check --locked --bin local-mcp` -> passed for the non-test production target; process-evidence state/functions are test-gated.
- B focused -> `50/50`; A focused -> `50/50`.
- A+B simultaneous -> `20/20`.
- Unix runtime suite -> `20/20`; sandbox contract suite -> `20/20`; process-group ownership/stress -> `10/10`.
- Combined A/B + Unix runtime + sandbox + process-group stress -> `10/10` rounds.
- Retained final repetition summaries: `runtime-focused-repetitions-final.log`, `runtime-parallel-repetitions-final.log`, `runtime-suite-repetitions-final.log`, `runtime-combined-stress-final.log` (untracked evidence only; excluded from commit).
- `cargo build --locked --bin atomic-publish` -> passed; required helper existed at `target/debug/atomic-publish` before the final all-targets gate.
- `CARGO_TARGET_DIR=target cargo test --locked --all-targets --quiet` -> final run #1 passed, `1127/1127`.
- Same default-parallel command -> final run #2 passed, `1127/1127`.
- One earlier full-suite attempt failed because `target/debug/atomic-publish` had not been built; the exact helper-dependent test then passed after the required helper build. That setup failure was not counted as a pass.
- Failure A disposition -> `NOT REPRODUCED`; no A root cause or production fix is claimed.

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `F1` | `entry` | [`B test`](../../src/phase0_sandbox_tests.rs#L281) | The readiness and cleanup witness sequence under review. |
| `F1` | `risk` | [`runner await`](../../src/phase0_sandbox_tests.rs#L301) | The runner task is awaited without a separate test watchdog. |
| `F2` | `risk` | [`external-kill boundary`](../../src/process_group_stress_tests.rs#L36) | The guidance refers to an identity that is not durably persisted. |

### Dismissed Coordinator Candidates

| Candidate | Decision | Evidence |
| --- | --- | --- |
| B FIFO EOF can be mistaken for startup readiness | dismissed | The test waits for the helper-created marker, which is written only after child spawn succeeds; FIFO EOF is read only after runner completion. |
| B still depends on PID/zombie state | dismissed | The target descendant-retaining-pipes test uses a UUID FIFO and contains no PID liveness probe. Other unrelated sandbox tests still use their own PID checks. |
| A test changes production deadline or group signaling | dismissed | A readiness hook and process evidence are test-only; `cargo check --locked --bin local-mcp` passed, and the production signal operation remains the same group `SIGKILL`. |
| A is resolved by the readiness change | dismissed | No original FIFO-pending A failure reproduced; final status remains `NOT REPRODUCED`. |
| `sleep 300` proves an external-kill production containment defect | dismissed | The guards' inability to run after external death is a test-fixture cleanup limitation, not a production runtime containment result. |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A2` | Historical Failure A root cause was not reproduced. | The tests now distinguish readiness from cleanup but cannot establish why the earlier FIFO-pending report occurred. | Capture the failure with the new failure-only evidence; retain `NOT REPRODUCED` until then. |
| `A5` | Windows/Linux native runtime was not executed on this macOS host. | Cross-platform behavior must come from the branch's CI jobs, not Unix-only test skip behavior. | Inspect every CI job, especially platform-specific jobs, before handoff. |

## Prior Resolution Reconciliation

None - initial review generation.

## Receiving Handoff

- Handoff status: `Ready for receiving-code-review`
- Automatic receiving permitted: `No`
- Source report ID: `cr-20261008-7bc91a`
- Scope fingerprint to recheck: `sha256:6a2786d62dcef4bdb9f568c93305984b442d91b7624e11b3f1a0925fe52ee161`
- Actionable finding IDs: `None`
- Deferred finding IDs: `F1, F2`
- Actionable test-gap IDs: `None`
- Deferred test-gap IDs: `None`
- Open question IDs: `None`
- Open coverage area IDs: `None`
- Highest-risk verification to repeat: `Re-run default-parallel focused and full suites if either deferred finding is changed; inspect all 11 CI jobs for this commit before strict-FF recommendation.`
- Suggested implementation boundaries: `If addressed, add only a bounded task-join watchdog in B and correct the external-kill wording; do not touch production process behavior or Slice 1.`
- Re-review note: `Treat every finding as a claim to verify. Challenges require a counterclaim, argument, evidence, limits, and settlement criterion.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or product owner.`

## Report Self-Check

- `yes` Actual assessment mode and rationale are recorded: parallel specialists were selected for the independent B and A/process semantics angles.
- `yes` Every changed review-relevant or unknown-impact area appears once in `Review Coverage Ledger`.
- `yes` Every final finding appears once in the index and once as a matching card.
- `yes` Every `Finding F#` area references an existing finding.
- `yes` Every standalone test gap has a stable ID and severity; none were identified.
- `yes` Every `F#` has a unique semantic fingerprint and authoritative expected basis.
- `yes` Generation, trigger, parent resolution, scope mode, and receiving handoff satisfy the bounded chain contract.
- `yes` Generation 0 has no prior-resolution dispositions to reconcile.
- `yes` Every non-Question finding is partitioned once into deferred IDs; no Questions or Not-covered areas remain open.
- `yes` Every meaningful subagent candidate is adjudicated.
- `yes` No Not-covered area remains.
- `yes` Recommendation is `Pass with caveat` because two Minor findings remain.
- `yes` Validator passes; Git state was not mutated during review.
