# S0-0 — Serea future integration: contract assessment (no Serea code changed)

Sources of truth used:

* `Serea/docs/protocols/08-goallatch-adapter-protocol.md`
  (surface `serea.goallatch/1`, architecture `serea-arch/2.6.0`, status: **FROZEN
  current contract set**) — fetched 2026-11-09 from
  `https://github.com/Yrika819/Serea` via the GitHub API.
* The running GoalLatch tool contract at PR #50 head, plus the H4-A correction
  (published `goal_start.write_scope`).

Nothing here proposes a change to Serea's frozen contract. It records what the
adapter would have to satisfy, and where GoalLatch currently cannot satisfy it.

---

## 1. Responsibility split (unchanged, and must stay unchanged)

| Serea Core owns | GoalLatch owns |
| --- | --- |
| Task state machine, step planning, leases, recovery | PC-side Goal execution |
| Model routing and budgets | Workspace authority (session, root, path rules) |
| Policy evaluation, approval/grant evaluation | Writer / Reviewer legs |
| Capability registry, device sessions, timeline | Host verification and finalization |
| Event ordering and the audit trail | Publication and publication recovery |

Two rules from the Serea document dominate every recommendation below:

* **G8** — a model-reported "done" is never an outcome; `COMPLETED` requires a
  schema-valid, host-observed `host.goal.result` read.
* **G6 / §5.1** — a model can never route itself to Codex through delegation.
* **G4 / §9** — a real adapter may not be written until six live-artifact
  verifications pass; a verification that cannot be performed is a failure.

## 2. Verb-by-verb mapping (and the gaps)

| Serea | GoalLatch today | Verdict |
| --- | --- | --- |
| `host.goal.start` (`EXTERNAL_WRITE`, `SCOPED_GRANT`, 15 s) | `goal_start` (with `workspace_mode`, `model_backend`, `write_scope`) | **Mappable.** One call, durable Goal, no execution. `write_scope` is a *proposal*, never a grant — the adapter must not present it as approval. |
| `host.goal.run` (`EXTERNAL_WRITE`, 300 s, `CONDITIONAL`) | `goal_next` (+ `goal_submit`) | **Not mappable 1:1.** See §3.1. |
| `host.goal.status` (`OBSERVE`, 5 s) | `goal_status` | **Mappable**, but the state vocabulary does not line up (§3.2). |
| `host.goal.cancel` (`EXTERNAL_WRITE`, 10 s) | `goal_cancel` | **Mappable with a caveat**: GoalLatch cancels durable authority; it does not stop an in-flight job and never undoes published files. Matches "cancellation is not an inverse", so the adapter must map the *known* state, not assert a stop. |
| `host.goal.result` (`OBSERVE`, 10 s) | `goal_result` | **Not mappable as specified**: the required output has `summary`, `evidence[]` (`evidence_id` + `payload_reference`), `artifacts[]` (`name`/`media_type`/`byte_length`/`payload_reference`) and `execution_root`; `goal_result` returns counts and a managed-workspace block, not those refs (§3.3). |

Non-goal that the mapping must preserve: **no `goallatch.` capability prefix**,
five `host.goal.*` descriptors only, `provider_id: "host"`. GoalLatch must not
publish a tool named after a Serea capability, and Serea must not gain a
`goallatch.*` namespace. Nothing in GoalLatch's tool names (`goal_start`,
`goal_next`, …) needs to change; the adapter is the rename layer.

## 3. Findings

### 3.1 The model work is on the wrong side of the wire (most serious)

`host.goal.run` is specified as: Serea calls → the host advances the goal → Serea
*observes*. GoalLatch's EXTERNAL backend is the opposite: `goal_next` returns
`MODEL_REQUIRED` with a `ModelWorkItem` (`request`, `request_digest`,
`output_contract`, `response_required_fields`) and does nothing until the client
POSTs a `goal_submit`.

Consequences that must be decided before any adapter exists:

1. If the **adapter** answers the ModelWorkItem, then Serea is performing model
   work for GoalLatch — exactly what §1 says Serea does not do, and it makes
   Serea's model routing, budget and `codex_allowed` policy apply to a workload
   Serea believes it delegated. It also means the model prompt and output
   contract come from GoalLatch, i.e. GoalLatch would be steering Serea's model.
2. If the **adapter refuses** to answer, EXTERNAL Goals cannot progress at all.
3. If GoalLatch uses the **CODEX** backend instead, GoalLatch spawns Codex
   locally, which is the one behaviour Serea's protocol is built to prevent a
   model from reaching by delegation (G6). It would be a *product* decision
   (host-configured, not model-configured), but it must be explicit, not a
   default.

