# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261001-ce824af3`
- Review chain ID: `rc-20261001-ce824af3`
- Review generation: `0`
- Review trigger: `initial`
- Parent review report ID: `None`
- Parent review report path: `None`
- Parent resolution ID: `None`
- Parent resolution path: `None`
- Generated at: `2026-10-01T14:34:18Z`
- Report path: `tmp/reviews/2026-10-01-code-review-report-commexit-c84f29.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:715a876f68f450570a406aa53eaab04f72d211872fd2af8a63ea9e23888965b9`

## Scope

- Review date: `2026-10-01`
- Scope kind: `working tree`
- Scope description: Tracked working-tree diff against `HEAD` (`591145d52d1f9b2602593ba2a58c4f56e2c3edd4`), focused on Planner/replanner `COMMAND_EXIT` materialization, legacy durable enum decoding, verifier terminal behavior, scheduler reselection, and host-owned Git observation.
- Scope mode: `full frozen scope`
- Baseline: `HEAD 591145d52d1f9b2602593ba2a58c4f56e2c3edd4`
- Target: `working tree`
- Changed paths: `12`
- Diff size: `538 additions / 828 deletions`
- Completion: `Complete within reviewed scope`
- Requirements consulted: User contract in review request: reject all newly proposed model-authored `VerificationSpec::CommandExit`; retain durable enum decoding; legacy Verifying Tasks durably block/replan once without generic command spawn or scheduler loop; internal host-owned Git observation stays distinct.
- Prior resolution consulted: `None`
- Assumptions: “Without spawn” means no model-specified/generic command is launched for legacy `COMMAND_EXIT`; separately authorized host-generated Git observations needed for `GIT_SCOPE` / `NO_FORBIDDEN_CHANGES` remain allowed.
- Excluded as unrelated: Untracked pre-existing review reports/resolutions and user worktree artifacts; no untracked files were read, changed, or included in the scope hash.

## Review Orchestration

- Assessment subagent: `Coordinator assessment - cohesive cross-layer contract and one scheduler/verifier execution chain`
- Orchestration decision: `Single reviewer`
- Decision confidence: `high`
- Decision rationale: The touched code is a cohesive capability-removal and compatibility transition. Planner, replanner, verifier, persistence, scheduler, and Git observation must be traced together; specialist partitions would reread the same small call chain. No subagent primitive was available or used.
- Coordinator override: `None`
- Context or tool limits: Windows-only process/approval runtime behavior was not executed on this macOS host; its code and cross-platform tests were inspected.

### Risk Dimensions

- Model-authored command is an execution capability; rejection must happen before materialization and legacy durable records must never regain generic execution authority.
- A failed verifier call historically left Tasks in `VERIFYING`, which the scheduler prioritizes and could repeatedly select; persistence and terminal selection behavior therefore matter.
- The verifier still needs host-owned Git observations for scope enforcement, so removing generic model commands must not accidentally remove or broaden this separate authority.
- Git path framing is security-sensitive when checking allowed/forbidden paths, particularly rename records, whitespace/newline names, and non-UTF-8 Unix paths.

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `R1` | Correctness, security boundary, durability, tests | `src/planner.rs`, `src/replanner.rs`, `src/verifier.rs`, `src/scheduler.rs`, `src/task.rs`, `src/task_store.rs`, `src/verifier_git_observation.rs`, related tests and prompt schema | Materialization callers, decode compatibility, no-spawn path, commit transaction, selection priority, internal Git authority | `Complete` |

### Synthesis Statement

Coordinator reviewed the full tracked diff and independently traced all plausible candidates through materialization, verifier evaluation/commit, and scheduler selection. Candidate loop, command escape, persistence, and Git-authority conflation concerns were resolved by code-path and focused-test evidence. No findings remain. The only environment limitation is lack of Windows runtime execution.

## Review Snapshot

- Recommendation: `Pass`
- Completion: `Complete within reviewed scope`
- Why now: The diff rejects the model-authored command capability at both materialization entry points and converts legacy durable records into one persisted blocked result that no longer satisfies scheduler verification selection.
- Must-review now: `None`
- Findings count: `Blocker 0 | Major 0 | Minor 0 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 0`
- Coverage confidence: `high`
- Biggest blind spot: Windows-only native-process/approval behavior was statically reviewed but not run on Windows.

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

