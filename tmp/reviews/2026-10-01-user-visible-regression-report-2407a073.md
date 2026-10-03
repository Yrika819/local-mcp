# User-Visible Regression Audit

## Scope

- Review date: `2026-10-01`
- Requested outcome: `review and fixes`
- Continuation: `return final behavior audit after authorized fixes and CI`
- Scope reviewed: `branch diff / implementation working tree`
- Baseline: `Phase 3-integrated main 56ac3403070c80a171daf4aada65b1170ea08c0e`
- Target: `managed-worktrees/v1-phase4-execution-root at f899ad31033540d666389d2520fd040fe267cbc7`
- Completion: `Complete within reviewed scope`
- Assumptions: `The Phase 4 branch is pushed but intentionally not merged. Managed Goals are explicit opt-in; PRIMARY remains the default. Phase 5 remains out of scope. CI conclusions are limited to what each of the 11 jobs tests; Windows remains experimental per SECURITY.md.`

## Gate Snapshot

- Recommendation: `Pass`
- Completion: `Complete within reviewed scope`
- Why now: `Managed Goals now use the reconciled linked worktree while PRIMARY remains rooted at Goal.cwd, and the completed 11-job matrix found no remaining user-visible regression in the reviewed journeys.`
- Must-review now: `None`
- Findings count: `Block 0 | Discuss 0 | Watch 0 | Intentional 1`
- Coverage confidence: `high for local tests and the configured CI matrix; not a claim of Windows security parity`
- Behavior graph coverage: `built for 5 execution surfaces`
- Biggest blind spot: `CI validates its defined matrix only; Windows remains experimental and no release/real-user deployment validation is claimed.`

## Complete Findings Index

No user-visible regression findings identified in the reviewed scope.

## Block

None.

## Discuss

None.

## Watch

None.

## Intentional Changes

