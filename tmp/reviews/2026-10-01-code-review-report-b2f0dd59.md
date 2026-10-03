# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261001-b2f0dd59`
- Review chain ID: `rc-20261001-f8fce257`
- Review generation: `0`
- Review trigger: `initial`
- Parent review report ID: `None`
- Parent review report path: `None`
- Parent resolution ID: `None`
- Parent resolution path: `None`
- Generated at: `2026-10-01T00:00:00Z`
- Report path: `tmp/reviews/2026-10-01-code-review-report-b2f0dd59.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:9df9ea9a9283ed7969c3504e10f5cb56bab08981a1898923a120464695de26cf`

## Scope

- Review date: `2026-10-01`
- Scope kind: `working tree`
- Scope description: `Phase 4 Managed Worktrees execution-root plumbing compared with HEAD 56ac3403070c80a171daf4aada65b1170ea08c0e; includes Planner/replanner, writer, readonly worker, verifier, goal-run integration, shared authority derivation, a real-Git integration test, and SECURITY.md.`
- Scope mode: `full frozen scope`
- Baseline: `HEAD 56ac3403070c80a171daf4aada65b1170ea08c0e on managed-worktrees/v1-phase4-execution-root`
- Target: `working tree on managed-worktrees/v1-phase4-execution-root`
- Changed paths: `10`
- Diff size: `606 insertions, 119 deletions`
- Completion: `Complete within reviewed scope`
- Requirements consulted: `User Phase 4 mission; docs/MANAGED_WORKTREES_V1_DESIGN.md §§7, 13-15, 20-21, 23, 25-27; SECURITY.md; docs/GOAL_TASK_ORCHESTRATOR_V1_DESIGN.md §§1, 5, 8, 10-15, 17, 23, 25.`
- Prior resolution consulted: `None`
- Assumptions: `Phase 5 durable worktree verification evidence, snapshot digest, and finalizer binding are explicitly out of scope per the user mission.`
- Excluded as unrelated: `Phase 5 evidence/finalization; cleanup; publication; automatic Git integration; unrelated legacy tools.`

## Review Orchestration

- Assessment subagent: `Coordinator assessment - execution-root routing is cohesive but spans independent authority and execution/test risks; independent read-only security and correctness reviewers improve coverage.`
- Orchestration decision: `Parallel specialists`
- Decision confidence: `high`
- Decision rationale: `The same shared root flows through several independent high-risk paths; security authority and execution routing/lease/test evidence are distinct review angles with intentional overlap at the shared gate.`
- Coordinator override: `None`
- Context or tool limits: `Two read-only specialists completed. Coordinator independently checked findings. Full local test suite and clippy completed. GitHub cross-platform CI for Phase 4 has not yet run.`

### Risk Dimensions

- `A stale, unauthorized, or misidentified managed path must not become a Planner/worker/Verifier execution root.`
- `Path materialization, Writer mutation, and Verifier observations must use the candidate without reaching the primary checkout or Git administrative data.`
- `PRIMARY compatibility and one-writer/unknown-side-effect lease semantics must remain unchanged.`

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `R1` | Security and authority | Shared execution-root gate; Session authority; worktree ownership; Writer path boundary; Security documentation | Planner/replanner and worker callers; Git admin/symlink paths; phase boundary | `Complete` |
| `R2` | Correctness, routing, and tests | Planner/replanner, runner, Writer, readonly, Verifier, integration coverage | Lease/revisions, PRIMARY behavior, resume, candidate isolation | `Complete` |
| `Coordinator` | Synthesis and independent verification | Entire changed-file inventory and candidate mismatch gate | Reproduced tests and adjudicated specialist claims against contracts | `Complete` |

### Synthesis Statement

The coordinator independently verified the single retained test-gap candidate. The Phase 5 evidence candidate from R1 was dismissed because the user explicitly froze Phase 4 at execution-root plumbing and the changed SECURITY.md records that boundary. R2's claim that the integration/full tests timed out is contradicted by completed coordinator runs: the real-Git integration passed and `cargo test --locked --all-targets --quiet` completed 872/872. No runtime behavior defect was established; one direct-gate negative test remains valuable.

## Review Snapshot

- Recommendation: `Discuss`
- Completion: `Complete within reviewed scope`
- Why now: `A direct dispatch-gate test for a present worktree whose branch/HEAD identity has changed is missing, and cross-platform execution CI has not yet run.`
- Must-review now:
  1. `T1` Present but mismatched ACTIVE worktree at execution-root gate
- Findings count: `Blocker 0 | Major 0 | Minor 0 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 1`
- Coverage confidence: `high`
- Biggest blind spot: `Cross-platform Phase 4 CI has not yet run.`

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

