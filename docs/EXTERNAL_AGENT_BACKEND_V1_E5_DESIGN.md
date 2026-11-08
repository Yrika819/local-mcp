# GoalLatch External Agent Backend V1 — E5 Architecture & Delivery Plan

> Status: DRAFT / Owner review required. Design only; no production code copied or modified.
> Prepared: 2026-11-08 (Asia/Tokyo).
> Source inspected: Yrika819/local-mcp commit 4f2264249d1b40e7b43314c3392da58007b24333 (PR #41, E4-C).
> Current main at inspection: e6950fdc3bab72982285888a81cce822743fbba5.
> Existing dependencies: #30–#41 are unmerged Draft PRs. E5 implementation must stack on #41, NOT on the main-only design branch containing this file.
> Companion documents: EXTERNAL_AGENT_BACKEND_V1_E5_TEST_PLAN.md and EXTERNAL_AGENT_BACKEND_V1_OSS_LICENSE_REVIEW.md.

## 1. Objective and non-goals

E4 has demonstrated a real MCP transport loop for Planner → Readonly → host Task Verifier → host Goal Verifier → Finalizer. E5 extends that loop to **multi-task DAGs, Writer, Reviewer, Replanner, and managed-worktree evidence**, without allowing an external AI response to grant itself filesystem, approval, verification, or publication authority.

The external MCP client performs inference and may use tools already authorized for that client. GoalLatch MUST retain host ownership of the Goal/Task lifecycle, request identities, path scope, preimage checks, workspace mutation, approval, durable journal, review gate, verification, and completion.

Out of scope: replacing ModelTransport, automatic CODEX fallback, relaxing Windows approval, arbitrary shell commands through Writer responses, auto-merge/push to the primary Git branch, HYBRID scheduling, autonomous installation of other agents, or importing entire orchestration frameworks.

This document contains implementation decisions and proposed interfaces, **not a claim that E5 functionality exists**.

## 2. Confirmed codebase seams (at PR #41)

| Existing authority | Confirmed code | E5 adaptation |
|---|---|---|
| Task selection and READY | src/scheduler.rs: select_scheduler_action, dependencies_completed, ReadinessPropagationDeferred | Add host-only durable READY propagation; preserve stable mandatory-first order |
| Writer execution | src/writer.rs: begin_writer_attempt, run_writer_attempt_with_boundary | Split proposal validation, durable preparation, publication and post-write transitions without weakening existing guards |
| Scoped writer proposals | src/writer.rs: parse_and_validate_writer_result, materialize_operations, revalidate_before_commit | Reuse exact WRITE_UTF8 / preimage / canonical-path policy; external proposals remain untrusted |
| Durable mutation journal | src/writer.rs: MutationIntent PREPARED/BeginOperation/CompleteOperation; src/task_store.rs | Preserve intent and operation/request identity across crash |
| Host publication | src/workspace_publish.rs: PublishRequest, publish, ExpectedPreimage | Use existing guarded publication boundary, not direct std::fs write |
| Reviewer | src/writer.rs: parse_and_validate_reviewer_result, resume_writer_reviewer | Issue independent Reviewer work only after durable publish/reconciliation |
| Replanner | src/replanner.rs: replanner_request_for_goal, materialize_replan_output | Reuse validation, PlanRevision and closure/supersession checks; commit receipt + replan atomically |
| Host goal verification | src/goal_verifier.rs and src/goal_finalizer.rs | Keep evidence-derived terminal verdict unchanged |
| Managed workspace evidence | src/scheduler.rs: persist_managed_workspace_verification_evidence; managed_worktree* | Extract shared final-verification evidence hook for EXTERNAL |
| External request/receipt | src/external_goal_api.rs, external_goal_driver.rs, external_model_work.rs | Continue host-authored requests, immutable IDs, CAS, scoped receipts, bounded host steps |
| Model accounting | src/model_accounting.rs | EXTERNAL must have zero GoalLatch-induced Codex spawns, including failure/fallback paths |

Important observations:
- In E4 external_goal_driver.rs, SchedulerDecision::RunWriter/RunReviewer/Replan return RoleNotWired, and ReadinessPropagationDeferred is a typed stop.
- In writer.rs a WriterAttempt is begun before the Writer backend is invoked. The host records a MutationIntent, rechecks preimages before commit, and uses a bounded publication helper. Post-mutation Reviewer can resume without replaying Writer.
- Existing MutationIntent records paths/preimages/content hashes/request IDs, but not necessarily all original WRITE_UTF8 bytes required to execute a queued external submission after restart. E5 must explicitly close this gap.
- Reviewer and Writer are separate models but belong to the same mutating Task's attempt and workspace lease; they must not be represented as two unrelated Tasks.
- Existing Replanner has a host-validated materialization path, plan history bounds, and replay/identity guards; adding a generic free-form graph writer is unnecessary and unsafe.

## 3. Non-negotiable invariants

I-01. EXTERNAL never calls GoalLatch Codex transport, including exceptions, retry, recovery, cancel, Reviewer/Replanner, and fallback paths.
I-02. A model response is data, not authorization. Session/Goal/WorkItem/Task/Attempt/Revision/scope checks are host-owned.
I-03. No filesystem mutation before a durable host-owned intent and the existing authorization/approval boundary.
I-04. A Submitted/Accepted receipt is not proof of successful publication, review, task verification, or Goal completion.
I-05. An unknown side effect is NOT assumed absent and NEVER retried as if nothing occurred.
I-06. Publication is at-most-once per verified operation identity; multi-file publication is not falsely described as one atomic filesystem transaction.
I-07. No new unbounded model context, evidence, operation, receipt, log, output, path, or token storage.
I-08. Reviewer does not grant approval, publish, or bypass deterministic Host Verifier.
I-09. Replanner may not bypass original scope, required-criteria coverage, replacement rules, monotonic plan revision or history budgets.
I-10. A Task becomes READY only after all active authoritative predecessors are host-verified COMPLETED.
I-11. Existing CODEX defaults, APIs, result states and failure semantics remain compatible.
I-12. When a managed workspace is used, evidence is bound to that worktree identity and verified snapshot; EXTERNAL cannot silently operate in the primary tree.
I-13. Receipt replay key includes work_id, submission_id and payload digest with session/Goal ownership checks.
I-14. Current Windows permission mode remains under host/operator control; absent approval channel fails closed.
I-15. No automatic commit, merge, push, or worktree removal from a model answer.
I-16. Critical/High security defects, false completion, unauthorized writes or unknown-side-effect replay are NO-GO.

## 4. Staged delivery and dependencies

### E5-0: Multi-task DAG readiness and regression freeze (must precede Writer)

- Implement one bounded host-only scheduler step that transitions eligible PENDING Tasks to READY under TaskStore revision CAS.
- Reuse Goal::dependencies_satisfied and existing Task transition rules; do not infer dependencies merely from model narrative.
- Select deterministic order (mandatory-first, plan revision, TaskId). Commit all eligible transitions in one snapshot if existing invariants permit; otherwise bounded single-task transitions with explicit progress codes.
- If predecessors are BLOCKED/FAILED/NEEDS_REPLAN, distinguish WAITING/REPLAN_REQUIRED/BLOCKED; never promote dependents to READY.
- Replan supersession must preserve active-graph authority and cause READY recomputation once.
- Test linear chains, diamonds, branching, multi-ready, blocked predecessor, obsolete/superseded predecessor, restart, concurrent goal_next and exhausted step budget.
- No new ModelWorkItem is issued while a required host-only readiness transition is pending.

### E5-A: External Writer, two-phase durable mutation

Writer is materially different from Planner/Readonly: the host may mutate files, require approval, and face uncertain side effects. Do **not** copy E4's immediate one-commit model-output application pattern into Writer publication.

**Phase W0 — issue**
- Scheduler selects RunWriter only after READY and lease check.
- Under one TaskStore commit, start the Writer Attempt (READY → RUNNING), record the workspace writer lease, and issue a host-authored WorkItem (goal/task/attempt/plan revision, scoped paths, result schema, operation budgets).
- No model or host file write occurs on issuance.

**Phase W1 — validate + privately stage**
- goal_submit verifies all identities and revision CAS; reuse parse_and_validate_writer_result and materialize_operations from writer.rs.
- For non-mutating statuses (blocked/needs_replan/failed), commit the permitted Task transition + Receipt without touching files.
- For candidate_complete, permit only existing bounded WRITE_UTF8 proposal schema in V1. Validate path/forbidden paths, canonical parent, symlink and .git restrictions, preimage kind/hash, duplicate destination, writer lease, total bytes and Attempt identity.
- The original model JSON should NOT be logged or stored wholesale.
- Since existing MutationIntent stores hashes rather than necessarily the full content, privately stage only the **host-validated, bounded typed operation bytes** plus identity in a dedicated secure, content-addressed state area, with temp write + fsync + atomic rename + controlled permissions/ACL. This is not a workspace mutation.
- Do not place secrets or raw model output in Receipt, stdout, CI artifacts, logs, or user-visible stop details.
- Persist the sealed object references + hashes and prepared MutationIntent + submission receipt together under TaskStore CAS. The WorkItem may remain SUBMITTED and Receipt ACCEPTED_FOR_HOST_PROCESSING while host execution is pending.
- If the implementation cannot guarantee private staging, durable linkage and cleanup/recovery correctness on every platform, STOP and split E5-A into a non-mutating preparation PR and a separate publish PR.

**Phase W2 — host-side publish**
- A later bounded goal_next host step loads the prepared intent, verifies object integrity, rechecks session/Goal/task/attempt/worktree/lease/revision/path/preimage, and obtains existing host approval.
- Never hold TaskStore lock while awaiting approval or filesystem IO.
- Before each mutation, durably record BeginOperation with stable operation_id and request_id; use the existing ExecutionWriteBoundary / workspace_publish publication API, not a bypass path.
- Recheck preimage immediately before publication; verify postimage before CompleteOperation.
- On approval denial, record typed non-success state. On unknown result, **do not auto-replay**; require existing Writer reconciliation and bounded host observation.
- Partial multi-file writes stay journaled individually; do not claim an all-files atomic commit.
- After all operations are confirmed, persist WorkerReport / FileSnapshot / mutation metadata using the existing Writer authority path and transition into reviewer-required state.
- Only then mark Writer's external WorkItem COMMITTED with an accurate committed revision. COMMITTED here means the writer output was host-applied, **not** task-verified or Goal-complete.

**Phase W3 — crash/recovery**
- Test crash at staged blob writing, before/after linkage, before/after PREPARED, before approval, before BeginOperation, during publication, before/after postimage observation, after CompleteOperation and before Reviewer issue.
- If the state is PREPARED with no begun mutation, it may resume only if host proves no operation occurred and preimages still match; otherwise block.
- If the state is BeginOperation/UNKNOWN, use existing mutation reconciliation. Do not infer that an operation was performed solely because a matching file now exists; require sufficient causal evidence or fail closed.
- A restarted client must not be asked to regenerate a previously accepted Writer operation as a substitute for durable bytes.

### E5-B: External Reviewer and ReviewGate

- Reuse writer.rs ReviewerRequest and parse_and_validate_reviewer_result through an internal, bounded, redacted validation seam.
- Scheduler RunReviewer may issue a Reviewer WorkItem only when the writer's mutation intent has been reconciled as performed; never re-run Writer to obtain review.
- Reviewer request is host-built from durable Writer Attempt, FileSnapshot, before/after hashes and current workspace identity, not a model-supplied diff.
- Submission rechecks Goal/Task/Attempt/PlanRevision and current postimage coherence. A changed file after Writer must stop review as STALE_POSTIMAGE / BLOCKED, not quietly approve an unrelated diff.
- Commit ReviewResult evidence, ReviewGate threshold decision, Receipt and WorkItem COMMITTED atomically. A passing review transitions Task to VERIFYING, never directly COMPLETED.
- Any reviewer model verdict is only advisory to the host-owned ReviewGate. On rejection/blocked status, leave an explicit typed stop and preserve the mutation journal.

### E5-C: External Replanner and NeedsReplan E2E

- Host Scheduler issues Replanner WorkItem only for an actual durable Replan trigger, including failed-task request identity and current plan/criterion context, under existing size and history budgets.
- Introduce a bounded external validation seam that reuses replanner_request_for_goal / parse_and_validate_proposal / materialize_validated_replan, without changing CODEX behavior.
- Apply the validated replacement/supersession + criterion bindings + PlanRevision increment + Receipt + WorkItem COMMITTED in one TaskStore CAS snapshot, with no model-controlled direct Task mutations.
- Preserve current host guards for no-op non-amplification, unchanged retry prevention, active-vs-history DAG separation, path restrictions, and replan request replay identity.
- Test readonly NEEDS_REPLAN → Replanner → replacement Task READY → new readonly result → deterministic Task/Goal verification, with an actual external model submission in at least one dogfood run.
- Do not use an automatic retry loop for schema-invalid proposals, and do not let replan failures silently consume unrelated attempts.

### E5-D: Managed Worktree × EXTERNAL

- Extract the existing host function from Scheduler's Goal verification path that generates/persists managed-workspace verification evidence; use it for CODEX and EXTERNAL, without changing verification authority.
- Bind WorkItems and evidence to the managed worktree root, worktree_id, head/snapshot identity, and verified artifacts.
- Require clean-primary, explicit opt-in, and the existing creation/approval/lease/reconciliation rules; never create managed worktrees from model-supplied paths.
- Finalizer must retain its restrictions: no automatic merge/rebase/push to the primary branch; no success claim while GOAL_FINALIZATION_BLOCKED.
- Test primary/managed separation, uncommitted/dirty primary, primary branch advance, stale worktree identity, crash-recovery and cross-session isolation.

### E5-E: Real external-model dogfooding and launch gate

- Start a separate verified E5 MCP binary with isolated state/worktree; do not overwrite or restart the existing localmcp server.
- Run both deterministic MCP wire drivers and a distinct real DeepAI Code inference run with **no hardcoded planner/writer/reviewer/replanner answers**.
- Scenario A: multi-task DAG, readonly dependencies and host-verified Goal completion.
- Scenario B: one scoped WRITE_UTF8 modification, host approval, Writer journal, Reviewer decision, Task/Goal verification; verify before/after Git status and file hashes.
- Scenario C: deliberate invalid evidence → NEEDS_REPLAN → replacement plan → bounded success or explicit non-success stop.
- Scenario D: process interruption at Writer mutation and after reviewer request; demonstrate recovery without an extra writer publish or Codex CLI spawn.
- Record transcript, host state, WorkItem/Receipt lineage, operation/request IDs, approval evidence mode, verifier records and actual Codex process/spawn counters. Separate scripted-driver evidence from authentic model behavior.
- Preserve all secrets, user data and unrelated repository trees.

## 5. Public API and state-machine rules

- Do not add a generalized command-execution RPC as a shortcut. Continue goal_start, goal_next, goal_submit, goal_status, goal_result.
- WorkRole includes WRITER, REVIEWER, REPLANNER but each role is enabled only after its complete host-side validator and reconciliation path has passed relevant security tests.
- goal_next host steps may perform approved host operations; they are NEVER interpreted as approval grants by the client. A pending approval must be typed and bounded by the existing policy.
- Keep 1 active ModelWorkItem per Goal, lifetime 64, per-role 8, request 256 KiB, response 2 MiB, depth 32, receipts 64, max host steps 8 unless a separate ADR and evidence justify changing them.
- Use distinct outcome/progress codes: WRITE_INTENT_PREPARED, APPROVAL_REQUIRED, WRITE_SIDE_EFFECT_UNKNOWN, WRITER_PUBLISHED, REVIEW_MODEL_REQUIRED, REVIEW_GATE_BLOCKED, REPLAN_MODEL_REQUIRED, REPLAN_APPLIED, READINESS_PROPAGATED, GOAL_FINALIZATION_BLOCKED. Final names are provisional and must be frozen against a machine-readable schema.
- For Writer, a receipt with ACCEPTED_FOR_HOST_PROCESSING is durable acceptance only; COMMITTED is emitted once host application is proven. Reviewer and Replanner may follow E4's atomic one-snapshot application pattern.
- Never convert result_state/GoalStatus/result_kind meaning. Existing CODEX Goals remain unchanged.

## 6. Threat model additions

TM-E5-01: untrusted Writer content / dangerous path / symlink / .git target.
TM-E5-02: validation-to-publication TOCTOU, changed preimage or worktree identity.
TM-E5-03: Stage blob orphan, partial write, fsync failure or malicious replacement.
TM-E5-04: Writer approval bypass through goal_next or implicit fallback.
TM-E5-05: UNKNOWN or partially performed side effect replay.
TM-E5-06: Reviewer accepts stale postimage or forged evidence.
TM-E5-07: Replanner changes mandatory criteria or expands authority.
TM-E5-08: DAG readiness promotes dependent Task too early.
TM-E5-09: managed evidence bound to primary instead of worktree.
TM-E5-10: submit race, stale Revision, cross-session leakage and receipt identity confusion.
TM-E5-11: secrets leak in staged operations, model transcript or CI evidence.
TM-E5-12: downgrade loses durable content refs or journal and silently resumes.
TM-E5-13: resource exhaustion via multiple pending artifacts / unlimited host steps.
TM-E5-14: model-driven unintended Git commit, push or destructive cleanup.

Mitigation for all: existing strict validators + TaskStore CAS + per-operation journal + scope and preimage recheck + approval + bounded data + verifier + fail-closed recovery. A local host agent's own direct tools remain governed by its own permissions; GoalLatch must not promise to mediate unrelated operations outside GoalLatch.

## 7. Stack and review governance

- Main is not the implementation base while #30–#41 remain unmerged. Start E5-0 on PR #41 head, and stack E5-A → E5-B → E5-C → E5-D → E5-E, adjusting to actual dependencies.
- This standalone design PR may target main because it contains docs only; E5 code PRs must explicitly reference this design and the #41 feature stack, not silently use design main as implementation base.
- Separate owner-review lane for #30–#41: audit exact HEADs, CI, merge tree and dependencies. Do not mark Owner APPROVED or mutate existing PRs merely because their CI is green.
- Do not merge, un-draft, release, tag, force-push, delete old worktrees or change approvals without Owner instruction.
- Each new Draft PR must include base/head SHA, threat-model diff, OS CI status, test evidence, Codex spawn 0, migration impact, remaining NOT VERIFIED items and exact worktree footprint.

## 8. Go/no-go

READY_FOR_OWNER_REVIEW (E5 scope) requires:
1. All deterministic AT-E5 tests in companion test plan pass, with exact OS/CI evidence.
2. Writer mutation journal + immutable operation IDs + preimage/approval and crash reconciliation demonstrated.
3. Reviewer never replays Writer, no REVIEWER self-approval and no false COMPLETED.
4. Replanner preserves host criteria/authority and has a real NeedsReplan E2E.
5. Multi-task DAG readiness and managed-worktree evidence work without new bypasses.
6. Two distinct real DeepAI Code model scenarios are logged separately from scripted fixtures.
7. No unknown-side-effect automatic replay, no critical/high security finding, no unapproved mutation, no GoalLatch Codex spawn.
8. CODEX legacy mode and old GoalStore migration remain GREEN.
9. New worktree and PR changes are isolated; main untouched.

If an E5 subphase fails any authority invariant, stop that implementation path and report PARTIAL or BLOCKED. Do not substitute aspirational architecture claims for test evidence.

## 9. Primary reference links

- GoalLatch Writer: https://github.com/Yrika819/local-mcp/blob/4f2264249d1b40e7b43314c3392da58007b24333/src/writer.rs
- GoalLatch Scheduler: https://github.com/Yrika819/local-mcp/blob/4f2264249d1b40e7b43314c3392da58007b24333/src/scheduler.rs
- GoalLatch Replanner: https://github.com/Yrika819/local-mcp/blob/4f2264249d1b40e7b43314c3392da58007b24333/src/replanner.rs
- GoalLatch workspace publication: https://github.com/Yrika819/local-mcp/blob/4f2264249d1b40e7b43314c3392da58007b24333/src/workspace_publish.rs
- GoalLatch E4-C Dogfood: https://github.com/Yrika819/local-mcp/pull/41
- OSS/reference license inventory: companion E5 OSS LICENSE REVIEW.
