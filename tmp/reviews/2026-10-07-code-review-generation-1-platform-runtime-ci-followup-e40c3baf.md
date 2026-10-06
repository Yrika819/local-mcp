# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261007-e40c3baf`
- Review chain ID: `rc-20261007-d40cb7aa`
- Review generation: `1`
- Review trigger: `post-implementation`
- Parent review report ID: `cr-20261007-d40cb7aa`
- Parent review report path: `tmp/reviews/2026-10-07-code-review-report-platform-runtime-ci-followup-d40cb7aa.md`
- Parent resolution ID: `rr-20261007-4ac797b9`
- Parent resolution path: `tmp/reviews/2026-10-07-receiving-code-review-resolution-platform-runtime-ci-followup-4ac797b9.md`
- Generated at: `2026-10-07T00:00:00Z`
- Report path: `tmp/reviews/2026-10-07-code-review-generation-1-platform-runtime-ci-followup-e40c3baf.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:c6108c3178c6e8fc26c2782653098ab6b178b6761101a673d1d1e4fbaf7ee3a0`

## Scope

- Review date: `2026-10-07`
- Scope kind: `file set`
- Scope description: Terminal review of the three CI follow-up paths: helper-only lint allowances, Windows real descendant helper/readiness witness, and deterministic test-only edge-top-up ordering.
- Scope mode: `implementation delta plus affected execution chains`
- Baseline: G0 `cr-20261007-d40cb7aa` and receiving resolution `rr-20261007-4ac797b9`
- Target: current worktree on `hardening/platform-runtime-closure-v1`
- Changed paths: `3`
- Diff size: `13 additions / 2 deletions` against the branch commit at G0 freeze
- Completion: `Complete within reviewed scope`
- Requirements consulted: user’s platform-runtime CI requirements; complete G0 report and resolution; Windows Job design; Replanner test contract and exact-edge-ceiling assertion.
- Prior resolution consulted: `rr-20261007-4ac797b9`
- Assumptions: native Windows runtime proof must come from the current branch CI; Replanner changes are test-only and do not revise any production limit or request-window design.
- Excluded as unrelated: production Replanner implementation and all previously reviewed process-runtime paths.

## Review Orchestration

- Assessment subagent: `Coordinator assessment - the bounded follow-up has separate Windows process-tree witness and deterministic fixture concerns.`
- Orchestration decision: `Parallel specialists`
- Decision confidence: `high`
- Decision rationale: independent Windows lifecycle and exact-edge fixture reasoning materially improve review coverage.
- Coordinator override: `None`
- Context or tool limits: Windows runtime cannot execute locally; the next GitHub Actions run is authoritative.

### Risk Dimensions

- Windows witness tests must prove the descendant has actually started and belongs to the Job before asserting teardown.
- Exact-edge tests must release sufficient dependency edges from the reserved trigger independent of random identifiers.
- Helper-only lint suppressions must not weaken linting of the main executable.

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `R1` | Windows descendant fixture | helper leader/leaf, ready marker, abnormal owner pipe | Job inheritance, marker ordering, timeout bounds, no false-positive EOF | Complete statically; Windows CI pending |
| `R2` | Test fixture determinism | Replanner edge top-up and exact-ceiling regression | random UUID ordering, >=3 trigger edges, active/history totals, no production change | Complete |
| `R3` | Lint-scope configuration | helper module-level attributes | allowances local to helper target, main target remains warning-clean | Complete |

### Synthesis Statement

The coordinator independently traced both helper processes and the test assertion path. The ready marker is required by the abnormal-owner parent before EOF counts as proof; the helper itself also checks for two active Job members before abort. Edge top-up now deterministically visits the reserved trigger first, and the regression asserts the three edges required by its decomposed replacement. No new finding remains; the native Windows result is still required.

## Review Snapshot

- Recommendation: `Discuss`
- Completion: `Complete within reviewed scope`
- Why now: the source/test fixture paths are coherent, but native Windows execution remains the sole uncovered area in this narrow follow-up.
- Must-review now: `A1` native Windows Job/descendant witness.
- Findings count: `Blocker 0 | Major 0 | Minor 0 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 0`
- Coverage confidence: `medium` static; `low` Windows runtime
- Biggest blind spot: Windows Job inheritance and KILL_ON_JOB_CLOSE execution.

