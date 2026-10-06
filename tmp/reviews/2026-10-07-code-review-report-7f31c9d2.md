# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261007-7f31c9d2`
- Review chain ID: `rc-20261007-7f31c9d2`
- Review generation: `0`
- Review trigger: `initial`
- Parent review report ID: `None`
- Parent review report path: `None`
- Parent resolution ID: `None`
- Parent resolution path: `None`
- Generated at: `2026-10-07T00:00:00Z`
- Report path: `tmp/reviews/2026-10-07-code-review-report-7f31c9d2.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:bc7e21df660ecb4c2702e0a18a2be66cc6ec0d1e7a2807f4eb8fa70157eda07b`

## Scope

- Review date: `2026-10-07`
- Scope kind: `working tree`
- Scope description: Read-only security review of the current diff in `src/execution.rs`, `src/managed_worktree_create.rs`, `src/platform_runtime_tests.rs`, and `src/process_blocking.rs`, tracing affected sandbox, managed-worktree preparation/observation, side-effect/retry, approval, execution-root, and Writer helper contracts.
- Scope mode: `full frozen scope`
- Baseline: `HEAD 23fa4af0a3acb49c6878773bd4c84be2cb490569`
- Target: `working tree`
- Changed paths: `4`
- Diff size: `296 additions / 112 deletions`
- Completion: `Complete within reviewed scope`
- Requirements consulted: User's explicit security invariants; current `src/sandbox.rs`, managed worktree implementation, fallback side-effect model, and Writer/execution contracts.
- Prior resolution consulted: `None`
- Assumptions: Security conclusions are based on static traces plus macOS execution of the Unix runtime tests. Linux/Bubblewrap and Windows Job Object runtime behavior were not executed.
- Excluded as unrelated: Pre-existing untracked files and unrelated source changes; no changes were made to source files.

## Review Orchestration

- Assessment subagent: `Coordinator assessment - cohesive process ownership/security scope; independent partitions would repeatedly require the same lifecycle trace`
- Orchestration decision: `Single reviewer`
- Decision confidence: `high`
- Decision rationale: The working diff is small and shares a single ownership/cleanup chain. Sandbox, mutation/retry, and authority surfaces were traced as relevant callers/contracts by the coordinator.
- Coordinator override: `None`
- Context or tool limits: No subagent execution tool used. Host is macOS; Linux and Windows runtime paths unavailable.

### Risk Dimensions

- Unix process-group signals require proof of process identity ownership; stale PID/PGID signaling could affect unrelated processes.
- Managed `git worktree add` mutates durable repository state; an uncertain result must not be upgraded to no-side-effect evidence or replenish retry budget.
- Host Git identity/environment, platform sandbox setup, and Writer's writable-root boundary are authority controls.
- Windows approval and process containment have OS-specific behavior not executable on this host.

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `Coordinator` | Security, lifecycle, authority, state/retry | Four changed files plus sandbox, process group/job, managed prepare/observe, fallback, and Writer callers | No sandbox bypass; trusted tool/env; ownership-gated signaling; UNKNOWN propagation; no retry restoration; unchanged execution root/writer authority | Complete - static trace and macOS tests |

### Synthesis Statement

No implementation finding was identified. The coordinator traced the changed Unix ownership-loss branch through bounded capture and managed mutation errors into durable reconciliation; verified the clean Git environment and absolute host Git identity; checked the foreground abort guard's ownership transfer; and verified that sandbox policy, approval code, execution roots, and Writer helper authority are unchanged. Linux and Windows platform execution remains a blind spot, not proof of a defect.

## Review Snapshot

- Recommendation: `Discuss`
- Completion: `Complete within reviewed scope`
- Why now: Changed paths passed focused macOS runtime tests and static invariants, but Linux/Bubblewrap and Windows-specific behavior remain unverified.
- Must-review now: `A7` Linux/Windows runtime verification
  1. `A7` Linux Bubblewrap and Windows Job Object execution
- Findings count: `Blocker 0 | Major 0 | Minor 0 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 0`
- Coverage confidence: `medium`
- Biggest blind spot: Windows Job Object and Linux/Bubblewrap runtime behavior.

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

