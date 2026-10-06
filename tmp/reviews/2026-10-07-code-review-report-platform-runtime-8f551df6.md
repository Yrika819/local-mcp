# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261007-8f551df6`
- Review chain ID: `rc-20261007-8f551df6`
- Review generation: `0`
- Review trigger: `initial`
- Parent review report ID: `None`
- Parent review report path: `None`
- Parent resolution ID: `None`
- Parent resolution path: `None`
- Generated at: `2026-10-07T00:00:00Z`
- Report path: `tmp/reviews/2026-10-07-code-review-report-platform-runtime-8f551df6.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:1a613c399abcb745dd9810e78ff2c2d6506e6ceccbd0b6bad740ed3efb041078`

Treat this completed report as fixed input for receiving. Record later dispositions and implementation in a separate resolution report.

## Scope

- Review date: `2026-10-07`
- Scope kind: `branch diff`
- Scope description: Full Platform Runtime Closure branch diff plus current uncommitted implementation, against `origin/main` `f4bfb9e0c339d0338825a5bdb0ed7a6d994322b1`; includes process ownership, bounded blocking Git, Windows Job Objects, sandbox and host-tool environment, async cancellation, managed-worktree lifecycle, and the late Bubblewrap probe/test-witness hardening.
- Scope mode: `full frozen scope`
- Baseline: `origin/main` `f4bfb9e0c339d0338825a5bdb0ed7a6d994322b1`
- Target: working tree on `hardening/platform-runtime-closure-v1`, `HEAD` `23fa4af0a3acb49c6878773bd4c84be2cb490569`
- Changed paths: `18`
- Diff size: `2,390 additions / 83 deletions`
- Completion: `Complete within reviewed scope`
- Requirements consulted: user’s Platform Runtime Closure V1 task; `docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md`; `SECURITY.md`; `docs/MANAGED_WORKTREES_V1_DESIGN.md`; `docs/RESOURCE_BOUNDS_V1_DESIGN.md`; GoalLatch Maintainer authority and compatibility contracts.
- Prior resolution consulted: `None`
- Assumptions: process-group guarantees exclude descendants that deliberately escape containment; Linux/macOS abnormal host death remains an explicitly documented residual; Windows runtime claims require Windows CI. Same-user races in final Git-config/filesystem/executable checks are an acknowledged limitation in `SECURITY.md` §73–75.
- Excluded as unrelated: protected untracked `tmp/reviews/2026-10-05-resource-bounds-v1-stress-audit.md`; it is absent from the branch diff and remains byte-identical.

## Review Orchestration

- Assessment subagent: `Coordinator assessment - high-risk diff crosses Unix process ownership, Windows kernel Jobs, host-owned Git mutation, async cancellation, and sandbox boundaries.`
- Orchestration decision: `Parallel specialists`
- Decision confidence: `high`
- Decision rationale: independent OS-specific and side-effect/security semantics require distinct expertise; the coordinator owns synthesis and cross-path evidence.
- Coordinator override: `None`
- Context or tool limits: Linux-specific Bubblewrap probe and Windows Job runtime could not execute on the macOS host. Windows cross-check was attempted but stopped in dependency C compilation because the host lacks the Windows SDK (`windows.h`).

### Risk Dimensions

- Process-group and Job Object signaling must never target a stale or unrelated process identifier.
- Child trees must be terminated on ordinary completion, timeout, cancellation, server EOF, and output overflow without detached ownership.
- Git capture and timeout must remain bounded and fail closed without accepting incomplete observations or restoring retry budget.
- Repository/session state, model output, Git configuration, environment, and paths must not become execution authority or widen sandbox roots.
- Cross-platform runtime behavior cannot be inferred from macOS compilation or tests.

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `R1` | Unix process ownership and witness races | `src/process_group.rs`, `src/process_blocking.rs`, Unix `src/platform_runtime_tests.rs` | SIGCHLD auto-reap, leader unreaped proof, FIFO descriptor inheritance, descendant escape | Complete |
| `R2` | Windows Job Objects | `src/process_job.rs`, Windows process-group and blocking-runner branches/tests | suspended assignment, resume failure, handle lifetime, nested Jobs, abnormal owner death, unrelated process safety | Complete statically; Windows runtime pending |
| `R3` | Blocking Git and host-tool environment | `src/process_blocking.rs`, managed create/observe and verifier Git callers | bounds, timeouts, overflow/incomplete output, filter drivers, clean environment, unknown side effects | Complete |
| `R4` | Async cancellation and registry lifecycle | `src/execution.rs`, `src/job_registry.rs`, `src/mcp.rs`, affected `src/agent.rs` paths | foreground drop, background admission, stop/poll, shutdown cancellation, response errors | Complete |
| `R5` | Sandbox and authority semantics | `src/sandbox.rs`, `src/main.rs`, Writer/execution-root and retry callers | Bubblewrap/Seatbelt/Windows refusal, PATH shadowing, unsafe env forwarding, UNKNOWN/retry rules | Complete statically; Linux/Windows runtime pending |

