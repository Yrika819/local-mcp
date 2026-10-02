# Replanner Hardening V1

Status: frozen design for `hardening/replanner-compaction-v1`.
Base: `d4ad4c26b605cb4a6eccde521909408f6e898994` (`origin/main`, CI run `36935078856`,
11/11 success).

This document is reviewable independently of the code. It does not widen any
authority. Where it describes host behavior it describes behavior the host must
keep deriving itself; where it describes model context it describes only what the
model is *shown*, never what the model is *allowed*.

Non-goals for this branch, explicitly:

- Managed Worktrees Phase 5. No `workspace_snapshot_digest`, finalizer worktree
  binding, `BASE_ADVANCED`, cleanup eligibility, unlock/remove/prune, automatic
  commit/stage, merge/rebase/cherry-pick, push/PR/release automation, or parallel
  writers.
- Writer Integrity. No preimage/write TOCTOU work, no atomic multi-file write
  transaction, no `write_file` UTF-8 behavior change.
- Resource-bound hardening. No generic stdout/stderr ceiling, job TTL, finished-job
  GC, MCP frame/input ceiling, `read_file`/image size, directory entry limit, or
  server orphan recovery.
- `goal_status` pagination or summary-mode redesign. Current `goal_status` size is
  measured in this branch's stress tests and reported, but not redesigned.

## 1. The confirmed defect

A worker that hits `HOST_OUTPUT_LIMIT` is routed to failed-task replacement. The
Replanner decomposes it into a new closure and supersedes the old Task. Three
structural properties then compound:

**A. The Replanner request serializes the whole Goal.** `replanner_request_for_goal`
built a `ReplannerTaskSnapshot` for *every* durable Task, each carrying title,
objective, mandatory, status, dependencies, worker, scope, verification,
verification results, evidence bodies, blockers, and every attempt summary, plus
Goal-wide criteria, bindings, blockers, replan records and history.

**B. The feedback loop is positive.** Worker `HOST_OUTPUT_LIMIT` produces more
replacement Tasks; replacement Tasks enlarge the durable Task graph; a larger graph
produces a larger Replanner request; a larger request makes the Replanner itself
more likely to hit its own output limit; that failure produces more replacement
history. Nothing in the loop was bounded by the size of the work that actually
needed repairing.

**C. Superseded history consumed active-plan quota.** The four host plan ceilings
in `parse_and_validate_proposal` were computed over *all* durable Tasks:

```
replanner.rs  goal.tasks().len()                     + add_tasks.len()  > 128
replanner.rs  Σ task.dependencies().len()            + proposed edges   > 1024
replanner.rs  Σ allowed_paths + forbidden_paths      + proposed paths   > 1024
replanner.rs  Σ task.verification_specs().len()      + proposed specs   > 1024
```

A superseded Task is permanently non-runnable, permanently unsatisfiable as proof,
and referenced by nothing active (section 3). It was nonetheless charged against
the budget for the live plan, so each repair permanently consumed headroom that the
repaired plan still needed. This is the mechanism that turns a local failure into a
Goal that can no longer be repaired.

**D. The ceilings stopped growth only after the graph was already large.** They are
correct as safety bounds. They are not a compaction mechanism, and they cannot be,
because they do not control what the model is shown.

## 2. Core principle: model context is not authority

The host owns the entire durable Goal. The model is shown a bounded, host-selected
relevance snapshot sufficient to *propose*. The host continues to validate any
returned candidate against the **complete** durable Goal, using data the model
never saw.

Compaction must not weaken any of: dependency validation, criterion-binding
validation, scope validation, verification limits, replacement legality,
`base_goal_revision` binding, `base_plan_revision` binding, Task status rules,
writer serialization, or authority rules. If omitted context would have made a
proposal valid, the host rejects it. That is a correctness cost of compaction, and
it is the intended trade: bounded, predictable repair attempts in exchange for
unbounded, unpredictable growth. Context pressure is never relieved by granting
the model more authority.

## 3. Active execution graph versus durable history