| ID | Severity | Surface | Missing coverage | Risk | Origin | Evidence | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `T1` | `Minor` | Managed execution-root dispatch gate | No direct regression assertion that a present ACTIVE worktree whose branch/HEAD/common-dir no longer matches is refused by the new execution-root gate before Planner/worker dispatch. Existing Phase 3 preparation mismatch tests cover the earlier reconciliation seam, and the new gate rejects all non-`ActiveExact` classifications in code. | A future caller or branch change could weaken the newer dispatch boundary without a focused regression test. | `R2-C1, Coordinator` | `src/managed_worktree_prepare.rs:164-230`; `src/managed_worktree_creation_tests.rs` covers revoked authority and moved/missing path but not a still-present branch/HEAD mismatch at this gate. | `test-gap; entry=managed Goal execution-root gate; contract=execution dispatch refuses an ACTIVE record unless linked-worktree identity reconciles exactly; gap=no direct dispatch-gate regression test for present branch/HEAD/common-dir mismatch` | `ifp-sha256:e33475fa0c607266e486dc031c9ca0cc160d39143af9b2ec9b5341736a3aa6ad` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §§13,20` |

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | Shared execution-root derivation and current Session authority | `src/managed_worktree_prepare.rs:164-230` | `R1, Coordinator` | `contract trace; runtime verified` | `Reviewed - no issue found` | Primary identity remains separate; managed root requires current authority, ACTIVE, exact reconciliation, and consumed creation attempt. | `real Git preparation and planner gate tests; add direct present-mismatch gate case per T1` |
| `A2` | Planner and replanner routing/scope materialization | `src/planner.rs`, `src/replanner.rs` | `R2, Coordinator` | `contract trace; runtime verified` | `Reviewed - no issue found` | Managed Planner cwd/permitted root and scope normalization use candidate; primary absolute effectful targets reject. | `managed_task_scope_materializes_under_candidate...; managed workspace Planner test` |
| `A3` | Writer request, path/preimage/mutation boundary, lease | `src/writer.rs`, `src/scheduler.rs` | `R1, R2, Coordinator` | `dependency trace; runtime verified` | `Reviewed - no issue found` | Single existing lease remains; mutation routes candidate; primary remains clean; sequential writer reads prior candidate change. | `real_creation_planning_and_writer_mutate_only_the_managed_candidate; existing writer lease tests` |
| `A4` | Readonly worker root and repository observation | `src/readonly_worker.rs`, `src/goal_backends.rs` | `R2, Coordinator` | `dependency trace; runtime verified` | `Reviewed - no issue found` | Request root is candidate and test reads the candidate's initial file state, not primary substitution. | `CandidateReadonly integration assertion` |
| `A5` | Verifier file/hash/command/Git observation | `src/verifier.rs`, `src/execution.rs` | `R1, R2, Coordinator` | `dependency trace; runtime verified` | `Reviewed - no issue found` | Snapshot root is candidate; file/hash, command cwd, Git scope observe candidate. Phase 5 binding intentionally absent. | `real-Git integration verifies file, digest, git rev-parse, changed scope; primary stays clean` |
| `A6` | Goal-run resume and scheduler precondition | `src/goal_runner.rs`, `src/managed_worktree_prepare.rs` | `R2, Coordinator` | `contract trace; runtime verified` | `Reviewed - no issue found` | Managed preparation/reconciliation remains before scheduler; PRIMARY path returns NotManaged. | `Phase 3 preparation tests; helper revocation/missing-path tests` |
| `A7` | Managed integration and PRIMARY regression tests | `src/managed_worktree_creation_tests.rs`; existing planner/writer/verifier/readonly tests | `R2, Coordinator` | `runtime verified` | `Not covered` | `Real test-owned repository/worktree covers create→ACTIVE→plan→readonly→write→verify and primary isolation. Present but mismatched execution gate state lacks a direct assertion.` | `872 local tests passed; add T1 test and rerun focused validation` |
| `A8` | Security/public behavior documentation | `SECURITY.md` | `Coordinator` | `contract trace` | `Reviewed - no issue found` | `Text describes exact authority, dual-root identity, reconciliation fail-closed behavior, and Phase 5 deferral.` | `Cross-checked against user phase boundary and design §§13,23` |
| `A9` | Cross-platform CI matrix | GitHub Actions jobs for managed execution routing | `Coordinator` | `not run` | `Not covered` | `The Phase 4 branch has not yet been pushed, so Linux/macOS/Windows runner behavior is not available.` | `Push Phase 4 branch and wait for all 11 jobs.` |

## Subagent Candidate Adjudication

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `R1-C1` | `R1` | `dismissed` | `None` | `User mission §§13,24; SECURITY.md Phase 4 sentence` | `Worktree ID/common-dir/branch/base binding and final snapshot digest are Phase 5 features, expressly forbidden in this Phase 4 scope; recording their absence as a defect would violate the phase boundary.` |
| `R2-C1` | `R2` | `accepted` | `T1` | `Coordinator inspected exact-active classifier path and tests; full suite actually passed 872/872` | `Existing creation/preparation tests prove earlier reconciliation; direct active execution-root gate still lacks a present-but-mismatched negative case. R2's timeout assertion is disproved by coordinator test output and is not carried forward.` |

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `SECURITY.md` | `docs-only` | User-visible managed execution and security boundary |
| `src/goal_runner.rs` | `surface/dependency` | `goal_run` prepare-before-scheduling behavior |
| `src/managed_worktree_creation_tests.rs` | `test-only` | Managed integration and authority/recovery coverage |
| `src/managed_worktree_observe.rs` | `dependency` | Strict read-only Git observation |
| `src/managed_worktree_prepare.rs` | `dependency` | Shared execution-root authority and reconciliation |
| `src/planner.rs` | `surface/dependency` | Planner request cwd and path normalization |
| `src/readonly_worker.rs` | `surface/dependency` | Readonly model root and request validation |
| `src/replanner.rs` | `surface/dependency` | Replanned TaskScope root and eligibility |
| `src/verifier.rs` | `surface/dependency` | Candidate file, command, and Git observation |
| `src/writer.rs` | `surface/dependency` | Candidate mutation and lease-preserving dispatch |

