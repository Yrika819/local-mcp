# GoalLatch External Agent Backend V1 — E5-H3 Reliability & Security Closure

> Status: implementation + verification record for the stacked Draft PR on top of
> PR #46. Nothing here approves V1 Beta, merges to `main`, or changes an existing
> PR.
>
> Scope: the **existing** External Writer / Reviewer recovery path, audited and
> hardened *before* E5-D (Managed Worktree) integration. E5-D itself is not
> implemented.

## 0. Baselines

| Item | Value |
|---|---|
| `main` | `e6950fdc3bab72982285888a81cce822743fbba5` |
| PR #46 (`feat/e5-c-replanner`) HEAD | `844dab2f065cd06eaff382f699a0aef81115f4d5` |
| PR #45 (`feat/e5-b2-reviewer-recovery`) HEAD | `36a45fb4fd19f6ed6a60ae54ee740c23b29ac5b1` |
| PR #44 (`feat/e5-h2-writer-stage-race`) HEAD | `12afe6e04a0a08275005e5e9cfcf6fdb3fe87d71` |
| PR #43 (`feat/external-e5-0-dag-readiness`) HEAD | `b7c9d37ae57929abd91adf6d2e7ce98a0713e3a7` |
| PR #42 (`docs/external-backend-v1-e5-design-20261108`) HEAD | `bca1230ffc8f487f340e1436a35ffda1d5ebf88f` |
| H3 base | PR #46 HEAD `844dab2f` (verified equal to the live `origin/feat/e5-c-replanner`) |
| H3 branch | `feat/e5-h3-reliability-closure` |

The acceptance plan was read from PR #42's branch
(`docs/EXTERNAL_AGENT_BACKEND_V1_E5_TEST_PLAN.md`), never assumed to exist on the
PR #46 branch.

## 1. What changed

| File | Change |
|---|---|
| `src/external_publish.rs` | + `published_intent_awaiting_completion`: the recovery predicate for the crash window between the last publication commit and the host's WorkItem completion. |
| `src/external_goal_driver.rs` | Submitted-Writer recovery branch that completes an already-published WorkItem instead of dead-ending; removed an unconditional `eprintln!("DEBUG publish error: …")`. |
| `src/external_h3_reliability_tests.rs` | New deterministic suite (11 tests). |
| `src/main.rs` | Registers the new test module behind `#[cfg(test)]`. |
| `scripts/e5-h3-dogfood/driver_h3_writer.py` | Real stdio-MCP Scenario B reproducer (isolated Git repo, separate process, new session). |
| `docs/EXTERNAL_AGENT_BACKEND_V1_E5_H3_RELIABILITY.md` | This record. |

No production behaviour changed except the recovery branch. No dependency was
added, no public API field changed, no Codex-path code was touched.

## 2. Findings

### 2.1 DEFECT — H3-01: a published Writer WorkItem dead-ends after a crash

*Boundary*: crash-matrix entry 8 (`CompleteOperation` commit … `commit_complete_active_work`).

Publication and WorkItem completion are deliberately two commits
(`publish_submitted_writer_work`). A stop between them leaves:

* `MutationIntent.state = APPLIED` (every operation `APPLIED`),
* the external `active_work` still `SUBMITTED` and waiting for the client,
* `reviewer_state = NOT_STARTED`.

The next `goal_next` entered `publish_submitted_writer_work`, found no *prepared*
intent (`pending_prepared_intent` requires all operations `PREPARED`), and refused
with `WRITER_INTENT_NOT_PREPARED` — while the reviewer branch that would have
resumed the leg sits *after* the active-WorkItem branch and is therefore never
reached. The Goal was permanently `RUNNING` with a blocker the client could never
clear, holding a `PENDING` WorkItem nobody could satisfy.

Reproduced (before the fix) by
`external_h3_reliability_tests::at_e5_040b_a_crash_after_the_last_publication_commit_resumes_the_reviewer`:

```
outcome: "BLOCKED"
blockers: [{"code":"WRITER_INTENT_NOT_PREPARED","mandatory":true}]
execution_wait: "WAITING_FOR_HOST_APPLICATION"
pending_work_id: <a WorkItem no client can answer>
```

*Fix*: `external_publish::published_intent_awaiting_completion` recognises exactly
one shape — the active WorkItem's Writer role, its Task, its current Attempt, and
an intent that is `APPLIED` or `RECONCILED_PERFORMED` — and the driver then commits
`commit_complete_active_work` with `WRITER_PUBLISHED`. That step touches no file,
issues no WorkItem and starts no model turn; it only records the bookkeeping the
previous process failed to write. The reviewer leg resumes on the next step.

The predicate deliberately returns `None` for `PREPARED` (must publish),
`APPLYING` (unknown; reconciliation owns it), `PARTIAL`/`UNKNOWN` (fail closed) and
`RECONCILED_NOT_PERFORMED` (nothing was applied), so it cannot launder an unproven
mutation into a "published" verdict.

### 2.2 DEFECT — H3-02: unconditional debug print in the publication error path

`external_goal_driver.rs` printed `eprintln!("DEBUG publish error: {error}")` on
every publication error. `OrchestratorError` messages can carry host paths, and
nothing in the design allows a server-process stderr write for a typed refusal.
Removed; the error still propagates as `DriverError::Persistence` and is mapped to
a redacted `goal_next` code.

### 2.3 VERIFIED — the publication journal is honest for multi-file sets (AT-034)

Two operations, second one fails:

| Observation | Value |
|---|---|
| `a.txt` (operation 0) | intended postimage written |
| `b.txt` (operation 1) | untouched |
| intent state | `APPLYING` (never `APPLIED`) |
| operation 0 / 1 | `APPLIED` / `APPLYING` |
| attempt `side_effect_state` | not `CONFIRMED_PERFORMED` |
| `FileSnapshot` evidence | none (no per-file claim) |
| external WorkItem | `SUBMITTED` (nothing claimed) |
| stage | retained (an operation has begun, so it is recovery evidence) |
| restart | no model work issued, no replay |
| `goal_resume` | `PARTIAL` + `MUTATION_RECONCILIATION_REQUIRED`, Goal `BLOCKED`, no revert, no retry |

"At most once" and "all operations atomically" stay distinct: the code never
records an all-files claim, and reconciliation classifies from the durable journal
plus host observation.

### 2.4 VERIFIED — a concurrent cancel cannot extend a publication

The publication loop commits `BeginOperation`/`CompleteOperation` per operation and
releases the store lock across every filesystem write. Each commit carries the
revision the previous commit returned, so a `goal_cancel` (or any other durable
transition) committed during a write makes the next commit fail the revision CAS.
Test `at_e5_042_a_cancel_between_operations_stops_the_publication_journal` cancels
*inside* the first write and pins the consequence: operation 0 is left `APPLYING`
(unproven), operation 1 stays `PREPARED` (never begun, file untouched), the intent
is never `APPLIED`, and reconciliation reports `PARTIAL`.

No new guard was required: the existing revision CAS already provides this. That is
worth stating explicitly because the loop *looks* unguarded.

### 2.5 GAP — approval (AT-E5-029/030/032 in the plan's numbering)

The formal plan requires "no unapproved write" and "no session lock held during the
approval wait". The External Writer path has **no approval channel at all**: the
E5 design asks phase W2 to "obtain existing host approval", but
`external_publish` never names `approvals::` (asserted by
`external_flow_tests::the_external_submission_path_never_reaches_the_write_or_approval_boundary`),
and neither does the Codex Writer path — for both, the effective authorisation is
the host-validated plan + TaskScope + preimage + the durable PREPARED intent.

Consequences, stated precisely:

* "No session lock is held during an approval wait" holds vacuously — there is no
  approval wait, and the store lock is only held inside short `mutate_goal_snapshot`
  commits, never across the write boundary.
