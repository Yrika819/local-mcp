# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261007-b13b27a5`
- Review chain ID: `rc-20261007-8f551df6`
- Review generation: `1`
- Review trigger: `post-implementation`
- Parent review report ID: `cr-20261007-8f551df6`
- Parent review report path: `tmp/reviews/2026-10-07-code-review-report-platform-runtime-8f551df6.md`
- Parent resolution ID: `rr-20261007-1d7f3637`
- Parent resolution path: `tmp/reviews/2026-10-07-receiving-code-review-resolution-platform-runtime-1d7f3637.md`
- Generated at: `2026-10-07T00:00:00Z`
- Report path: `tmp/reviews/2026-10-07-code-review-generation-1-platform-runtime-b13b27a5.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:1a613c399abcb745dd9810e78ff2c2d6506e6ceccbd0b6bad740ed3efb041078`

## Scope

- Review date: `2026-10-07`
- Scope kind: `branch diff`
- Scope description: Terminal synthesis of the Platform Runtime Closure V1 branch diff plus current worktree, focused on implementation changes and their process ownership, cancellation, bounded capture, host Git, sandbox, retry, and cross-platform execution chains. The design-document wording correction is separately tracked and reviewed in chain `rc-20261007-bb006bdf`.
- Scope mode: `implementation delta plus affected execution chains`
- Baseline: `origin/main` `f4bfb9e0c339d0338825a5bdb0ed7a6d994322b1`
- Target: `hardening/platform-runtime-closure-v1`, `HEAD` `23fa4af0a3acb49c6878773bd4c84be2cb490569` plus current implementation worktree
- Changed paths: `18`
- Diff size: `2,390 additions / 83 deletions`
- Completion: `Complete within reviewed scope`
- Requirements consulted: Platform Runtime Closure V1 user task; generation-0 report and receiving resolution listed above; `SECURITY.md`; Managed Worktrees, Resource Bounds, and Platform Runtime Closure designs.
- Prior resolution consulted: `rr-20261007-1d7f3637` at `tmp/reviews/2026-10-07-receiving-code-review-resolution-platform-runtime-1d7f3637.md`
- Assumptions: Windows and Linux runtime guarantees require native CI. Unix process-group containment excludes descendants that deliberately escape. The standalone executable owns its SIGCHLD policy and does not install a competing reaper after startup.
- Excluded as unrelated: protected untracked Resource Bounds stress audit; it remains outside every proposed commit tree.

## Review Orchestration

- Assessment subagent: `Coordinator assessment - generation-0 risk inventory showed independent OS kernels, Git side effects, cancellation, and sandbox authority surfaces.`
- Orchestration decision: `Parallel specialists`
- Decision confidence: `high`
- Decision rationale: OS-specific process containment, async job ownership, Git capture, and authority/retry semantics have materially distinct failure modes.
- Coordinator override: `Coordinator synthesis overrides one specialist Major classification on same-user Git-config TOCTOU: SECURITY.md §73–75 explicitly accepts this non-atomic check limitation, and managed-worktree design makes no stronger atomic-snapshot promise.`
- Context or tool limits: Native Linux/Windows runtime unavailable locally. Windows cross-target check stopped in third-party C compilation because the host lacks Windows SDK headers; no project Windows compile result is claimed.

### Risk Dimensions

- Unix stale-PGID signaling and descendant survival.
- Windows suspended child assignment, Job lifetime, nested Jobs, and owner-death cleanup.
- Async cancellation and batch shutdown must never detach unowned work.
- Bounded Git output/timeout must preserve UNKNOWN side-effect state and consumed retry budget.
- Host Git, filter configuration, PATH, environment, sandbox roots, and Writer authority must remain host-owned.

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `R1` | Unix lifecycle | `process_group`, `process_blocking`, Unix runtime/ownership tests, startup signal policy | unreaped-leader proof, auto-reap, PID/PGID reuse, FIFO witness fork/exec race, escaped descendants | Complete |
| `R2` | Windows Jobs | `process_job`, Windows `ProcessGroup`, blocking runner, Windows runtime tests | suspended spawn/assignment/resume, setup failure, kill-on-close, nested outer Job, abnormal owner death | Static complete; runtime pending |
| `R3` | Blocking Git and host tools | bounded runner and managed create/observe callers | env/tool identity, bounds, timeout, incomplete output, filter checks, retry/side-effect semantics | Complete |
| `R4` | Async cancellation and registry | foreground guard, background registry, EOF cleanup, admission/response paths | cancelled Future, JoinHandle detachment, all-job abort-before-await | Complete |
| `R5` | Security/authority | sandbox, Git filters, Writer/execution root, side-effect state | no authority widening, no unsafe env, no false not-performed, documented TOCTOU | Complete |