### Synthesis Statement

The coordinator independently traced accepted candidates and their callers. No new code finding remains after adjudication. A proposed major finding about concurrent same-user Git-config changes is not treated as a regression: the non-atomic same-user check window is explicitly documented in `SECURITY.md` §73–75 and this patch does not claim serialization. Three focused test gaps and platform runtime coverage remain open and are recorded below.

## Review Snapshot

- Recommendation: `Discuss`
- Completion: `Complete within reviewed scope`
- Why now: static and macOS runtime evidence is favorable, but Windows/Linux platform runtime and three narrow lifecycle composition/failure tests remain unverified.
- Must-review now: `A10` platform runtime; `T1` managed timeout-to-reconciliation; `T3` Windows setup failure
  1. `A10` Linux/Windows/macOS CI runtime evidence
  2. `T1` managed creation timeout/overflow through durable reconciliation
  3. `T3` Windows assignment/resume failure cleanup
- Findings count: `Blocker 0 | Major 0 | Minor 0 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 3`
- Coverage confidence: `medium` static and macOS runtime; `low` cross-platform runtime
- Biggest blind spot: Windows Job assignment/kill-on-close behavior and Linux Bubblewrap probe execution in their native CI environments.

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
| `T1` | `Minor` | Managed worktree creation lifecycle | No single test drives `HostWorktreeCreator` timeout/overflow/incomplete capture through durable attempt consumption, mandatory reconciliation, and retry/block classification. | A future regression could connect generic runner error handling and managed retry logic incorrectly while their separate tests pass. | `Coordinator; R3` | `src/managed_worktree_create.rs` timeout/incomplete branch; `src/managed_worktree_prepare.rs` durable attempt/reconciliation flow; generic runner tests and managed lifecycle tests cover those sides separately. | `test-gap; entry=managed worktree creation attempt; contract=ambiguous mutation attempts remain consumed and require reconciliation before retry; gap=managed creator timeout overflow and incomplete capture lack end-to-end durable lifecycle assertions` | `ifp-sha256:6547df29a5a3982782b9d23a38bf2a0df9777ea9c8ce28eaf7e14ba1707025ed` | `kind:approved-design; strength:authoritative; evidence:docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md §7 and §12; src/managed_worktree_prepare.rs durable attempt/reconciliation path` |
| `T2` | `Minor` | Background shutdown cancellation | The regression proves every drained task receives abort before joins, but does not cancel the actual shutdown future while joining a batch containing a real descendant witness. | The composition of registry drain, shutdown-future cancellation, and tree teardown is supported by source trace and separate process-tree tests but not one integration test. | `Coordinator; R4` | `src/mcp.rs` batch-abort helper/test; `src/platform_runtime_tests.rs` real descendant cancellation witness. | `test-gap; entry=background job shutdown cancellation; contract=cancelled server shutdown terminates every drained owned execution tree; gap=batch abort regression uses pending tasks without a real descendant witness` | `ifp-sha256:a72aad9305e32afc84913f30315d1eb3b7113d49f5ec45cfdfeb90bf18980ac7` | `kind:approved-design; strength:authoritative; evidence:docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md §4; process ownership requirement` |
| `T3` | `Minor` | Windows containment setup failure | No Windows test injects Job assignment/resume failure and asserts the child never executes outside containment and cleanup completes. | The source ordering is fail-closed, but Windows-only API behavior and failure cleanup are not established by macOS tests. | `Coordinator; R2` | `src/process_group.rs` Windows `spawn_contained`; `src/process_blocking.rs` Windows setup path; Windows runtime tests cover successful assignment and owner death. | `test-gap; entry=Windows child containment failure; contract=assignment and resume failures leave no child executing outside its Job and release handles; gap=Windows fault-injected assignment and resume failure cleanup lacks runtime coverage` | `ifp-sha256:45d8590e0d77b920317797aa0d4a5ade2527f19a533a2f9ae307afe5b67e2a1f` | `kind:approved-design; strength:authoritative; evidence:docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md §6.1 and Windows Job assignment contract` |

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | Unix ownership proof and startup SIGCHLD policy | `src/main.rs`, `src/process_group.rs`, `src/process_group_ownership_tests.rs` | `R1` | runtime + contract trace | `Reviewed - no issue found` | SIGCHLD disposition is normalized before Tokio/runtime child spawn; negative group signaling remains tied to unreaped leader ownership. | Isolated ignored-SIGCHLD normalization test and ownership tests passed on macOS; no in-process competing reaper found. |
| `A2` | Blocking child capture, timeout, overflow, and cleanup | `src/process_blocking.rs`, `src/platform_runtime_tests.rs` | `R1; R3` | runtime + code trace | `Reviewed - no issue found` | Tree cleanup covers ordinary completion/timeout/overflow; capture limits, late overflow re-read, reader spawn and panic failures are fail-closed. | Focused timeout descendant witness passed 20/20 and under seven-way concurrent process pressure. |
| `A3` | Foreground cancellation and explicit background transfer | `src/execution.rs`, sandbox/host execution callers | `R4` | caller/callee trace + runtime | `Reviewed - no issue found` | Abort-on-drop retains task ownership unless explicitly transferred to the registry. | Foreground descendant test passed; complete parallel suites passed twice. |
| `A4` | Background admission, stop/poll, server EOF cleanup | `src/mcp.rs`, `src/job_registry.rs` | `R4` | state transition + focused test | `Reviewed - no issue found` | All drained jobs receive abort before the first join await; admission failure terminates, successful admission retains ownership. | Job-registry 18 tests and batch-abort tests passed; direct cancellation-to-real-descendant composition is T2. |
| `A5` | Host Git creation/observation identity and environment | `src/managed_worktree_create.rs`, `src/managed_worktree_observe.rs`, `src/sandbox.rs` | `R3; R5` | authority/environment trace + runtime | `Reviewed - no issue found` | Absolute host-resolved Git and clean environment remain; configured local/worktree filter drivers refuse creation; incomplete config query fails closed. | Stable configured-filter fixture passed. No atomic guarantee is claimed against a same-user config change; SECURITY.md documents that residual race. |
| `A6` | Managed mutation uncertainty and retry budget | `src/managed_worktree_create.rs`, `src/managed_worktree_prepare.rs` | `R3; Coordinator` | durable-state trace | `Reviewed - no issue found` | Timeout/incomplete capture is an error requiring reconciliation; attempts remain durably consumed and retries require proven no-side-effect evidence. | Managed-worktree 187 tests and full suite passed; composition test remains T1. |
| `A7` | Windows Job Object lifetime and blocking runner | `src/process_job.rs`, Windows branches of `src/process_group.rs`, `src/process_blocking.rs`, Windows tests | `R2` | static platform trace | `Not covered` | Suspended spawn -> assign -> resume ordering is statically fail-closed; `KILL_ON_JOB_CLOSE` and descendant owner-death tests exist. | Windows toolchain unavailable locally; require actual Windows CI, including outer/nested Job behavior and failure injection T3. |
| `A8` | Linux Bubblewrap gate, macOS Seatbelt, Windows sandbox/approval boundary | `src/bubblewrap_support.rs`, `src/bin/codex-linux-sandbox.rs`, `src/sandbox.rs`, `src/approvals.rs` | `R5; R3` | platform contract trace | `Not covered` | Linux probe now uses the bounded blocking runner; sandbox construction/approval authority is unchanged. | Native Linux Bubblewrap runtime and Windows approval/Job behavior require CI. macOS sandbox suite passed locally. |
| `A9` | Writer helper and managed execution root | `src/execution.rs`, `src/writer.rs`, `src/managed_worktree_prepare.rs` | `R5` | contract/caller trace | `Reviewed - no issue found` | No writable-root expansion or Writer atomic-helper authority change was found. | Static trace and existing full-suite tests; no new authority path. |
| `A10` | Complete platform runtime matrix | Linux, both macOS architectures, Windows jobs | `Coordinator` | runtime | `Not covered` | Local evidence is macOS x64 only. | Push to the unpublished branch only after local gates; require complete 11-job CI before claiming closure. |