* "Approval denied → zero unapproved writes" cannot be demonstrated, because
  approval cannot be denied: there is no operator gate on this path.

Closing this requires an Owner decision (it changes the external Writer's authority
model and the WorkItem dialect), so H3 does not invent one. It is listed in §9.

### 2.6 GAP — attempt-level `UNKNOWN` during an in-flight publication

While operations are being published, both Writer paths record
`side_effect_class = LOCAL_MUTATION` but leave `side_effect_state` unset until the
final commit sets `CONFIRMED_PERFORMED`. Therefore
`Goal::has_unknown_side_effects()` is `false` during the window, and a
`goal_cancel`/`goal_pause` taken in that window is classified as an ordinary
cancellation rather than "cancel during unknown side effect" (AT-E5-043). The
journal is still `APPLYING`, so `goal_resume` reconciles it to
`PARTIAL`/`UNKNOWN` and blocks; `UNKNOWN` is never collapsed into
`NOT_PERFORMED`, and nothing is retried or deleted. The gap is in the *immediate*
classification, not in the eventual one.

This shape is identical in the Codex Writer path. Changing it in one backend only
would create a divergence, and changing it in the frozen Codex path is out of H3
scope (§9).

### 2.7 VERIFIED — the Scheduler's `REVIEWER_RECOVERY_DEFERRED` branch is unreachable

`advance_external_step` resolves `external_publish::reviewer_pending` before it
consults the Scheduler. The Scheduler selects `RunReviewer` only for
`Task::needs_reviewer_recovery()`, which is defined as

```
status == RUNNING && worker == CODEX_WRITER
&& intent.state == RECONCILED_PERFORMED
&& intent.reviewer_state == NOT_STARTED
```

`reviewer_pending` matches the same Task/Attempt/worker/status with a *wider*
intent predicate (`APPLIED | RECONCILED_PERFORMED`), a *wider* reviewer predicate
(`NOT_STARTED | INVOKING`) and a narrower skip (`PERSISTED`). So
`needs_reviewer_recovery() ⇒ reviewer_pending().is_some()`, and the
`REVIEWER_RECOVERY_DEFERRED` refusal below it can never be produced.

`at_e5_052_the_scheduler_reviewer_recovery_branch_is_unreachable_from_the_driver`
pins the whole state matrix (`APPLIED|RECONCILED_PERFORMED` ×
`NOT_STARTED|INVOKING|PERSISTED`) on real durable states and asserts the
implication plus the exact `needs_reviewer_recovery()` truth table. The branch is
kept as defence in depth — it is a reported refusal, not an inference — and is now
documented as dead.

### 2.8 VERIFIED — reviewer recovery identity and non-duplication

| Boundary | Evidence |
|---|---|
| WorkItem issued, client never answered | `at_e5_052_a_…`: same `work_id`, same `request_digest`, `host_steps_executed == 0`, one Attempt, file unchanged |
| Invocation begun, WorkItem never issued | `at_e5_052c_a_crash_before_the_reviewer_work_item_is_issued_reuses_the_invocation`: the durable `invocation_id` is byte-identical after restart, one Attempt |
| Reconciled-performed, no WorkItem | `at_e5_052b_…` and `at_e5_052_the_scheduler_…`: `MODEL_REQUIRED`/`REVIEWER`, never `REVIEWER_RECOVERY_DEFERRED` |
| Reviewer verdict committed, host not restarted | `at_e5_056_…`: Task `VERIFYING` (not `COMPLETED`), restart replays without a second `ReviewResult`, only the host Verifier completes |
| Duplicate submission | `at_e5_053_a_…` `REPLAY_OF_ACCEPTED_SUBMISSION`, one `ReviewResult` |
| Rejected verdict | `at_e5_054_a_…` Task `BLOCKED`, one Attempt, no publish request |
| Changed postimage | `at_e5_055_…` `REVIEWER_STALE_POSTIMAGE`, Task `BLOCKED` |