### Synthesis Statement

The coordinator independently verified the changed execution chains and reconciled specialist disagreements against the explicit security contract. No new code defect remains. The same-user config TOCTOU is a documented residual rather than an atomicity guarantee. The main unresolved items are three test gaps and unexecuted Linux/Windows runtime coverage.

## Review Snapshot

- Recommendation: `Discuss`
- Completion: `Complete within reviewed scope`
- Why now: code paths are statically coherent and local macOS tests pass, but Windows/Linux runtime and three focused composition/failure tests remain open.
- Must-review now: `A7/A8/A10` platform runtime; `T1` managed mutation reconciliation test; `T3` Windows failure path
  1. `A7` Windows Job runtime and failure handling
  2. `A8` Linux Bubblewrap probe/helper runtime
  3. `T1` managed timeout/overflow through durable reconciliation
- Findings count: `Blocker 0 | Major 0 | Minor 0 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 3`
- Coverage confidence: `medium` local/static; `low` platform runtime
- Biggest blind spot: native Windows Job behavior and Linux helper compilation/runtime.

## Complete Findings Index

No code-review findings identified after independent candidate adjudication.

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
| `T1` | `Minor` | Managed worktree creation lifecycle | No single test drives creator timeout/overflow/incomplete capture through durable attempt consumption, reconciliation, and retry/block classification. | A future regression could connect otherwise separately tested runner and retry layers incorrectly. | `Coordinator; R3` | `src/managed_worktree_create.rs` and `src/managed_worktree_prepare.rs`; generic runner and managed lifecycle tests pass separately. | `test-gap; entry=managed worktree creation attempt; contract=ambiguous mutation attempts remain consumed and require reconciliation before retry; gap=managed creator timeout overflow and incomplete capture lack end-to-end durable lifecycle assertions` | `ifp-sha256:6547df29a5a3982782b9d23a38bf2a0df9777ea9c8ce28eaf7e14ba1707025ed` | `kind:approved-design; strength:authoritative; evidence:docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md §7 and §12; src/managed_worktree_prepare.rs` |
| `T2` | `Minor` | Shutdown cancellation | Batch-abort test does not cancel the actual shutdown future during a join while the drained jobs own real descendant witnesses. | Source order is safe, but one integration test would pin the registry-drain/task-abort/process-tree composition. | `Coordinator; R4` | `src/mcp.rs` batch helper/test and `src/platform_runtime_tests.rs` separate descendant test. | `test-gap; entry=background job shutdown cancellation; contract=cancelled server shutdown terminates every drained owned execution tree; gap=batch abort regression uses pending tasks without a real descendant witness` | `ifp-sha256:a72aad9305e32afc84913f30315d1eb3b7113d49f5ec45cfdfeb90bf18980ac7` | `kind:approved-design; strength:authoritative; evidence:docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md §4` |
| `T3` | `Minor` | Windows containment setup failure | No native Windows fault-injection test forces Job assignment/resume failure and checks no uncontained user code runs and cleanup completes. | The setup ordering is fail-closed by inspection, but Windows API failure cleanup is not locally established. | `Coordinator; R2` | `src/process_group.rs` Windows `spawn_contained`; `src/process_blocking.rs` Windows setup branch; Windows tests currently exercise successful containment. | `test-gap; entry=Windows child containment failure; contract=assignment and resume failures leave no child executing outside its Job and release handles; gap=Windows fault-injected assignment and resume failure cleanup lacks runtime coverage` | `ifp-sha256:45d8590e0d77b920317797aa0d4a5ade2527f19a533a2f9ae307afe5b67e2a1f` | `kind:approved-design; strength:authoritative; evidence:docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md §6.1` |

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | Startup SIGCHLD and Unix PGID ownership | `src/main.rs`, `src/process_group.rs`, ownership tests | `R1` | runtime + contract | `Reviewed - no issue found` | Inherited auto-reap is normalized before runtime/child spawn; group signal remains tied to unreaped-leader proof. | macOS ownership tests passed; assumes no later competing reaper. |
| `A2` | Blocking runner cleanup/capture | `src/process_blocking.rs`, Unix runtime tests | `R1; R3` | runtime + code trace | `Reviewed - no issue found` | Normal completion, timeout, overflow, reader-start failure/panic, and incomplete output fail closed. | Timed-out descendant test passed 20/20 and under concurrent process pressure. |
| `A3` | Foreground cancellation and transfer | `src/execution.rs`, sandbox/host callers | `R4` | caller + runtime | `Reviewed - no issue found` | Abort-on-drop remains until explicit background handoff. | Descendant cancellation and full test suites passed. |
| `A4` | Background registry/shutdown | `src/mcp.rs`, `src/job_registry.rs` | `R4` | lifecycle trace + focused test | `Reviewed - no issue found` | Every drained job receives abort before join awaits; admission failure terminates, accepted jobs remain registry-owned. | Job-registry and batch tests passed; T2 composition gap remains. |
| `A5` | Managed Git/filter guard/tool environment | `src/managed_worktree_create.rs`, `src/managed_worktree_observe.rs`, `src/sandbox.rs` | `R3; R5` | authority trace + test | `Reviewed - no issue found` | Host Git executable and clean environment remain; configured filters refuse; incomplete queries fail closed. | Stable configured-filter integration test passed. Same-user config race is documented and not claimed atomic. |
| `A6` | Managed timeout/side-effect/retry lifecycle | `src/managed_worktree_prepare.rs`, creator | `R3` | durable state trace | `Reviewed - no issue found` | Timeout/incomplete outcomes stay unknown and attempts remain consumed through reconciliation. | 187 managed tests passed; T1 remains a composition gap. |
| `A7` | Windows Job Object runtime | `src/process_job.rs`, Windows process group/runner/tests | `R2` | static platform trace | `Not covered` | Suspended assignment/resume ordering appears fail-closed; runtime and failure injection not proven locally. | Require actual Windows CI; native SDK cross-check did not reach crate code. |
| `A8` | Linux Bubblewrap probe/helper | `src/bubblewrap_support.rs`, `src/bin/codex-linux-sandbox.rs`, process runner | `R5; R3` | static platform trace | `Not covered` | Probe uses 5 s and 4 KiB per stream via shared tree runner; Linux-only build/runtime not locally executed. | Linux CI must build helper and execute sandbox/runtime tests. |
| `A9` | Writer and execution-root authority | `src/execution.rs`, `src/writer.rs`, managed prepare | `R5` | contract/caller trace | `Reviewed - no issue found` | No writable-root widening or helper packaging change. | Existing tests and source trace. |
| `A10` | Complete cross-platform runtime matrix | project CI matrix | `Coordinator` | runtime | `Not covered` | Local macOS results cannot establish Linux/Windows behavior or the full matrix. | Require all 11 named CI jobs to complete successfully. |

