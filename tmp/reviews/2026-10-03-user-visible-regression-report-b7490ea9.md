# User-Visible Regression Audit

## Scope

- Review date: 2026-10-03
- Requested outcome: review only
- Continuation: report only
- Scope reviewed: branch diff `git diff origin/main...HEAD` on `hardening/replanner-compaction-v1` (8 files, +4342/-136)
- Baseline: `origin/main` = `d4ad4c26b605cb4a6eccde521909408f6e898994`
- Completion: Complete within reviewed scope, with two named blind spots (see Blind Spots)
- Assumptions: the four "INTENTIONAL" behaviors listed in the request are treated as the intent contract and are not reported as regressions. Read-only review: no file, index, worktree, or Git state was mutated. The report artifact itself is the only file written.
- Intent contract read first: `docs/REPLANNER_HARDENING_V1_DESIGN.md`, `SECURITY.md`, `docs/GOAL_TASK_ORCHESTRATOR_V1_DESIGN.md` §27.1-27.4, `README.md`

## Gate Snapshot

- Recommendation: Discuss
- Completion: Complete within reviewed scope
- Why now: no user-visible behavior is broken relative to baseline and the four declared intentional changes are correctly implemented, but two concrete gaps contradict the branch's own frozen design and one of them silently disables the only whole-plan-revision repair path for multi-task plans.
- Must-review now:
  1. `F1` Discuss - scope-path budget is the one dimension that never credits supersession
  2. `F2` Discuss - `pristine_plan_supersession` precondition (4) is no longer checkable from the shown context
  3. `F3` Discuss - the `failed_task_replan_requests` window of 8 is not a real bound on the durable total
- Findings count: Block 0 | Discuss 3 | Watch 5 | Intentional 8
- Coverage confidence: high for every diff-touched surface; medium for model-behavior-dependent outcomes (F2, F3, `I5`)
- Behavior graph coverage: built for 4 surfaces (replan hot path, budget enforcement, new error surface, `goal_status`/result view)
- Biggest blind spot: the actual Replanner model was never invoked, so every "the model cannot justify X" claim is inferred from the prompt contract, not observed.

## Complete Findings Index

| ID | Action | Surface | User-visible outcome | Confidence |
| --- | --- | --- | --- | --- |
| `F1` | Discuss | `goal_run` / failed-task replacement at the scope-path ceiling | A Goal at 1024 active scope paths still cannot be repaired by replacement, because the superseded Task's paths are still charged; `goal_run` stops with `LowerAuthorityError` and the Goal stays `REPLANNING` forever | High |
| `F2` | Discuss | `pristine_plan_supersession` (pre-execution plan rejection repair) | The model is instructed to verify pristine-ness of every Task at the rejected plan revision by reading `attempts`/`evidence`/`verification_results`/`blockers`, but the new tiering exposes those fields for the single trigger Task only, so the only whole-plan-revision repair path becomes hard or impossible to justify for multi-task plan revisions | Medium |
| `F3` | Discuss | `failed_task_replan_requests` authority window | With 9+ unconsumed replacement authorities, the older ones are invisible to the model and become permanently unrepairable once any replacement bumps `plan_revision` | Medium |
| `F4` | Watch | Replanner trigger visibility for a superseded trigger | An unconsumed authority whose trigger is already `SUPERSEDED` now presents a `trigger_task_id` with no corresponding `tasks[]` entry; host still fails closed, and `history[]` usually carries the signal | Low |
| `F5` | Watch | `cargo test --locked --all-targets` reliability | 8 timing-sensitive sandbox/process-group tests failed under the default parallel run and all 8 pass in isolation; the new 26-test module adds ~157 s of heavy fixture work that raises suite pressure | High |
| `F6` | Watch | `docs/REPLANNER_HARDENING_V1_DESIGN.md` §10 measured table | The committed "Measured result" byte table is not reproducible from the branch's own measurement test (e.g. `pokecpu-36` design 27,051 vs measured 12,228) and is asserted nowhere | High |
| `F7` | Watch | `docs/REPLANNER_HARDENING_V1_DESIGN.md` non-goals | The design claims `goal_status` size "is measured in this branch's stress tests and reported"; no such measurement exists anywhere in the diff | High |
| `F8` | Watch | Inline comment accuracy | `replanner.rs` states the fixed prompt overhead is "measured at under 10 KiB"; the real composed value is 11,548 bytes | High |
| `F9` | Watch | `ReplannerError::ReplannerContextUnserializable` | The new variant is unreachable with the current `Serialize` impl and its only test asserts the success path, so it has no behavioral proof | Medium |
| `I1` | Intentional | Tiered `request.tasks[]` | model context is a bounded relevance snapshot | design §5 |
| `I2` | Intentional | Four active-plan budgets | superseded Tasks no longer consume them | design §4.1 |
| `I3` | Intentional | 224 KiB request ceiling | hard host-side ceiling, typed bounded blocker | design §6 |
| `I4` | Intentional | `planner::task_sizing_profile_for_goal` | no longer counts superseded Tasks | design §4.3 |
| `I5` | Intentional | Ordinary replans against `COMPACT` Tasks | the model can no longer see `verification`/`scope` for unrelated Tasks, so duplicate-verification and scope-narrowing proposals are more likely to be refused; explicitly accepted in design §5.1 | design §5.1 |
| `I6` | Intentional | Bounded `goal_blockers` window of 32 | older Goal blockers are dropped from the prompt with an explicit omitted count | design §5.4 |
| `I7` | Intentional | New `history[]` / `history_omitted_count` / `detail` fields | new fields in the Replanner request JSON | design §5.2-5.3 |
| `I8` | Intentional | `REPLANNER_RULES` grew 8,611 -> 10,449 bytes | new prompt text describing tiers and history | design §6 |

## Block

None.

## Discuss

### F1 Discuss - scope-path budget is the one dimension that never credits supersession

