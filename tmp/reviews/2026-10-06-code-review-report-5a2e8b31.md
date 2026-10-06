# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261006-5a2e8b31`
- Review chain ID: `rc-20261006-5a2e8b31`
- Review generation: `0`
- Review trigger: `initial`
- Parent review report ID: `None`
- Parent review report path: `None`
- Parent resolution ID: `None`
- Parent resolution path: `None`
- Generated at: `2026-10-06T00:00:00Z`
- Report path: `tmp/reviews/2026-10-06-code-review-report-5a2e8b31.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:2350f029b563ba254d6bdc411da4774bfe2f35fd142a949cba9bd3bcd5527185`

Treat this completed report as the fixed review input for downstream work. Do not rewrite it during receiving or implementation; record any later dispositions and verification in a separate resolution report.

## Scope

- Review date: `2026-10-06`
- Scope kind: `commit range`
- Scope description: Read-only security review of Platform Runtime Closure branch commit `23fa4af0a3acb49c6878773bd4c84be2cb490569` against baseline `f4bfb9e0c339d0338825a5bdb0ed7a6d994322b1`, focusing on Linux Bubblewrap/macOS Seatbelt sandbox invariants, Windows containment and approval boundaries, authority/tool and environment resolution, timeout/abnormal-cleanup side effects and retry behavior, managed execution roots, and Writer atomic publication impact.
- Scope mode: `full frozen scope`
- Baseline: `f4bfb9e0c339d0338825a5bdb0ed7a6d994322b1`
- Target: `23fa4af0a3acb49c6878773bd4c84be2cb490569` (`test: prove abnormal Windows owner death containment`)
- Changed paths: `10`
- Diff size: `1,873 additions / 32 deletions`
- Completion: `Complete within reviewed scope`
- Requirements consulted: User's explicit security-review scope; `docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md`; changed code's stated invariants; existing managed-worktree authority/reconciliation contracts in `src/managed_worktree_prepare.rs` and `src/managed_worktree_create.rs`.
- Prior resolution consulted: `None`
- Assumptions: Windows Job Object behavior and macOS/Linux kernel sandbox runtime behavior must be proven on their respective OSes; target/baseline commits define scope, while current worktree changes are excluded. Review makes no product claim that Windows has a filesystem/network sandbox: the checked-in API documents it does not.
- Excluded as unrelated: Existing untracked `tmp/reviews/2026-10-05-resource-bounds-v1-stress-audit.md`; no current worktree changes were included. No unrelated Writer implementation or managed Phase 5 changes were reviewed as branch changes.

## Review Orchestration

- Assessment subagent: `Coordinator assessment - cohesive runtime-closure diff crosses independent OS process-containment, sandbox setup, and mutation/retry invariants; no subagent execution tool was available`
- Orchestration decision: `Single reviewer`
- Decision confidence: `medium`
- Decision rationale: The core change is cohesive around child lifetime, and all platforms meet at `ProcessGroup`/`process_blocking`; splitting would duplicate critical process ownership reasoning. Coordinator covered OS-specific branches and separately traced managed side effects, authority/environment and caller integrations. Independent platform runtime verification would improve confidence but cannot be delegated or run here.
- Coordinator override: `None`
- Context or tool limits: Subagents unavailable. Current host is macOS; target-platform Linux Bubblewrap and Windows Job execution were unavailable. Focused Cargo tests repeatedly timed out while compiling dependencies before test execution; no test result was obtained. No Windows approval UI or CI execution available.

### Risk Dimensions

