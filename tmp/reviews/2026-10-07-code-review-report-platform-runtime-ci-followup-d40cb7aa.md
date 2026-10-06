# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261007-d40cb7aa`
- Review chain ID: `rc-20261007-d40cb7aa`
- Review generation: `0`
- Review trigger: `initial`
- Parent review report ID: `None`
- Parent review report path: `None`
- Parent resolution ID: `None`
- Parent resolution path: `None`
- Generated at: `2026-10-07T00:00:00Z`
- Report path: `tmp/reviews/2026-10-07-code-review-report-platform-runtime-ci-followup-d40cb7aa.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:c6108c3178c6e8fc26c2782653098ab6b178b6761101a673d1d1e4fbaf7ee3a0`

## Scope

- Review date: `2026-10-07`
- Scope kind: `file set`
- Scope description: Narrow CI follow-up changes after run `37520685010`: the Linux sandbox helper's lint expectation scope; Windows process-tree test fixture and ready-witness; deterministic test-only Replanner edge-top-up ordering after a parallel suite exposed UUID-order dependence.
- Scope mode: `full frozen scope`
- Baseline: `hardening/platform-runtime-closure-v1` commit `c52c01c271e85d2df148fdcc756c11a31a59e934`
- Target: current working tree
- Changed paths: `3`
- Diff size: `13 additions / 2 deletions`
- Completion: `Complete within reviewed scope`
- Requirements consulted: CI run `37520685010` failed logs; Platform Runtime Closure V1 instructions; previous terminal code review and receiving-resolution records; existing Replanner compaction fixture contract.
- Prior resolution consulted: `None`
- Assumptions: no production Replanner behavior is changed; Windows runtime proof must come from Windows CI.
- Excluded as unrelated: all production process/sandbox/Git code and any Replanner production code.

## Review Orchestration

- Assessment subagent: `Coordinator assessment - three narrow test/lint files, one Windows-only process fixture, one random-order test fixture.`
- Orchestration decision: `Parallel specialists`
- Decision confidence: `high`
- Decision rationale: independent Windows process inheritance and test-fixture determinism need different evidence.
- Coordinator override: `None`
- Context or tool limits: Windows runtime is unavailable locally; native Windows CI is the required execution proof.

### Risk Dimensions

- Windows helper-process tests must prove real Job membership and must not pass vacuously when the descendant never starts.
- Random task identifiers must not determine whether an exact-edge-ceiling regression fixture satisfies its own preconditions.
- Shared modules included into the Linux helper must preserve lint expectations without weakening the main executable's lint policy.

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `R1` | Windows process witness | Windows helper leader/leaf, readiness marker, inherited stdout | descendant joins same Job; readiness precedes owner death and cleanup assertion; bounded helper lifetime | Complete statically; Windows CI pending |
| `R2` | Test fixture determinism | Replanner compaction edge-top-up fixture | random UUID ordering, exact edge ceiling, replacement edge count, no production behavior change | Complete statically |
| `R3` | Linux helper lint scope | `src/bin/codex-linux-sandbox.rs` shared module attributes | helper-only partial use must not suppress main target lint expectations | Complete statically |

### Synthesis Statement

The coordinator verified the edge-ordering precondition and current readiness assertion. The previous Windows reviewer candidate about a vacuous abnormal-owner test is addressed in this target: the parent now checks the descendant-ready marker, and the helper confirms at least two Job processes before aborting. This remains static review until Windows CI. The lint attributes are confined to the helper's included modules.

## Review Snapshot

- Recommendation: `Discuss`
- Completion: `Complete within reviewed scope`
- Why now: no new defect is present in the narrow follow-up, but Windows runtime must exercise the new helper fixture and Job behavior.
- Must-review now: `A1` Windows helper runtime.
- Findings count: `Blocker 0 | Major 0 | Minor 0 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 0`
- Coverage confidence: `medium` static, `low` Windows runtime
- Biggest blind spot: Windows CI execution of the helper process tree.

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

