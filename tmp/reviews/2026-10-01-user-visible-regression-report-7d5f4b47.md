# User-Visible Regression Audit

## Scope

- Review date: `2026-10-01`
- Requested outcome: `review and fixes`
- Continuation: `continue already-authorized fixes; report is the scoped pre-push behavior audit`
- Scope reviewed: `working tree`
- Baseline: `HEAD 56ac3403070c80a171daf4aada65b1170ea08c0e on managed-worktrees/v1-phase4-execution-root`
- Completion: `Complete within reviewed scope; cross-platform runtime behavior remains explicitly unverified pending branch CI.`
- Assumptions: `Managed Goals are explicitly opted in; ordinary PRIMARY Goals remain the default. Review compares the current working tree to HEAD and relies on local macOS tests plus static call-path tracing. Phase 5 is prohibited.`

## Gate Snapshot

- Recommendation: `Discuss`
- Completion: `Complete within reviewed scope`
- Why now: `No PRIMARY regression or managed execution-path regression was identified, but Linux/Windows execution behavior remains unverified until the requested CI matrix completes.`
- Must-review now: `None`
- Findings count: `Block 0 | Discuss 0 | Watch 0 | Intentional 1`
- Coverage confidence: `high for local macOS and static path coverage; medium across platforms`
- Behavior graph coverage: `built for 5 execution surfaces; direct path trace used for documentation and regression-only tests`
- Biggest blind spot: `Cross-platform managed execution behavior before Phase 4 CI completes.`

## Complete Findings Index

No user-visible regression findings identified in the reviewed scope.

## Block

None.

## Discuss

None.

## Watch

None.

## Intentional Changes

- `I1` An explicitly opted-in managed Goal can now plan and execute in its exact ACTIVE linked worktree rather than being stopped by Phase 3's temporary planner refusal. `Goal.cwd` remains the primary Session/repository identity root, while candidate operations use the host-derived execution root; current Session authority and exact ownership reconciliation remain required. This is the requested Phase 4 behavior, not a regression. See [execution-root gate](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L167) and [Planner request routing](/Users/yuta/local-mcp-connector-parity/src/planner.rs#L218).

## Coverage Ledger

| Surface / path | Touched files or entry points | Status | Result | Evidence |
| --- | --- | --- | --- | --- |
| `goal_run` preparation and managed resume | `src/goal_runner.rs`; `src/managed_worktree_prepare.rs` | `Reviewed - no user-visible regression found` | Managed preparation and exact reconciliation remain before scheduling; invalid or unauthorized state fails closed. | Static trace, prior Phase 3 preparation tests, and focused revoked-authority/missing/present-mismatch Planner-gate test. |
| Planner and replanner request/path materialization | `src/planner.rs`; `src/replanner.rs` | `Intentional I1` | PRIMARY keeps its prior root; managed ACTIVE requests use execution root and effectful scopes are narrowed to it. | `primary_planner_behavior_is_unchanged`; managed relative-path/primary-absolute rejection tests; static request-to-materialization trace. |
| Writer mutation, preimage, and lease behavior | `src/writer.rs`; scheduler lease path | `Reviewed - no user-visible regression found` | Managed mutations resolve in the candidate while retaining the existing single Goal mutation lease and reconciliation semantics. | Test-owned real Git integration: writer-created/edited candidate file, sequential second writer, primary unchanged, primary Git status clean. Existing lease suite remained green in the prior full run. |
| Readonly Goal worker repository context | `src/readonly_worker.rs` | `Reviewed - no user-visible regression found` | Managed readonly requests use the candidate root; request identity remains separately tied to primary Session/Goal binding. | Candidate readonly integration assertion and static guard/request construction trace. |
| Verifier file/hash/command/Git observations | `src/verifier.rs` and existing execution authority | `Reviewed - no user-visible regression found` | Verification observes candidate files and Git state rather than treating primary changes as candidate changes. | Real-Git integration checks file/hash/command/Git scope against the candidate and confirms primary cleanliness. |
| PRIMARY Planner/Writer/Verifier/readonly compatibility | Corresponding implementation modules and existing tests | `Reviewed - no user-visible regression found` | PRIMARY remains the default and effective root remains Goal.cwd; PRIMARY avoids managed-worktree reconciliation. | Existing regression tests and complete local test suite passed (872/872 after the detached-HEAD assertion). |
| Phase 4 behavior on Linux and Windows | `src/managed_worktree_prepare.rs`; `src/planner.rs`; `src/writer.rs`; `src/verifier.rs` | `Not covered` | Local macOS verification does not establish Linux sandbox/path behavior or Windows reparse/approval behavior. | Run and wait for the authoritative 11-job Phase 4 CI matrix. Windows remains experimental by contract. |
| Security contract/public behavior | `SECURITY.md` | `Intentional I1` | Documentation now accurately describes Phase 4 execution-root routing and explicitly defers Phase 5 evidence/finalization. | Compared against user scope and Managed Worktrees design §§13-15, 20, 23-26. |

## Evidence Appendix

### Behavior Graph Deltas