None. Platform executions unavailable on this host are recorded as a `Not covered` area rather than a code finding or standalone test gap.

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | Foreground task ownership and cancellation | `src/execution.rs:22-45,157-170,1687-1697`; runtime tests | `Coordinator` | dependency trace | `Reviewed - no issue found` | Guard aborts while retaining ownership; `into_inner` explicitly transfers to background registry. Focused test proves dropping owner terminates descendant. |
| `A2` | Bounded blocking process capture, overflow and timeout | `src/process_blocking.rs:125-240,256-303,332-385` | `Coordinator` | changed-code trace | `Reviewed - no issue found` | Reader spawn failures terminate contained tree; reader panic/timeout is incomplete; overflow sampled after joins and callers reject incomplete output. |
| `A3` | Unix PID/PGID ownership on lost child observation | `src/process_blocking.rs:342-380,406-444` | `Coordinator` | security trace | `Reviewed - no issue found` | If `waitid` loses proof, code does not signal remembered PID/PGID and returns synthetic unobserved status with timeout/unknown indication. Potential orphaning under external auto-reaping is fail-closed rather than unsafe signaling. |
| `A4` | Windows Job Object containment and assignment/resume | `src/process_blocking.rs:133-170,460-520`; `src/process_job.rs:85-215` | `Coordinator` | contract trace | `Reviewed - no issue found` | Suspended spawn, assign-before-resume, kill-on-close and fail-closed assignment/resume paths remain present. Windows runtime unavailable; see A7. |
| `A5` | Managed creator executable/environment and unknown mutation result | `src/managed_worktree_create.rs:234-321`; `src/managed_worktree_observe.rs:118-159`; `src/sandbox.rs:1435-1479` | `Coordinator` | authority/data-flow trace | `Reviewed - no issue found` | Uses validated absolute cached host Git; clears inherited environment and reconstructs allowlisted safe environment plus Git config isolation. Timeout, overflow, and incomplete capture become errors, not a `ConfirmedNotPerformed` verdict. |
| `A6` | Durable attempt consumption, reconciliation, retry and other authority boundaries | `src/managed_worktree_prepare.rs:745-797`; `src/fallback.rs:853-865,1336-1372`; `src/execution.rs:53-75` | `Coordinator` | caller/contract trace | `Reviewed - no issue found` | Managed attempt is persisted before invocation; observation follows each outcome; only classified proven no-side-effect reconciliation retries. UNKNOWN locks fallback budget. Writer helper still runs through sandbox with validated parent as writable root; execution root unchanged. Approval code unchanged. |
| `A7` | Linux/macOS/Windows platform runtime matrix | `src/platform_runtime_tests.rs`; platform-specific branches in `src/sandbox.rs`, `src/process_blocking.rs`, `src/process_job.rs` | `Coordinator` | static platform trace | `Not covered` | Linux Bubblewrap and Windows Job Object runtime/API behavior were not executed; only macOS runtime was available. | Run platform CI and sandbox/process tests on Linux and Windows to verify runtime and API behavior. |

## Subagent Candidate Adjudication

No subagents were used. Coordinator candidates were adjudicated directly.

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `C1` | `Coordinator` | `dismissed` | `None` | `src/process_blocking.rs:342-380`; `src/process_group.rs:160-230` | Concern that ownership-loss path could signal an unrelated PID/PGID is contradicted: it returns unknown without signaling either identifier. It can leave an unobservable process running, but signaling cannot safely be justified after ownership proof is lost. |
| `C2` | `Coordinator` | `dismissed` | `None` | `src/managed_worktree_create.rs:289-314`; `src/managed_worktree_prepare.rs:745-797` | Concern that timeout/output failure restores retry budget is contradicted by durable attempt consumption and mandatory observation before any retry. |
| `C3` | `Coordinator` | `dismissed` | `None` | `src/sandbox.rs:366-477,495-583`; `src/fallback.rs:1345-1372` | No changed sandbox construction or fallback authority. Linux setup refusal remains typed host-side refusal; wrapper setup ambiguity is not promoted to a no-side-effect proof. Windows explicitly is not represented as an OS filesystem/network sandbox. |
| `C4` | `Coordinator` | `dismissed` | `None` | `src/managed_worktree_create.rs:276-303`; `src/sandbox.rs:1435-1469` | Host Git runs by resolved absolute identity; environment is cleared/rebuilt; command shape is typed/frozen. PATH is retained as host environment for child lookup, not used to resolve the Git executable in this invocation. No repo-selected executable was found in the reviewed path. |
| `C5` | `Coordinator` | `dismissed` | `None` | `src/execution.rs:53-75,87-121`; diff inventory | Writer helper/execution-root implementation is unchanged; helper path remains host-derived and only the validated request parent is writable under Unix sandbox. |
| `C6` | `Coordinator` | `dismissed` | `None` | `src/approvals.rs` unchanged; `src/sandbox.rs:450-459` | No change to Windows approval/consent behavior. Windows command execution remains explicitly documented as lacking application filesystem/network sandboxing; no silent platform claim was introduced. |

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `src/execution.rs` | surface | Task ownership/cancellation; Writer and execution-root boundary context |
| `src/managed_worktree_create.rs` | surface | Host Git tool identity, environment, timeout, unknown mutation result |
| `src/platform_runtime_tests.rs` | test-only | Unix FIFO witness and Windows descendant containment tests |
| `src/process_blocking.rs` | surface | Reader setup/cleanup, unknown outcome, safe process signaling |
| `src/sandbox.rs` | dependency | Bubblewrap/Seatbelt setup, Windows no-sandbox disclosure, safe env and fallback lifecycle |
| `src/managed_worktree_prepare.rs` | dependency | Durable consumption and post-mutation reconciliation |
| `src/managed_worktree_observe.rs` | dependency | Read-only host Git identity and observation failure behavior |
| `src/fallback.rs` | dependency | UNKNOWN state and budget lock semantics |
| `src/process_job.rs`, `src/process_group.rs` | dependency | Platform containment and owner proof contracts |
| `src/writer.rs`, `src/approvals.rs` | dependency | Unchanged Writer root and approval boundaries |

