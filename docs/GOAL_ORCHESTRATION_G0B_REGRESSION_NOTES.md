# G0-B: Goal Orchestration Regression Notes (External Agent Backend V1 — Local Campaign)

Status: **Draft — for Owner review**
Baseline: `main` @ `e6950fdc3bab72982285888a81cce822743fbba5`
Reproduction tests: `src/goal_orchestration_regression_tests.rs`
Observed in Dogfooding V1 and pinned here as regression tests.

## 1. Failure A — `criterion binding references an unknown host CompletionCriterionId`

Observed message (unchanged, stable):

```
planner proposal violates the Phase 4 schema: criterion binding references an unknown host CompletionCriterionId
```

### 1.1 Authority flow (host-owned ids)

1. `goal_start` receives `completion_criteria: Vec<String>` (descriptions). When
   omitted, the host synthesizes **one** criterion whose description is the
   objective (`Goal::new`).
2. The host mints every `CompletionCriterionId` as a UUID
   (`CompletionCriterionId::new`) — the model never receives authority to mint
   ids.
3. `planner_request_for_goal` hands the model
   `completion_criteria: [{criterion_id, description, required}]`.
4. The Phase 4 validator resolves every `criterion_bindings[].criterion_id`
   by **exact string match** against the host ids and fails closed otherwise.

### 1.2 Cause separation (campaign requirement)

| Hypothesis | Verdict | Evidence |
|---|---|---|
| Model output schema violation | **Primary cause** | Any id the host did not issue (invented UUID, description text, alternate spelling) is rejected at the single lookup in the Phase 4 validator; reproduced by `invented_criterion_id_is_rejected_with_the_observed_error`, `description_used_as_criterion_id_is_rejected`, `alternate_uuid_spelling_is_not_auto_corrected` |
| Host request-generation inconsistency | **Contributing gap** | The Planner request itself is consistent (pinned by `criteria_omitted_synthesizes_one_host_owned_criterion_handed_to_planner`), but `PLANNER_RULES` never tells the model that `criterion_id` must be echoed verbatim from `request.completion_criteria[].criterion_id`. It says "Do not invent durable Task IDs" — nothing about criterion ids, and the binding section only says "Each criterion binding has criterion_id and task_refs" |
| Validator bug | **Rejected** | The validator is fail-closed and never auto-corrects; all rejection arms are pinned (`duplicate_criterion_bindings_are_rejected`, `missing_required_criterion_binding_is_rejected`, `criterion_binding_to_unknown_task_is_rejected`). Auto-correcting unknown ids is explicitly out of policy |

The dominant real-world shape is almost certainly "model echoed the
**description** as `criterion_id`" — the description is the human-readable
field the model latches onto, and for omitted criteria the description *is* the
objective text.

### 1.3 Improvement proposals (for External Backend V1 / Owner decision)

* **P-1 (prompt contract):** add one rule to `PLANNER_RULES`:
  "criterion_id MUST be the exact string from request.completion_criteria[].criterion_id (a host-owned UUID); never echo the description and never invent an id."
  Not implemented in this PR (prompt text is CODEX-facing behavior; Owner
  should approve the wording change).
* **P-2 (diagnostics):** include the set of expected host criterion ids and the
  offending binding in `PlannerSchemaViolation` detail so a failure report
  identifies the model mistake without re-running the model.
* **P-3 (External Backend reuse):** `goal_next` must hand out **host-owned**
  WorkItem/criterion identifiers in the same way, and `goal_submit` must reject
  unknown/duplicate/invented identifiers with the same fail-closed semantics.
  The E1-A threat model treats submitted identifiers as untrusted data.

## 2. Failure B — `READONLY_BLOCKED` stall

Observed chain: Readonly Worker returns `blocked` → mandatory Task `BLOCKED`
with mandatory blocker `READONLY_BLOCKED` → `next_runnable_tasks: []` → Goal
stays `RUNNING` → `goal_run` reports `NO_ACTION`.

Reproduced end to end (real readonly persistence path, scripted transport) by
`readonly_blocked_proposal_blocks_task_without_replan_and_stays_running`, and
shown durable across store reloads.

### 2.1 Cause taxonomy (campaign requirement: the six causes stay distinguishable)

| # | Cause | Mechanism / code path | Durable marker |
|---|---|---|---|
| 1 | Model proposes `blocked` | `READONLY_RULES` allows `status:"blocked"` for "a genuine external prerequisite that can be resolved without changing the Task plan"; `persist_valid_result` records it | TaskBlocker code **`READONLY_BLOCKED`** (mandatory) |
| 2 | Codex execution policy rejection | surfaces as a worker/backend error, not as a model verdict | `READONLY_BACKEND_ERROR` on the retryable failure path (`readonly_backend_error_is_not_recorded_as_readonly_blocked`) |
| 3 | Approval denial | `approvals::request` denial fails the host-native attempt before execution (`user denied …`) | attempt failure class / backend error, never `READONLY_BLOCKED` |
| 4 | Execution environment missing | transport/`TOOL_MISSING`-style classification at the worker boundary | `READONLY_TRANSPORT_UNAVAILABLE` after the interruption budget (`MAX_READONLY_TRANSPORT_INTERRUPTS_PER_TASK`), or `TOOL_MISSING` on writer paths |
| 5 | External condition (genuine) | the legitimate `blocked` semantics (cause 1) | `READONLY_BLOCKED` |
| 6 | Task needs replan | the model must return `needs_replan`, not `blocked` | Task → `NEEDS_REPLAN`, scheduler emits `Replan { trigger_task_id }` |