## Subagent Candidate Adjudication

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `R1-C1` | `R1` | `dismissed` | `None` | startup order and SIGCHLD normalization test | Fixed under the documented standalone/no-competing-reaper invariant. |
| `R1-C2` | `R1` | `dismissed` | `None` | process-group escape witness and design §4 | Deliberate escape is outside the guarantee; no fake tree-wide claim is made. |
| `R2-C1` | `R2` | `merged` | `T3` | Windows spawn ordering and absence of injected failure tests | A test gap, not a demonstrated normal-path code defect; actual Windows CI remains necessary. |
| `R3-C1` | `R3` | `dismissed` | `None` | SECURITY.md §73–75 and stable filter test | Earlier Major filter-race candidate is a documented same-user check limitation, not an atomicity contract violation. |
| `R4-C1` | `R4` | `merged` | `T2` | synchronous batch abort and test/source trace | Code ordering is safe; missing real-tree composition belongs in the test-gap ledger. |
| `R5-C1` | `R5` | `dismissed` | `None` | host executable resolution, clean env, sandbox and retry paths | No authority widening or UNKNOWN-to-not-performed conversion found. |
| `R5-C2` | `R5` | `fixed in separate documentation chain` | `None` | docfix G0/G1 reports `bb006bdf` and `6ce556f7` | The stale prose about an uncontained probe and unbounded managed creator was corrected and separately re-reviewed. |

## Evidence Appendix

### Verification Commands