The reviewer leg never re-runs the Writer: every reviewer entry point requires the
Attempt's existing `MutationIntent`, and no reviewer path can create an Attempt or
a stage.

## 3. Crash-recovery matrix

| # | Boundary | Durable state after restart | Recovery | Status |
|---|---|---|---|---|
| 1 | During stage creation | stage dir without manifest, no intent | orphan is inert, never loadable | VERIFIED (`at_e5_026`, `writer_stage` unit tests) |
| 2 | Stage complete, before intent commit | sealed stage, no intent | orphan; next submission stages a new candidate id | VERIFIED (`at_e5_035`, `external_writer_race_tests`) |
| 3 | Intent PREPARED | `PREPARED`, stage linked, WorkItem `SUBMITTED` | publish from durable bytes after recheck | VERIFIED (`at_e5_034*`, `at_e5_028_to_034`) |
| 4 | Around `BeginOperation` | `APPLYING` for that index | reconcile from observation; never reissue | VERIFIED (`at_e5_037`, `at_e5_034`) |
| 5 | Around `WorkspacePublish` start | `APPLYING`, target untouched | reconcile → `RECONCILED_NOT_PERFORMED` or `UNKNOWN` | VERIFIED (`at_e5_037`, `at_e5_039`) |
| 6 | File 1 done, file 2 failed | per-operation `APPLIED`/`APPLYING` | `PARTIAL` + `MUTATION_RECONCILIATION_REQUIRED` | VERIFIED (new `at_e5_034`) |
| 7 | Around postimage observation | `APPLYING`, target may equal postimage | reconcile by digest; no blind `CompleteOperation` | VERIFIED (`at_e5_038`, `at_e5_039b`) |
| 8 | Around `CompleteOperation`, and the WorkItem completion after it | `APPLIED` + WorkItem `SUBMITTED` | **was a dead-end → fixed** (`at_e5_040b`) | DEFECT → FIXED |
| 9 | Reviewer invocation start | `INVOKING`, no WorkItem | issue with the same `invocation_id` | VERIFIED (new `at_e5_052c`) |
| 10 | Reviewer WorkItem issuance | `INVOKING`, WorkItem `PENDING` | same WorkItem redelivered | VERIFIED (`at_e5_052_a`) |
| 11 | Around the reviewer submission | receipt + `ReviewResult` + `PERSISTED` + `VERIFYING` | replay is a receipt replay; one result | VERIFIED (`at_e5_053`, new `at_e5_056`) |
| 12 | Around host Task Verification | `VERIFYING` → host `VerificationResult` | only the host Verifier transitions; Goal Verifier last | VERIFIED (new `at_e5_056`, `at_e5_028_to_034`) |

No state in this table is reported `COMPLETED` without a host verification record,
and no `UNKNOWN` is reported as `NOT_PERFORMED`.

## 4. Acceptance-test traceability (in-scope range 028–056)

`PASS` = a test asserts the plan's *content*. Where an existing test carries a
plan number for a different scenario, the actual content is listed and the planned
case is marked `GAP` — numbers alone are never used as evidence.