The repository already has the correct predicate and already uses it in 26 places:
`Task::is_active_plan_authority()` (`src/task.rs:886-888`), which is exactly
`status != TaskStatus::Superseded`. This design adds no new concept.

**Active execution graph.** Every Task with `status != Superseded`. This is
deliberately *not* "non-terminal". `Completed`, `Failed` and `Cancelled` Tasks all
remain active:

- a `Completed` Task is a legal dependency target and a valid `TASK_VERIFIED`
  proof, and `dependencies_satisfied` requires `status == Completed` on the
  prerequisite (`src/goal.rs:4214-4221`);
- the finalizer's mandatory-unsettled scan filters on
  `is_active_plan_authority() && mandatory` (`src/goal.rs:4232`), so excluded
  history would let a `Completed` mandatory Task skip its proof requirement;
- `task_counts` and `task_views` in `goal_status` report all eleven statuses,
  including a dedicated `SUPERSEDED` bucket, and must keep doing so.

**Durable history.** Tasks with `status == Superseded`, together with
`failed_task_replacement` and `pristine_plan_supersession` records. History is
retained forever. Nothing in this branch deletes, prunes, compacts on disk, or
rewrites a durable Task, attempt, evidence item, or record.

### 3.1 Why history is provably unreferenced

Every durable invariant that could connect history to the live plan already exists
and already runs on every load (`TaskStore::load_goal` calls `goal.validate()`):

- `Goal::validate_dag` (`src/goal.rs:4131`): an active Task may not depend on a
  superseded Task. Replacement rewiring (`src/goal.rs:2175-2201`) is exhaustive —
  it iterates the whole task map filtered on `is_active_plan_authority()` once per
  replaced Task and unions per dependent, so ordering is irrelevant.
- `Goal::validate_final_verification_contract` (`src/goal.rs:4083`): a
  `TASK_VERIFIED` binding must name a Task that `is_active_plan_authority() &&
  mandatory`. This holds unconditionally, independent of supersession history, so
  **no** durable Goal can have a criterion bound to a superseded Task.
- `validate_failed_task_replacement_history` (`src/goal.rs:3974-4004`) and the
  pristine-supersession history check (`src/goal.rs:3735-3749`) re-assert both.

`TaskStatus::Superseded` has exactly one assignment (`src/task.rs:899`,
`supersede_for_host`) and exactly two callers (`src/goal.rs:1894` pristine
supersession, `src/goal.rs:2144` task replacement). Both rebind criteria; the
replacement path also rewires dependents exhaustively. The pristine path does not
rewire dependents — it supersedes exactly the Tasks created at the rejected plan
revision, and any surviving dependent would fail `validate_dag` and commit nothing.

Therefore excluding superseded Tasks from quota accounting cannot make an existing
validation weaker, because no live authority can name them in the first place.

One deliberate asymmetry is preserved: a superseded Task may itself still list a
dependency on another superseded Task, because a Task superseded by the transaction
that just rewired the graph is not itself a surviving dependent
(`src/goal.rs:2179` filters on `is_active_plan_authority()`). This is pinned by
`two_replaced_tasks_sharing_one_closure_converge_on_a_single_edge`
(`src/replanner_replacement_tests.rs:1630`). Those historical edges are durable
history and are charged to the history budget below, not the active budget.

## 4. Quota separation

### 4.1 Active-plan budgets (values unchanged, base is now active-only)

| Budget | Constant | Value | Base |
| --- | --- | --- | --- |
| active Tasks | `planner::MAX_PLAN_TASKS` | 128 | active Tasks + `add_tasks` |
| active dependency edges | `planner::MAX_PLAN_DEPENDENCY_EDGES` | 1024 | active edges + proposed |
| active scope paths | `planner::MAX_SCOPE_PATHS_TOTAL` | 1024 | active scope paths + proposed |
| active verification entries | `planner::MAX_VERIFICATION_TOTAL` | 1024 | active specs + proposed |
| per-Task fan-in | `planner::MAX_DEPENDENCIES_PER_TASK` | 64 | unchanged |
| per-Task scope paths | `planner::MAX_SCOPE_PATHS_PER_KIND` | 64 | unchanged |
| per-Task verification | `planner::MAX_VERIFICATION_PER_TASK` | 32 | unchanged |

