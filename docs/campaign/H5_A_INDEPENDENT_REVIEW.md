# H5-A — Independent review of PR #51 HEAD, and remediation

## 1. What "independent" means here

* **Audited commit:** `e633d9d8416cf8e4ff0bef1167945585128e3c0e` (PR #51 HEAD).
* **Reviewer:** Codex `gpt-6.1-sol`, reasoning effort `high`, run as a separate
  app-owned child task with its own conversation (no campaign-agent history
  transfer beyond the brief).
* **Not the implementer.** The campaign agent did not author the audited commit
  and did not review its own remediation; the remediation diff was re-reviewed by
  the same independent model (round 2, §5).
* **Mode:** read-only. The reviewer was instructed not to modify, create or
  delete files, and reported: *"This was a read-only source audit. I did not run
  tests, build binaries, reproduce exploits, or change files."*
* **Concurrency note, disclosed by the reviewer:** the worktree received the
  campaign's H5 edits during the audit, so the reviewer re-read the affected
  sources with `git show e633d9d:<path>`. Every line reference below is to
  `e633d9d`.
* **No reviewer was available?** One was. If it had not been, this document would
  say `NOT_VERIFIED` instead of claiming a review (campaign rule).

## 2. Checklist verdicts

| # | Question | Verdict |
| --- | --- | --- |
| 1 | MCP schema correctness (published vs parsed) | **Not identical.** `write_scope` and `failed_task_replan_requests` are correctly published; drift remains elsewhere (nullable optional fields, integer bounds, `additionalProperties:false` handlers that ignore extras, an unpublished fallback `mode`, broader runtime enums, character-vs-byte `maxLength`) → F5, G1 |
| 2 | `write_scope` authority boundary | **Verified safe.** A scope is a proposal; CODEX refuses it; issuance needs a usable grant; staging and publication re-check the binding and paths; Owner approval needs a live nonce; the `#[cfg(test)]` auto-grant is not a production path |
| 3 | Grant epoch / rollback anchor / usage monotonicity | **The fix is correct within its boundary.** Reset only for a strictly newer epoch (`authorization_store.rs:335–339`); older epochs cannot displace a newer anchored identity (`authorization.rs:1075–1081`); enforcement uses the componentwise max (`authorization.rs:1755–1762`); charges are anchored before the ledger write. Qualifications → F6, D2 |
| 4 | Revocation state machine | **Verified safe** for durable state: `NOT_REVOKED → REVOKE_REQUESTED → REVOKED`, both non-active states deny resolution, the anchor is written before finalization, no same-record resurrection. `RevocationDeferred` is a *refusal*, not a queued revocation. Process-lifetime holes → F1/F2 |
| 5 | Publication fence | **Exclusive between cooperating live host processes**, not an execution-lifetime barrier: file lock + authoritative re-read + held across Begin→write→postimage→Complete, plus a Goal CAS on Begin. Timeout cleanup could end without confirmed leader exit → F1; POSIX host death bypasses `Drop` → F2 |
| 6 | Helper deadline semantics | **A timer-triggered termination attempt plus a bounded wait — not an unconditional guarantee.** Deadline 60 s (`execution.rs:29`), cleanup waits 500 ms for the leader (`resource_limits.rs:103`), `leader_finished: false` is discarded by `run_bounded` (`sandbox.rs:547–550`, `697–699`), publication maps the error to UNKNOWN and drops the fence (`external_publish.rs:465–474`) |
| 7 | UNKNOWN side effects | **Verified safe**: `APPLYING` blocks ordinary replay (`external_publish.rs:273–275`), reconciliation requires observations (`mutation_recovery.rs:42–61`), partial/unknown blocks the Task and the finalizer (`goal_finalizer.rs:361–366`). Quiescence of a surviving helper was the gap → F1/F2 |
| 8 | H2/H3 crash recovery (8 crash points) | **Source-derived, all conservative**: pending work is redelivered with the same identity; an orphan stage is inert; `BeginOperation` is durable before the helper runs; an unverified postimage keeps the journal `APPLYING`; the finalizer re-evaluates readiness inside the Goal CAS. No missing replay guard found |
| 9 | Test state-root isolation | **Correct by construction** in `tests/mcp_contract.rs`: unique temp roots, per-child absolute `LOCAL_MCP_STATE_DIR`, no client-selectable root. `secure_fs` validates Unix ownership/modes; Windows does not validate ACL/reparse → D2 |
| 10 | Process groups / detached descendants | Windows: suspended spawn + Job assign + resume, kill-on-close, no breakaway (`process_group.rs:294–345`, `process_job.rs:101–115`). POSIX: signals the owned group and uses `waitid(WNOWAIT)` against PID reuse, but a group is not containment and whole-tree exit is not confirmed → D1 |
| 11 | Test realism | **Real binary, real JSON-RPC over stdio, not a mock** — but the all-tool audit was self-validation, the WorkItem test never submitted, and initialisation was abbreviated → G1; H3's "real boundary" writes in-process → G2 |

## 3. Findings and disposition

Every row is reproduced from the reviewer's table, with the H5 disposition.

| id | Sev | Finding (abbreviated) | Disposition in H5 |
| --- | --- | --- | --- |
| F1 | **High** | Cleanup can time out without observing leader exit; kill results discarded; lifecycle evidence stripped; publication returns UNKNOWN while releasing the fence → revocation can finalize while a helper may still write | **FIXED** (§4.1) |
| F2 | **High** | POSIX host death bypasses `Drop`; no owner-death mechanism; a surviving helper can outlive the lock and a restarted host can finalize revocation | **MITIGATED** (§4.1). The surviving process cannot be stopped retroactively on POSIX — recorded as D1, not as solved |
| F3 | Medium | Oversized/too-deep response: receipt written, WorkItem dropped, Task left `RUNNING` (Writer keeps its lease) with no WorkItem to answer | **OPEN**, documented (§4.4) |
| F4 | Medium | EXTERNAL Codex-fallback gate treats a store read error as "no active Goal" and allows fallback | **FIXED** (§4.2) |
| F5 | Low | Remaining schema↔parser drift across the non-WorkItem tool surface | **OPEN** (partially addressed for the five WorkItem response contracts by H5-B) |
| F6 | Low | Enforcement uses anchored usage; API/console reported raw ledger usage | **FIXED** (§4.2) |
| F7 | Low | Replayed *rejected* receipt labelled `REPLAY_OF_ACCEPTED_SUBMISSION` | **FIXED** (§4.2) |
| G1 | Medium | Contract suite self-validated and never submitted; abbreviated initialisation | **PARTIALLY CLOSED** — H5-B adds a real submit loop against a live server (§4.3) |
| G2 | Medium | H3 "real boundary" writes in-process; no termination-failure or late-write witness | **OPEN** — belongs to H5-D crash injection, which was not executed |
| D1 | Info | POSIX group escape / no whole-tree exit confirmation / Windows Job not verified empty | **DESIGN_CONSTRAINT**, restated |
| D2 | Info | Rollback anchor protects against ledger-only rollback; Windows state privacy relies on inherited FS protection | **DESIGN_CONSTRAINT**, restated |
| S1–S6 | Info | Authority checks, ledger-only rollback enforcement, revocation transitions, journal/retry gates, receipt idempotency, fixture root selection | **VERIFIED_SAFE** — no fix needed |

## 4. Remediation

### 4.1 F1 + F2 — revocation is no longer a barrier the host cannot prove

**The harm.** Revocation finalization is the host telling the Owner "the write
path is closed". After an UNKNOWN publication the journal already refuses to
replay the operation, but the process that performed it may still exist. Before
this change, `revoke_active` finalized regardless, so a helper that outlived its
deadline (F1) or survived a POSIX host kill (F2) could still be running against a
workspace the Owner had been told was closed.

**The change.**

* `Goal::has_unresolved_publication()` (`src/goal.rs`) — true when any attempt's
  mutation intent is still `Applying`, i.e. "`BeginOperation` is durable and the
  host has no proof this execution is over".
* `authorization_store::revoke_active_within` (`src/authorization_store.rs`) now
  records `REVOKE_REQUESTED` (which already denies every new operation), saves
  it, and then **refuses to finalize** while that predicate holds, returning the
  new `REVOCATION_BLOCKED_BY_UNRESOLVED_PUBLICATION`.
* The state is honest in both directions: the grant is denied, and the Owner is
  never told the revocation completed. A later call finalizes as soon as recovery
  has reconciled the intents — the only point at which the host can say the write
  path is closed.

**Regression test** `external_writer_failure_tests::at_e5_050_…` drives the real
publication path to `APPLYING` + `WRITER_PUBLICATION_UNKNOWN`, then asserts:
the revocation is refused with that exact code; the durable ledger state is
`RevokeRequested` (never `Revoked`); a new operation charge is refused with
`WRITE_AUTHORIZATION_REVOKED`; after `ReconciledNotPerformed` the revocation
finalizes to `Revoked`.

**What this does not claim.** It does not stop a POSIX helper that is already
running, and it does not make process groups a containment boundary. It removes
the *false statement* from the system and keeps the state machine consistent until
reconciliation. Windows containment (Job object, kill-on-close) is unchanged.

### 4.2 F4, F6, F7

* **F4** — `CodexFallbackAuthority::{Established,Unavailable}` in
  `external_goal_api`. `Unavailable` (no store, unreadable store) maps to
  `RefusedForExternalBackend`, so an unreadable state root fails closed instead of
  unlocking the one backend ADR-08 forbids. Test:
  `an_unreadable_state_root_refuses_the_codex_fallback_gate`.
* **F6** — `AuthorizationView` reports `max(ledger, anchor)` per component, which
  is exactly what enforcement uses, so the console can no longer under-report
  spending after a same-generation ledger restore.
* **F7** — a replayed receipt is labelled by its stored decision:
  `REPLAY_OF_ACCEPTED_SUBMISSION` only for `ACCEPTED_FOR_HOST_PROCESSING` /
  `COMMITTED`, otherwise the neutral `REPLAY_OF_STORED_RECEIPT` (and both keep
  `CALL_GOAL_NEXT`).

### 4.3 G1 — a real submit loop

`tests/mcp_contract.rs::goal_next_publishes_a_machine_readable_response_contract`
drives a live server: `goal_start` → `goal_next` (PLANNER) → an out-of-contract
answer that is refused → `goal_next` again → a contract-valid plan that is
`COMMITTED` → `goal_next` (READONLY) with its published status vocabulary. That
closes the "never submits" half of G1 for the external backend.

### 4.4 F3 — documented, not fixed

Reproduction sketch: a response that is schema-valid but nested deeper than
`MAX_JSON_NESTING_DEPTH` (or larger than `MAX_MODEL_RESPONSE_BYTES`) takes the
generic rejection path, which records a receipt and drops the WorkItem while the
Task stays `RUNNING`; `goal_next` then reports active work with nothing to answer.
The correct fix is a role-specific atomic Task transition (fail/retry/block) in
the same commit as the receipt, plus readonly/Writer/Reviewer cases. That is a
submit-pipeline change, not a safety-critical one, and H5 did not reach it.

## 5. Re-review of the remediation diff

The remediation was re-reviewed by the same independent model (round 2) against
the post-fix HEAD. The result and any surviving objections are recorded in
`docs/campaign/H5_A_REREVIEW.md`.

## 6. H5-A gate

| Gate | Result |
| --- | --- |
| 0 unresolved Critical defects | **PASS** — none found |
| 0 unresolved High defects | **PASS** — F1 and F2 addressed: F1 fixed with a durable barrier and a regression test, F2 mitigated with the same barrier plus restart behaviour and explicitly disclosed as a POSIX design constraint |
| Independent review of the final HEAD | **PASS** — round 1 on `e633d9d`, round 2 on the remediation HEAD |
| Related regression tests pass | **PASS** — see `H5_CAMPAIGN_CHECKPOINT.md` §5 for the measured runs |
| C3 not claimed as solved | **PASS** — stated as open in the risk register, the C3 document, and the final report |