| AT | Plan scenario | Evidence | Status |
|---|---|---|---|
| 028 | valid proposal → PREPARED + receipt before publication | `external_writer_tests::at_e5_028_to_034_…`; new `at_e5_034` (asserts `PREPARED` + two operations before any write) | PASS |
| 029 | approval denied / channel missing | no approval channel exists (§2.5) | GAP (Owner decision) |
| 030 | approval delayed → no session lock held | same (§2.5); the lock is only held inside short commits | GAP |
| 031 | preimage changes after validation | `external_writer_tests::at_e5_030_a_preimage_that_changed_after_staging_blocks_publication` (**mis-numbered**: content is AT-031) | PASS |
| 032 | path authority changes before commit | `external_writer_failure_tests::at_e5_032_a_target_that_became_a_directory_is_refused_before_publication` | PASS |
| 033 | one `WRITE_UTF8`, postimage verified, journal complete | `at_e5_045_publication_writes_a_regular_file_with_the_exact_bytes` | PASS |
| 034 | multi-file partial publication | new `at_e5_034`, `at_e5_034b`, `at_e5_034c` | PASS (new) |
| 035 | crash before intent commit | `at_e5_035_a_failed_intent_commit_leaves_no_intent_receipt_or_stage` | PASS |
| 036 | crash after PREPARED before `BeginOperation` | `at_e5_032` (authority), `at_e5_030` (preimage), `at_e5_037` (durable bytes reused); restart is simulated by re-loading the store | PASS (no OS-level kill) |
| 037 | crash after `BeginOperation` | `at_e5_037_…` | PASS |
| 038 | crash during publish → never `NOT_PERFORMED` | `at_e5_038_…`, `at_e5_034b` | PASS |
| 039 | crash after publish before postimage commit | `at_e5_039_…`, `at_e5_039b_…` | PASS |
| 040 | crash after `CompleteOperation` before Reviewer | `at_e5_040_…`; new `at_e5_040b_…` (the fixed dead-end) | PASS |
| 041 | tampered/deleted stage | `writer_stage::tests::a_tampered_content_file_is_refused_not_reconstructed`, `a_deleted_content_file_is_refused` | PASS |
| 042 | cancel after intent before write | `at_e5_027c`, `at_e5_042_…`, new `at_e5_042_…` (cancel between operations) | PASS |
| 043 | cancel during unknown side effect | `at_e5_043_…` constructs the state; the real in-flight window is not classified as `UNKNOWN` at attempt level (§2.6) | PARTIAL / GAP |
| 044 | two clients, one execution | `at_e5_044_two_clients_publish_exactly_once`, `external_writer_race_tests` | PASS |
| 045 | permissions / executable bit | `at_e5_045_…` | PASS |
| 046 | review before Writer confirmed | `external_reviewer_recovery_tests::at_e5_052c_a_reviewer_work_item_is_never_issued_for_an_unpublished_intent` | PASS |
| 047 | reviewer WorkItem bound to the same Task/Attempt | `at_e5_040_…`, `at_e5_053_…` | PASS |
| 048 | passing review is not a completion | `at_e5_028_to_034_…` (Task `VERIFYING`), new `at_e5_056_…` | PASS |
| 049 | too many blocking findings | `external_writer_tests::at_e5_029_a_review_gate_failure_blocks_the_writer_task` (**mis-numbered**: content is AT-049) | PASS |
| 050 | forged ReviewFileEvidence | `ReviewerResult` derives `deny_unknown_fields` and has no file-evidence field, so a model cannot declare what it reviewed; new `at_e5_050_a_reviewer_cannot_supply_its_own_file_evidence` submits forged `files` digests and asserts `REVIEWER_OUTPUT_INVALID`, no persisted forged digest, no `ReviewResult`, Task `BLOCKED` | PASS |
| 051 | file modified after Writer | `at_e5_031_a_changed_postimage_stops_the_review`, `at_e5_055_…` | PASS |
| 052 | reviewer crash after issuance | `at_e5_052_a_…`, new `at_e5_052_…` (state matrix), new `at_e5_052c` | PASS |
| 053 | duplicate reviewer submission | `at_e5_053_a_…`, new `at_e5_056_…` | PASS |
| 054 | rejected reviewer output | `at_e5_054_a_…` | PASS |
| 055 | reviewer summary injection / bounded storage | new `at_e5_055_a_reviewer_summary_is_stored_once_bounded_and_out_of_the_receipt`: the sentinel appears exactly once in the durable document (the `ReviewResult` evidence), never in a receipt detail (bounded to 512 bytes) and never in the work history; `parse_and_validate_reviewer_result` bounds the summary to 8 KiB and the payload to 128 KiB | PASS |
| 056 | only the host Task Verifier completes | new `at_e5_056_…`, `at_e5_028_to_034_…` | PASS |