- `I1` Explicitly opted-in managed Goals whose worktree is ACTIVE, exactly reconciled, and currently authorized by the Session now plan and execute in the linked worktree. `Goal.cwd` remains the primary Session/repository identity; PRIMARY Goals retain their prior behavior. This is the intended Phase 4 execution-root split. See [execution-root validation](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L167) and [Planner routing](/Users/yuta/local-mcp-connector-parity/src/planner.rs#L218).

## Coverage Ledger

| Surface / path | Touched files or entry points | Status | Result | Evidence |
| --- | --- | --- | --- | --- |
| `goal_run` preparation/resume and invalid managed lifecycle | `src/goal_runner.rs`; `src/managed_worktree_prepare.rs` | `Reviewed - no user-visible regression found` | Managed preparation/reconciliation precedes worker scheduling; missing, mismatched, ambiguous, inactive, or unauthorized roots fail closed. | Existing lifecycle tests, focused revoked-authority/missing/present-branch-mismatch gate test, full local suite, and CI test jobs. |
| Planner and replanner | `src/planner.rs`; `src/replanner.rs` | `Intentional I1` | Planner and replanner use the validated candidate root; relative effectful paths resolve under the worktree, primary absolute effectful paths are rejected, and revision/plan revision rules remain unchanged. | Managed scope tests and PRIMARY Planner compatibility tests; Windows path spelling regression and replanner tests passed in CI. |
| Writer, preimage, and mutation lease | `src/writer.rs`; existing scheduler lease logic | `Reviewed - no user-visible regression found` | Candidate writes do not mutate primary; one existing Goal-level writer lease and side-effect/reconciliation semantics remain. | Real-Git test creates/edits only candidate, confirms primary bytes and Git status unchanged, and runs a second sequential writer that sees the prior candidate change. Existing lease tests pass. |
| Readonly Goal worker repository context | `src/readonly_worker.rs` | `Reviewed - no user-visible regression found` | Managed readonly requests inspect the candidate root after separate primary identity and Session authority validation. | Real-Git integration asserts candidate repository contents are supplied; full test suite and CI passed. |
| Verifier files, hashes, commands, Git snapshots and TaskScope | `src/verifier.rs`; `src/config.rs::canonical_path_like` | `Reviewed - no user-visible regression found` | Managed Git-observed changed paths and TaskScope now share compact candidate spelling on Windows; PRIMARY retains verbatim spelling. Escaped junction paths fail the actual production TaskScope predicate. | Real linked-worktree verifier checks file/hash/command/GitScope; Windows-only compact-managed/verbatim-primary/junction gate test ran in Windows x64 CI. |
| Windows host-native verifier approval flow | `src/approvals.rs` test responder and production approval request path | `Reviewed - no user-visible regression found` | Test harness exercises the existing per-session approval IPC; no production approval authority changed. | Windows x64 full test job passed, including the managed verifier command flow. Existing current-user-only named-pipe ACL tests passed. |
| PRIMARY Planner/Writer/Verifier/readonly behavior | Relevant execution modules and regression tests | `Reviewed - no user-visible regression found` | PRIMARY remains the default, retains Goal.cwd as execution root, and avoids managed Git reconciliation. | PRIMARY-specific regressions and existing behavior tests passed locally and across applicable CI legs. |
| Security/public behavior contract | `SECURITY.md` | `Intentional I1` | Security documentation now describes Phase 4 routing and explicitly defers Phase 5 evidence/finalization/cleanup. Windows experimental classification is unchanged. | Cross-checked against the user request and Managed Worktrees design §§13-15, 20, 23-27. |

## Evidence Appendix

### Behavior Graph Deltas

| ID | Surface | Baseline path | After-change path | Delta | Ledger / finding link |
| --- | --- | --- | --- | --- | --- |
| `B1` | `goal_run` to Planner for managed Goal | `goal_run -> prepare managed workspace -> Phase 3 blanket Planner refusal -> no plan` | `goal_run -> prepare/reconcile -> shared ACTIVE/current-authority execution-root gate -> Planner rooted at linked worktree` | Removes the temporary stop only for exact ACTIVE, authorized, reconciled workspaces; other lifecycle/authority states continue to stop before scheduling. | `I1` |
| `B2` | Planner and replanner scope materialization | `Planner cwd=Goal.cwd -> relative paths under primary` | `Planner cwd=execution_root -> relative paths under candidate; primary absolute effectful path rejected` | Managed path root changes by explicit opt-in; PRIMARY unchanged. | `I1` |
| `B3` | Writer and mutation lease | `Writer path/preimage at Goal.cwd; one Goal-level writer lease` | `Writer path/preimage at execution_root; same one Goal-level writer lease` | Candidate receives edits; primary remains unchanged; no second lease namespace. | Reviewed |
| `B4` | Readonly worker context | `Readonly request cwd/root from Goal.cwd` | `Readonly request uses execution_root after primary identity and Session authority checks` | Repository observations come from candidate without expanding filesystem permission. | Reviewed |
| `B5` | Verifier candidate | `File/hash/command/Git verification rooted at Goal.cwd` | `File/hash/command/Git verification rooted at execution_root; Windows spelling follows validated root` | Candidate is the checked workspace; no Phase 5 durable workspace evidence is added. | Reviewed |

### Diff Inventory

| File or area | Classification | User-visible path considered |
| --- | --- | --- |
| `SECURITY.md` | `docs-only` | Managed execution security boundary and Phase 5 deferral |
| `src/goal_runner.rs` | `dependency` | `goal_run` preparation before scheduling |
| `src/managed_worktree_creation_tests.rs` | `test-only` | Creation-to-execution isolation, recovery gate, Windows path spelling and junction negative test |
| `src/managed_worktree_observe.rs` | `dependency` | Machine-readable exact worktree observation |
| `src/managed_worktree_prepare.rs` | `dependency` | Shared managed execution-root/current Session authority gate |
| `src/config.rs` | `dependency` | Root-aware canonical path spelling and existing reparse resolution |
| `src/planner.rs` | `surface/dependency` | Planner/replanner cwd, roots, scope normalization |
| `src/readonly_worker.rs` | `surface/dependency` | Readonly request root |
| `src/replanner.rs` | `surface/dependency` | Replan scope/path normalization |
| `src/verifier.rs` | `surface/dependency` | Candidate file/hash/command/Git observation and scope gates |
| `src/writer.rs` | `surface/dependency` | Writer request cwd, path/preimage/mutation routing, existing lease |
| `src/approvals.rs` | `test-only/dependency` | Test-only Windows verifier approval responder; production approval path unchanged |
| `src/phase0_mcp_tests.rs` | `test-only` | Existing job poll test uses a bounded timed wait instead of a tight yield-only loop |

### Candidate Sweep Log

| Candidate | Decision | Reason |
| --- | --- | --- |
| `An ACTIVE label alone may authorize execution` | `dismissed` | Shared gate requires current Session authority, exact durable ownership, consumed creation attempt, and fresh reconciliation before returning the managed root. |
| `Persisted managed state may recreate Session filesystem permission` | `dismissed` | No directories are appended and authorization is resolved from current Session state for the exact worktree root. |
| `Managed scope may target the primary checkout or Git administration` | `dismissed` | Managed Planner roots are narrowed to execution_root; primary absolute effectful paths and .git/common-dir targets are rejected. The Windows junction test drives an escaped path through the production gate. |
| `Managed Goal may get a parallel writer lease` | `dismissed` | Existing goal-wide mutation lease predicates remain unchanged; performed/unknown states keep prior semantics. |
| `Phase 5 evidence omission is a regression` | `intentional I1` | User explicitly prohibits snapshot digests, verifier evidence binding, finalizer recheck, cleanup result surface, and cleanup operations in Phase 4. |
| `Reviewer needs a new filesystem root` | `dismissed` | Existing reviewer consumes structured evidence and does not directly inspect the workspace. |
| `PRIMARY TaskScope/replan equality on Windows may change` | `dismissed` | Windows CI exposed and then verified the root-style-aware fix: PRIMARY retains verbatim canonical spelling; managed roots retain Git-compatible compact spelling. |
| `Existing phase0 job poll is flaky under CI scheduling` | `dismissed after repair` | Replaced 100 yield-only polls with a bounded 500×10ms timed poll; the affected Windows and Ubuntu CI jobs now pass. This is a deterministic test-only wait adjustment. |

### Verification Commands

- `cargo test --locked --all-targets managed_worktree -- --quiet` -> `185 passed, 0 failed`
- `cargo test --locked --all-targets --quiet` -> `872 passed, 0 failed`
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> `passed`
- `cargo fmt --all -- --check` -> `passed`
- `git diff --check` -> `passed`
- Phase 4 CI run `36817487166` on `f899ad31033540d666389d2520fd040fe267cbc7` -> `11/11 jobs succeeded`
- The CI matrix includes Windows x64 full tests, Windows 2022 compatibility, Windows ARM64 compatibility, macOS Intel/ARM64 tests, Linux x64 tests, quality, and compatibility jobs; Windows support remains experimental and no Unix security parity is claimed.

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `I1` | `execution-root authority` | [validated_execution_root](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L173) | Separates Goal.cwd identity from managed execution root and fails closed. |
| `I1` | `Planner request` | [planner_request_for_goal](/Users/yuta/local-mcp-connector-parity/src/planner.rs#L218) | Sends the validated execution root and narrows managed planner roots. |
| `I1` | `Writer` | [begin_writer_attempt](/Users/yuta/local-mcp-connector-parity/src/writer.rs#L353) | Checks execution root before dispatch while retaining the existing lease. |
| `I1` | `Verifier path normalization` | [canonicalize_existing_prefix](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L928) | Uses root-matched spelling after filesystem canonicalization before TaskScope comparison. |
| `I1` | `Windows junction regression` | [verifier_git_path_spelling_matches_managed_scope_and_primary_root_mode](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_creation_tests.rs#L3507) | Covers candidate/primary spelling and junction escape through the TaskScope predicate. |

### Blind Spots

| Area | Risk introduced by the blind spot | What would resolve it |
| --- | --- | --- |
| `Real-user application deployment and explicit host review/promotion` | CI validates code/test/build contracts only; it does not provide a user’s approval UI experience, deploy to a real project, or promote the candidate. These are outside Phase 4. | A future separately authorized acceptance/promotion phase. No automatic commit, merge, rebase, push, PR, release, or cleanup is authorized here. |

### Report Self-Check

- `yes` Every touched user-visible or unknown-impact surface appears in `Coverage Ledger`.
- `yes` Every finding in action sections appears in `Complete Findings Index`; none remain.
- `yes` Every finding ledger row has a matching card; there are no findings.
- `yes` No Not covered coverage row remains for the CI matrix or scoped behavior paths.
- `yes` User-visible/unknown-impact paths have behavior graphs or direct path evidence.
- `yes` Recommendation follows the mapping: no regression findings or uncovered review-relevant surfaces yields Pass.