No numeric active limit changes. Only the population being summed changes.

The commit-time backstop in `Goal::apply_task_replacements` (`src/goal.rs:2209`),
which re-counts the *resulting* graph exactly, is switched to the same active
predicate so the validator and the commit-time authority agree. Leaving them
disagreeing would fail closed but would make a legal replacement impossible, which
is the defect this branch removes.

### 4.2 Durable-history budgets (new, separate, and bounded)

Removing history from the active budget must not make durable growth unbounded, so
each dimension gets an explicit, separate history ceiling:

| Budget | Constant | Value | Multiple of active |
| --- | --- | --- | --- |
| superseded Tasks | `MAX_DURABLE_SUPERSEDED_TASKS` | 1024 | 8x |
| superseded dependency edges | `MAX_DURABLE_SUPERSEDED_DEPENDENCY_EDGES` | 4096 | 4x |
| superseded scope paths | `MAX_DURABLE_SUPERSEDED_SCOPE_PATHS` | 4096 | 4x |
| superseded verification entries | `MAX_DURABLE_SUPERSEDED_VERIFICATION_ENTRIES` | 4096 | 4x |

**Compatibility argument, and why these values.** Before this change the active
ceilings were computed over all durable Tasks, so *every* durable Goal ever written
satisfies total Tasks <= 128, total edges <= 1024, total scope paths <= 1024, and
total verification entries <= 1024. Every new history ceiling is strictly above
those totals. Therefore **no existing durable Goal can be invalidated by this
change**, no migration is required, no historical fact is fabricated, and
`GOAL_SCHEMA_VERSION` stays at 5. This is the reason the active/history split needs
no schema bump: the distinction is already derivable from existing `TaskStatus`.

Headroom reasoning: one replacement round supersedes at most a handful of Tasks and
adds at most a handful, so a long-lived Goal that is repeatedly repaired accumulates
tens, not thousands, of history entries. 8x the Task budget and 4x the other three
leave room for a large repair campaign while keeping the absolute worst case
bounded and small.

Enforcement point: `parse_and_validate_proposal`, in the same place and the same
pass as the active budgets, computing the *resulting* history totals (current
history plus the Tasks this proposal supersedes). Nothing is deleted; a proposal
that would exceed a history ceiling is rejected whole, like any other schema
violation.

### 4.3 Sizing profile

`planner::task_sizing_profile_for_goal` (`src/planner.rs:313`) counted distinct
`STRUCTURED_EVIDENCE.requirement_id` values over *all* Tasks, so superseded history
inflated `evidence_dimension_count`, and therefore `evidence_shape_estimate` and
`over_budget`, on every subsequent repair. It now filters on
`is_active_plan_authority()`.

This is a no-op for the Planner: `ensure_initial_planning_state`
(`src/planner.rs:388`) requires `plan_revision == 0` and an empty Task map, so at
initial planning time there are no superseded Tasks to exclude. It changes only the
Replanner's `sizing`, and only for Goals that already contain history.

## 5. Replanner context construction

Deterministic, host-owned, and derived from replacement-validation semantics rather
than guessed. All sets are `BTreeSet<TaskId>`; the serialized `tasks` array is
sorted by `(tier, task_id)`. `Goal.tasks` is already a `BTreeMap`, so no hash
iteration can reach the prompt.

### 5.1 Selection algorithm

Given the durable `Goal`:

1. `replacement_requests` = durable `failed_task_replan_requests` with no consumed
   `failed_task_replacement`. (Unchanged filter.)
2. `replacement_triggers` = their `trigger_task_id`s.
3. `resolvable` = `eligible_needs_replan_task_ids`. (Unchanged.)
4. `seed` = `replacement_triggers` ∪ `resolvable`. Every unconsumed replacement
   trigger is a seed, so the model always sees the Task it is being asked to
   replace at full fidelity even if its authority token is truncated (5.5).