Test-name drift found and left in place (renaming adds churn without evidence):
`at_e5_029_…`→AT-049, `at_e5_030_…`→AT-031, `at_e5_031_…`→AT-051, and the launch
case `at_e5_028_to_034_…` covers AT-028/033 content rather than AT-034.

## 5. Invariants

| # | Invariant | Evidence | Status |
|---|---|---|---|
| 1 | EXTERNAL ⇒ 0 Codex CLI spawns | no `std::process`/`Command::new` in any `external_*` production module; `model_accounting` spawn counters asserted 0 in `external_flow_tests`, `external_goal_recovery_tests`, `at_e5_028_to_034_…`; real-process dogfooding (§6) ran the whole Writer/Reviewer/Verify chain with no Codex binary involvement | PASS |
| 2 | model output is untrusted | every external payload is parsed into a closed schema; identity, path, preimage, revision and scope are host-derived (`materialize_operations`, `build_*_request*`) | PASS |
| 3 | host-owned scope and path authority | `resolve_absolute_write_path` re-run immediately before each publication; symlink/directories refused by `workspace_publish::observe_destination` | PASS |
| 4 | 0 unapproved `WorkspacePublish` | no approval channel exists on this path at all (§2.5); publication still requires a durable `PREPARED` intent, a `RUNNING` Goal, the writer lease and a verified stage | PASS with the §2.5 caveat |
| 5 | 0 blind re-execution of `UNKNOWN` | `pending_prepared_intent` refuses any intent whose operations are not all `PREPARED`; `at_e5_037`/`038`/`039`/new `at_e5_034` | PASS |
| 6 | 0 duplicate Writer publication | staged `APPLYING`/`APPLIED` state + `at_e5_044` + `external_writer_race_tests` + new `at_e5_042` | PASS |
| 7 | Reviewer cannot acquire Host Verifier authority | reviewer output yields only `blocking_findings` + summary; a pass reaches `VERIFYING` only (`apply_reviewer_verdict_to_goal`) | PASS |
| 8 | no Task/Goal `COMPLETED` without host verification | `at_e5_054`, new `at_e5_056` (asserts a host final-verification record), `at_e5_034` (partial → `BLOCKED`) | PASS |
| 9 | revision CAS / idempotency preserved | every new/changed path commits through `mutate_goal_snapshot`; `at_e5_024`, `at_e5_025`, `at_e5_053` | PASS |
| 10 | Codex backend compatibility | the Codex Writer/Reviewer path is byte-identical to `844dab2f` (no diff outside `external_*`); the full suite is green | PASS |
| 11 | an MCP client cannot widen authority | no new tool, no new argument; `goal_next`/`goal_submit` schemas unchanged | PASS |
| 12 | primary workspace and existing worktrees unchanged | §7 | PASS |

## 6. Real-process dogfooding (Scenario B, multi-file Writer)

`scripts/e5-h3-dogfood/driver_h3_writer.py` drove a **separate** `local-mcp`
process of the H3 build over real stdio JSON-RPC in an isolated `git init`
repository (`%TEMP%\gl-e5h3-writer\<ts>\repo`), with a new session id
(`e5h3-writer-<8 hex>`). The running `localmcp` server was not replaced, restarted
or reconfigured.

| Observation | Value |
|---|---|
| Goal | `b55c5582-…` (EXTERNAL), start revision 1 |
| Plan | `COMMITTED`, 1 mandatory `CODEX_WRITER` Task, 2 `FILE_DIGEST` + `REVIEW_GATE` specs |
| Writer WorkItem | `d6d7c452-…`, attempt `185a7725-…` |
| Writer submission | `ACCEPTED_FOR_HOST_PROCESSING` (two `WRITE_UTF8` operations) |
| Host publication | `alpha.txt`/`beta.txt` = the requested bytes after one bounded `goal_next` |
| Reviewer WorkItem | issued in the same bounded call, role `REVIEWER` |
| Reviewer submission | `COMMITTED`, 0 blocking findings |
| Host verification | 1 Task verification, `PASSED`; Goal final verification `PASSED` |
| Final | `COMPLETED`, revision 17, `execution_wait = NONE` |
| Attempt side effect | `CONFIRMED_PERFORMED`, 1 attempt |
| Git | `M alpha.txt`, `M beta.txt` only — no other path touched |
| Codex spawns | 0 |