- Process containment is a security boundary: unsafe PID/PGID signaling, a spawn-to-assignment race, or lost descendants on cleanup can affect processes outside the requested execution.
- Sandbox setup and fallback classification must not convert Bubblewrap/Seatbelt setup failures into permission approvals or unsandboxed retries.
- `git worktree add` is a host-authorized mutation: timeout/output truncation must not be misclassified as “not performed” or return a durable attempt to the retry budget.
- Clean-environment/executable resolution must keep host-owned Git from inheriting tool/config/code injection channels or moving managed repository authority.
- Windows approval wait and Writer atomic helper integration were checked as affected-context paths; no branch delta was found in either contract.

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `Coordinator` | Security, process lifecycle, authority/retry, platform contracts | All 10 changed paths plus sandbox/fallback, approvals, managed preparation, execution, and Writer publication callers | Unix unreaped-owner rule; suspended Windows assignment; Bubblewrap/Seatbelt and Windows fallback behavior; Git identity/environment; timeout unknown-state propagation; managed execution root; Writer helper boundary | Complete - static trace; runtime validation blocked |

### Synthesis Statement

No sandbox bypass, authority widening, retry-budget restoration, or unsafe process identifier signaling was identified by static trace. The coordinator independently traced Linux/macOS wrapper start evidence and Windows Job assignment/drop behavior; followed managed creation timeout errors through durable pre-consumption and read-only reconciliation; checked the host Git resolver and clean environment; and verified the unchanged execution-root and atomic Writer helper boundaries. One targeted end-to-end test gap remains for managed mutation timeout/incomplete-capture classification, and target OS execution could not be established locally; both are recorded below rather than presented as proven runtime safety.

## Review Snapshot

- Recommendation: `Discuss`
- Completion: `Complete within reviewed scope`
- Why now: Static security invariants and the mutation/retry path appear preserved, but the required Linux/Windows platform executions were not available and the managed timeout-to-durable-reconciliation path lacks a dedicated regression test.
- Must-review now: `A9` platform-runtime evidence; `T1` managed ambiguous-mutation lifecycle test
  1. `A9` Windows Job Object and Linux Bubblewrap platform execution
  2. `T1` timeout/overflow through actual managed creation and durable lifecycle