None.

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | New-plan schema/prompt and initial Planner materialization | `src/goal_backends.rs`, `src/goal_backends_tests.rs`, `src/planner.rs` | `R1` | `contract trace + targeted test` | `Reviewed - no issue found` | Prompt states legacy decoding remains but new command checks are unsupported; shared validator rejects `CommandExit` before task plan materialization. | `planner::tests::an_unverifiable_command_exit_is_refused_when_the_plan_is_materialized` passed. |
| `A2` | Replanner materialization | `src/replanner.rs`, `src/planner.rs` | `R1` | `contract trace + targeted test` | `Reviewed - no issue found` | Replanner task additions and verification changes use `planner::validate_and_normalize_verification`; rejection occurs before durable mutation. | `replanner::tests::replanner_rejects_command_exit_without_mutating_durable_goal` passed; durable bytes remain identical. |
| `A3` | Durable enum decoding and compatibility | `src/task.rs`, `src/task_store.rs`, `src/verifier_tests.rs` | `R1` | `serialization/state trace + runtime test` | `Reviewed - no issue found` | `VerificationSpec::CommandExit` remains in the serde-tagged enum; verifier fixtures persist/reload a `CommandExit`-containing Goal before verification. | The legacy verifier tests create through `TaskStore` then load in `verify_task`; no enum/schema removal is in the diff. |
| `A4` | Legacy verifier result and durable transition | `src/verifier.rs`, `src/verifier_tests.rs` | `R1` | `dependency trace + targeted tests` | `Reviewed - no issue found` | `CommandExit` evaluation returns `Blocked` with no generic execution call; commit stores an `Indeterminate` result/observation, adds a blocker, and transitions the Task from `Verifying` to `Blocked` in one revision-checked transaction. | `legacy_command_exit_blocks_without_spawning_or_writing_anywhere`, `legacy_command_exit_is_blocked_even_for_host_git_observation_shape`, and `mutating_legacy_verification_command_is_blocked_without_spawn` passed. |
| `A5` | Scheduler reselection / loop risk | `src/scheduler.rs`, `src/verifier_tests.rs` | `R1` | `control-flow trace + targeted test` | `Reviewed - no issue found` | `select_next_action` chooses only Tasks whose status is `Verifying`; after the durable Blocked transition, repeated selection cannot return `VerifyTask`. | Host-Git-shape legacy test repeats selection three times and asserts none is `VerifyTask`; scheduler's remaining blocked-task path yields `NoAction(BlockedTasks)`. |
| `A6` | Generic execution removal / authority reachability | `src/verifier.rs`, `src/main.rs`, deleted `src/verifier_command_authority.rs`, `src/execution.rs` (supporting) | `R1` | `call graph trace` | `Reviewed - no issue found` | Generic verifier `run_command`/`start_command` path and command authority module are removed; the legacy match arm has no caller or generic execution edge. | Inspected verifier call path and all `CommandExit` references in `src`; remaining command variant usages are decoding, rejection, tests, and explicit blocked evaluation. |
| `A7` | Separate host-owned Git observation | `src/verifier_git_observation.rs`, `src/sandbox.rs`, `src/verifier.rs`, `src/phase0_sandbox_tests.rs` | `R1` | `authority trace + focused test suite` | `Reviewed - no issue found` | Git observation builds argv from a fixed `GitQuery` table, resolves host Git, checks Session cwd authority, applies Windows approval, bounds output/time, and uses raw bytes; it is not a generic `VerificationSpec::CommandExit` execution route. | `cargo test --bin local-mcp verifier_git_observation::tests`: 11 passed, including no-write, repository executable rejection, approval gate, and host command shape. |
| `A8` | Git status path parsing and forbidden scope enforcement | `src/verifier_git_observation.rs`, `src/verifier_tests.rs` | `R1` | `parser/control-flow trace + runtime test` | `Reviewed - no issue found` | NUL framing is strict; status parser consumes rename/copy source records and retains both sides; byte-preserving Unix path decoding avoids lossy path matching. | Parser unit cases, real Git rename, non-UTF-8 path, and forbidden rename source/destination production-gate tests passed. |
| `A9` | Bounded raw trusted runner | `src/sandbox.rs`, `src/phase0_sandbox_tests.rs` | `R1` | `implementation trace + targeted test` | `Reviewed - no issue found` | Existing text-output runner delegates to the raw bounded runner and retains lossy text API compatibility; trusted Git observer receives raw bytes without changing timeout/output limits. | `phase0_sandbox_tests::bounded_clean_raw_runner_preserves_non_utf8_stdout_bytes` is present; general verifier/Git tests passed. |
| `A10` | Managed-worktree and related test adaptation | `src/managed_worktree_creation_tests.rs`, `src/goal_backends_tests.rs`, `src/verifier_tests.rs` | `R1` | `diff + related tests` | `Reviewed - no issue found` | Tests/documentation now distinguish unsupported legacy command specs from internal host-owned Git observation; no new model command path remains. | Prompt contract assertions and relevant verifier tests passed. |
| `A11` | Platform-specific approval/process behavior | `src/verifier_git_observation.rs`, `src/sandbox.rs`, `src/phase0_sandbox_tests.rs` | `R1` | `static platform trace` | `Reviewed - no issue found` | Windows branch remains approval-gated and Unix path uses trusted raw spawn; no Windows runtime available in this review. | Blind spot is runtime-specific only; `the_observation_approval_gate_is_windows_only_and_binds` passed on this host's cross-platform policy test. |
| `A12` | Working-tree inventory and unrelated surfaces | All 12 tracked changed paths | `R1` | `diff inventory` | `Not review-relevant` | All 12 tracked paths were categorized above; no unrelated production path was silently omitted. | Existing untracked review artifacts were explicitly excluded and left untouched. |