User impact: A Goal whose active execution graph sits at the 1024 scope-path ceiling still cannot be repaired by failed-task replacement, because the paths of the Task being superseded are still charged against the active budget. `goal_run` returns `LowerAuthorityError { authority: REplanner, detail: "candidate plan exceeds the 1024 scope-path limit" }` and the Goal stays in `REPLANNING` with no forward path.

Review reason: this is the exact defect the branch exists to remove, and the branch's own code comment four lines above the defect says it removes it for all three non-count dimensions. It is not a regression against baseline (the baseline base was strictly larger), but it is an incomplete application of the frozen design and it is invisible because the "replacement at the exact ceiling is admitted" test exists only for edges.

Surface: `goal_run` -> `SchedulerDecision::Replan` -> `replanner::enforce_plan_budgets`

Confidence: High on the code fact and its reachability; Medium on how often a real Goal sits at 1024 active scope paths.

Look here first:
- [replanner.rs](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L1456)
- [replanner.rs](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L1435)

Behavior delta:
- Before: `existing_scope_paths` summed over **all** durable Tasks (`goal.tasks().values()`), so both history and the Task about to be superseded were charged.
- After: `totals.active_scope_paths` sums over **active** Tasks but is still the *pre-supersession* total. Tasks/edges/verification all read `resulting` (post-supersession); scope paths alone does not.

Evidence:
- `enforce_plan_budgets` computes `let resulting = totals.history_after_superseding(goal, superseded);` at replanner.rs:1441. Three of the four checks read `resulting.*` (lines 1442, 1447, 1465). The scope-path check at line 1456 reads `totals.active_scope_paths` instead.
- `PlanTotals::history_after_superseding` does compute `active_scope_paths` correctly (replanner.rs:1391), so the post-supersession value is available and simply not used.
- The comment directly above the check (replanner.rs:1435-1440) states: "Every Task this proposal supersedes leaves the active execution graph, so its edges, scope paths, and verification entries leave the *active* budget too."
- `docs/REPLANNER_HARDENING_V1_DESIGN.md` §4.1 (line 155): "Every superseded Task leaves the active graph carrying its edges, scope paths, and verification entries with it, so all four checks sum the **post-supersession** active totals."
- There is no commit-time backstop for scope paths. The only commit-time backstop in the crate is the dependency-edge count in `Goal::apply_task_replacements` (goal.rs:2241-2251); `MAX_SCOPE_PATHS_TOTAL`, `MAX_PLAN_TASKS`, and `MAX_VERIFICATION_TOTAL` appear nowhere outside `enforce_plan_budgets`.
- Test coverage gap that hides it: `a_replacement_at_the_exact_edge_ceiling_is_admitted_because_the_dead_task_releases_its_budget` (replanner_compaction_tests.rs:2031) exists for edges only. `an_oversized_active_scope_path_budget_is_still_rejected` (line 2083) and `an_oversized_active_verification_budget_is_still_rejected` (line 2115) only prove the *rejection* direction, and both use a growing proposal that supersedes nothing. There is no scope-path (or verification) analogue of the edge "released its budget" test.
- The branch's own review history records this exact class of bug: replanner_compaction_tests.rs:2032-2037 says "supersession was credited to the active Task count but not to the edge, scope-path, or verification budgets ... That is the same failure class as the original bug, just narrowed to three dimensions." Two of the three were fixed; scope paths was not.

Reviewer action: raise in review; either read `resulting.active_scope_paths` at replanner.rs:1456, or amend design §4.1 and the inline comment to state that scope paths are deliberately pre-supersession and explain why. Add the missing "released its budget" test for scope paths either way.

### F2 Discuss - `pristine_plan_supersession` precondition (4) is no longer checkable from the shown context

User impact: `pristine_plan_supersession` is the only whole-plan-revision rejection path. After this change the model is told to verify that every Task created at the rejected plan revision has no attempts, evidence, verification results, blockers, or UNKNOWN side effects by reading `request.tasks[].attempts`, `.evidence`, `.verification_results`, `.blockers` - but those four fields now exist only on `FULL`-tier Tasks, and only the single explicit rejection trigger is a seed. For any plan revision with more than one Task, the model cannot evaluate the precondition. Depending on how the model resolves the contradiction it will either refuse to propose (Goal stuck in `REPLANNING`, `goal_run` returns `ReplanAuthorityViolation`) or read absence as "empty" and be refused by the host.

Review reason: the prompt text now contains a direct internal contradiction, and no test covers it. The host still fails closed, so this is a viability regression in a repair path rather than a safety hole.

Surface: `goal_run` -> pre-execution plan rejection -> `pristine_plan_supersession`

Confidence: High on the prompt-contract contradiction; Medium on the runtime outcome, which is model-dependent and was not observed.