- `cargo test --locked --all-targets --quiet` -> two consecutive default-parallel runs after witness correction; each main target 1125/1125.
- `cargo test --locked --all-targets managed_worktree -- --quiet` -> 187/187.
- Focused runtime 11; process-group ownership 8; stress 9; job registry 18; sandbox 37; blocking reader 1 -> passed.
- Timeout descendant test -> 20/20 sequential; 4 concurrent copies plus Unix runtime/ownership/stress siblings all passed.
- `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, `git diff --check` -> passed.
- Windows target check failed before project compilation in third-party C dependencies because Windows SDK headers are unavailable on this host.
- Protected audit SHA256 -> unchanged.

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A7` | Windows Job runtime, outer/nested Job assignment, and injected failures | Cannot establish kernel behavior from macOS | Complete Windows matrix; targeted failure tests if feasible. |
| `A8` | Linux Bubblewrap helper/runtime | New `cfg(target_os=linux)` wiring has no native run | Complete Linux CI and sandbox tests. |
| `A10` | Full cross-platform matrix | Branch cannot be called closed based on macOS alone | All 11 named jobs green. |

## Prior Resolution Reconciliation

| Issue key | Issue fingerprint | Parent item/verdict | Relevant change or new evidence | Decision |
| --- | --- | --- | --- | --- |
| `test-gap; entry=managed worktree creation attempt; contract=ambiguous mutation attempts remain consumed and require reconciliation before retry; gap=managed creator timeout overflow and incomplete capture lack end-to-end durable lifecycle assertions` | `ifp-sha256:6547df29a5a3982782b9d23a38bf2a0df9777ea9c8ce28eaf7e14ba1707025ed` | `T1 deferred` | `kind:evidence; ref:cargo test managed_worktree 187/187 and current src/managed_worktree_prepare.rs trace; change:coverage remains separate rather than a single timeout composition test` | `kept open as T1` |
| `test-gap; entry=background job shutdown cancellation; contract=cancelled server shutdown terminates every drained owned execution tree; gap=batch abort regression uses pending tasks without a real descendant witness` | `ifp-sha256:a72aad9305e32afc84913f30315d1eb3b7113d49f5ec45cfdfeb90bf18980ac7` | `T2 deferred` | `kind:evidence; ref:src/mcp.rs batch-abort test and src/platform_runtime_tests.rs descendant witness; change:tests remain separate, so composition is still uncovered` | `kept open as T2` |
| `test-gap; entry=Windows child containment failure; contract=assignment and resume failures leave no child executing outside its Job and release handles; gap=Windows fault-injected assignment and resume failure cleanup lacks runtime coverage` | `ifp-sha256:45d8590e0d77b920317797aa0d4a5ade2527f19a533a2f9ae307afe5b67e2a1f` | `T3 deferred` | `kind:evidence; ref:Windows specialist review and absent native SDK; change:no fault injection or Windows runtime was available locally` | `kept open as T3` |

## Receiving Handoff

- Handoff status: `Terminal post-review - return to user/owner`
- Automatic receiving permitted: `No`
- Source report ID: `cr-20261007-b13b27a5`
- Scope fingerprint to recheck: `sha256:1a613c399abcb745dd9810e78ff2c2d6506e6ceccbd0b6bad740ed3efb041078`
- Actionable finding IDs: `None`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `None`
- Deferred test-gap IDs: `T1, T2, T3`
- Open question IDs: `None`
- Open coverage area IDs: `A7, A8, A10`
- Highest-risk verification to repeat: full platform CI; prioritize Windows Job owner-death and Linux Bubblewrap helper/runtime legs.
- Suggested implementation boundaries: keep residual same-user config race documented; do not widen authority to force atomicity.
- Re-review note: `Generation 1 is terminal; return remaining test gaps and platform evidence requirements to the owner.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or product owner.`

## Report Self-Check

- `yes` Complete parent resolution was read and every inherited T1/T2/T3/A7/A8/A10 item reconciled.
- `yes` Scope is implementation delta and affected execution chains; no unrelated Goal/performance work reopened.
- `yes` Every candidate is independently adjudicated; the filter TOCTOU disagreement is settled against the explicit SECURITY.md same-user race clause.
- `yes` Every changed review-relevant area has an A# row; A7/A8/A10 are Not covered with specific CI steps.
- `yes` Every test gap preserves a stable issue key and fingerprint.
- `yes` No accepted findings lack cards; recommendation is Discuss due Not-covered runtime areas and test gaps.
- `pending` Run generation-1 validator against the listed parent report and resolution.
- `yes` No Git state was mutated during review.