- Findings count: `Blocker 0 | Major 0 | Minor 0 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 1`
- Coverage confidence: `medium` static; `low` cross-platform runtime
- Biggest blind spot: Windows-only Job Object API/lifetime execution, followed by Linux Bubblewrap setup and macOS Seatbelt runtime.

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
| `T1` | `Minor` | Managed `git worktree add` timeout/overflow and durable retry/reconciliation | No targeted test drives `HostWorktreeCreator` timeout, output overflow, or incomplete capture through `prepare_managed_workspace_with_policy` and asserts the already-consumed attempt remains consumed, reconciliation runs, and no retry happens absent proven no-side-effect evidence. Existing generic runner tests cover timeout/overflow and managed lifecycle tests cover lost-response/retry behavior separately. | A later regression could accidentally interpret an ambiguous mutating command as a retryable spawn failure while preserving individually passing helper and lifecycle tests. | `Coordinator` | Static trace: `src/managed_worktree_create.rs:323-344`; `src/managed_worktree_prepare.rs:745-797`; generic tests `src/platform_runtime_tests.rs:271-285,352-370`; existing lifecycle tests `src/managed_worktree_creation_tests.rs:2551-2561,2632-2665`. | `test-gap; entry=managed worktree creation attempt; contract=ambiguous mutation attempts remain consumed and require reconciliation before retry; gap=managed creator timeout overflow and incomplete capture lack end-to-end durable lifecycle assertions` | `ifp-sha256:6547df29a5a3982782b9d23a38bf2a0df9777ea9c8ce28eaf7e14ba1707025ed` | `kind:approved-design; strength:authoritative; evidence:docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md §7 and §12; src/managed_worktree_prepare.rs:745-797` |

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | Linux Bubblewrap setup, macOS Seatbelt profile and fallback classification | `src/sandbox.rs:244-269,366-478,495-583`; fallback caller | `Coordinator` | contract trace | `Reviewed - no issue found` | Linux retains parent-side Bubblewrap support gate before spawn; command still built by Landlock profile helper; macOS retains Seatbelt args with permission-derived roots/network; typed setup rejection remains Linux-only and production restricted. Windows explicitly does not claim a filesystem/network sandbox. | No relevant sandbox policy construction change in this branch. Linux/macOS runtime not executed; see A9. |
| `A2` | Windows approval consent boundary and shutdown behavior | `src/approvals.rs` (unchanged), design §8 | `Coordinator` | contract trace | `Reviewed - no issue found` | Branch does not change approval request/reply or consent semantics. Design explicitly records unbounded Windows reply wait as a known availability issue and excludes it because timeout changes consent behavior. | No changed approval code; no issue attributable to this diff. |
| `A3` | Windows Job Object creation, suspended spawn, assignment, resume, kill-on-close | `src/process_job.rs:85-215`; `src/process_group.rs:250-302`; runtime tests | `Coordinator` | dependency trace | `Reviewed - no issue found` | Creates unnamed Job with kill-on-close, spawns suspended, assigns before resume, errors closed on failed assignment/resume; Job is held in lease and terminated on drop; abnormal owner-death witness test intentionally aborts child owner. | Static only; Windows kernel/runtime test unavailable here (A9). |
| `A4` | Linux/macOS process-group identity ownership, termination and drop | `src/process_group.rs:98-117,160-233,318-385`; `src/process_blocking.rs:293-408,486-503` | `Coordinator` | dependency trace | `Reviewed - no issue found` | Negative PGID kill remains gated by live unreaped direct-child ownership; blocking runner uses `waitid(WNOWAIT)` and avoids signaling after loss of proof. Failed observation kills only direct child and reports unknown. | macOS/Linux runtime test command did not reach test execution. |
| `A5` | Blocking process capture, deadlines, overflow, abnormal cleanup | `src/process_blocking.rs:119-203,205-265,268-503`; `src/platform_runtime_tests.rs:220-418` | `Coordinator` | code-path trace | `Reviewed - no issue found` | Concurrent readers avoid pipe deadlock; output cap is detected at limit+1 and causes tree termination; timed-out/incomplete outcomes are explicit; post-kill wait and reader drain are bounded. Unix tests cover leader-exited descendants, timeout, overflow, and escaped descendants. | Tests were inspected but Cargo test timed out during compile. Escaped descendants beyond process-group/Job enforcement are intentionally not claimed as contained; only caller wait is bounded. |
| `A6` | Host Git binary identity and environment/configuration boundaries | `src/managed_worktree_observe.rs:121-154`; `src/managed_worktree_create.rs:235-277,298-350`; `src/execution.rs` resolver; `src/sandbox.rs:1435-1479` | `Coordinator` | authority/environment trace | `Reviewed - no issue found` | Read-only seam now uses validated host Git identity and `env_clear` plus the narrow safe Git environment; mutator retains cached identity and strips GIT_/loader/config-location overrides while preserving the already-resolved binary. Exact read-only allowlist and typed mutator remain separate. | Confirmed clean env includes PATH/SystemRoot essentials and suppresses system/global Git config. No model-selectable argv or executable added. |
| `A7` | Managed execution root, mutation timeout and durable retry semantics | `src/managed_worktree_create.rs:298-350`; `src/managed_worktree_prepare.rs:745-797`; managed lifecycle tests | `Coordinator` | contract trace | `Reviewed - no issue found` | Creation remains rooted at validated `record.primary_root()` and `ManagedWorktreeCreation` derives target/branch/base from durable intent. Attempt is persisted before Git. Timeout/capture error maps to observation failure evidence; post-attempt read-only reconciliation must run; only classified proven no-side-effect state loops to retry. | No retry/replenishment defect found. Add T1 to pin timeout/overflow through this entire lifecycle. |
| `A8` | Writer atomic helper, execution root and sandbox writable-root impact | `src/execution.rs:27-60,98-129`; `src/sandbox.rs:366-478`; branch diff inventory | `Coordinator` | caller/contract trace | `Reviewed - no issue found` | No Writer/atomic helper or execution-root implementation change in target. Existing Unix publication still invokes host-owned helper through `sandbox::run` with validated parent as sole writable root; Windows limitation remains explicit. Shared ProcessGroup containment only narrows child lifetime. | Checked as affected context; not a changed publication authority. |
| `A9` | Cross-platform runtime verification (Linux Bubblewrap, macOS Seatbelt, Windows Job Objects) | `src/platform_runtime_tests.rs`; CI not available | `Coordinator` | static platform trace | `Not covered` | Windows-specific Jobs and OS sandbox behavior cannot be established by this macOS host. Three focused Cargo invocations timed out during dependency compilation before test execution; therefore no target tests passed or failed. | Run full platform CI including Windows x64/Windows 2022 and Linux/macOS test targets; specifically execute `platform_runtime_tests` and sandbox setup tests. |
| `A10` | Change inventory and module/dependency integration | `Cargo.toml`, `src/main.rs`, `docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md`, all runtime source/tests | `Coordinator` | diff + dependency trace | `Reviewed - no issue found` | Added Windows API feature gates match `process_job` imports; modules are cfg-scoped; design documents residual Linux/macOS abnormal-host-death limitation, unbounded Windows approval wait, and Bubblewrap probe caveat rather than promising universal behavior. | Existing untracked review artifact was excluded; Git status was not changed by review. |