Look here first:
- [goal_backends.rs](/Users/yuta/local-mcp-connector-parity/src/goal_backends.rs#L18) (`REPLANNER_RULES`)
- [replanner.rs](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L499) (`replanner_task_snapshot`)

Behavior delta:
- Before: every durable Task was serialized with `title`, `objective`, `scope`, `verification`, `verification_results`, `evidence`, `blockers`, `attempts`, `max_attempts`. The model could evaluate precondition (4) for every Task at the rejected plan revision.
- After: only `FULL` tasks carry `verification_results`, `evidence`, `blockers`, `attempts`, `max_attempts` (replanner.rs:496-500, gated on `matches!(detail, ReplannerTaskDetail::Full)`). `STRUCTURAL` and `COMPACT` omit them entirely via `skip_serializing_if = "Option::is_none"`.

Evidence:
- `Goal::reject_pre_execution_plan` (goal.rs:2803-2892) transitions **only** `trigger_task_id` to `NEEDS_REPLAN`. Sibling Tasks at the rejected plan revision stay `PENDING`/`READY`.
- `replanner_context_plan` builds `full` from `eligible_needs_replan_task_ids` (all `NeedsReplan` tasks, replanner.rs:969-976) plus unconsumed replacement triggers, then intersects with `active`. The single pre-execution trigger is therefore the only `FULL` Task from that rejection.
- `validate_pristine_plan_supersession` (replanner.rs:2129-2163) still evaluates pristine-ness host-side over `goal.tasks()` filtered on `created_plan_revision() == plan_revision()`, i.e. the complete durable graph. So the host is correct and fails closed; only the model's ability to justify the proposal is degraded.
- The new rules sentence "A field absent from a task is genuinely absent for that tier, never empty" contradicts the unchanged precondition text in the same string: "(4) every Task created at request.plan_revision is pristine: no attempts, no evidence, no verification_results, no blockers, no UNKNOWN side effects -- check request.tasks[].attempts, evidence, verification_results, and blockers".
- Coverage: `grep -n "pristine|pre_execution" src/replanner_compaction_tests.rs` returns exactly one hit, `"pristine_plan_supersession": Value::Null` in a helper. The two pre-existing pristine-supersession tests (replanner.rs:3706, replanner.rs:3810) call `materialize_replan_output` with hand-written JSON and never build a request, so they cannot detect this.
- The design does not address it: §5.1 justifies `full` only by replacement validation (replanner.rs:1674-1719) and `task_ref_is_mandatory`, and §5.2's tier table shows the pristine fields as `FULL`-only, with no note that this makes precondition (4) unverifiable.

Reviewer action: raise in review; either widen the seed set to include every Task created at a rejected plan revision present in `request.pre_execution_plan_rejections`, or amend the rules text so precondition (4) is scoped to what the shown tier actually exposes and let the host remain the sole authority. Add a request-level test asserting what a `STRUCTURAL`/`COMPACT` task at the rejected plan revision exposes.

### F3 Discuss - the `failed_task_replan_requests` window of 8 is not a real bound on the durable total

User impact: When a Goal accumulates more than 8 unconsumed replacement authorities, only the newest 8 appear in the Replanner request. The older `replan_request_id` strings are host-generated and unguessable, so the model cannot name them in `replace_tasks`. As soon as any of the visible 8 is replaced, `plan_revision` increments and every remaining authority becomes permanently unusable, because replacement validation requires `request.trigger_plan_revision() == goal.plan_revision()`. Those triggers stay `NEEDS_REPLAN` forever and the Goal can never be repaired.

Review reason: the constant's justification is factually wrong, and the state it describes is reachable through the public MCP surface.

Surface: `goal_resume` (`failed_task_replan_requests`) -> `goal_run` -> `replace_tasks`

Confidence: High on reachability and the code path; Medium on how likely an operator is to submit 9+ authorities before a replan runs.

Look here first:
- [replanner.rs](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L163) (the constant's stated justification)
- [replanner.rs](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L1063) (the window)

Behavior delta:
- Before: every unconsumed `failed_task_replan_request` was serialized, so the model could emit `replace_tasks` entries for all of them in one proposal.
- After: `.skip(replan_request_omitted)` keeps only the newest `REPLANNER_REPLAN_REQUEST_LIMIT = 8`; the rest are reported only as a count.

Evidence:
- The constant's doc comment (replanner.rs:163-169) says: "Matches the `goal_resume` `maxItems: 8` MCP bound, so the number of authorities the host will accept is bounded by the same contract that created them."
- `MAX_FAILED_TASK_REPLAN_REQUESTS = 8` (goal_api.rs:30) is enforced **per `goal_resume` call** (goal_api.rs:678), not on the durable total. `record_failed_task_replan_requests` is reached before any other `goal_resume` handling (goal_api.rs:586-593) and has no total-count guard.
- `Goal::request_failed_task_replan` accepts `GoalStatus::Running | GoalStatus::Replanning` (goal.rs:2470), and only transitions `Running -> Replanning` (goal.rs:2584-2586). A second `goal_resume` with 8 fresh request ids is therefore admissible while the Goal is already `REPLANNING`, with `plan_revision` unchanged.
- `Goal::validate_failed_task_replacement_history` (goal.rs:3854-3921) enforces unique `request_id` and a replaceable trigger status only. There is no cap on `failed_task_replan_requests.len()`.
- Stranding is real: replanner.rs:2253 (`request.trigger_plan_revision() != goal.plan_revision()`) refuses a replacement whose request targets an older plan revision.
- Coverage: no test in `src/replanner_compaction_tests.rs` mentions `REPLANNER_REPLAN_REQUEST_LIMIT` or `failed_task_replan_requests_omitted`. `bounded_windows_report_their_omitted_counts` (line 1766) exercises only `goal_blockers` and asserts `history_omitted_count == 0`.

Reviewer action: raise in review; either drop the authority window (it is the one bounded vector that carries authority, and the design's own §5.4 rationale - "costs the model the ability to *name* that specific replacement" - is a permanent loss, not a bounded one), or bound the durable total at 8 so the window is provably lossless, or emit the omitted ids' `replan_request_id` values in a separate unbounded-but-tiny array.

## Watch

### F4 Watch - an unconsumed authority whose trigger is already superseded is now invisible in `tasks[]`

User impact: `failed_task_replan_requests[].trigger_task_id` can name a Task that is no longer active (two request ids for one trigger in one batch is durable-valid; goal.rs:3910-3920 permits `Superseded` as a trigger state). `replanner_context_plan` intersects `full` with `active` (replanner.rs:429), so the trigger is dropped from `tasks[]` entirely, and the model sees an authority with no corresponding task entry.

Review reason: host fails closed either way, and `history[]` usually carries the signal, so this is narrow - but it is a new shape the prompt never had to describe.

Surface: `goal_run` -> `replace_tasks`

Confidence: Low on user-visible impact.

Look here first:
- [replanner.rs](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L429)

Behavior delta:
- Before: the superseded trigger appeared in `tasks[]` with `status: "SUPERSEDED"`.
- After: it appears only as a `history[]` entry, and only if it falls inside the 64-entry window.

Evidence:
- goal.rs:3910-3920 rejects only `Completed|Cancelled|Running|Verifying` as a durable trigger state, so `Superseded` validates.
- `replaced_task_ids` is unique per record (goal.rs:3935), so the second authority can never be consumed - it is dead either way. The change is visibility, not reachability.
- The new rules text mitigates: "do not propose superseding a SUPERSEDED Task", and `history[]` entries name `superseded_task_id` and `replan_request_id`.

Reviewer action: approve with caveat; note for later.

### F5 Watch - the new test module makes 8 pre-existing timing-sensitive tests flaky under the default parallel run

User impact: `cargo test --locked --all-targets` on this machine reported `929 passed; 8 failed`, all eight in `execution::tests`, `phase0_tests::sandbox_contract`, `phase0_tests::execution_contract`, and `process_group_stress_tests`. All eight pass with `--test-threads=1`. A developer or CI runner on a loaded machine can read this as a regression in the replanner work when it is load, not code.

Review reason: the branch adds 26 tests that consume ~157 s of wall time and heavy in-memory fixtures, which raises parallel contention for `Elapsed(())`-based sandbox assertions.

Surface: developer/CI test run

Confidence: High.

Look here first:
- [replanner_compaction_tests.rs](/Users/yuta/local-mcp-connector-parity/src/replanner_compaction_tests.rs#L2613) (`each_durable_history_ceiling_is_individually_reachable_and_fires`, the slowest test, flagged ">60 seconds" in the parallel run)

Behavior delta:
- Before: the same 8 tests passed in a parallel run on this machine (they are unrelated to the diff - `execution.rs`, `phase0_sandbox_tests.rs`, and `process_group_stress_tests.rs` are untouched).
- After: they fail under the added parallel load. Isolated re-run: `11 passed` for the sandbox/process-group set and `2 passed` for the two `execution::tests` cases.

Evidence:
- Full run: `test result: FAILED. 929 passed; 8 failed; ... finished in 1495.35s`. Every failure is `Elapsed(())` or `started.elapsed() < Duration::from_secs(2)`.
- `cargo test --locked --quiet -- --test-threads=1 phase0_sandbox_tests phase0_tests::sandbox_contract phase0_tests::execution_contract::stop_job_cancellation_kills_sandboxed_process_group process_group_stress_tests::timeout_terminates_the_group_and_no_descendant_survives` -> `11 passed`.
- `cargo test --locked --quiet -- --test-threads=1 execution::tests::...` (both) -> `2 passed`.
- Measured new-module cost: `cargo test --locked -- --test-threads=1 replanner_compaction_tests` -> `26 passed ... finished in 156.66s`, 77 s user CPU.

Reviewer action: approve with caveat; document the flake set or run the suite single-threaded for this branch.

### F6 Watch - the design's "Measured result" byte table is not reproducible from the branch's own test

User impact: reviewers and operators relying on `docs/REPLANNER_HARDENING_V1_DESIGN.md` §10 to judge the compaction's benefit are reading numbers the committed test does not produce. Nothing fails, because the measurement test asserts only `bytes > 0` and prints the rest to stderr.

Review reason: this is the only quantitative evidence for the change and it is off by up to 2.2x.

Surface: design documentation

Confidence: High.

Look here first:
- [REPLANNER_HARDENING_V1_DESIGN.md](/Users/yuta/local-mcp-connector-parity/docs/REPLANNER_HARDENING_V1_DESIGN.md#L461)
- [replanner_compaction_tests.rs](/Users/yuta/local-mcp-connector-parity/src/replanner_compaction_tests.rs#L958)

Behavior delta:
- Before: n/a (new document).
- After: design §10 claims these "after" bytes: small 6,943 / pokecpu-36 27,051 / pokecpu-36-with-history 36,425 / history-heavy 32,659 / deep-chain 20,507 / wide-fan-out 25,267 / many-unrelated-completed 34,977 / blocker-and-criterion-weight 19,112.
- Measured at HEAD from `measurement_baseline_shapes_report_serialized_request_bytes`: small 6,461 / pokecpu-36 **12,228** / pokecpu-36-with-history 22,634 / history-heavy 35,200 / deep-chain 11,060 / wide-fan-out 15,778 / many-unrelated-completed 33,552 / blocker-and-criterion-weight 17,675.

Evidence:
- Command: `cargo test --locked --quiet -- --nocapture --exact replanner_compaction_tests::measurement_baseline_shapes_report_serialized_request_bytes`.
- The non-amplification numbers in design §7 are closer but still ~10-15% high: design says 7.6/10.4/18.7/29.6/29.3 KB; measured `AMPLIFY` output is 6,874 / 10,321 / 20,655 / 34,415 / 34,484 bytes.
- The *qualitative* claim does hold and is independently verified: at 128 superseded Tasks the request is 34,484 bytes against a durable goal of 274,073 bytes for the history-heavy shape, and the sweep is flat past 64.

Reviewer action: note for later; regenerate §10 from a `--nocapture` run at this commit, or drop the table.

### F7 Watch - the design's `goal_status` measurement obligation is unmet

User impact: none directly; `goal_status` cannot regress here because `src/goal_api.rs` is byte-identical to baseline. But the design lists, as a non-goal boundary, that "Current `goal_status` size is measured in this branch's stress tests and reported", and no such measurement exists, so the claim an operator would rely on is unbacked.

Review reason: an explicit verification statement in the intent contract is false.

Surface: design documentation / `goal_status`

Confidence: High.

Look here first:
- [REPLANNER_HARDENING_V1_DESIGN.md](/Users/yuta/local-mcp-connector-parity/docs/REPLANNER_HARDENING_V1_DESIGN.md#L23)

Behavior delta:
- Before: n/a.
- After: `grep -n "goal_status|status_view|result_view" src/replanner_compaction_tests.rs` returns no matches. The only size measurements in the new module are `request_bytes` and `durable_goal_bytes`.

Evidence:
- `git diff --quiet origin/main...HEAD -- src/goal_api.rs` -> unchanged, so the `goal_status` shape and its per-Task serialization are provably identical.
- Worst-case growth is nonetheless real and unmeasured: pre-change the durable total was capped at 128 Tasks; post-change it is up to 128 active + 1024 superseded = 1152 Tasks, and there is no MCP output frame ceiling (`grep` for `MAX_OUTPUT|frame` in `mcp.rs` finds only framing comments). `TaskSummaryView` is emitted per Task, so worst-case `goal_status` grows roughly 9x. That follows directly from intentional `I2`/history retention, so it is a consequence to document, not a regression.

Reviewer action: note for later; either add the measurement or drop the claim from the non-goals list.

### F8 Watch - the inline prompt-overhead figure is stale

User impact: none. The margin itself is fine and is pinned by a test.

Review reason: a code comment states a measured constant that is now wrong, in the exact area where the design insisted the cost "must be measured rather than quoted".

Surface: source comment

Confidence: High.

Look here first:
- [replanner.rs](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L180)

Behavior delta:
- Before: `REPLANNER_RULES` was 8,611 bytes.
- After: 10,449 bytes; the composed `replanner_prompt_overhead_bytes()` value is 11,548 bytes (preamble 219 + `ROLE: REPLANNER\n` 17 + rules 10,449 + `\n` + schema 841 + `DATA_BEGIN`/`DATA_END` framing 21).

Evidence:
- Recomputed by extracting the literals from `src/goal_backends.rs` and composing the identical `prompt()` format string: `overhead = 11548`, `224*1024 + 11548 = 240924 <= 262144`, slack `21220` bytes. The comment says "measured at under 10 KiB"; 11,548 > 10,240.

Reviewer action: note for later; correct the number.

### F9 Watch - `ReplannerContextUnserializable` has no behavioral proof and is effectively unreachable

User impact: none today. Flagged because it is a new user-visible error string with no test that can ever produce it, so a future change to the `Serialize` impl could not be validated against it.

Review reason: dead defensive branch presented as a distinguished failure mode.

Surface: new error surface

Confidence: Medium.

Look here first:
- [replanner_compaction_tests.rs](/Users/yuta/local-mcp-connector-parity/src/replanner_compaction_tests.rs#L2883)

Behavior delta:
- Before: variant did not exist; `serialize_request(...).map_err(ReplannerError::Model)` reported a serialization failure as an `AgentError::InvalidConfiguration`.
- After: `to_prompt_json` maps it to a dedicated `ReplannerContextUnserializable`, whose Display is "replanner request context could not be serialized: the model was not invoked". This is a genuine diagnosability improvement.

Evidence:
- The only test asserting this variant's existence asserts the *success* path: `assert!(request.to_prompt_json().is_ok())` followed by a byte-length equality. There is no failing-serialization case.
- Every field of `ReplannerRequest` is a plain string/bool/u32/`Vec`/enum/struct, so `serde_json::to_string` has no realistic failure mode. The variant is currently unreachable.

Reviewer action: note for later; acceptable as defense in depth, but do not describe it as proven by a test.

## Intentional Changes

- `I1` Tiered `request.tasks[]` with a `detail` discriminator and per-tier field omission - matches design §5.1-5.2; host validation unchanged.
- `I2` The four active-plan budgets now sum the active execution graph, plus four new durable-history ceilings (`MAX_DURABLE_SUPERSEDED_*` in planner.rs:47-50) - matches design §4.1-4.2. Note the partial application tracked as `F1`.
- `I3` 224 KiB host-side request ceiling with `ReplannerError::ReplannerContextTooLarge`, enforced in `ProductionReplannerBackend::propose_replan` before the model call, no JSON truncation - matches design §6; covered by four tests.
- `I4` `planner::task_sizing_profile_for_goal` filters on `is_active_plan_authority()` - matches design §4.3; provably a no-op for the Planner because `ensure_initial_planning_state` (planner.rs:415) requires `plan_revision == 0` and an empty Task map, and the only two call sites are planner.rs:303 and replanner.rs:1152.
- `I5` `COMPACT` Tasks expose no `verification`/`scope`, so `strengthen_verification` on an unrelated Task is more likely to be refused with "verification strengthening cannot duplicate an existing requirement" (replanner.rs:1897-1902) - explicitly accepted in design §5.1: "ordinary monotonic replans on unrelated Tasks become impossible; that is accepted, because the host rejects an unprovable proposal rather than acting on it."
- `I6` `goal_blockers` windowed to the newest 32 with `goal_blockers_omitted` - design §5.4. Blockers are host-generated and informational; the host never treats them as replan authority.
- `I7` New request fields `history`, `history_omitted_count`, `goal_blockers_omitted`, `failed_task_replan_requests_omitted`, `detail` - design §5.2-5.4. `tasks[]` array ordering changed from pure task-id order to `(tier, task_id)`; prompt-only, no MCP output ordering change.
- `I8` `REPLANNER_RULES` grew 8,611 -> 10,449 bytes to describe the tiers and the history summary - design §6; margin still 21,220 bytes.

## Coverage Ledger

| Surface / path | Touched files or entry points | Status | Result | Evidence |
| --- | --- | --- | --- | --- |
| `goal_run` replan hot path (`SchedulerDecision::Replan` -> request -> backend -> `materialize_replan_output`) | [scheduler.rs:475](/Users/yuta/local-mcp-connector-parity/src/scheduler.rs#L475), [replanner.rs:960](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L960), [goal_backends.rs:157](/Users/yuta/local-mcp-connector-parity/src/goal_backends.rs#L157) | Intentional I1, I3, I7 | ordinary small Goals replan identically in budget terms; `superseded_task_ids` is empty and `history_after_superseding` is a no-op, so `enforce_plan_budgets` reduces to the pre-change arithmetic exactly | static trace + `cargo test` |
| Active-plan budget enforcement | [replanner.rs:1426](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L1426) | Finding F1 | 3 of 4 dimensions credit supersession; scope paths do not | static trace |
| Durable-history ceilings | [replanner.rs:1478](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L1478), [planner.rs:47](/Users/yuta/local-mcp-connector-parity/src/planner.rs#L47) | Reviewed - no user-visible regression found | all four above their pre-change totals; each proven to fire by `each_durable_history_ceiling_is_individually_reachable_and_fires` | static trace + test |
| Commit-time replacement backstop | [goal.rs:2241](/Users/yuta/local-mcp-connector-parity/src/goal.rs#L2241) | Reviewed - no user-visible regression found | switched to the active predicate, matching the validator; `a_replacement_that_would_oversize_the_final_graph_is_rejected` still rejects for the same reason it did before | static trace + test |
| `goal_status` shape, `task_counts`, `task_views`, `superseded_by_replacement`, result-view mandatory filter | [goal_api.rs:1403](/Users/yuta/local-mcp-connector-parity/src/goal_api.rs#L1403) | Reviewed - no user-visible regression found | file is byte-identical to baseline | `git diff --quiet origin/main...HEAD -- src/goal_api.rs` |
| `goal_status` size | [goal_api.rs](/Users/yuta/local-mcp-connector-parity/src/goal_api.rs) | Finding F7 | shape unchanged; worst-case Task count can now reach 1152 vs 128; never measured by this branch | static trace; no measurement exists |
| Replanner authority window (`failed_task_replan_requests`) | [replanner.rs:1063](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L1063) | Finding F3, F4 | window of 8 is not a bound on the durable total | static trace |
| Superseded trigger visibility | [replanner.rs:429](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L429) | Finding F4 | trigger dropped from all tiers when not active | static trace |
| `pristine_plan_supersession` repair path | [goal_backends.rs:18](/Users/yuta/local-mcp-connector-parity/src/goal_backends.rs#L18), [replanner.rs:2101](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L2101) | Finding F2 | host validation intact over the full durable set; model-side precondition unverifiable | static trace; no request-level test |
| Ordinary `strengthen_verification` / `add_dependencies` on `COMPACT` Tasks | [replanner.rs:1897](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L1897) | Intentional I5 | more refusals, all host-side, explicitly accepted | static trace |
| New error surface (`ReplannerContextTooLarge`, `ReplannerContextUnserializable`) -> `map_replanner_error` -> `run_goal_foreground` | [scheduler.rs:683](/Users/yuta/local-mcp-connector-parity/src/scheduler.rs#L683), [goal_runner.rs:716](/Users/yuta/local-mcp-connector-parity/src/goal_runner.rs#L716) | Reviewed - no user-visible regression found | `LowerAuthorityError{authority: Replanner, detail}`; trace outcome `LowerAuthorityError`; the runner returns immediately, no retry, no new replan request | static trace + `an_oversized_context_is_a_bounded_terminal_block_and_never_a_new_replan_request` |
| Planner sizing profile | [planner.rs:329](/Users/yuta/local-mcp-connector-parity/src/planner.rs#L329) | Intentional I4 | no-op at initial planning (empty Task map required) | static trace |
| Managed-worktree execution root / `Goal.cwd` identity | [planner.rs:426](/Users/yuta/local-mcp-connector-parity/src/planner.rs#L426) | Reviewed - no user-visible regression found | function not in the diff; `planner.rs` diff is constants + one filter line + one doc comment | `git diff` hunk list |
| Verifier / Planner security closures | [verifier.rs](/Users/yuta/local-mcp-connector-parity/src/verifier.rs), [planner.rs](/Users/yuta/local-mcp-connector-parity/src/planner.rs) | Reviewed - no user-visible regression found | `verifier.rs` byte-identical; `planner.rs` additive only | `git diff --quiet origin/main...HEAD -- src/verifier.rs` |
| Windows experimental status / platform capability | [mcp.rs](/Users/yuta/local-mcp-connector-parity/src/mcp.rs), `SECURITY.md`, `README.md` | Reviewed - no user-visible regression found | `mcp.rs` byte-identical, no tool added or removed, no `cfg(windows)` in the diff, docs untouched | `git diff --quiet` + `grep` |
| MCP tool catalog and schemas (18 tools) | [mcp.rs](/Users/yuta/local-mcp-connector-parity/src/mcp.rs) | Reviewed - no user-visible regression found | unchanged, so design §27.4's "exposed MCP tool count is unchanged" holds | `git diff --quiet` |
| Durable-data compatibility / schema version | [goal.rs:39](/Users/yuta/local-mcp-connector-parity/src/goal.rs#L39) | Reviewed - no user-visible regression found | `GOAL_SCHEMA_VERSION` still 5; the diff adds only getters, no serialized field; history ceilings 1024/4096/4096/4096 all exceed pre-change totals 128/1024/1024/1024 | static trace + `durable_history_is_still_bounded_by_its_own_ceilings` compile-time assertions |
| Durable history retention / auditability | [replanner.rs](/Users/yuta/local-mcp-connector-parity/src/replanner.rs) | Reviewed - no user-visible regression found | no `remove`/`retain`/`clear` added to `goal.tasks`; only in-memory `history.truncate` | `git diff` grep |
| Test suite health and runtime | [replanner_compaction_tests.rs](/Users/yuta/local-mcp-connector-parity/src/replanner_compaction_tests.rs), [main.rs:77](/Users/yuta/local-mcp-connector-parity/src/main.rs#L77) | Finding F5 | 26 new tests, +157 s single-threaded; 8 unrelated tests flake under parallel load | `cargo test` runs |
| Design documentation accuracy | [REPLANNER_HARDENING_V1_DESIGN.md](/Users/yuta/local-mcp-connector-parity/docs/REPLANNER_HARDENING_V1_DESIGN.md) | Finding F6, F7 | §10 table and the `goal_status` measurement claim are not reproducible from the branch | `--nocapture` measurement run |
| Prompt overhead comment | [replanner.rs:180](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L180) | Finding F8 | states "under 10 KiB"; actual 11,548 | recomputed from literals |
| Pre-existing replanner/replacement tests | [replanner_replacement_tests.rs](/Users/yuta/local-mcp-connector-parity/src/replanner_replacement_tests.rs), [replanner.rs:3706](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L3706) | Reviewed - no user-visible regression found | file byte-identical; all still pass and still assert what they claim (traced each budget-sensitive case by hand) | static trace + test |
| Real model invocation behaviour | none | Not covered | the Replanner model was never invoked | would require a live model backend |

## Evidence Appendix

### Behavior Graph Deltas

| ID | Surface | Baseline path | After-change path | Delta | Ledger / finding link |
| --- | --- | --- | --- | --- | --- |
| `B1` | `goal_run` replan step | Entry `goal_run` -> Input `selection.snapshot` -> Guards `replanner_request_for_goal` (eligibility, session binding, execution root) -> Transform `ProposeReplan` serializes whole Goal -> `materialize_replan_output` -> Output durable plan revision bump | Entry/guards unchanged -> Transform `replanner_context_plan` selects tiers, `request.to_prompt_json()` bounded at 224 KiB before `agent.invoke` -> Output unchanged durable commit, or `LowerAuthorityError` | input to the model changed (tiered + bounded); guards, host validation and durable effect unchanged | I1, I3, I7 / Reviewed |
| `B2` | Active-plan budget enforcement | Entry proposal bytes -> Guards `parse_and_validate_proposal` -> Transform base = `goal.tasks()` totals (all durable) -> Output reject `ReplannerSchemaViolation` | Entry/guards unchanged -> Transform base = `PlanTotals::of(goal)` active-only, post-supersession for 3 of 4 dimensions -> history ceilings added -> Output same reject type, new message for history | changed input to one transform; one guard (`scope-path` base) not moved with the others | F1 |
| `B3` | New oversized-context error | Entry replan step -> Transport refuses prompt > 256 KiB with generic `AgentError::InvalidConfiguration` -> `LowerAuthorityError`, message names no bound | Entry unchanged -> Guard `json.len() > REPLANNER_REQUEST_MAX_BYTES` before `agent.invoke` -> `ReplannerContextTooLarge { bytes, limit }` -> `LowerAuthorityError{authority: Replanner}` -> run returns, no retry | guard moved earlier and made typed; user-visible message text is new and diagnosable | Reviewed |
| `B4` | `goal_status` / result view | Entry `goal_status` -> `status_view` -> `task_views` over `goal.tasks()`, `task_counts` with `SUPERSEDED` bucket, `result_view` filters `status != Superseded` -> Output JSON | identical, file byte-identical | none | Reviewed |

### Diff Inventory

| File or area | Classification | User-visible path considered |
| --- | --- | --- |
| `docs/REPLANNER_HARDENING_V1_DESIGN.md` | docs-only | intent contract; also read by reviewers/operators (F6, F7) |
| `src/goal.rs` | surface | getters (none user-visible) + commit-time edge backstop predicate |
| `src/goal_backends.rs` | surface | `REPLANNER_RULES` prompt text; new typed ceiling in `propose_replan`; new `replanner_prompt_overhead_bytes` |
| `src/main.rs` | config | registers the new test module only |
| `src/planner.rs` | surface | four additive history constants; one sizing filter; doc comment |
| `src/replanner.rs` | surface | request construction, tiering, history summary, bounded windows, budget split, two new error variants |
| `src/replanner_compaction_tests.rs` | test-only | none (but adds ~157 s of load, F5) |
| `src/scheduler.rs` | test-only in diff | `map_replanner_error_for_test` shim only; the production mapping is unchanged |

### Candidate Sweep Log

| Candidate | Decision | Reason |
| --- | --- | --- |
| `verify_goal` / verifier security closure weakened | dismissed | `src/verifier.rs` is byte-identical to baseline |
| Windows capability widened | dismissed | `src/mcp.rs` byte-identical; no `cfg(windows)` added; `SECURITY.md`/`README.md` untouched |
| MCP tool count or schema changed | dismissed | `src/mcp.rs` byte-identical |
| `goal_status` output shape or ordering changed | dismissed | `src/goal_api.rs` byte-identical |
| Superseded history pruned or rewritten | dismissed | diff adds no `remove`/`retain`/`clear` on `goal.tasks`; only in-memory `history.truncate` |
| `parse_and_validate_proposal` reads the compacted view instead of `goal` | dismissed | the function signature takes `goal: &Goal` and never receives a `ReplannerRequest`; `grep` for `request` inside its body finds only local proposal variables |
| `materialize_replan_output` weakened | dismissed | function is not in the diff; it reloads the durable Goal and validates against it |
| `proposal_size_and_dependency_limits_match_phase4_safety_bounds` weakened by the budget-base change | dismissed | fixture has 1 Task; 1 + 128 > 128 still fires. Its `ReplannerSchemaViolation(_)` assertion is loose (does not name which limit) but that looseness is pre-existing |
| `a_collapsing_replacement_at_the_edge_ceiling_is_accepted` weakened | dismissed | 1022 base edges; post-supersession base is also 1022 (both replaced Tasks have 0 dependencies), +2 closure edges = 1024, admitted; final graph 1023 as asserted |
| `a_replacement_that_would_oversize_the_final_graph_is_rejected` weakened | dismissed | 1021 base; +3 closure = 1024 passes the validator before and after; the commit-time backstop still computes 1025 and rejects, for the same reason as baseline |
| `two_replaced_tasks_sharing_one_closure_converge_on_a_single_edge` weakened | dismissed | asserts dependency rewiring and attempt preservation, which the budget base does not touch; passes |
| Existing durable Goal could fail `Goal::validate` or fail to load | dismissed | `GOAL_SCHEMA_VERSION` still 5, no serialized field added, `Goal::validate` unchanged, and history ceilings strictly exceed any pre-change total |
| `ReplannerContextTooLarge` could loop or spawn a new replan request | dismissed | `failed_task_replan_requests` are created only by `goal_resume` (goal_api.rs:586-593), never by a replan failure; `run_goal_foreground` returns on `LowerAuthorityError` |
| Pre-existing durable Goal with >1024 superseded Tasks | dismissed | impossible: pre-change `enforce_plan_budgets` capped total Tasks at 128, and initial planning caps at 128 |
| `ReplannerContextUnserializable` reachable in production | merged into F9 | no realistic failure mode for the current `Serialize` impl; test asserts only the success path |
| scope-path budget releases on supersession | merged into F1 | the one dimension that does not |
| "superseded Task's dependency on another superseded Task" asymmetry broken by the active filter | dismissed | the filter is only on the budget sums and the commit-time edge count; `goal.rs:2204` rewiring filter is unchanged and still `is_active_plan_authority()`; pinned by the unchanged test at replanner_replacement_tests.rs:1630 |

### Verification Commands

- `cargo fmt --all -- --check` -> clean (exit 0).
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> clean, `Finished dev profile ... in 5.73s`.
- `cargo test --locked --all-targets --quiet` -> `test result: FAILED. 929 passed; 8 failed; ... finished in 1495.35s`. All 8 failures are `Elapsed(())` / sub-2-second sandbox-timing assertions in files untouched by this diff. 929 + 8 = 937 total.
- `cargo test --locked --quiet -- --test-threads=1 execution::tests::allowed_local_approval_runs_read_only_preflight_and_verified_staging execution::tests::codex_preflight_state_change_is_rejected_before_staging` -> `2 passed`.
- `cargo test --locked --quiet -- --test-threads=1 phase0_sandbox_tests phase0_tests::sandbox_contract phase0_tests::execution_contract::stop_job_cancellation_kills_sandboxed_process_group process_group_stress_tests::timeout_terminates_the_group_and_no_descendant_survives` -> `11 passed`. Together these account for all 8 failures, confirming `F5` is load, not code.
- `cargo test --locked --quiet -- --test-threads=1 replanner_compaction_tests` -> `26 passed ... finished in 156.66s`.
- `cargo test --locked --quiet -- --nocapture --exact replanner_compaction_tests::measurement_baseline_shapes_report_serialized_request_bytes` -> the eight `MEASURE` lines quoted in `F6`.
- `cargo test --locked --quiet -- --nocapture --exact replanner_compaction_tests::replacement_history_does_not_amplify_the_next_replanner_request` -> `AMPLIFY history=0 request_bytes=6874`, `8 -> 10321`, `32 -> 20655`, `64 -> 34415`, `128 -> 34484`; `TASKPAYLOAD` widest entry is 1,419 bytes at every depth. Non-amplification claim independently confirmed.
- Prompt overhead recomputed from the literals in `src/goal_backends.rs` using the identical `prompt()` format: 11,548 bytes; `224*1024 + 11548 = 240924 <= 262144`; slack 21,220.
- `git diff --quiet origin/main...HEAD -- src/{goal_api,verifier,goal_runner,task,mcp}.rs` -> all unchanged.

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `F1` | entry | [scheduler.rs:475](/Users/yuta/local-mcp-connector-parity/src/scheduler.rs#L475) | the replan step that surfaces the budget refusal to `goal_run` |
| `F1` | behavior | [replanner.rs:1456](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L1456) | `totals.active_scope_paths` instead of `resulting.active_scope_paths` |
| `F1` | output | [goal_runner.rs:731](/Users/yuta/local-mcp-connector-parity/src/goal_runner.rs#L731) | `GoalRunStopReason::LowerAuthorityError` the user sees |
| `F1` | coverage gap | [replanner_compaction_tests.rs:2031](/Users/yuta/local-mcp-connector-parity/src/replanner_compaction_tests.rs#L2031) | the edge-only "released its budget" test |
| `F2` | input | [goal_backends.rs:18](/Users/yuta/local-mcp-connector-parity/src/goal_backends.rs#L18) | both halves of the contradiction live in this one string |
| `F2` | behavior | [replanner.rs:496](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L496) | pristine fields gated on `Full` |
| `F2` | guard (intact) | [replanner.rs:2146](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L2146) | host still checks pristine-ness over the full durable set |
| `F3` | entry | [goal_api.rs:586](/Users/yuta/local-mcp-connector-parity/src/goal_api.rs#L586) | `goal_resume` accepts a new batch with no total guard |
| `F3` | behavior | [replanner.rs:1063](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L1063) | the `.skip(omitted)` window |
| `F3` | guard (stranding) | [replanner.rs:2253](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L2253) | `trigger_plan_revision != plan_revision` refusal |
| `I3` | guard | [goal_backends.rs:157](/Users/yuta/local-mcp-connector-parity/src/goal_backends.rs#L157) | the ceiling, enforced before `agent.invoke` |
| `I3` | output | [replanner.rs:777](/Users/yuta/local-mcp-connector-parity/src/replanner.rs#L777) | the new user-visible Display strings |
| `I2` | behavior | [goal.rs:2241](/Users/yuta/local-mcp-connector-parity/src/goal.rs#L2241) | commit-time backstop moved to the active predicate |

### Blind Spots

| Area | Risk introduced by the blind spot | What would resolve it |
| --- | --- | --- |
| Real Replanner model behavior (`F2`, `F3`, `F4`, `I5`) | every "the model can/cannot justify X" claim is inferred from the prompt contract, not observed; a strong model may work around all of them, or fail in ways not predicted here | run one live `goal_run` replan against a real model on (a) a 3-task rejected plan revision, (b) a Goal with 12 unconsumed replacement authorities, and record whether the proposal is produced |
| Baseline test comparison | the 8 parallel-run failures were classified as load flakes from an isolated re-run on HEAD; `origin/main` was never executed, because checking it out would mutate Git state | run `cargo test --locked --all-targets` on a clean `origin/main` clone (outside this repo) and compare the same 8 tests |
| Durable-schema migration under a *future* history ceiling | the compatibility argument is sound only because pre-change totals were bounded; if the active/history split is ever applied to a Goal written after this branch, the argument no longer applies by construction | none needed now; re-derive the argument if `MAX_DURABLE_SUPERSEDED_*` values ever change |

### Report Self-Check

- yes - Every touched user-visible or unknown-impact surface appears in `Coverage Ledger`.
- yes - Every finding in an action section appears in `Complete Findings Index`.
- yes - Every `Finding F#` ledger row has a matching card.
- yes - Every `Not covered` row has a reason and next verification step.
- yes - Every user-visible or unknown-impact surface has graph or direct path evidence, or is explicitly marked as not covered.
- yes - Recommendation follows the mapping rules: no `Block`, three `Discuss` findings unresolved, so the recommendation is `Discuss`.