## Subagent Candidate Adjudication

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `R1-C1` | `Coordinator` | `dismissed` | `None` | `src/verifier.rs` legacy arm returns a blocked Check; no `execution::start_command` reference remains in the verifier; `src/verifier_tests.rs` probes command write effects. | No generic command is launched for a durable `CommandExit`; blocked check is committed. |
| `R1-C2` | `Coordinator` | `dismissed` | `None` | `src/scheduler.rs:215-219` selects only `TaskStatus::Verifying`; `src/verifier_tests.rs` repeatedly selects after block. | Persisted `Blocked` Task is not verifier-selected, so the previous `VERIFYING` reselection loop is closed. |
| `R1-C3` | `Coordinator` | `dismissed` | `None` | `src/planner.rs:865`; `src/replanner.rs:1048,1168`; `src/replanner.rs` regression test. | Both initial plan and replan normalization route through the same rejecting validator; planner/replanner candidate rejection is covered. |
| `R1-C4` | `Coordinator` | `dismissed` | `None` | `src/verifier_git_observation.rs:40-110,202-269`; 11 observation tests passed. | Internal host-owned Git checks are fixed-query observations, not a concealed model-authored command route. |
| `R1-C5` | `Coordinator` | `dismissed` | `None` | `src/task.rs:169-198`; durable fixture creation/reload in `src/verifier_tests.rs`; durable commit in `src/verifier.rs:656-771`. | The enum variant remains decodable and legacy test data traverses TaskStore before the one-shot transition. |

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `src/goal_backends.rs` | surface | Model prompt/schema and command capability contract |
| `src/goal_backends_tests.rs` | test-only | Prompt regression assertion |
| `src/main.rs` | surface | Removed authority module wiring |
| `src/managed_worktree_creation_tests.rs` | test-only | Host-owned Git vs model command distinction |
| `src/phase0_sandbox_tests.rs` | test-only | Raw output byte preservation |
| `src/planner.rs` | surface | Initial materialization rejection |
| `src/replanner.rs` | surface/test | Replan normalization and rejection transaction |
| `src/sandbox.rs` | surface | Raw bounded trusted runner compatibility |
| `src/verifier.rs` | surface | Legacy block, commit and removal of generic command execution |
| `src/verifier_command_authority.rs` | surface/deleted | Removed command authority code |
| `src/verifier_git_observation.rs` | surface | Internal trusted Git observation and path parsing |
| `src/verifier_tests.rs` | test-only | No-spawn, durable transition, scheduler reselection and rename gate |

### Verification Commands