Distinctions kept explicit:

* this is a real MCP transport run with host-issued requests, not a fixed-JSON
  unit test;
* the *crash* boundary H3-01 fixes cannot be forced in a real process without a
  fault-injection switch, so that boundary is covered by the deterministic test
  only;
* the child process shares the machine's `local-mcp` state root (this build has no
  state-directory override); isolation is by unique session id and an isolated Git
  repository, exactly as in the accepted E5 dogfooding records.

## 7. Executed verification (Windows x64, Rust 1.96.0)

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo check --locked --all-targets` | clean (via the build below) |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | clean |
| `cargo test --locked` | 16 + 0 + 1575 + 5 + 21 = **1617 passed, 0 failed** |
| `cargo test --locked --bin local-mcp external_` | 164 passed, 0 failed (includes the 11 new tests) |
| `cargo test --locked --bin local-mcp external_h3_reliability` | 11 passed, 0 failed |
| Real-MCP dogfooding (§6) | Scenario B complete |

Linux/macOS execution was **not** performed on this machine; only CI can supply
that evidence.

## 8. Git / worktree protection

* New worktree only: `.local/gl-e5-h3-reliability` (git-ignored), branch
  `feat/e5-h3-reliability-closure`, created from `844dab2f` — never from local
  `main`.
* Primary checkout `C:\Users\Yuta\Desktop\local-mcp-connector-parity` was left on
  `managed-worktrees/phase5-slice3b-writer-paths-v1` at `e0f98048` with its
  pre-existing untracked files untouched.
* `.local/gl-e5-hardening` (`feat/e5-c-replanner` @ `844dab2f`) was **not**
  edited: still clean, still at the same HEAD.
* No `git reset --hard`, no `git clean`, no `git worktree prune`, no force push, no
  base/head change to PR #43–#46, no local `main` update, no running server
  replacement, no dependency addition.

## 9. Handoff to E5-D and open items

1. **Managed-worktree publication**: `publish_prepared_writer_intent` takes the
   Goal execution root from the durable Goal + session via
   `planner::execution_root_for_goal`/`revalidate_intent_operation_before_publish`,
   so a managed worktree must be bound there before any publication; do not add a
   second path-resolution path in E5-D.
2. **Crash matrix reuse**: E5-D should extend §3 rather than invent a parallel
   table. The WorkItem-completion window (H3-01) is a general pattern — every host
   step that must be recorded *after* a durable mutation needs the same recovery
   predicate.
3. **Approval (Owner decision)** — decide whether the external Writer needs a real
   operator approval channel (AT-029/030), or whether the plan-level authorisation
   is the accepted model. Do not implement one inside E5-D.
4. **Attempt-level `UNKNOWN` during in-flight publication** (§2.6) — a cross-backend
   change (Codex + External). Out of H3 scope; needs its own regression plan.
5. **Reviewer order vs Scheduler policy**: `reviewer_pending` picks the lowest
   `TaskId`, while the Scheduler orders by `(mandatory, created_plan_revision,
   TaskId)`. Harmless today (no writes, each leg eventually runs) but a managed DAG
   with several published Writers should sort by the Scheduler key.
6. **Reconciliation entry point**: `goal_next` cannot reconcile; a client must call
   `goal_resume` after a partial/unknown publication. A typed `goal_next` refusal
   that says so would help clients, but changing the code vocabulary needs an Owner
   decision.

## 10. Not verified

* Linux/macOS test execution and any non-Windows dogfooding.
* OS-level crash (kill -9 / power loss) injection: every "restart" here is a fresh
  `TaskStore` over the same state root, which exercises the durable boundary but not
  a torn write.
* Approval semantics (AT-029/030) — see §2.5.