So the honest statement is: **the frozen `host.goal.run` contract and the EXTERNAL
backend are architecturally incompatible today.** Three candidate resolutions
(ADR material, Owner decision):

* **ADR-1 — "adapter is the model client".** Extend `host.goal.run`'s *adapter
  internal* behaviour so the adapter answers ModelWorkItems using a
  host-configured model, and record that as `implementation_id`
  `mcp-goallatch`'s declared behaviour. Requires a Serea-side note that a
  delegated goal may consume model tokens on the host, plus the descriptor's
  `cost_class: PAID` already anticipates.
* **ADR-2 — split the verb.** A new `host.goal.work` (or an `output` mode on
  `run`) that returns "the host needs this model output", with Serea's router
  producing it. This is the architecturally clean option and the one that keeps
  §1 intact, but it is an architecture-*minor* change requiring an ADR and a new
  verb, and it moves GoalLatch's role validators into Serea's trust boundary.
* **ADR-3 — GoalLatch self-executes (CODEX).** Simplest mapping, but it puts a
  Codex CLI in the PC path and must be approved as a deliberate exception, with
  its own capability flag.

Recommendation: **ADR-2 for the long term, ADR-1 as the only option that needs no
Serea contract change** — and do neither before the §9 gate is green.

### 3.2 State mapping needs an explicit table, not inference

Serea freezes six observed states: `PENDING`, `RUNNING`, `WAITING_APPROVAL`,
`COMPLETED`, `FAILED`, `CANCELLED`. GoalLatch has eleven
(`Planning`, `Running`, `Replanning`, `Pausing`, `Paused`, `Blocked`, `Verifying`,
`Cancelling`, `Completed`, `Failed`, `Cancelled`).

A lossy-but-honest mapping is possible (`Planning`/`Replanning`/`Verifying` →
`RUNNING`; `Pausing`/`Paused` → `WAITING_APPROVAL` **only** if a pending approval
actually exists, otherwise `RUNNING`; `Blocked` → `FAILED`? **No** — `BLOCKED` is
not terminal in GoalLatch, so it must map to `RUNNING` with a `blocked` evidence
record, or to `PROVIDER_ERROR`/`GOAL_STATE_UNRECOGNISED` per §3.1 of the Serea
doc).

This matters because the failure mode the Serea doc worries about is silent
coercion into `RUNNING`. The adapter must therefore be explicit: a table in code
with a `_ => GOAL_STATE_UNRECOGNISED` arm, and a test per GoalLatch state.

### 3.3 `host.goal.result` cannot be satisfied from `goal_result` today

The Serea output schema requires host-observed `evidence[]` with
`payload_reference` digests and `artifacts[]` with content-addressed refs.
`goal_result` currently returns: `result_state`/`result_kind`, task summaries,
`blockers`, `verification` summary, `evidence_items: usize` (a **count**), an
optional `managed_workspace` block (snapshot digest, changed paths, verification
ids), and `latest_checkpoint`.

The `managed_workspace` block is genuinely host-observed and digest-backed — that
is the only part that can honestly back `evidence[]`. `evidence_items` is a count
and must not be turned into N fabricated evidence records.

Two options: (a) additive GoalLatch fields on `goal_result`
(`evidence[]` with id + sha256 of the payload it already holds, `artifacts[]` for
the diff/patch the publication journal can derive, `execution_root` from the
managed worktree record); or (b) a narrower `host.goal.result` output version.
(a) is additive and keeps the Serea contract frozen, so it is the recommendation.

### 3.4 Task ID ↔ Goal ID

Serea's `GoalHandle` is opaque and must stay opaque. GoalLatch's `goal_id` is a
UUID string. The correct mapping is: `GoalHandle` = the GoalLatch `goal_id`
verbatim, stored in `SideEffectReceipt.provider_reference`, never parsed, never
correlated with a Serea `AssistantTask` by any structural rule. Serea's
`task_id` must be stored **by the adapter**, not inside GoalLatch: GoalLatch has
no task-id concept and adding one is a GoalLatch-side coupling that buys nothing.
Idempotency across restarts comes from Serea's `idempotency_key`, not from the
handle.

### 3.5 Two approval systems, one write

Serea's `SCOPED_GRANT` approval and GoalLatch's `GoalWriteGrantV1` OCS approval
are different objects with different lifetimes and different humans-in-the-loop
(today: the same human, on the same PC).