## Subagent Candidate Adjudication

No subagents were available. Coordinator candidates were independently adjudicated below.

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `C1` | `Coordinator` | `dismissed` | `None` | `src/process_group.rs:318-385`; `src/process_blocking.rs:293-408,486-503` | Suspected PGID reuse/foreign-process signaling is not supported: signal occurs only while the direct-child leader remains unreaped; on loss of observation, only the direct child is killed. |
| `C2` | `Coordinator` | `dismissed` | `None` | `src/process_blocking.rs:149-170`; `src/process_group.rs:263-302`; `src/process_job.rs:121-197` | Suspected Windows spawn/assignment escape window is closed structurally by suspended creation and assign-before-resume; failure paths do not resume an uncontained child. Requires Windows CI for runtime proof, but no static defect was found. |
| `C3` | `Coordinator` | `dismissed` | `None` | `src/managed_worktree_prepare.rs:745-797`; `src/managed_worktree_create.rs:323-344` | Suspected timeout being treated as retry permission is contradicted by durable attempt consumption before invoke and mandatory reconciliation afterward; retry loop only continues for a reconciliation state that permits bounded retry. |
| `C4` | `Coordinator` | `dismissed` | `None` | `src/managed_worktree_observe.rs:121-154`; `src/sandbox.rs:1435-1479`; `src/managed_worktree_create.rs:252-277` | Suspected host Git/tool-resolution/environment widening is not borne out: validated host binary identity, fixed allowlisted read-only forms, GIT/config/loader scrubbing and separate typed mutation API remain. |
| `C5` | `Coordinator` | `dismissed` | `None` | `src/execution.rs:27-60,98-129`; `src/sandbox.rs:366-478`; diff inventory | Suspected Writer atomic helper or managed execution-root change is not in the target diff. Existing helper remains called with validated parent writable root; no authority widening found. |
| `C6` | `Coordinator` | `accepted` | `T1` | Generic blocking-runner timeout/overflow tests and managed retry/reconciliation tests exist separately, but no test binds the new HostWorktreeCreator timeout/capture error to durable managed-attempt state and reconciliation. | Meaningful, non-blocking regression-test gap; implementation trace itself appears correct. |

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `Cargo.toml` | config | Windows API dependency feature surface |
| `docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md` | docs-only | stated contracts, residual limitations, verification obligations |
| `src/main.rs` | config | cfg module wiring |
| `src/managed_worktree_create.rs` | surface | typed host Git mutation, env, timeout and unknown side-effect handling |
| `src/managed_worktree_observe.rs` | surface | read-only allowlist, Git identity/environment, timeout failure classification |
| `src/platform_runtime_tests.rs` | test-only | real process descendant witnesses and platform tests |
| `src/process_blocking.rs` | surface | deadlines, bounded capture, tree cleanup and identifier ownership |
| `src/process_group.rs` | surface | Unix group ownership and Windows process lease integration |
| `src/process_job.rs` | surface | Windows Job assignment/lifecycle/kill-on-close |
| `src/sandbox.rs` | surface | shared clean Git environment (one line change), sandbox/fallback integration |
| `src/approvals.rs` | dependency/context | unchanged Windows approval wait, explicitly reviewed as out-of-diff context |
| `src/execution.rs` | dependency/context | unchanged host Git resolver, managed execution root, Writer atomic helper boundary |
| `src/managed_worktree_prepare.rs` | dependency/context | durable attempt consumption and post-mutation reconciliation |