### Verification Commands

- `cargo fmt --all -- --check` -> `passed`
- `cargo test --locked --all-targets --quiet` -> `872 passed; 0 failed`
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> `passed after removing one unused planning helper`
- `git diff --check` -> `passed`
- `cargo test --locked --all-targets real_creation_planning_and_writer_mutate_only_the_managed_candidate -- --nocapture` -> `passed; test-owned real linked worktree; primary status clean`

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `T1` | `gate` | [`validated_execution_root`](../../src/managed_worktree_prepare.rs#L164) | `Only exact ACTIVE reconciliation returns a managed root.` |
| `T1` | `test` | [`managed_planner_gate_refuses_revoked_authority_and_missing_or_mismatched_worktree`](../../src/managed_worktree_creation_tests.rs#L3410) | `Covers revoked authority and moved/missing root but not present branch/HEAD mismatch.` |
| `A3` | `integration` | [`real_creation_planning_and_writer_mutate_only_the_managed_candidate`](../../src/managed_worktree_creation_tests.rs#L3173) | `Proves primary isolation, subsequent candidate visibility, and candidate verifier checks.` |
| `A2` | `routing` | [`planner_request_for_goal`](../../src/planner.rs#L218) | `Routes managed planning to validated execution root and narrows model-visible roots.` |
| `A5` | `verification` | [`prepare`](../../src/verifier.rs#L195) | `Builds verification snapshot from validated execution root.` |

### Dismissed Coordinator Candidates

| Candidate | Decision | Evidence |
| --- | --- | --- |
| `Missing Phase 5 verification identity/snapshot digest` | `dismissed` | `Explicit phase non-goal in user mission §13 and SECURITY.md; must remain deferred.` |
| `Reviewer filesystem root routing` | `dismissed` | `Reviewer consumes structured file evidence and does not inspect repository cwd; `src/goal_backends.rs:117-126`.` |
| `PRIMARY does unnecessary managed Git reconciliation` | `dismissed` | `validated_execution_root returns PRIMARY after identity/current path checks and before HostGit construction.` |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
- `A9` | Cross-platform Phase 4 CI | `Local macOS tests cannot establish Linux sandbox or Windows reparse/approval behavior.` | `Run and wait for all 11 authoritative GitHub CI jobs on the pushed Phase 4 branch.` |

## Prior Resolution Reconciliation

None - initial review generation.

## Receiving Handoff

- Handoff status: `Ready for receiving-code-review`
- Automatic receiving permitted: `Yes`
- Source report ID: `cr-20261001-b2f0dd59`
- Scope fingerprint to recheck: `sha256:9df9ea9a9283ed7969c3504e10f5cb56bab08981a1898923a120464695de26cf`
- Actionable finding IDs: `None`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `T1`
- Deferred test-gap IDs: `None`
- Open question IDs: `None`
- Open coverage area IDs: `A7, A9`
- Highest-risk verification to repeat: `Run a present-but-branch-mismatched ACTIVE worktree through validated_execution_root and prove refusal before returning PlannerRequest.`
- Suggested implementation boundaries: `Add one test-only branch mismatch to the test-owned real Git fixture; do not change lifecycle, authority, or Phase 5 evidence.`
- Re-review note: `Treat every finding as a claim to verify. Challenges require a counterclaim, argument, evidence, limits, and settlement criterion.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or product owner.`

## Report Self-Check

- `yes` Actual assessment mode and rationale are recorded.
- `yes` Every changed review-relevant or unknown-impact area appears in the Review Coverage Ledger.
- `yes` Every final finding appears once in the index and once as a matching card; no F# findings exist.
- `yes` Every Finding F# area references an existing finding; T1 is the only standalone gap.
- `yes` T1 has a stable ID, severity, issue fingerprint, and authoritative expected basis.
- `yes` Generation, trigger, parent, scope mode, and receiving handoff are bounded and consistent.
- `yes` Candidate adjudication includes all meaningful specialist candidates.
- `yes` Every Not covered area has a reason and next verification step.
- `yes` Recommendation follows the mapping: the Not covered cross-platform execution area yields Discuss.
- `yes` Git state was not mutated during review.