5. `active` = all Task ids with `is_active_plan_authority()`.
6. `ancestors` = transitive dependency ancestors of `seed`, restricted to `active`.
7. `dependents` = transitive dependency descendants of `seed`, restricted to `active`.
8. `criterion_bound` = every Task id named by a `TASK_VERIFIED` requirement in the
   current final-verification spec, restricted to `active`.
9. `full` = `seed`.
   `structural` = (`ancestors` ∪ `dependents` ∪ `criterion_bound`) − `full`.
   `compact` = `active` − (`full` ∪ `structural`).

Why each set, tied to a validation rule the model must satisfy:

- **full** — `seed` is what the Replanner is *for*. Replacement validation reads the
  trigger's `attempts`, `effective_failure_class`, `side_effect_state`,
  `verification_results`, `evidence` and `blockers` to confirm the durable request
  authority still matches (`replanner.rs:1674-1719`) and that a pre-execution
  trigger is pristine. Without these the model cannot tell a size failure from an
  authority failure.
- **ancestors** — a replacement Task may legally declare an existing prerequisite
  only among active Tasks, and `task_ref_is_mandatory`
  (`replanner.rs:2243`) requires the model to know each candidate's `mandatory`
  flag and identity. Ancestors are exactly the legal prerequisite pool.
- **dependents** — replacing a Task rewires its dependents host-side. The model must
  know the downstream exists, and its identity and status, to avoid proposing a
  cycle among new Tasks and to reason about what the repair preserves.
- **criterion_bound** — `criterion_rebindings` must cover *exactly* the criteria
  that required the replaced Task (`replanner.rs:1833-1901`), and
  `strengthen_criterion_bindings` adds proof. The model needs the whole binding
  table. Criteria are bounded at 64 by the `goal_start` schema and bindings name
  only mandatory active Tasks, so the table is small; it is provided as its own
  field rather than per Task.
- **compact** — enough to reason about the remaining topology and about cycles when
  proposing an `add_dependencies` edge. Anything less and ordinary monotonic
  replans on unrelated Tasks become impossible; that is accepted, because the host
  rejects an unprovable proposal rather than acting on it.

### 5.2 Per-tier payload

One `tasks` array, each entry tagged with a `detail` discriminator, so the existing
prompt contract "an exact existing UUID from `request.tasks[].task_id`" and the
existing tests keep working. Fields outside a tier are omitted, not nulled.

| Field | FULL | STRUCTURAL | COMPACT |
| --- | --- | --- | --- |
| `task_id`, `status`, `mandatory`, `worker`, `dependencies`, `created_plan_revision` | yes | yes | yes |
| `title`, `objective`, `scope`, `verification` | yes | yes | no |
| `verification_results`, `evidence`, `blockers`, `attempts`, `max_attempts` | yes | no | no |

Omitted from the prompt at every tier: unrelated evidence bodies, unrelated
verification result bodies, every attempt of every non-seed Task, full superseded
Task bodies, and unrelated historical blockers.

### 5.3 History summarization

When replacement ancestry is relevant, the model needs to know that a Task was
replaced and by what — not the old evidence. A host-generated, deterministic
`history` array of at most `REPLANNER_HISTORY_SUMMARY_LIMIT = 64` entries:

```
superseded_task_id, status, superseded_plan_revision,
replan_request_id, committed_plan_revision,
replaced_by (sorted completion-closure / replacement Task ids),
preserved_max_attempts, preserved_consumed_attempts,
rebound_criterion_ids (sorted)
```

`superseded_plan_revision` is the window's selection key, exposed so the ordering
is self-evident to a reader of the prompt rather than implicit.

Selection: superseded Tasks ordered by `(superseded_plan_revision desc,
task_id asc)` — a total order, since `TaskId` is unique — truncated to 64, with
`history_omitted_count` reporting the remainder so the model knows history exists
beyond the window rather than believing the Goal has none.

`replaced_by` names the Task that took over *at the time*. In a multi-round chain
that successor may itself have been superseded later, so the assertion is that the
name resolves durably, not that it is still active. No evidence bodies, no
attempt bodies, no paths, no prose from the model itself. The model is never asked
to summarize its own authority history back to itself as input.