### Verification Commands

- `git --no-pager diff --check` -> clean.
- `cargo test --locked platform_runtime_tests -- --nocapture` -> 11 macOS Unix tests passed; 0 failed. Windows-only tests were cfg-excluded.
- `git --no-optional-locks status --short` -> observed before report generation; source working-tree changes remained limited to the four reviewed files.

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `A1` | changed task guard | [`execution.rs`](/Users/yuta/local-mcp-connector-parity/src/execution.rs#L22) | Guard semantics and transfer path. |
| `A2` | process lifecycle | [`process_blocking.rs`](/Users/yuta/local-mcp-connector-parity/src/process_blocking.rs#L125) | Bounded capture/cleanup implementation. |
| `A3` | ownership-loss branch | [`process_blocking.rs`](/Users/yuta/local-mcp-connector-parity/src/process_blocking.rs#L342) | Shows no unsafe signal after lost ownership proof. |
| `A5` | managed mutation seam | [`managed_worktree_create.rs`](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_create.rs#L289) | Trusted executable, clean env, unknown timeout/capture result. |
| `A6` | durable retry gate | [`managed_worktree_prepare.rs`](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L745) | Attempt consumption and mandatory reconciliation. |
| `A7` | platform gap | [`platform_runtime_tests.rs`](/Users/yuta/local-mcp-connector-parity/src/platform_runtime_tests.rs#L608) | Windows-only runtime witnesses are not run on macOS. |

### Dismissed Coordinator Candidates

| Candidate | Decision | Evidence |
| --- | --- | --- |
| Unknown child observation may be treated as not performed | dismissed | Creator treats timeout/incomplete capture as error; prepare reconciles after every attempt; fallback model keeps UNKNOWN locked. |
| Cleanup may restore retry budget | dismissed | Attempt is durably consumed before creator call; retry only follows reconciliation permitting bounded retry. |
| Environment can choose a repo-controlled Git executable | dismissed | Git executable is resolved/canonicalized from host PATH and passed as an absolute path; `env_clear` removes inherited variables, then controlled safe env is installed. |
| Writer root or execution authority widened | dismissed | No Writer or execution-root diff; existing validated parent remains sole writable root through sandbox call. |
| Windows approval invariant changed | dismissed | No approval implementation changes; no timeout/consent modification in the diff. |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A7` | Linux Bubblewrap and Windows Job Object code/tests were not executed; macOS Seatbelt runtime tests were not included in the focused platform-runtime selection. | Static review cannot prove OS API/runtime behavior or platform compile/test health. | Run CI on Linux and Windows; execute sandbox support and process runtime suites on Linux, macOS, and Windows. |

## Prior Resolution Reconciliation

None - initial review generation.

## Receiving Handoff

- Handoff status: `Ready for receiving-code-review`
- Automatic receiving permitted: `No`
- Source report ID: `cr-20261007-7f31c9d2`
- Scope fingerprint to recheck: `sha256:bc7e21df660ecb4c2702e0a18a2be66cc6ec0d1e7a2807f4eb8fa70157eda07b`
- Actionable finding IDs: `None`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `None`
- Deferred test-gap IDs: `None`
- Open question IDs: `None`
- Open coverage area IDs: `A7`
- Highest-risk verification to repeat: Linux Bubblewrap and Windows Job Object platform runtime CI.
- Suggested implementation boundaries: `None`
- Re-review note: `Treat every finding as a claim to verify. Challenges require a counterclaim, argument, evidence, limits, and settlement criterion.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or product owner.`

## Report Self-Check

- `yes` Actual assessment mode and rationale are recorded.
- `yes` Every changed review-relevant or unknown-impact area appears in the coverage ledger.
- `yes` No final findings were accepted; the findings index correctly records none.
- `yes` No standalone test gaps were identified; unavailable platform execution is recorded as uncovered area A7.
- `yes` Generation, trigger, scope mode, and review-only handoff are consistent.
- `yes` Every meaningful coordinator candidate is adjudicated.
- `yes` A7 has an exact reason and next verification step.
- `yes` Recommendation is `Discuss` because a review-relevant platform area remains `Not covered`.
- `pending` Validator could not be located in the project; report contract checked manually.
- `yes` No source files or Git state were changed by review. This report is the review artifact.