| ID | Surface | Baseline path | After-change path | Delta | Ledger / finding link |
| --- | --- | --- | --- | --- | --- |
| `B1` | Managed `goal_run` to Planner | `goal_run -> managed PREPARE gate -> Phase 3 blanket Planner refusal -> no plan` | `goal_run -> prepare/reconcile exact ACTIVE -> current Session authority + execution-root gate -> Planner` | Removes only the temporary blanket refusal; stale, ambiguous, unauthorized, or non-ACTIVE states still stop before scheduling. | Reviewed; intended Phase 4 behavior |
| `B2` | Managed Planner materialization | `Planner cwd = Goal.cwd -> relative paths under primary` | `Planner cwd = execution_root -> relative paths under linked worktree; primary absolute effectful paths reject` | Candidate root replaces primary only for managed execution; Goal/session identity remains primary. | Intentional `I1` |
| `B3` | Managed Writer and lease | `Writer path/preimage/mutation rooted at Goal.cwd; one Goal writer lease` | `Writer path/preimage/mutation rooted at execution_root; same one Goal lease` | Candidate receives writes; no second lease namespace or changed side-effect budget. | Reviewed |
| `B4` | Managed readonly worker | `Readonly request root/cwd from Goal.cwd` | `Readonly request root/cwd from execution_root after separate identity/authority checks` | Reads candidate repository state without widening Session authority. | Reviewed |
| `B5` | Managed Verifier | `File/hash/command/Git observations rooted at Goal.cwd` | `File/hash/command/Git observations rooted at execution_root` | Candidate is the verification target. Phase 5 durable workspace evidence remains absent by design. | Reviewed |

### Diff Inventory

| File or area | Classification | User-visible path considered |
| --- | --- | --- |
| `SECURITY.md` | `docs-only` | Managed-worktree security contract |
| `src/goal_runner.rs` | `dependency` | `goal_run` preparation before scheduling |
| `src/managed_worktree_creation_tests.rs` | `test-only` | Planner/writer/readonly/verifier integration and PRIMARY regression evidence |
| `src/managed_worktree_observe.rs` | `dependency` | Exact mechanical ownership observation |
| `src/managed_worktree_prepare.rs` | `dependency` | Shared execution-root, authority, and reconciliation gate |
| `src/planner.rs` | `surface/dependency` | Planner cwd, permitted roots, and TaskScope path materialization |
| `src/readonly_worker.rs` | `surface/dependency` | Readonly request root |
| `src/replanner.rs` | `surface/dependency` | Replan scope/path materialization |
| `src/verifier.rs` | `surface/dependency` | File, hash, command, and Git verification root |
| `src/writer.rs` | `surface/dependency` | Writer request cwd, mutation materialization, preimage, and lease |

### Candidate Sweep Log

| Candidate | Decision | Reason |
| --- | --- | --- |
| `Managed lifecycle label alone may permit execution` | `dismissed` | Shared gate requires ACTIVE lifecycle, current Session authority, consumed creation attempt, and fresh exact reconciliation; invalid or ambiguous states error before dispatch. |
| `Managed state may silently grant filesystem permission` | `dismissed` | The gate resolves existing Session authority for the exact root and does not append directories or reconstruct permission from Goal persistence. |
| `Writer may mutate primary checkout through a managed TaskScope` | `dismissed` | Managed Planner narrows roots to execution root and rejects primary absolute effectful paths; real-Git integration confirms primary file/status stay unchanged. |
| `Managed worktree may create a parallel writer lease` | `dismissed` | Existing lease predicate remains goal/task based; full preexisting lease tests passed and no managed lease namespace was added. |
| `Phase 5 evidence omission is a regression` | `intentional I1` | User explicitly froze this phase before worktree-bound evidence, snapshot digests, finalizer recheck, cleanup, or result surface. |
| `Reviewer requires new workspace filesystem routing` | `dismissed` | Existing reviewer consumes already-produced structured evidence and does not inspect repository cwd. |

### Verification Commands

- `cargo test --locked --all-targets managed_planner_gate_refuses_revoked_authority_and_missing_or_mismatched_worktree -- --nocapture` -> `passed; 1 test, including revoked authority, present detached-HEAD mismatch, and missing path`
- `cargo test --locked --all-targets --quiet` -> `passed after the detached-HEAD assertion; 872 passed, 0 failed`
- `cargo test --locked --all-targets managed_worktree -- --quiet` -> `passed; 185 passed, 0 failed, 687 filtered out`
- `cargo fmt --all -- --check` -> `passed`
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> `passed after removing an unused Phase 3 wrapper`
- `git diff --check` -> `passed`
- Phase 4 cross-platform CI -> `not run yet; required before completion`

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `I1` | `gate` | [validated_execution_root](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L173) | Shared host-owned gate routes only exact authorized ACTIVE ownership. |
| `I1` | `Planner` | [planner_request_for_goal](/Users/yuta/local-mcp-connector-parity/src/planner.rs#L218) | Managed requests receive only execution-root permitted scope. |
| `I1` | `readonly` | [begin_readonly_attempt](/Users/yuta/local-mcp-connector-parity/src/readonly_worker.rs#L155) | Validates execution root before readonly request construction. |
| `I1` | `Writer` | [begin_writer_attempt](/Users/yuta/local-mcp-connector-parity/src/writer.rs#L353) | Preserves Goal mutation lease while checking managed root. |
| `I1` | `Verifier` | [prepare](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L195) | Uses validated execution root for candidate verification. |

### Blind Spots

| Area | Risk introduced by the blind spot | What would resolve it |
| --- | --- | --- |
| Linux and Windows Phase 4 runtime/CI | Local macOS tests do not establish platform-specific sandbox, reparse-point, Git, or approval behavior. | Complete and inspect all 11 Phase 4 CI jobs; preserve Windows experimental interpretation. |

### Report Self-Check

- `yes` Every touched user-visible or unknown-impact surface appears in `Coverage Ledger`.
- `yes` Every finding in an action section appears in `Complete Findings Index`; no regression findings were identified.
- `yes` Every `Finding F#` ledger row has a matching card; no finding rows exist.
- `yes` Every `Not covered` row has a reason and next verification step.
- `yes` Every user-visible or unknown-impact surface has behavior graph or direct path evidence, or is explicitly marked not covered.
- `yes` Recommendation follows the mapping rules: the unverified cross-platform execution surface is marked `Not covered`, so recommendation is `Discuss`.
