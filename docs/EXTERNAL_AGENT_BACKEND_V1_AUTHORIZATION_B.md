# GoalLatch External Agent Backend V1 — Authorization B (GoalWriteGrantV1)

> **Status:** DRAFT / Owner review required. Design + implementation record for
> the stacked Draft PR on top of PR #47.
> **Baseline:** `main` = `e6950fdc3bab72982285888a81cce822743fbba5`,
> PR #47 = `08f3edf9e1b03cfa5642c820c581c8183396908a` (OPEN/Draft, CI green at
> the time of writing). PR #43–#47 are not modified by this work.
> **Owner decision implemented:** External Writer authorization option **B** —
> the Owner limits workspace, paths, operations, expiry and budget once per Goal,
> and the host enforces exactly that scope. A model response, an MCP request, or
> any local IPC client can never create or extend a grant.
> **Non-claim:** this document does not approve V1 Beta. It does not merge,
> un-draft, tag, or release anything.

---

## 0. Executive summary

The External backend today has no Goal-scoped write authorization at all. An
EXTERNAL Goal becomes able to publish a Writer intent because the *host* decided
to issue a Writer WorkItem; the only human gate on the publication path is the
per-operation host approval inherited from Windows policy (and none on Unix).
B-option requires something stronger and narrower: an Owner-signed, Goal-scoped,
budgeted, expiring grant that the host validates at **issuance**, at **stage
creation**, and again **immediately before each filesystem write**.

This document specifies `GoalWriteGrantV1`, its durable ledger, its state
machine, the Owner Control Surface (OCS) that is the only writer of grants, the
publication fence that linearizes revocation against in-flight writes, the
budget accounting rules, the crash/revocation matrix, and the acceptance tests.

The hard part is not the data model. It is **who may say "the Owner approved
this"**. That is specified honestly in §2: what the architecture can prove, what
it cannot, and which deployments must therefore fail closed.

---

## 1. Assets, trust boundaries, and what changed in the model

### 1.1 Existing authorization surfaces (audited at PR #47)

| Surface | Code | Authority today | Reusable for B? |
|---|---|---|---|
| Generic host-native approval IPC | `src/approvals.rs` | Operator answers on the `local-mcp start` console (`stdin`); MCP callers can only *request*; `yolo` is console-local | Yes: its **decision channel** is reused as the OCS transport |
| Session path authority | `src/config.rs` | Canonicalised permitted roots; rejects `..`, symlink/junction escapes, ADS, drive-relative paths | Yes: every grant path is validated through it |
| Goal state / CAS | `src/goal.rs`, `src/task_store.rs` | Session OS lock, optimistic revision CAS, atomic replace + fsync | Yes: used as the rollback anchor (§6.3) |
| External durable state | `src/external_model_work.rs`, `src/external_model_store.rs` | One active WorkItem, receipts, revision binding | Extended additively (schema 1 → 2) |
| Writer journal | `src/mutation.rs` | Prepared / Applying / Applied per operation, preimage, postimage | Yes: the grant binding is recorded in it |
| Writer stage | `src/writer_stage.rs` | Private, content-addressed, identity-bound bytes | Yes: identity gains the grant binding |
| Publication | `src/workspace_publish.rs` + `src/external_publish.rs` | Preimage CAS, per-operation journal, postimage verification | **The only** write path; the gate is inserted here, no second path is created |
| Managed worktree identity | `src/managed_worktree*.rs` | Host-derived root, creation intent, approval, lease, execution-root revalidation | Yes: the grant binds the same identity |

There are exactly two process roles:

```text
Owner Control Surface (OCS)                     Untrusted side
local-mcp start  ── stdin/stdout of the Owner   MCP client / external model
  │  only writer of the Authorization Ledger      │  goal_start / goal_next / goal_submit
  │  shows live nonce, writes decisions           │
  └─────────────── named pipe / unix socket ──────┘
                    (existing approvals pipe; requests only)
```

### 1.2 New trust boundary: Owner Control Surface (OCS)

`GoalWriteGrantV1` is issued **only** by the `local-mcp start` process after:

1. a grant *request* arrives over the existing session pipe (the requester may
   be the MCP server, which is untrusted input plumbing);
2. the OCS validates the request against the durable Goal (session ownership,
   EXTERNAL backend, workspace identity, paths inside session authority);
3. the OCS generates a **live nonce** that exists only in the OCS process and on
   the Owner's console, and is written to the ledger only as a SHA-256 digest
   *after* a decision;
4. the Owner types an explicit command that contains the request id **and the
   nonce**:
   `/grant approve <request-id> <nonce>` (or `/grant deny <request-id>`);
5. the OCS writes the decision to the ledger, then mirrors the authorization
   epoch into the Goal document (the rollback anchor).

No MCP field, no model output, and no generic `approve=true` value is ever read
as an Owner decision. `yolo` is explicitly **not** an Owner approval: it may
auto-answer per-operation host approvals, but it can never issue, extend, or
revive a grant (AT-GW-012).

### 1.3 The honest limit: same OS user, same console

GoalLatch runs as the logged-in user. If the agent itself has unsandboxed
host-native command execution (its own terminal, or an `execute` call the Owner
already approved), that process runs as the same OS user and shares the same
interactive console as `local-mcp start`. Such a process can, in principle,
write to the console, read process memory of the same user, or rewrite state
files. **No user-mode Rust design can distinguish that process from the Owner.**

The design therefore does not pretend otherwise. It separates three claims:

| Claim | Enforced by | Status |
|---|---|---|
| C1: An MCP client/model response cannot create/extend a grant through GoalLatch | Grant creation exists only in the OCS code path; MCP schemas use `deny_unknown_fields`; the live nonce is never durable or MCP-visible | **Enforced + tested** |
| C2: A grant cannot be transplanted across Session/Goal/workspace/backend, replayed after revocation, extended past its budget, or used after expiry | Identity digests, epoch + Goal anchor, ledger lock, per-operation checks | **Enforced + tested** |
| C3: A same-user process with unsandboxed host execution cannot impersonate the Owner | OS security principal / user-presence attestation / separate service account — none of which exists in the current architecture | **NOT VERIFIED / residual risk R-1** |

R-1 is a **Critical deployment constraint**, not a hidden assumption:

* GoalLatch's stated guarantee is bounded to operations that go *through*
  GoalLatch (`docs/EXTERNAL_AGENT_BACKEND_V1_THREAT_MODEL.md` §0.3-5). A process
  that bypasses GoalLatch is outside that guarantee.
* Any deployment where the external agent's own process tree can read the OCS
  terminal, write the state directory, or attach to the OCS process is
  **equivalent to handing the grant to the agent**. Operators must either
  sandbox the agent outside the OCS console/state, or accept R-1 explicitly.
* `session_info` and every grant view report
  `owner_attestation = "CONSOLE_LIVE_NONCE_V1"` with
  `same_uid_process_resistance = false`. The value is deliberately *not* named
  "Owner authenticated" so no report can over-claim.

A future C3 design (separate OS principal, Windows Hello/TPM-backed signature,
or a protected service) plugs into the same `OwnerDecision` record and the same
ledger without changing the grant, journal, or publication fence. That is why
the grant stores `owner_attestation` as a typed enum rather than a boolean.

---

## 2. Threat model additions (TM-GW-01 … TM-GW-14)

| ID | Threat / scenario | Impact | Mitigation | Residual |
|---|---|---|---|---|
| TM-GW-01 | Model output or MCP request asserts `approved`/`grant_id`/`scope` to widen authority | Unapproved write | Grant types are never deserialized from `goal_submit`; `deny_unknown_fields`; grant existence is re-read from the ledger, never from the response | — |
| TM-GW-02 | MCP client forges an Owner decision over the session pipe | Unapproved write | The pipe carries **requests only**; decisions exist only as OCS console input + live nonce; the nonce is not durable and not MCP-visible | R-1 |
| TM-GW-03 | `yolo` used as a grant shortcut | Unapproved write | Grant issuance path ignores session yolo entirely; regression test asserts denial | — |
| TM-GW-04 | Grant replay across Goal / Session / backend | Cross-goal write | `goal_identity_digest`, `session_id`, `backend_identity` bound at issuance and recomputed at every check | — |
| TM-GW-05 | Grant transplant to another workspace (primary ↔ managed, other worktree) | Write outside approved tree | `workspace_identity` includes mode, canonical root, worktree id, base commit; compared with `validated_execution_root` | — |
| TM-GW-06 | Revocation race: Owner revokes while the writer publishes | Post-revocation write | Per-operation ledger lock is the linearization point; `REVOKE_REQUESTED` (durable) blocks new operations; `REVOKED` is only written after the fence is acquired | — |
| TM-GW-07 | Rollback: restore an older ACTIVE ledger file | Reuse a revoked grant | `authorization_epoch` is monotonic; Goal-side `authorization_anchor.highest_epoch` / `revoked_at_epoch` is written under Goal CAS; a ledger below the anchor is refused | Full local state rollback (documented) |
| TM-GW-08 | Budget bypass: replay an operation id, refund an UNKNOWN side effect, split a submission | Unbounded writes | Charge is idempotent per `operation_id`, occurs **before** the side effect, never refunded; `UNKNOWN` stays charged; single active WorkItem per Goal | — |
| TM-GW-09 | Path traversal / symlink / junction swap between stage and publish | Write outside scope | Grant path rules are canonicalised; `revalidate_intent_operation_before_publish` + session path authority re-check immediately before each write | OS-level TOCTOU outside the host (existing) |
| TM-GW-10 | Grant extension by re-issuing while an old WorkItem/stage exists | Old work silently re-authorized | New issuance creates a new `grant_id` and increments epoch; WorkItem and stage carry the old binding and are refused | — |
| TM-GW-11 | Expiry bypass via clock rollback | Write after expiry | `now < issued_at` or `now > expires_at` fails closed (`WRITE_AUTHORIZATION_EXPIRED`); no monotonic-clock assumption across restarts | — |
| TM-GW-12 | Ledger tampering / malformed record | Forged ACTIVE grant | Strict schema + `deny_unknown_fields` + digest/identity revalidation + bounded fields; unreadable ledger ⇒ fail closed, never "no grant means allowed" | R-1 |
| TM-GW-13 | `REVOKE_REQUESTED` observed but publication continues | Write after revocation intent | Publishing checks `state == ACTIVE` under the lock, not "not yet revoked" | — |
| TM-GW-14 | Console request flood / stale pending request | DoS, accidental approval | One pending request per Goal, bounded pending cap, 15-minute pending TTL, nonce required, `/grant list` shows state | — |

Safety invariants (NO-GO if violated):

1. No EXTERNAL filesystem write occurs without an `ACTIVE` grant whose epoch,
   identity, workspace, path, operation, expiry and budget all pass at the
   moment of the write.
2. A grant is never created, widened, renewed, or revived by an MCP message, a
   model response, `yolo`, or a generic approval answer.
3. After `REVOKED` is durable, no new operation can begin.
4. An operation that may have occurred is never reported as not performed and is
   never refunded.
5. Existing CODEX goals, APIs, result states, and failure semantics are
   unchanged.

---

## 3. Durable placement: extending `ExternalModelState` vs a separate ledger

| Criterion | Extend `ExternalModelState` | Separate Authorization Ledger | **Chosen: hybrid** |
|---|---|---|---|
| Atomicity with Goal state | One CAS | Needs cross-file protocol | Ledger is authoritative for grants; a small **anchor** lives in `ExternalModelState` |
| Write path | Any Goal writer | Dedicated | Only the OCS issues; publisher updates usage under a per-Goal ledger lock |
| CODEX compatibility | Touches the shared Goal document | Zero effect on CODEX | Schema bump confined to the EXTERNAL sub-record |
| Revocation latency | Requires the Goal session lock | Per-Goal lock, no session-wide contention | Required |
| Crash recovery | Goal snapshot | Ledger + anchor | Ledger record + Goal anchor reconciled fail-closed |
| Rollback resistance | Only Goal revision | None by itself | Anchor epoch in the Goal CAS (§6.3) |
| Tamper surface | Goal doc | Ledger file | Both validated; unreadable ⇒ refuse |
| Old EXTERNAL Goal migration | Field default | File absence | Anchor defaults to `0` ⇒ no grant ⇒ Writer refused |

Decision:

* `authorization/<session>/<goal>.json` is the **authoritative grant ledger**
  written only by the OCS (plus the publisher's locked usage update).
* `ExternalModelState` (EXTERNAL sub-schema 2; the containing Goal schema stays 6) stores
  `authorization_anchor: { highest_epoch, revoked_at_epoch, last_grant_id }`,
  updated only through Goal CAS. This is the rollback anchor and the point that
  makes a revoked grant unusable even if the ledger is restored.
* Neither structure duplicates authority: the ledger cannot authorize without a
  matching anchor, and the anchor cannot authorize anything by itself.

Paths (private, `secure_fs` permissions):

```text
<state>/authorization/<session_id>/<goal_id>.json   ledger (atomic replace + fsync)
<state>/authorization/<session_id>/<goal_id>.lock   per-Goal publication/revocation fence
```

---

## 4. `GoalWriteGrantV1` schema

```jsonc
{
  "schema_version": 1,
  "grant_id": "<uuid-v4>",
  "owner_decision_id": "<uuid-v4>",
  "session_id": "<session>",
  "goal_id": "<goal-uuid>",
  "backend_identity": "EXTERNAL/writer-v1",
  "goal_identity_digest": "<sha256-hex>",
  "workspace": {
    "mode": "PRIMARY" | "MANAGED_WORKTREE",
    "root": "<canonical absolute path>",
    "worktree_id": "<uuid|null>",
    "base_commit": "<40-hex|null>"
  },
  "allowed_paths": [
    { "path": "<canonical absolute path>", "kind": "FILE" | "TREE" }
  ],
  "allowed_operations": ["WRITE_UTF8"],
  "budget": { "max_operations": 16, "max_bytes": 1048576 },
  "issued_at": "2026-11-08T12:00:00Z",
  "expires_at": "2026-11-08T20:00:00Z",
  "authorization_epoch": 3,
  "revocation": {
    "state": "NOT_REVOKED" | "REVOKE_REQUESTED" | "REVOKED",
    "requested_at": "<rfc3339|null>",
    "revoked_at": "<rfc3339|null>",
    "owner_decision_id": "<uuid|null>"
  },
  "usage": {
    "operations": 2,
    "bytes": 4096,
    "charged_operation_ids": ["e5-writer:<attempt>:<uuid>"]
  },
  "owner_attestation": "CONSOLE_LIVE_NONCE_V1"
}
```

`PendingGrantRequestV1` (durable part of a request, no nonce):

```jsonc
{
  "request_id": "<uuid-v4>",
  "session_id": "...", "goal_id": "...",
  "backend_identity": "EXTERNAL/writer-v1",
  "goal_identity_digest": "...",
  "workspace": { ... },
  "allowed_paths": [ ... ],
  "allowed_operations": ["WRITE_UTF8"],
  "budget": { "max_operations": 16, "max_bytes": 1048576 },
  "requested_at": "...", "expires_at": "...",
  "nonce_sha256": "<sha256(hex(nonce))>"
}
```

Bounds (host-enforced, not Owner-supplied): `allowed_paths ≤ 64`,
`allowed_operations` must be a subset of `{WRITE_UTF8}`, `max_operations ≤ 64`,
`max_bytes ≤ 16 MiB`, `expires_in ∈ [60 s, 7 d]`, `allowed_paths` each
`≤ 4096` bytes and inside the session's permitted roots and the workspace root.

`goal_identity_digest` is SHA-256 over a framed encoding of
`(goal_id, session_id, objective, title, constraints, completion criteria ids +
descriptions, created_at)`. It changes if any authority-relevant Goal text
changes, so a grant cannot be reused for a different Goal even with the same id
after a state transplant.

`workspace_identity` is SHA-256 over
`(mode, canonical root, worktree_id, base_commit)`. For `MANAGED_WORKTREE` the
values come from the host-generated `ManagedWorktreeRecord` (`REQUESTED` before
creation, `ACTIVE` afterwards) and are compared with
`managed_worktree_prepare::validated_execution_root` at every check.

---

## 5. Authorization state machine

```text
                  OCS: /grant approve <id> <nonce>
   (none) ──request──▶ PENDING ─────────────────────────▶ ACTIVE
                         │                                  │
                         │ OCS: /grant deny                 │ /grant revoke
                         ▼                                  ▼
                       DENIED                        REVOKE_REQUESTED
                         (terminal)                         │ fence acquired
                                                            ▼
                                                         REVOKED  (terminal)

   ACTIVE ──now > expires_at──▶ EXPIRED (derived, terminal for use)
   ACTIVE ──epoch mismatch with anchor──▶ INVALIDATED (refused)
```

| State | Durable | New operations | Re-issuable | Notes |
|---|---|---|---|---|
| `PENDING` | yes (ledger) | no | yes (new request) | live nonce only in OCS memory + console; TTL 15 min |
| `ACTIVE` | yes | yes | — | requires anchor epoch match |
| `REVOKE_REQUESTED` | yes | **no** | after finalization | written under the fence; blocks the next operation |
| `REVOKED` | yes | no | yes, with a **new** grant id + epoch | terminal for this grant id |
| `EXPIRED` | derived | no | yes, new grant | derived from `expires_at`, never auto-revived |
| `DENIED` | yes | no | yes, new request | terminal for this request id |

Rules:

* States are never collapsed into a boolean. In particular
  `REVOKE_REQUESTED ≠ REVOKED`.
* An expired or revoked grant is never re-activated. Re-issuance always creates a
  new `grant_id`, increments `authorization_epoch`, and therefore invalidates
  every WorkItem, stage, and journal binding that named the old grant.
* A pending request whose OCS process died cannot be approved: the nonce is gone.
  `/grant list` reports it as `PENDING_STALE`, and a new request supersedes it.

---

## 6. Durability, CAS, recovery

### 6.1 Ledger write protocol

All ledger mutations happen under the per-Goal `.lock` file
(`secure_fs::create_or_open_lock` + bounded `try_lock` retry):

1. read the ledger (whole file; atomic replace makes torn reads impossible);
2. validate schema and identity;
3. apply exactly one transition in memory;
4. write a private temp file, `flush` + `sync_all`, atomic replace, `sync_all`
   of the parent directory.

There is no in-place edit and no lock-free write. The OCS is the only issuer;
the publisher only updates `usage` and `REVOKE_*` is written only by the OCS.

### 6.2 Goal-side anchor

`ExternalModelState.authorization_anchor` is written in the same CAS commit that:

* binds a new Writer WorkItem (`highest_epoch = max(anchor, grant.epoch)`,
  `last_grant_id = grant.grant_id`), or
* records an OCS revocation (`revoked_at_epoch = grant.epoch`,
  `highest_epoch = max(anchor, grant.epoch)`).

A grant is usable only if `grant.epoch >= anchor.highest_epoch`,
`anchor.revoked_at_epoch` is absent or `< grant.epoch`, and
`anchor.last_grant_id` is either absent or equal — i.e. the ledger cannot be
older than the Goal's own memory of it.

### 6.3 Crash / revocation matrix

| Crash point | Durable state | Next host step | Guarantee |
|---|---|---|---|
| Before ledger temp write | old ledger | request can be retried; nothing granted | no authority |
| After temp, before replace | old ledger + orphan temp | orphan is ignored/removed | no authority |
| After ledger `ACTIVE`, before anchor write | ACTIVE ledger, old anchor | anchor is refreshed on the next issuance/status step; publication refuses until anchor matches (`AUTHORIZATION_ANCHOR_STALE`) | **fail closed** |
| After anchor write, before response to caller | ACTIVE + anchor | caller re-reads the ledger; idempotent | grant usable |
| PENDING written, OCS dies | PENDING without live nonce | stale request; new request needed | no authority |
| Revocation: `REVOKE_REQUESTED` written, crash before `REVOKED` | REQUESTED | publishers refuse; OCS finalizes on `/grant revoke` or next status | no new write |
| Revocation: `REVOKED` written, crash before anchor | REVOKED + old anchor | ledger already refuses; anchor finalized on retry | no new write |
| Publication: after budget charge, before `BeginOperation` | charged, journal Prepared | operation may publish later; charge is not double-counted | at-most-once accounting |
| Publication: after `BeginOperation`, before file write | journal Applying, charged | existing Writer reconciliation; **not** treated as not-performed | UNKNOWN preserved |
| Publication: after file write, before postimage | journal Applying, file changed | reconciliation observes postimage; no blind retry | UNKNOWN preserved |
| Publication: after postimage, before `CompleteOperation` | journal Applying, verified postimage | existing reconciliation completes the operation | no duplicate write |
| Publication: after `CompleteOperation`, before WorkItem completion | journal Applied | existing `published_intent_awaiting_completion` bookkeeping (H3) | no second write |
| Revocation arrives during a multi-file publication | earlier ops Applied; fence waits for the current op | next operation refused; Task blocked with `WRITE_AUTHORIZATION_REVOKED`; earlier ops stay journaled | partial publication is not misreported as atomic |

### 6.4 Concurrency

* Two MCP clients, two processes, one Goal: the ledger lock serializes grant
  reads/charges; the Goal session lock serializes journal commits. Lock order is
  always **ledger → Goal**; nothing takes them in reverse, so no deadlock.
* Two Writer submissions for one WorkItem (E5-H2): only one commits; its stage
  is charged at publish. The loser's stage is removed as today.
* The OCS revocation waits at most `AUTHORIZATION_FENCE_TIMEOUT` (30 s). On
  timeout it reports `REVOCATION_DEFERRED` and does **not** claim REVOKED.
* No session-wide lock is held during file I/O or during an approval wait.

### 6.5 Tampering and old-state migration

* Ledger / anchor mismatch (identity, epoch, grant id): refuse, never repair
  silently. `AUTHORIZATION_STATE_MISMATCH` is a mandatory blocker.
* A pre-authorization EXTERNAL Goal has no ledger and anchor `0`. It decodes
  normally (fail-closed): any Writer issuance returns
  `WRITE_AUTHORIZATION_REQUIRED`. No grant is invented by migration.
* CODEX Goals never create, read, or write authorization state. The migration
  is confined to `EXTERNAL_MODEL_SCHEMA_VERSION` 1 → 2 (additive defaults), so
  `GOAL_SCHEMA_VERSION` stays 6 and the CODEX migration chain is untouched.
* A newer EXTERNAL sub-record read by an older binary yields
  `UNSUPPORTED_SCHEMA` (fail-closed, no silent downgrade).

---

## 7. Enforcement points (no second write path)

| Point | Code | Checks |
|---|---|---|
| `goal_start` | `goal_api::goal_start` | `write_scope` accepted only for EXTERNAL; host bounds; request sent to OCS **after** the Goal is durable and **after** the create lock is released |
| Owner request | `authorizations::request_owner_grant` | session pipe only; bounded response wait; no Goal lock held |
| OCS approval | `approvals::handle_input` + ledger | goal ownership, EXTERNAL backend, goal identity digest, workspace identity, path authority, nonce |
| Writer WorkItem issuance (W0) | `external_writer::issue_external_writer_work_item` | ACTIVE + epoch + anchor + expiry + workspace + path scope + remaining budget; binds `grant_id`/`epoch` into the WorkItem and the anchor in the same commit |
| Stage commit (W1) | `external_writer::commit_external_writer_preparation` | re-validate the binding, every operation path under `allowed_paths`, operations ⊆ allowed set, projected budget ≤ grant budget; record the binding in the durable `MutationIntent` |
| Publication (W2) | `external_publish::publish_prepared_writer_intent` | **per operation**: ledger lock → ACTIVE + epoch + anchor + expiry + workspace + path + op + budget → idempotent charge → `BeginOperation` → write via `ExecutionWriteBoundary`/`workspace_publish` → postimage → `CompleteOperation` |
| Revocation | OCS `/grant revoke` | ledger lock → `REVOKE_REQUESTED` → finalize `REVOKED` → anchor CAS; blocks the next operation |
| Recovery | existing Writer reconciliation + driver | a revoked/invalidated grant blocks publication; journal state is preserved; no auto re-grant |

Every point re-reads durable state. The grant object is never carried in a
cache across a process, lock release, or await.

---

## 8. Public API additions (additive only)

`goal_start` request (EXTERNAL only):

```jsonc
{
  "session_id": "...", "objective": "...", "completion_criteria": ["..."],
  "model_backend": "EXTERNAL",
  "write_scope": {
    "allowed_paths": ["src/**", "docs/design.md"],
    "allowed_operations": ["WRITE_UTF8"],
    "max_operations": 16,
    "max_bytes": 1048576,
    "expires_in_seconds": 28800
  }
}
```

* In a CODEX Goal `write_scope` is refused with `INVALID_ARGUMENT` (regression
  guard: CODEX behavior is unchanged).
* In an EXTERNAL Goal it is a **proposal to the Owner**, not an authorization.
  The response reports the durable outcome:

```jsonc
"authorization": {
  "state": "NOT_REQUESTED" | "PENDING" | "ACTIVE" | "DENIED" | "UNAVAILABLE",
  "grant_id": "...", "authorization_epoch": 3,
  "expires_at": "...", "allowed_paths": ["..."], "allowed_operations": ["WRITE_UTF8"],
  "budget": { "max_operations": 16, "max_bytes": 1048576 },
  "owner_attestation": "CONSOLE_LIVE_NONCE_V1",
  "same_uid_process_resistance": false
}
```

`goal_status` and `goal_result` gain the same `authorization` view (additive).
`goal_next` reports authorization state through its existing blocker/progress
codes so every branch keeps one typed shape:

```text
outcome: BLOCKED  blockers[0].code: WRITE_AUTHORIZATION_<...>
outcome: HOST_PROGRESS (grant active, work continues)
```

Progress/stop codes:

```text
WRITE_AUTHORIZATION_REQUIRED     no grant; Writer cannot be issued
WRITE_AUTHORIZATION_PENDING      Owner has not decided yet
WRITE_AUTHORIZATION_DENIED       Owner denied the request
WRITE_AUTHORIZATION_UNAVAILABLE  no OCS (session not running)
WRITE_AUTHORIZATION_EXPIRED      expires_at passed or clock regressed
WRITE_AUTHORIZATION_REVOKED      REVOKE_REQUESTED or REVOKED
WRITE_AUTHORIZATION_STATE_MISMATCH ledger/anchor/goal identity disagree
WRITE_AUTHORIZATION_SCOPE_MISMATCH path or operation outside the grant
WRITE_BUDGET_EXHAUSTED           operation or byte budget exhausted
```

Console commands (Owner, `local-mcp start`):

```text
/permission ...                       (unchanged)
/grant list                           pending + active + history (bounded)
/grant status
/grant approve <goal-id> <request-id> <nonce>
/grant deny <goal-id> <request-id>
/grant renew <goal-id>          (Owner-initiated re-issue after expiry/revocation)
/grant revoke <goal-id>
```

MCP callers cannot invoke any of these; the pipe has no decision message type,
and the nonce never crosses it before approval.

---

## 9. Acceptance test plan (AT-GW)

| Test | Scenario | Pass condition |
|---|---|---|
| AT-GW-001 | No grant, EXTERNAL Writer task reaches the scheduler | `WRITE_AUTHORIZATION_REQUIRED`, zero filesystem writes |
| AT-GW-002 | Owner approves with the correct live nonce | ACTIVE grant, anchor updated, Writer issuance proceeds |
| AT-GW-003 | Wrong/missing nonce, wrong request id | DENIED, no ACTIVE record |
| AT-GW-004 | `goal_submit` carries `approval`, `grant_id`, `scope`, `authorized` fields | strict schema refusal; no authority change |
| AT-GW-005 | Session `yolo` active | grant request still requires the nonce; yolo never issues |
| AT-GW-006 | Grant for Goal A used by Goal B (same session) | `REJECTED_AUTHORITY`; no write |
| AT-GW-007 | Grant for session A used by session B | opaque refusal; no cross-session leak |
| AT-GW-008 | Grant bound to PRIMARY used on a MANAGED goal (and vice versa) | `WRITE_AUTHORIZATION_SCOPE_MISMATCH` |
| AT-GW-009 | Path outside `allowed_paths`, `..`, symlink/junction swap | refusal before `BeginOperation`; no file change |
| AT-GW-010 | Operation other than WRITE_UTF8 | refusal |
| AT-GW-011 | Expiry passed | `WRITE_AUTHORIZATION_EXPIRED`; no write |
| AT-GW-012 | Clock moved backwards | `WRITE_AUTHORIZATION_EXPIRED`; no write |
| AT-GW-013 | Budget exhausted by operation count / bytes | `WRITE_BUDGET_EXHAUSTED`; earlier writes remain journaled |
| AT-GW-014 | Same operation id processed twice | single charge; no double budget consumption |
| AT-GW-015 | Revocation before publication | `WRITE_AUTHORIZATION_REVOKED`; no write |
| AT-GW-016 | Revocation during a multi-file publication | earlier ops Applied, later ops refused, Task blocked, journal honest |
| AT-GW-017 | Crash after charge, before BeginOperation | recovery does not over-charge; state is Prepared |
| AT-GW-018 | Crash after BeginOperation, before/after write | journal Applying; reconciliation required; no blind retry |
| AT-GW-019 | Re-issue grant, then use the old WorkItem/stage | refused (epoch mismatch) |
| AT-GW-020 | Old EXTERNAL Goal (schema 1, no anchor) | decodes; Writer refused `WRITE_AUTHORIZATION_REQUIRED` |
| AT-GW-021 | Ledger rolled back below the Goal anchor | `AUTHORIZATION_STATE_MISMATCH`; no write |
| AT-GW-022 | Corrupt/truncated ledger | fail closed; no write; no repair |
| AT-GW-023 | Two concurrent publishers, one Goal | ledger lock serializes; at-most-once per operation |
| AT-GW-024 | Two concurrent MCP clients submit the same WorkItem | one commit; loser stage removed; no double charge |
| AT-GW-025 | OCS absent / pipe unavailable | `WRITE_AUTHORIZATION_UNAVAILABLE`; goal remains usable read-only |
| AT-GW-026 | CODEX Goal regression suite | unchanged behavior |
| AT-GW-027 | Managed worktree: grant bound to worktree identity | Writer executes in the managed root; primary untouched |
| AT-GW-028 | Managed worktree stale/removed identity | publication refused before any write |
| AT-GW-029 | AT-E5-029 / AT-E5-030 re-evaluation | no unapproved write; no TaskStore lock held during the Owner wait |
| AT-GW-030 | AT-E5-043 re-evaluation | revocation/cancel during an UNKNOWN side effect stays UNKNOWN and requires reconciliation |

AT-E5-072…082 (managed worktree) and Scenarios A–D are re-run at the
integration phase and recorded separately from these unit/integration tests.

---

## 10. Rollout and compatibility

* `EXTERNAL_MODEL_SCHEMA_VERSION` 1 → 2 adds, with defaults: WorkItem
  `authorization`, `ExternalModelState.authorization_anchor`. `GOAL_SCHEMA_VERSION`
  remains 6; the CODEX migration chain and all CODEX result semantics are
  unchanged.
* Old EXTERNAL Goals load and are **read-only for Writer purposes** until the
  Owner issues a grant. No automatic grant, no automatic migration of intent.
* Disabling authorization is not a supported configuration: a deployment that
  cannot run an OCS cannot perform EXTERNAL writes. That is the point of B.
* Rollback of the software to a pre-authorization build would ignore the ledger;
  therefore the ledger and anchor are only produced by builds that enforce them,
  and the anchor lives inside the EXTERNAL sub-record so an older binary sees an
  unsupported sub-schema rather than silently dropping the requirement.

---

## 11. Implementation status (updated at the Authorization implementation PR)

| Area | Status |
|---|---|
| `GoalWriteGrantV1`, ledger, anchor, usage accounting | implemented (`src/authorization.rs`, `src/authorization_store.rs`) |
| OCS console commands + live nonce | implemented (`src/approvals.rs`) |
| `goal_start` `write_scope` proposal (EXTERNAL only) | implemented; durable in the EXTERNAL sub-record |
| `goal_next` Owner request / typed refusal | implemented |
| Writer issuance, stage, per-operation publication gate | implemented |
| Revocation fence + epoch + rollback anchor | implemented |
| Independent security review | round 1 complete; H1–H4 + M1/M2 remediated (see §12.1) |
| Managed-worktree preparation for EXTERNAL | implemented through the existing `prepare_managed_workspace` |
| Unit/integration acceptance (AT-GW-001..025) | 27 tests green on Windows; full suite green |
| Crash injection: charge→begin, begin→write, write→complete, re-grant, schema-1 migration | implemented (A3) |
| Concurrent same-operation charge and post-revocation refusal | implemented (A3) |
| AT-E5-029 / 030 / 043 re-evaluation | 029 and 030 covered by the no-grant/revocation tests and by the absence of a Goal lock during the Owner wait; 043 remains covered by existing reconciliation (no change to UNKNOWN semantics) |
| Independent security review | round 1 complete; remediation pending re-review |

Known implementation notes:

* The grant binding is recorded in the `ModelWorkItem` and in the durable
  `MutationIntent` of the prepared write. The stage is content-scoped by that
  same intent; publish re-reads the binding and the ledger under the fence.
* A test-only Approval Harness (`authorization_store::test_support`) mirrors the
  OCS two-write sequence so pre-Authorization-B Writer regression fixtures keep
  their coverage. It exists only under `cfg(test)` and cannot appear in a
  production binary.
* EXTERNAL sub-schema 1 is migrated to 2 in the store decode path with an empty
  scope and a zero anchor. No grant is invented.

### 11.1 Round-1 security remediation

An independent review (separate provider/model, read-only) found four High and
two Medium issues. All six are addressed on the integration branch:

| ID | Finding | Fix |
|---|---|---|
| H1 | The ledger fence ended before the filesystem write, so a revocation could finalize while a write was about to start | The per-Goal fence is now held across `BeginOperation` → write → postimage → `CompleteOperation`. Revocation waits for the in-flight operation (bounded by `AUTHORIZATION_FENCE_TIMEOUT` and the operation's own bounds); it never reports `REVOKED` while a write is executing. AT-GW-016b proves the wait. |
| H2 | Untrusted console text could repaint or reorder the approval prompt (path rules, activity, approval detail) | All untrusted console fields pass through `sanitize_console_field`; path rules reject C0/C1 and bidi characters at the model; the OCS resolves every rule against the workspace root and rejects symlink/junction escapes before approval. AT-GW-009b/009c. |
| H3 | `REVOKED` was saved before the Goal anchor; a crash left a replayable anchor and retry returned early | The revocation anchor is written **before** the ledger finalizes `REVOKED`; an idempotent retry repairs a missing anchor. AT-GW-021b. |
| H4 | Restoring an older ACTIVE ledger with the same grant/epoch restored spent budget | The Goal anchor now carries a monotonic usage floor (`usage_operations`, `usage_bytes`), written under Goal CAS before each charge. Effective usage is `max(ledger, anchor)`, so a restored ledger cannot regain budget. AT-GW-021c. |
| M1 | The Owner-requested grant lifetime was ignored; every grant got the 15-minute pending TTL | Pending TTL and grant lifetime are separate. Approval issues `now + requested_lifetime_seconds`, bounded to `[60s, 7d]`. AT-GW-028b. |
| M2 | A surviving command line could approve a pending request after an OCS restart | Approval requires the request to be live in this process's queue **and** the nonce digest to match. `/grant renew` adds its request to the same queue. |

### 11.2 Round-2 security remediation

Round 2 reviewed `dadfad5` and found four High and three Medium issues. The
fixes are in the integration branch; the exact bounds are stated so no claim
exceeds the code.

| ID | Finding | Fix |
|---|---|---|
| R2-H1 | A stale Goal snapshot plus a ledger-only restore could bypass the durable revocation anchor | `prepare_charge` re-reads the authoritative Goal **under the ledger fence** and validates the anchor from that read. `revoke_active` writes the anchor under the same fence, so a stale snapshot cannot authorize. AT-GW-021d. |
| R2-H2 | Raw IPC fields remained at several console sinks (cwd, queue notices, expiry, yolo drain, decision echoes) | Every IPC-derived field at every console sink passes through `sanitize_console_field`. |
| R2-H3 | The grant display could hide a broad rule behind a 4 KiB join cap and showed the pending TTL instead of the grant lifetime | Every path rule is printed in full, one per line; the console shows the requested grant duration, the grant expiry if approved now, and the separate pending-request expiry. |
| R2-H4 | The anchor CAS returned a newer revision, removing the revision conflict that would have stopped a cancellation race | The `BeginOperation` CAS now requires Goal `RUNNING` and the exact live `SUBMITTED` Writer WorkItem with the expected grant binding, not just Task/Attempt identity. |
| R2-M1 | A replacement grant inherited the previous generation's usage floor | A new grant generation resets the floor (its own budget); same-generation retries keep it. AT-GW-019c. |
| R2-M2 | A crash after the charge but before BeginOperation could not resume at the budget ceiling | An operation id that is already charged may resume after all other checks; a different id may not. AT-GW-013b. |
| R2-M3 | No hard deadline on the publication helper's execution | **Accepted residual (availability).** The revoker's lock wait is bounded at 30 s and it never reports `REVOKED` on timeout; a stalled helper can hold the fence until the Owner terminates it. Adding a process deadline requires a publication-specific child-process timeout/kill path and is tracked as V1 hardening, not claimed here. |

Also: concurrent publishers now receive typed refusals
(`WRITER_PUBLICATION_CONFLICT`, `WRITER_PUBLICATION_NOT_AUTHORIZED`,
`WRITER_COMPLETION_NOT_JOURNALED`) instead of a generic persistence error. A
completion revision conflict keeps the pre-existing `RevisionConflict` error and
the APPLYING journal (AT-E5-042 semantics), so `WRITER_COMPLETION_CONFLICT` is
not emitted.

### 11.3 Round-3 security remediation

Round 3 reviewed `35ad119` and found one High, two Medium and two Low issues.

| ID | Finding | Fix |
|---|---|---|
| R3-H1 | The approval parser diagnostic and the unrecognized-answer echo printed IPC-controlled text raw | Both are sanitized; the parser failure is bounded and escaped like every other console field. |
| R3-M1 | The public `goal_next` authorization gate rejected an already-charged operation at the budget ceiling and blocked the prepared slot, so the charge→crash recovery never reached the operation-specific resume | `BudgetExhausted` is no longer treated as authorization loss in the pre-step; the driver decides, and the publication path resumes the exact charged operation. AT-GW-013d exercises this through `goal_next` with a real `write_scope`. |
| R3-M2 | A concurrent publisher that advanced the journal between the initial check and the Begin CAS surfaced `GOAL_STATE_CORRUPT` | A `CorruptGoal` from the Begin CAS is now a typed `WRITER_PUBLICATION_CONFLICT`; no write occurs and no corruption is claimed. |
| R3-L1 | Re-anchoring the same grant generation reset its usage floor | Only a strictly new generation resets the floor; AT-GW-013c. |
| R3-L2 | A maximum-length tree rule could render past the display cap and hide its `/**` suffix | The display bound reserves room for the rendered suffix; the path model bound is unchanged. |

`REVOCATION_DEFERRED`: a fence timeout now returns that explicit code instead of
`AUTHORIZATION_STATE_IO`, matching §6.4. It still never reports `REVOKED`.


### 11.4 Round-4 remediation

| ID | Finding | Fix |
|---|---|---|
| R4-M1 | A READY Writer refused at issuance stayed READY, so the Scheduler re-selected it forever and later READY read-only Tasks never ran | The driver now durably cancels the never-issued READY Writer with a mandatory blocker (`record_writer_authorization_block`); READY has no direct BLOCKED edge. AT-GW-013e. |
| R4-L1 | A maximum-length tree rule could lose its `/**` suffix at the display cap | The display bound reserves the rendered suffix; the console bound test checks a 4,095-byte path + `/**`. |
| R4-L2 | Any `CorruptGoal` from the Begin CAS was labelled a publication conflict | Only an already-advanced journal/operation is a typed `WRITER_PUBLICATION_CONFLICT`; genuine durable corruption propagates. |
| R4-L3 | The sanitizer's bidi coverage was narrower than Unicode `Bidi_Control` | One shared `is_console_unsafe` covers C0/C1, ALM/LRM/RLM, embeddings/overrides/isolates, U+206A–F, Zl/Zp, BOM and tag characters; used by both the path-rule validator and the console sanitizer. AT-GW-009d. |
| R4-L4 | A stale-generation issuance could overwrite the anchor's `last_grant_id` and disable a newer grant | `record_grant` only advances for `epoch >= highest_epoch`; AT-GW-021e. |
| I-2 | No named CODEX regression test | `at_gw_026_codex_goals_are_unaffected_by_authorization`. |
| I-3 | A raw NUL byte in a source literal made `authorization.rs` look binary to grep | Replaced with the `\0` escape. |
| I-5 | `REVOCATION_DEFERRED` was also emitted for a publication fence timeout | Split into `AUTHORIZATION_FENCE_BUSY` (publication) and `REVOCATION_DEFERRED` (revocation). |

`I-6` (no 30-second fence-timeout test) remains a test-coverage gap; the timeout
is exercised indirectly by the in-flight fence test, not by waiting 30 s.

### 11.5 Round-5 remediation

| ID | Finding | Fix |
|---|---|---|
| R5-M1 | Expiry/revocation stopped the whole Goal, so read-only planning, review, verification and finalization could not finish work that was already authorized | The `goal_next` pre-step no longer turns Expired/Revoked/StateMismatch/RollbackDetected/ScopeMismatch into a whole-Goal stop. Write authorization is enforced at the Writer boundaries (issuance, stage, publish); read-only host steps continue. AT-GW-025b proves an expired grant still issues a Planner WorkItem and writes nothing. |
| R5-L1 | The console filter omitted several `Cf` format characters (soft hyphen, ZWSP/ZWNJ/ZWJ, word joiner, interlinear marks) | The shared `is_console_unsafe` covers those ranges; AT-GW-009e. |
| R5-I1 | The design document itself contained a literal NUL in the I-3 note | Replaced with printable escape text. |

`R-1/C3`, `R2-M3` and `I-6` remain as recorded constraints/gaps, not code fixes.

### 11.6 Round-6 remediation

| ID | Finding | Fix |
|---|---|---|
| R6-M1 | A Writer WorkItem issued under a valid grant and then left PENDING when the grant expired occupied the slot forever; a submission could not be staged and read-only work was starved | The driver now detects a PENDING Writer whose grant is no longer usable, releases the slot through the existing prepared-only cleanup, and continues; independent read-only steps remain selectable. AT-GW-025c exercises this through the public `goal_next`. APPLYING/UNKNOWN intents are never cancelled. |
| R6-L1 | 30 supplementary-plane `Cf` characters (U+110BD, U+110CD, U+13430–3F, U+1BCA0–A3, U+1D173–7A) still passed validation and display sanitization | Added to the shared `is_console_unsafe`; AT-GW-009f. |

`R-1/C3`, `R2-M3` and `I-6` remain as recorded constraints/gaps.

### 11.7 Round-7 remediation

| ID | Finding | Fix |
|---|---|---|
| R7-M1 | Normal Owner revocation advances the Goal revision through the anchor, so the pending-Writer authorization cleanup was skipped and the generic branch left a RUNNING Task holding the mutation lease | The pending-Writer authorization check now runs **before** the revision branch; an unusable grant always goes through the prepared-only cleanup. AT-GW-025d uses the real `revoke_active` path and asserts the Task is Blocked with `WRITE_AUTHORIZATION_REVOKED`, not left RUNNING. |

`R-1/C3`, `R2-M3` and `I-6` remain as recorded constraints/gaps.

### 11.8 Round-8 remediation

| ID | Finding | Fix |
|---|---|---|
| R8-M1 | If the Owner revoked G1 and approved G2 before `goal_next`, the pending Writer's G1 binding was not compared with the resolved G2 grant, so the generic revision branch cancelled only the WorkItem and left its Task RUNNING with the mutation lease | The pending-Writer check now compares the WorkItem's grant binding with the resolved grant. A mismatch goes through the prepared-only cleanup with `WRITE_AUTHORIZATION_REISSUED`. AT-GW-025e asserts BLOCKED, no active WorkItem and no held lease. |

The final R8-M1 fix is covered by the named regression but was not included in
an independent review round; rounds 1–8 found no unresolved Critical/High code
defect after each round's remediation.

The reviewer's CODEX regression check was static only; the full local suite
(including all CODEX regression modules) was run after the remediation.

## 12. Campaign checkpoint

| Phase | Branch | Base | Head | Draft PR |
|---|---|---|---|---|
| A0 design | `feat/goal-write-grant-a0` | PR #47 `08f3edf` | `3320dcd` | #48 |
| A1 + A2 + A3 | `feat/goal-write-grant-a1` | `3320dcd` | `327b461` | #49 |
| D0–D4 + security remediation | `feat/goal-write-grant-d` | `327b461` | see PR #50 head | #50 |

* `main` remains `e6950fdc3bab72982285888a81cce822743fbba5`.
* PRs #42–#47 are untouched. No merge, un-draft, tag, or release was performed.
* PR #49 Required CI: **22/22 success** on `327b461` (push and pull_request runs).
* PR #50 Required CI: see the PR checks at its final head.
* Local Windows verification on the final head: `cargo fmt --all -- --check`,
  `cargo check --locked --all-targets`,
  `cargo clippy --locked --all-targets --all-features -- -D warnings`, and
  `cargo test --locked` (**1616 unit + 16 + 5 + 21 integration, 0 failed**,
  including **37 AT-GW tests**).

### 12.1 Acceptance mapping (executed, fixed-JSON fixtures)

This campaign's evidence is deterministic host fixtures. It is **not** real-model
dogfooding, and the two are not conflated.

| Requirement | Test evidence (all run in the full suite) |
|---|---|
| AT-E5-072 PRIMARY binding | `external_writer_tests::at_e5_028_to_034_publication_and_review_reach_host_goal_completion` (primary root) |
| AT-E5-073 managed clean creation | `external_writer_tests::at_e5_073_the_external_driver_prepares_a_clean_managed_goal` (Unix; Windows creation gate is covered by the existing approval-gated managed suite) |
| AT-E5-074 dirty primary refused | `external_writer_tests::at_e5_074_a_dirty_primary_refuses_external_managed_preparation` |
| AT-E5-075 forged worktree root | `managed_worktree_creation_tests` (host-derived root; no model/client field) + `ManagedRootSource::Host` |
| AT-E5-076 stale worktree identity | `managed_worktree_*` execution-root/reconciliation suites + `authorization_tests::at_gw_008` |
| AT-E5-077 writer in managed workspace | `external_writer_tests::at_e5_072_082_external_managed_writer_reaches_host_evidence_without_touching_primary` |
| AT-E5-078 snapshot evidence | same test (`managed_workspace_verification_evidence`, `goal_result` re-observation) |
| AT-E5-079 finalization blocked | `managed_workspace_verification_evidence_tests` + `goal_finalizer_tests` (missing/mismatched evidence never COMPLETED) |
| AT-E5-080 concurrent primary advance | `managed_worktree_creation_tests` base-advanced/reconciliation cases |
| AT-E5-081 crash during evidence persistence | `managed_workspace_verification_evidence_tests` + `managed_worktree_*` fault cases |
| AT-E5-082 no auto-merge/push | `at_e5_072_082` asserts primary HEAD/status unchanged, managed HEAD at base, no ref change |
| Scenario A (readonly DAG) | `external_dag_tests::at_e5_001..007` (linear, diamond, multi-ready, blocked, failed, superseded) |
| Scenario B (Writer→Reviewer→Verifier) | `external_writer_tests::at_e5_028_to_034...` and the managed E2E |
| Scenario C (Replanner→DAG→Verifier) | `external_replanner_tests::at_e5_057`, `at_e5_066_070...` |
| Scenario D (crash/resume/reconcile) | `external_goal_recovery_tests`, `external_h3_reliability_tests`, `external_writer_failure_tests`, and AT-GW-017/018/019/021/023/024 |

### 12.2 Not verified in this campaign

* Real external-model dogfooding (E5-E): **NOT RUN**. All scenarios above use
  fixed JSON fixtures; no claim of real-model behaviour is made.
* Native Windows junction/ADS race execution against the grant gate: the
  existing session-authority tests cover junctions/ADS; the new
  grant-rule-escape test is Unix-only.
* A hard publication-helper execution deadline (R2-M3): accepted availability
  residual.
* Same-UID process impersonation resistance (R-1/C3): **NOT VERIFIED** by
  design.

## 13. Open Owner decisions## 13. Open Owner decisions

1. **R-1 acceptance.** Is the deployment assumption "the agent cannot use the
   OCS console or the state directory outside GoalLatch" acceptable for V1 Beta,
   or must C3 (separate principal / user-presence attestation) be implemented
   before any real EXTERNAL write is enabled?
2. Default grant caps (currently 64 operations / 16 MiB / 7 days). Lower?
3. Should `/grant approve` require the full scope digest to be retyped in
   addition to the nonce (stronger against a shoulder-surfed console, more
   friction)?

Nothing in this document merges, un-drafts, tags, or releases anything.