### 5.4 Bounded vectors

Two durable vectors are append-only and unbounded today. Both are capped with an
explicit omitted count so the omission is visible rather than silent:

- `goal_blockers`: at most `REPLANNER_GOAL_BLOCKER_LIMIT = 32`, keeping the most
  recent 32, plus `goal_blockers_omitted`.
- `failed_task_replan_requests`: at most
  `REPLANNER_REPLAN_REQUEST_LIMIT = 8`, matching the `goal_resume` `maxItems: 8`
  MCP bound, plus `failed_task_replan_requests_omitted`. Every unconsumed trigger
  is still a `full` seed (5.1 step 4), so a truncated authority token costs the
  model the ability to *name* that specific replacement, never the ability to see
  the work. The host validates against the complete durable request list.

`pre_execution_plan_rejections` is already bounded to 8
(`REPLANNER_PRE_EXECUTION_REJECTION_HISTORY_LIMIT`) and is unchanged.

### 5.5 Determinism

For identical durable `Goal` state and an identical replan trigger, the
host-generated request is byte-identical. Every collection is a `BTreeSet` or
explicitly sorted; `Goal.tasks` is a `BTreeMap`; no `HashMap` iteration reaches a
prompt; no field is derived from wall-clock time, randomness, or iteration order.
The only contractually varying fields are those that already vary: `goal_revision`,
`plan_revision`, and the durable history the Goal has accumulated. A regression
test asserts byte equality across repeated construction from the same Goal.

## 6. Hard request-size ceiling

The real transport limit already exists: `agent::MODEL_PROMPT_LIMIT = 256 KiB`, and
`GoalModelAgent::invoke` refuses a larger prompt with a generic
`AgentError::InvalidConfiguration`. That is fail-closed but undiagnosable, and it is
checked *after* a prompt string has been assembled.

New host-side bound, enforced in `ProductionReplannerBackend::propose_replan`
immediately after serialization and before the model is invoked:

```
REPLANNER_REQUEST_MAX_BYTES = 224 KiB (229376)
```

Margin: the fixed non-JSON prompt cost is `PROMPT_PREAMBLE` + `ROLE:` + 
`REPLANNER_RULES` (8611 B) + `COMMON_VERIFICATION_SCHEMA` (841 B) + the
`DATA_BEGIN`/`DATA_END` framing, measured at under 10 KiB. 229376 + ~10 KiB stays
below 262144 with roughly 23 KiB of headroom. A test pins
`REPLANNER_REQUEST_MAX_BYTES + measured_fixed_overhead <= MODEL_PROMPT_LIMIT` so the
margin cannot silently erode if the rules text grows.

On exceeding it, the Replanner fails closed with a new typed variant
`ReplannerError::ReplannerContextTooLarge { bytes, limit }`. That maps through
`map_replanner_error` to `SchedulerStepOutcome::LowerAuthorityError`, which returns
from `run_goal_foreground` with `GoalRunStopReason::LowerAuthorityError` and a
trace entry. It is a bounded terminal block: it is never retried, never
re-proposed, and never converted into another replan request. JSON is never
truncated at an arbitrary byte offset — a truncated proposal would be
unparseable and would hide the real condition.

## 7. Non-amplification

The specific claim to be proven: `HOST_OUTPUT_LIMIT` → replacement → larger durable
history does **not** imply that the next Replanner request grows proportionally to
all history.

The mechanism is section 5 evaluated on the PokéCPU shape. History contributes to
the request only through `history` (capped at 64 entries of fixed small fields) and
`history_omitted_count` (an integer). It contributes nothing to `tasks`, because
superseded Tasks are excluded from all three tiers by construction (5.1 step 5).
So a Goal can accumulate up to 1024 superseded Tasks and the request grows by at
most the difference between "fewer than 64 history entries" and "exactly 64",
after which it is flat.