## Subagent Candidate Adjudication

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `R1-C1` | `R1` | `dismissed` | `None` | `src/main.rs` startup ordering; isolated SIGCHLD test; `src/process_group.rs` proof | Startup resets inherited auto-reaping policy before runtime/child spawn; no code-installed competing reaper was found. This assumes standalone process ownership and no later competing reaper. |
| `R2-C1` | `R2` | `accepted` | `T3` | Windows setup branches are static-only on this macOS host. | No normal-path code defect is proven; failure injection and native runtime coverage remain missing. |
| `R3-C1` | `R3` | `dismissed` | `None` | `SECURITY.md` §73–75; configured-filter test; query-before-mutation source trace | Same-user config check/use race is explicitly acknowledged by the security contract; the patch rejects stable configured filters and does not claim an atomic snapshot. |
| `R4-C1` | `R4` | `merged` | `T2` | batch cancellation regression plus real process witness tests | Batch abort precedes awaits and is source-correct; missing test composes actual tree teardown with cancellation during shutdown. |
| `R5-C1` | `R5` | `dismissed` | `None` | `SECURITY.md` §73–75 and the generation-1 candidate report `tmp/reviews/2026-10-07-code-review-generation-1-platform-runtime-6bdc41.md` | The same-user check/use race is a documented residual; no managed-worktree contract promises a shared atomic Git-config snapshot. Do not claim it is serialized. |
| `R1-C2` | `R1` | `dismissed` | `None` | `src/platform_runtime_tests.rs` escaped-descendant witness; design §4 | A descendant that deliberately escapes a Unix process group is outside the stated guarantee; bounded caller cleanup is the contract. |
| `R5-C2` | `R5` | `dismissed` | `None` | host-resolved executable, `clean_git_environment()`, sandbox call graph, durable reconciliation | No repository-selected Git binary, unsafe environment forwarding, sandbox bypass, or UNKNOWN-to-not-performed conversion was found. |

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `Cargo.toml` | config | Windows Job API feature requirements |
| `docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md` | docs-only | guarantees, spawn inventory, residual limitations |
| `src/bin/codex-linux-sandbox.rs` | config/surface | shared bounded Bubblewrap probe and child signal policy |
| `src/bubblewrap_support.rs` | surface | Linux version-probe bounds and fail-closed classification |
| `src/execution.rs` | surface | foreground cancellation, host-native/Git execution, Writer authority |
| `src/job_registry.rs` | surface | cancellation request and job ownership |
| `src/main.rs` | config | signal normalization before runtime creation |
| `src/managed_worktree_create.rs` | surface | trusted Git mutation, filter configuration, bounds and side-effect uncertainty |
| `src/managed_worktree_creation_tests.rs` | test-only | filter refusal and managed lifecycle |
| `src/managed_worktree_observe.rs` | surface | read-only Git invocation and environment |
| `src/mcp.rs` | surface | admission, response paths, EOF cleanup |
| `src/platform_runtime_tests.rs` | test-only | Unix witness lifecycle and Windows Job runtime tests |
| `src/process_blocking.rs` | surface | bounds, timeout, output, process-tree teardown |
| `src/process_group.rs` | surface | Unix proof and Windows suspended Job assignment |
| `src/process_group_ownership_tests.rs` | test-only | ownership and SIGCHLD normalization |
| `src/process_job.rs` | surface | Windows Job creation, flags, handles, termination |
| `src/resource_limits.rs` | config | trusted Git capture ceilings |
| `src/sandbox.rs` | surface | sandbox policy and trusted Git limit re-export |