### 2.2 Invariants pinned

* **No unconditional BLOCKED → NEEDS_REPLAN conversion.** A blocked Task stays
  blocked; the scheduler reports `NoAction { BlockedTasks }` and never a
  `Replan` for it. This preserves the monotonic Replanner contract: only
  `NEEDS_REPLAN` tasks are replan triggers.
* Goal-level `BLOCKED` is distinct from task-level stalls:
  `NoAction { GoalBlocked }` (`goal_level_blocked_reports_goal_blocked_no_action`),
  so `goal_run` NO_ACTION reasons remain typed.
* The stall is durable (survives reload), so an external orchestrator can poll
  and act on it.

### 2.3 Improvement proposals

* **P-4:** give `blocked` an optional structured reason code in the Readonly
  output contract (bounded enum: `EXTERNAL_PREREQUISITE`, `ENV_MISSING`,
  `APPROVAL_REQUIRED`, `POLICY_REFUSED`) so causes 1/3/4/5 become
  machine-distinguishable without prose parsing. Until then, the taxonomy in
  2.1 is the authoritative mapping.
* **P-5:** surface a typed "stalled" summary in `goal_status`
  (`blocking_codes: [...]`) so callers can distinguish "waiting for
  dependencies" from "blocked on external prerequisite" without scanning tasks.

## 3. Target 3 — `goal_result` discrimination design

Constraint: `result_state` stays `TERMINAL`/`NOT_TERMINAL` (existing contract,
covered by existing tests and re-pinned here).

Design implemented in this PR (additive only):

* `result_kind: "COMPLETED" | "FAILED" | "CANCELLED" | "BLOCKED" | "NONTERMINAL"`
  — one new serialized field on `goal_result`, derived from the durable
  `GoalStatus` by `goal_api::result_kind_for_status`.
* `BLOCKED` is intentionally **not** `TERMINAL`: a blocked Goal is externally
  actionable (e.g. an External Backend can supply the missing prerequisite),
  and the frozen `is_terminal()` semantics are untouched.

Mapping (pinned exhaustively by `result_kind_mapping_is_pinned_for_every_goal_status`):

| GoalStatus | result_kind | result_state |
|---|---|---|
| Planning / Running / Replanning / Pausing / Paused / Verifying / Cancelling | NONTERMINAL | NOT_TERMINAL |
| Blocked | BLOCKED | NOT_TERMINAL |
| Completed | COMPLETED | TERMINAL |
| Failed | FAILED | TERMINAL |
| Cancelled | CANCELLED | TERMINAL |

## 4. Test matrix (campaign requirement → test)

| Required case | Test |
|---|---|
| completion_criteria omitted | `criteria_omitted_synthesizes_one_host_owned_criterion_handed_to_planner` |
| explicit single criterion | `host_criterion_id_echo_is_accepted_for_one_and_many_criteria` |
| explicit multiple criteria | `host_criterion_id_echo_is_accepted_for_one_and_many_criteria` |
| correct criterion id | `host_criterion_id_echo_is_accepted_for_one_and_many_criteria` |
| unknown criterion id | `invented_criterion_id_is_rejected_with_the_observed_error` |
| duplicate ids | `duplicate_criterion_bindings_are_rejected` |
| planner returns description as id | `description_used_as_criterion_id_is_rejected` |
| planner invents ids | `invented_criterion_id_is_rejected_with_the_observed_error`, `alternate_uuid_spelling_is_not_auto_corrected` |
| readonly worker returns blocked | `readonly_blocked_proposal_blocks_task_without_replan_and_stays_running` |
| required task BLOCKED / `next_runnable_tasks=[]` | same (scheduler `NoAction{BlockedTasks}`) |
| goal stays RUNNING / `goal_run` NO_ACTION | same |
| cause separation (backend error vs model blocked) | `readonly_backend_error_is_not_recorded_as_readonly_blocked` |
| goal-level BLOCKED NO_ACTION | `goal_level_blocked_reports_goal_blocked_no_action` |
| result discrimination (5-way, non-breaking) | `result_kind_discriminates_five_outcomes_without_touching_result_state`, `result_kind_mapping_is_pinned_for_every_goal_status` |

All tests use deterministic fixtures and scripted/mock transports; no real
model is invoked and no Codex process is spawned.

## 5. CODEX compatibility

* The only production-code change in this PR is the **additive**
  `result_kind` field on `goal_result`. No existing field, transition, prompt,
  or budget rule changed; existing goal_api tests (including the frozen
  `result_state` expectations) pass unchanged.
* Proposals P-1/P-4/P-5 are deliberately **not** implemented here (prompt and
  output-contract changes are CODEX-facing behavior and need Owner approval).