- `cargo test --bin local-mcp an_unverifiable_command_exit_is_refused_when_the_plan_is_materialized` -> `1 passed`
- `cargo test --bin local-mcp replanner_rejects_command_exit_without_mutating_durable_goal` -> `1 passed`
- `cargo test --bin local-mcp legacy_command_exit_is_blocked_even_for_host_git_observation_shape` -> `1 passed`; includes repeated scheduler selection assertion
- `cargo test --bin local-mcp legacy_command_exit_blocks_without_spawning_or_writing_anywhere` -> `1 passed`
- `cargo test --bin local-mcp forbidden_rename_destination_is_refused_by_production_scope_gate` -> `1 passed`
- `cargo test --bin local-mcp forbidden_rename_source_is_refused_by_production_scope_gate` -> `1 passed`
- `cargo test --bin local-mcp verifier_tests` -> `47 passed`
- `cargo test --bin local-mcp verifier_git_observation::tests` -> `11 passed`
- `git --no-pager diff --check` -> clean
- `git --no-optional-locks status --short` -> inspected before/after review; code/Git state not mutated by review

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `R1-C1` | entry | [`src/planner.rs`](/Users/yuta/local-mcp-connector-parity/src/planner.rs#L860) | Shared verification normalizer rejects all new `CommandExit` materialization. |
| `R1-C1` | replanner | [`src/replanner.rs`](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L1038) | Replanner verification normalization routes through the shared validator. |
| `R1-C1` | verifier | [`src/verifier.rs`](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L498) | Durable legacy variant returns blocked result without a spawn. |
| `R1-C2` | scheduler | [`src/scheduler.rs`](/Users/yuta/local-mcp-connector-parity/src/scheduler.rs#L204) | `VERIFYING` is selected by explicit status; blocked state does not match. |
| `R1-C2` | test | [`src/verifier_tests.rs`](/Users/yuta/local-mcp-connector-parity/src/verifier_tests.rs#L846) | Repeated post-transition selection asserts no verification reselection. |
| `R1-C4` | host observation | [`src/verifier_git_observation.rs`](/Users/yuta/local-mcp-connector-parity/src/verifier_git_observation.rs#L40) | Fixed, host-owned query authority remains separated from model specs. |
| `R1-C5` | durable commit | [`src/verifier.rs`](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L656) | Verification record, blocker, and Blocked status commit transactionally. |

### Dismissed Coordinator Candidates

| Candidate | Decision | Evidence |
| --- | --- | --- |
| Infinite selection loop for legacy durable `COMMAND_EXIT` | `dismissed` | Verifier persists TaskStatus::Blocked; scheduler only chooses VerifyTask from TaskStatus::Verifying; repeated-selection test confirms. |
| A remaining route to generic command execution | `dismissed` | Generic command runner and authority module removed from verifier; CommandExit arm is a static blocked check; tests probe writes across allowed/forbidden/session roots. |
| Planner-only rejection with a replan bypass | `dismissed` | Replanner calls the same validation helper for added/changed specs; explicit replan test proves schema rejection and unchanged durable bytes. |
| Deleting the durable enum decoder with the command executor | `dismissed` | `VerificationSpec::CommandExit` remains in serde enum; the verifier fixture persists and reloads it before executing the legacy block path. |
| Internal Git check accidentally exposed as generic model execution | `dismissed` | Host Git observer exposes only a closed `GitQuery` enum, fixed argv, host Git identity and explicit authority/approval checks; dedicated tests pass. |
| Rename source/destination omitted from security scope | `dismissed` | Byte parser consumes NUL source record and production forbidden-path tests cover both rename endpoints. |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A11` | Windows-specific native spawn/approval behavior was not executed on Windows. | Runtime differences in platform-specific approval integration cannot be empirically excluded here; static path and cross-platform policy test show no contract regression. | Run the focused `verifier_git_observation` and verifier suites on Windows CI. |

## Prior Resolution Reconciliation

None - initial review generation.

## Receiving Handoff

- Handoff status: `Ready for receiving-code-review`
- Automatic receiving permitted: `No`
- Source report ID: `cr-20261001-ce824af3`
- Scope fingerprint to recheck: `sha256:715a876f68f450570a406aa53eaab04f72d211872fd2af8a63ea9e23888965b9`
- Actionable finding IDs: `None`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `None`
- Deferred test-gap IDs: `None`
- Open question IDs: `None`
- Open coverage area IDs: `None`
- Highest-risk verification to repeat: `Legacy CommandExit durable block + scheduler reselection test; rerun Windows verifier/Git observation suites in CI.`
- Suggested implementation boundaries: `None`
- Re-review note: `Treat every finding as a claim to verify. Challenges require a counterclaim, argument, evidence, limits, and settlement criterion.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or owner.`

## Report Self-Check

- `yes` Actual assessment mode and rationale are recorded: coordinator.
- `yes` Every changed review-relevant or unknown-impact area appears once in `Review Coverage Ledger`.
- `yes` Every final finding appears once in the index and once as a matching card; there are no findings.
- `yes` Every `Finding F#` area references an existing finding; none are present.
- `yes` Every standalone test gap has a stable ID and severity; no standalone gaps identified.
- `yes` Every `F#` and `T#` has a unique semantic issue fingerprint and an authoritative expected basis, or is an explicit Question; no F/T items exist.
- `yes` Generation, trigger, parent resolution, scope mode, and receiving handoff satisfy the bounded chain contract.
- `yes` Generation `0` reconciliation is recorded as initial review.
- `yes` Every non-Question finding and standalone test gap appears exactly once in handoff; there are none. No open questions or Not-covered areas exist.
- `yes` Every meaningful candidate has an adjudication.
- `yes` Every Not-covered area has a reason and next step; none exist.
- `yes` Recommendation follows the skill mapping: no findings, test gaps, questions, or uncovered areas -> Pass.
- `yes` The review report validator was run and passed.
- `yes` Git state was not mutated.