### Verification Commands

- `cargo fmt --all -- --check` -> passed.
- `git diff --check` -> passed.
- `cargo test --locked --all-targets --quiet` -> two consecutive default-parallel runs; each main test target: 1125 passed, 0 failed.
- `cargo test --locked --all-targets managed_worktree -- --quiet` -> 187 passed, 0 failed.
- Focused platform runtime 11; ownership 8; stress 9; job registry 18; sandbox 37; blocking-reader test 1 -> all passed.
- Failing timeout-descendant witness after fixture correction -> 20/20 focused passes; under contention, 4 copies plus Unix runtime, ownership, and stress sibling suites: all 7 passed.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> passed.
- `cargo check --locked --target x86_64-pc-windows-msvc --all-targets` -> blocked in third-party `ring`/`aws-lc-sys` C compilation because this macOS host lacks Windows SDK headers (`assert.h`, `windows.h`); no project Windows result.

### Dismissed Coordinator Candidates

| Candidate | Decision | Evidence |
| --- | --- | --- |
| Same-user Git filter config changes between query and checkout | dismissed as an approval-blocking defect; retained as documented residual | `SECURITY.md` §73–75 explicitly accepts same-user races in final Git-config/filesystem/executable checks because no shared atomic Git snapshot exists. Current code rejects filters observed before mutation but does not claim serialization. |
| Unix descendant starts a new process group/session | dismissed as a defect within current guarantee | Design §4 defines the contained tree as descendants that have not deliberately escaped; test verifies bounded caller behavior without claiming escapee termination. |
| Stale PID/PGID signal after observation loss | dismissed after startup normalization | Startup SIGCHLD normalization closes inherited auto-reap; blocking runner refuses to signal after ownership observation is lost. |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A7` | Windows Job assignment, nested outer Job, kill-on-close, and fault paths not executed locally | Static code cannot prove Windows kernel/API behavior | Run Windows x64/2022/arm64 CI; add fault-injection where feasible. |
| `A8` | Linux Bubblewrap probe and sandbox runtime not executed locally | New `cfg(target_os=linux)` helper wiring has no local runtime proof | Linux CI builds `codex-linux-sandbox` and runs sandbox/runtime tests. |
| `A10` | Complete 11-job platform matrix not yet run for this branch | Cannot claim cross-platform closure | Push branch normally, then wait for every named matrix job to complete successfully. |

## Prior Resolution Reconciliation

None - initial review generation.

## Receiving Handoff

- Handoff status: `Ready for receiving-code-review`
- Automatic receiving permitted: `Yes`
- Source report ID: `cr-20261007-8f551df6`
- Scope fingerprint to recheck: `sha256:1a613c399abcb745dd9810e78ff2c2d6506e6ceccbd0b6bad740ed3efb041078`
- Actionable finding IDs: `None`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `None`
- Deferred test-gap IDs: `T1, T2, T3`
- Open question IDs: `None`
- Open coverage area IDs: `A7, A8, A10`
- Highest-risk verification to repeat: full Linux/macOS/Windows CI matrix, especially Bubblewrap probe and Windows Job abnormal owner death.
- Suggested implementation boundaries: no production authority widening; add only focused integration/fault-injection tests if justified.
- Re-review note: `Treat every finding as a claim to verify. Challenges require a counterclaim, argument, evidence, limits, and settlement criterion.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or product owner.`

## Report Self-Check

- `yes` Actual assessment mode and rationale are recorded.
- `yes` Every changed review-relevant or unknown-impact area appears in the coverage ledger.
- `yes` No accepted final finding exists; no mismatched finding card is present.
- `yes` Every standalone test gap has a stable issue key, fingerprint, and authoritative basis.
- `yes` Scope, chain, generation, trigger, and handoff are consistent.
- `yes` Meaningful subagent candidates are adjudicated.
- `yes` Every Not-covered area has a reason and next verification step.
- `yes` Recommendation is Discuss because platform runtime areas remain Not covered.
- `pending` Run the code-review validator after file creation.
- `yes` Git state was not mutated during review.