Regression tests build synthetic Goals with a roughly constant active affected
subgraph and 0, 8, 32, 64, 128, 256, 512, and 1024 superseded Tasks, and assert the
request is byte-stable across 64 and above, with a small documented growth only
below it.

## 8. Verification obligations

Every new behavior is proven by a test, and every preserved invariant keeps its
existing test. New test groups:

- active-graph definition: superseded is history; `Completed`/`Failed`/`Cancelled`
  are active; the predicate is `is_active_plan_authority()`.
- quota separation: history above the old total-128 ceiling still admits a legal
  replacement; an actually oversized *active* graph is still rejected — for task
  count, dependency edges, scope paths, and verification entries separately.
- history ceilings: each of the four is enforced; no existing-Goal shape can trip
  it.
- context compaction: each tier contains exactly the intended ids; omitted fields
  are absent; evidence bodies and attempts of non-seed Tasks never appear; full
  superseded Task bodies never appear.
- history summary: format, ordering, cap, omitted count, and that it is
  host-generated.
- byte ceiling: the margin assertion; an oversized compact request fails closed
  with the typed error; the model is never invoked; the condition does not
  recursively request another replan.
- non-amplification: the 0/8/32/64/128 superseded-Task sweep with the active graph
  held constant, plus the per-Task payload invariant at each depth.
- determinism: byte-identical request across repeated construction.
- large synthetic Goals: small normal; 30–40 Task PokéCPU-like; near active
  limit; many superseded plus small active graph; deep dependency chain; broad
  fan-out; repeated replacement chains; heavy attempts/evidence; many unrelated
  completed Tasks; criteria bound to affected and to unrelated Tasks. Each records
  the measured serialized request bytes, not just Task counts.

## 9. Frozen decisions

1. Active execution graph is `status != Superseded`, via the existing
   `is_active_plan_authority()`. Not "non-terminal".
2. Durable history is never deleted, pruned, or rewritten.
3. Active quota values are unchanged; only the summed population changes.
4. Four explicit history ceilings are added, all strictly above any value an
   existing Goal can hold, so no migration and no schema bump.
5. Model context is a bounded, deterministic, host-selected relevance snapshot; the
   host still validates against the complete durable Goal.
6. Compaction never adds authority. Omitted context yields rejection, not
   permission.
7. The request ceiling is 224 KiB, checked before the model call, failing closed
   with a typed bounded blocker and no JSON truncation.
8. `goal_status` shape, Planner behavior, Writer behavior, Verifier behavior,
   Managed Worktrees, approval, sandbox, and Windows experimental status are
   unchanged.

## 10. Measured result

Before/after serialized Replanner request bytes, over the same deterministic
synthetic Goals (`src/replanner_compaction_tests.rs`):

| shape | active | history | before | after |
| --- | --- | --- | --- | --- |
| small | 6 | 0 | 6,998 | 6,943 |
| 30-40 Task PokéCPU-like | 38 | 0 | 29,127 | 27,051 |
| PokéCPU-like with history | 38 | 24 | 63,157 | 36,425 |
| history-heavy | 8 | 64 | **148,977** | **32,659** |
| deep dependency chain | 26 | 0 | 21,827 | 20,507 |
| broad fan-out | 26 | 0 | 26,589 | 25,267 |
| many unrelated completed | 48 | 0 | 57,817 | 34,977 |
| blocker and criterion weight | 8 | 0 | 16,076 | 19,112 |

Non-amplification sweep, active graph constant at 8 Tasks: 7.6 KB, 10.4 KB,
18.7 KB, 29.6 KB, 29.3 KB at 0, 8, 32, 64, 128 superseded Tasks — flat past the
bounded window, where the pre-change request grew proportionally to every
superseded Task body.

Two honest notes. `blocker and criterion weight` grows slightly, because padded
criterion prose is semantically load-bearing and is deliberately never truncated.
And the ceiling test uses that same shape deliberately: 64 criteria at the
`goal_start` limit of 8192 characters is a Goal that genuinely cannot fit in any
transport, so it must fail closed rather than be silently cut.

**REPLANNER_HARDENING_V1_FROZEN**