### Verification Commands

- `git --no-pager diff --stat f4bfb9e0c339d0338825a5bdb0ed7a6d994322b1 23fa4af0a3acb49c6878773bd4c84be2cb490569` -> 10 changed paths, 1,873 additions / 32 deletions.
- `git --no-pager diff --check f4bfb9e0c339d0338825a5bdb0ed7a6d994322b1 23fa4af0a3acb49c6878773bd4c84be2cb490569` -> clean.
- `cargo test --locked --all-targets platform_runtime_tests -- --nocapture` -> timed out during dependency compilation; test binary did not run.
- `cargo test --locked --all-targets managed_worktree_creation_tests -- --nocapture` -> timed out during dependency compilation/build lock contention; test binary did not run.
- Repeated `cargo test --locked --all-targets platform_runtime_tests -- --nocapture` with a 300-second bound -> timed out compiling dependencies at 510/542; test binary did not run.
- Git status was read before/after; pre-existing untracked `tmp/reviews/2026-10-05-resource-bounds-v1-stress-audit.md` was left untouched. This report is newly created and is the only review artifact added.

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `A1` | sandbox construction | [`sandbox.rs`](/Users/yuta/local-mcp-connector-parity/src/sandbox.rs#L366) | Linux/macOS sandbox profile and Windows explicit non-sandbox behavior. |
| `A3` | Windows process lifetime | [`process_job.rs`](/Users/yuta/local-mcp-connector-parity/src/process_job.rs#L85) | kill-on-close Job creation, assignment, resume and termination. |
| `A4` | Unix group lifetime | [`process_group.rs`](/Users/yuta/local-mcp-connector-parity/src/process_group.rs#L318) | Explicit PGID ownership proof gates negative group signaling. |
| `A5` | bounded process capture | [`process_blocking.rs`](/Users/yuta/local-mcp-connector-parity/src/process_blocking.rs#L119) | Capture limits, timeout result, reader drain and process cleanup. |
| `A6` | host Git environment | [`managed_worktree_observe.rs`](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_observe.rs#L121) | Resolved trusted Git identity and cleared/rebuilt environment. |
| `A7` | mutation and retries | [`managed_worktree_prepare.rs`](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L745) | Durable attempt count and reconciliation before bounded retry. |
| `A8` | atomic Writer publication | [`execution.rs`](/Users/yuta/local-mcp-connector-parity/src/execution.rs#L27) | Helper path and sandbox writable-root boundary were unchanged. |
| `T1` | test gap | [`platform_runtime_tests.rs`](/Users/yuta/local-mcp-connector-parity/src/platform_runtime_tests.rs#L271) | Current tests assert generic runner timeout but not managed durable state transition. |

### Dismissed Coordinator Candidates

| Candidate | Decision | Evidence |
| --- | --- | --- |
| Bubblewrap/Seatbelt setup failure can trigger host fallback | dismissed | Linux typed setup refusal remains pre-spawn and terminal; macOS wrapper lifecycle remains unproven, not host-proven “not started”; `sandbox.rs:502-583` plus unchanged fallback classifier semantics. |
| Windows approval timeout/consent behavior changed | dismissed | `src/approvals.rs` was not modified; design §8 explicitly defers bounded reply semantics as a product decision. |
| Abnormal Linux/macOS host death leaves descendants despite group cleanup | dismissed as branch defect | Design §6 explicitly documents this residual limitation and rejects falsely claiming parent-death tree cleanup. It remains a known platform limitation, not a regression introduced here. |
| Timeout may reset creation attempt budget | dismissed | Durable attempt is consumed before Git, errors are evidence, mandatory observation follows and only a “permits bounded retry” reconciliation state loops (`managed_worktree_prepare.rs:745-797`). |
| Reader thread can wait forever on an escaped process | dismissed as caller hang | `join_reader_bounded` detaches after five seconds and marks capture incomplete; caller rejects the capture (`process_blocking.rs:70-76,185-203,248-265`). The escapee itself is explicitly outside process-group guarantee. |
| Writer helper binary or execution root authority changed | dismissed | Neither Writer helper nor execution-root code changes in diff; existing `execution.rs:27-60` still routes through `sandbox::run` with the validated parent as the only writable root. |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A9` | Linux Bubblewrap, Windows Job Objects/abnormal-owner death and full platform matrix not executed; macOS sandbox tests also did not run because compilation timed out. | Static traces cannot establish OS API behavior, platform compile correctness, or actual cleanup witnesses. | Complete the project’s Linux/macOS/Windows CI matrix; run the new `platform_runtime_tests` and sandbox support suites, especially Windows abnormal owner-death test. |

## Prior Resolution Reconciliation

None - initial review generation.

## Receiving Handoff

- Handoff status: `Ready for receiving-code-review`
- Automatic receiving permitted: `No`
- Source report ID: `cr-20261006-5a2e8b31`
- Scope fingerprint to recheck: `sha256:2350f029b563ba254d6bdc411da4774bfe2f35fd142a949cba9bd3bcd5527185`
- Actionable finding IDs: `None`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `T1`
- Deferred test-gap IDs: `None`
- Open question IDs: `None`
- Open coverage area IDs: `A9`
- Highest-risk verification to repeat: Full platform CI, prioritizing Windows Job Object lifecycle/abnormal owner death, followed by Linux Bubblewrap and macOS Seatbelt test execution; add the T1 managed timeout-to-reconciliation test.
- Suggested implementation boundaries: Keep any test addition confined to managed creator/prepare test seams; do not alter approval consent, fallback authority, Writer roots, or creation retry classification absent failing evidence.
- Re-review note: `Treat every finding as a claim to verify. Challenges require a counterclaim, argument, evidence, limits, and settlement criterion.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or product owner.`

## Report Self-Check

- `yes` Actual assessment mode and rationale are recorded; subagents were unavailable and the coordinator performed one coherent review.
- `yes` Every changed review-relevant or unknown-impact area appears once in `Review Coverage Ledger`.
- `yes` Every final finding appears once in the index and once as a matching card; no code findings were accepted.
- `yes` Every `Finding F#` area references an existing finding; none are claimed.
- `yes` Every standalone test gap has a stable ID and severity.
- `yes` `T1` has a unique semantic issue fingerprint and approved-design basis.
- `yes` Generation, trigger, parent resolution, scope mode, and receiving handoff satisfy the bounded chain contract.
- `yes` Generation 1 reconciliation is not applicable.
- `yes` Test gap `T1` is actionable exactly once; uncovered area `A9` appears in the open coverage list.
- `yes` Meaningful coordinator candidates are adjudicated.
- `yes` `A9` has an exact reason and next verification step.
- `yes` Recommendation is `Discuss` because an unknown-impact platform runtime area remains Not covered.
- `pending` Report validator to be run after generation.
- `yes` Git state was not mutated during review; only this report file was written.