None identified in this narrow delta; native Windows execution is an uncovered platform area, not a separate missing test in this file-set scope.

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | Windows tree helper and readiness witness | `src/platform_runtime_tests.rs` Windows module | `R1` | static path trace | `Not covered` | Leader helper launches a leaf helper, waits for a ready marker, and the owner test checks Job membership before abort; parent verifies the marker before treating EOF as owner-death evidence. | Run Windows x64 CI to establish Job inheritance and KILL_ON_JOB_CLOSE at runtime. |
| `A2` | Exact-edge Replanner test fixture | `src/replanner_compaction_tests.rs` | `R2` | fixture/data-flow trace | `Reviewed - no issue found` | Reserved trigger is sorted first for fixture edge top-up; test asserts it has at least three edges, matching the decomposed replacement's three dependency edges. Production Replanner code is unchanged. | 20 focused repetitions passed locally. |
| `A3` | Linux helper shared-module lint scope | `src/bin/codex-linux-sandbox.rs` | `R3` | compile/lint configuration trace | `Reviewed - no issue found` | Helper-local `dead_code` and `unfulfilled_lint_expectations` allowances cover partial shared modules only; main binary lint scope is unchanged. | Linux CI's production and all-targets clippy jobs are the authoritative check. |

## Subagent Candidate Adjudication

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `R1-C1` | `R1` | `dismissed` | `None` | The parent now asserts the readiness marker; helper asserts Job active-process count >=2 before abnormal abort. | The pre-fix vacuous-pass candidate is absent in the current target. |
| `R2-C1` | `R2` | `dismissed` | `None` | Reserved trigger priority and explicit `dependencies().len() >= 3` assertion. | The exact-edge fixture no longer depends on random UUID order; production logic is untouched. |
| `R3-C1` | `R3` | `dismissed` | `None` | Helper-specific lint attributes in `codex-linux-sandbox.rs`; root main lint policy unchanged. | Scoped allowance is needed because this binary includes only part of shared modules. |

## Evidence Appendix

### Verification Commands

- `cargo test --locked --bin local-mcp platform_runtime_tests -- --quiet` -> 11 passed on macOS; Windows helpers are cfg-excluded.
- Exact Replanner ceiling test -> 20/20 focused passes on macOS after deterministic fixture ordering.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> passed locally; Linux helper target's CI clippy remains pending.
- Full local parallel suite -> two consecutive 1125-test main-target passes.
- GitHub CI `37520685010` -> Windows x64 tests compiled but failed the old `cmd.exe start /b ping` fixture; the current test helper replaces it and will be checked by the next run.

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A1` | New Windows helper tree has not executed on Windows after this correction | Cannot establish descendant job inheritance or owner-death witness locally | Next branch CI Windows x64 run; all Windows matrix jobs must pass. |

## Prior Resolution Reconciliation

None - initial review generation for this narrow post-CI follow-up.

## Receiving Handoff

- Handoff status: `Ready for receiving-code-review`
- Automatic receiving permitted: `Yes`
- Source report ID: `cr-20261007-d40cb7aa`
- Scope fingerprint to recheck: `sha256:c6108c3178c6e8fc26c2782653098ab6b178b6761101a673d1d1e4fbaf7ee3a0`
- Actionable finding IDs: `None`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `None`
- Deferred test-gap IDs: `None`
- Open question IDs: `None`
- Open coverage area IDs: `A1`
- Highest-risk verification to repeat: native Windows x64 test/compat matrix and complete 11-job workflow.
- Suggested implementation boundaries: no production Replanner or process behavior change unless Windows CI supplies causal evidence.
- Re-review note: `Treat every finding as a claim to verify. Challenges require a counterclaim, argument, evidence, limits, and settlement criterion.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or product owner.`

## Report Self-Check

- `yes` Assessment mode and rationale are recorded.
- `yes` Every changed file is in the coverage ledger.
- `yes` No accepted code finding or standalone test gap is omitted.
- `yes` Not-covered Windows runtime has an exact next verification.
- `yes` Recommendation is Discuss because A1 is not covered.
- `pending` Run the report validator.
- `yes` Git state was not mutated during review.