Dangerous shortcut to rule out explicitly: **do not auto-approve a GoalLatch
grant because Serea approved a `host.goal.run`.** That would collapse two
independent authorities into one and would hand grant issuance to the untrusted
side of the C3 boundary. Correct design: Serea's approval authorises the *call*;
GoalLatch's OCS approval authorises the *write*, and the adapter surfaces "this
goal is waiting for a PC-side grant approval" as `WAITING_APPROVAL` rather than
blocking silently for 15 minutes (`PENDING_GRANT_TTL_SECONDS`).

### 3.6 Reconnection, resume, cancellation

* `goal_next` is replay-safe: the same request keeps the same `work_id` and
  `request_digest`, and `goal_submit` is idempotent on
  `(work_id, submission_id, payload)`. This is a good fit for Serea's
  `replay_safety: CONDITIONAL` + reconciliation-through-reads rule.
* `goal_cancel` only moves durable authority; `goal_status`/`goal_result` are the
  correct read-back path. That matches Serea §8 almost exactly — the adapter must
  not claim a stop it cannot observe.
* What GoalLatch does **not** have: a resumable long-poll. `goal_next` returns
  immediately with either a WorkItem or a typed stop. A PC-side long-running
  step is therefore invisible to `host.goal.run`'s 300 s window unless the
  adapter polls.

### 3.7 Capability manifest / namespace

No conflict: five `host.goal.*` capabilities with `provider_id: host` can be
registered with `implementation_id: mcp-goallatch` while GoalLatch keeps its own
tool names. The manifest requires exactly one active implementation, which maps
cleanly onto "one GoalLatch MCP server per user session".

The reverse constraint does bite: GoalLatch must not grow a tool that *is* a
Serea capability (e.g. a `host_goal_run` MCP tool). The adapter belongs in Serea
(or in a separate sidecar), not inside GoalLatch's tool table.

### 3.8 Docs vs. running binary (Serea §9 verification 3)

This campaign is direct evidence for why that rule exists: the *documented* MCP
schema for `goal_start` (the published `inputSchema`) was missing a field the
runtime accepts. A client written against the docs would have been broken while a
client written against a by-hand payload would have worked. After H4-A the
published schema and the parser agree, and `tests/mcp_contract.rs` asserts it
from a real server process — which is exactly the artifact Serea's verification 3
asks for.

### 3.9 Android-only Serea vs. PC-side GoalLatch

GoalLatch's MCP server is **stdio-only** (`local-mcp mcp` serves stdin/stdout; the
only other IPC is a per-session named pipe / Unix socket for the OCS, which is
local and per-user). There is no network listener. Therefore:

* an Android app cannot reach `local-mcp mcp` directly;
* a real adapter needs a transport between the phone and the PC (SSH/tunnel, a
  future authenticated HTTPS transport, or a companion PC-side Serea Core);
* this is a **hard blocker** for an "Android-only Serea + PC GoalLatch" topology,
  and it is independent of every protocol question above.

It also does not endanger the Android Personal Beta: nothing in the Serea
contract requires a transport today (the fake is in-memory and offline).

## 4. What must NOT change on the Serea side

Frozen and to be treated as read-only: the five `CapabilityId`s, their
descriptor fields, `GoalObservedState`'s six variants, `GoalHandle` opacity,
`ActionErrorKind`, the approval bounds, the `AMBIGUOUS` rule, and the §9 gate. Any
of the ADRs above must be an *additive* change with its own ADR and version
justification per §10.

## 5. Verdict

| Question | Answer |
| --- | --- |
| Can a real adapter be written today? | **No.** §9 gate is red for verifications 4–6 (execution root, evidence contract, session authority behaviour were never observed from a live run) and the `host.goal.run` mismatch in §3.1 is unresolved. |
| Does GoalLatch need to change to be Serea-ready? | **Yes, additively**: `goal_result` evidence/artifact refs (§3.3), and an explicit observed-state mapping table (§3.2). Neither is a rewrite. |
| Does Serea need to change? | Only if ADR-2 is chosen. ADR-1 needs no Serea change but needs an explicit statement that a delegated goal may consume host-side model tokens. |
| Does any of this endanger the Android Personal Beta? | No. No Serea code was changed, and the fake remains the only described implementation. |
| Is the transport gap real? | **Yes** (§3.9). It is the cheapest thing to verify and the most expensive thing to discover late. |