## Complete Findings Index

No code-review findings identified in the reviewed scope.

## Blocker

None.

## Major

None.

## Minor

None.

## Questions

None.

## Test Gaps

None identified in this narrow delta. The Windows runtime area is marked Not covered rather than inferred from compilation or macOS tests.

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | Windows Job tree fixture and abnormal-owner witness | `src/platform_runtime_tests.rs` Windows module | `R1` | static trace | `Not covered` | Leader helper waits for a leaf-created readiness marker; abnormal-owner parent verifies marker and Job process count before abort. | Require the next native Windows CI run to prove inheritance, Job count, and EOF after owner death. |
| `A2` | Replanner exact-edge fixture | `src/replanner_compaction_tests.rs` | `R2` | data-flow/test trace | `Reviewed - no issue found` | Reserved trigger sorts first, then the test asserts it holds at least three edges; exact 1024 ceiling and post-replacement bound remain asserted. | 20/20 focused local repetitions passed. |
| `A3` | Linux helper lint scope | `src/bin/codex-linux-sandbox.rs` | `R3` | configuration trace | `Reviewed - no issue found` | Helper-local allowances cover only partially used included modules; main binary lint scope remains independent. | Current Linux CI quality job must confirm all-target clippy. |

## Subagent Candidate Adjudication

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `R1-C1` | `R1` | `dismissed` | `None` | Parent checks `ready.exists()`; helper checks Job active-process count >=2 before abort. | A vacuous abnormal-owner pass is no longer possible through the identified pre-start failure path. |
| `R2-C1` | `R2` | `dismissed` | `None` | Reserved-trigger sort plus explicit dependency-count assertion and 20 repeated passes. | Nondeterministic fixture ordering is removed without changing production Replanner behavior. |
| `R3-C1` | `R3` | `dismissed` | `None` | Helper module declarations carry local lint attributes; main has a separate crate root. | The lint allowance does not leak into the `local-mcp` target. |

## Evidence Appendix

### Verification Commands

- `cargo test --locked --bin local-mcp replanner_compaction_tests::a_replacement_at_the_exact_edge_ceiling_is_admitted_because_the_dead_task_releases_its_budget` -> 20/20 passed.
- `cargo test --locked --all-targets --quiet` -> two consecutive local parallel passes; each main target 1125/1125.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> passed locally.
- Full native Windows proof is pending the next branch CI run.

## Prior Resolution Reconciliation

No parent findings/test gaps were reported. The parent resolution’s `A1` Windows runtime item is retained as Not covered. No parent issue fingerprint was reopened.

## Receiving Handoff

- Handoff status: `Terminal post-review - return to user/owner`
- Automatic receiving permitted: `No`
- Source report ID: `cr-20261007-e40c3baf`
- Scope fingerprint to recheck: `sha256:c6108c3178c6e8fc26c2782653098ab6b178b6761101a673d1d1e4fbaf7ee3a0`
- Actionable finding IDs: `None`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `None`
- Deferred test-gap IDs: `None`
- Open question IDs: `None`
- Open coverage area IDs: `A1`
- Highest-risk verification to repeat: Windows x64 Job/process-tree test suite and complete 11-job CI.
- Suggested implementation boundaries: none absent failing Windows runtime evidence.
- Re-review note: `Generation 1 is terminal. Do not automatically invoke receiving-code-review.`
- Chain rule: `Generation 1 is terminal. Return remaining platform evidence requirement to the owner.`

## Report Self-Check

- `yes` Complete parent resolution was read and reconciled.
- `yes` All changed surfaces in this delta have A# coverage rows.
- `yes` No finding or test gap is unmatched.
- `yes` Windows runtime remains Not covered with an exact next check.
- `yes` Recommendation is Discuss because A1 is Not covered.
- `pending` Run the generation-1 validator against parent report and resolution.
- `yes` Git state was not mutated during review.
