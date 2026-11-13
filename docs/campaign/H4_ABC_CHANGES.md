# H4-A / H4-B / H4-C — what changed and why

Base: PR #50 head `072fa1564072b86364330cd46560141118d210d8`
(`feat/goal-write-grant-d`).

Every claim below is backed by a test that fails without the change or by a
negative control that was actually run.

---

## H4-A — MCP contract correctness

### Defect

`goal_start` accepted `write_scope` (`src/goal_api.rs:257-260`) but the published
`tools/list` `inputSchema` did not declare it, and `goal_start`'s schema has
`additionalProperties: false`. A standard MCP client that validates arguments
against the published schema therefore *could not* request an EXTERNAL write
scope at all: the only path to a Writer grant was unreachable through the
documented contract.

### Change

`src/mcp.rs`: `tools()` now injects a `write_scope` schema into `goal_start`
(mirroring the existing `goal_resume` post-processing), built from the same
constants the runtime validator uses:

| Field | Schema | Runtime |
| --- | --- | --- |
| `allowed_paths` | array, 1..=`MAX_GRANT_PATH_RULES`, each ≤ `MAX_GRANT_PATH_BYTES` | `RequestedWriteScopeV1::validate` |
| `allowed_operations` | array, exactly 1, enum `WRITE_UTF8` | same |
| `max_operations` | integer 1..=`MAX_GRANT_OPERATIONS` | `WriteBudgetV1::new` |
| `max_bytes` | integer 1..=`MAX_GRANT_BYTES` | `WriteBudgetV1::new` |
| `expires_in_seconds` | integer `MIN_GRANT_TTL_SECONDS`..=`MAX_GRANT_TTL_SECONDS` | same |
| additional | `additionalProperties: false` | `#[serde(deny_unknown_fields)]` |

Scope is still **not** authority: a scope at `goal_start` leaves the Goal in
`authorization.state = REQUIRED` with no `grant_id`, and it is refused outright
for the CODEX backend.

### Test (real, out-of-process)

`tests/mcp_contract.rs` spawns the built `local-mcp` binary as an independent
server, uses the same newline-delimited JSON-RPC transport a real client uses,
takes `tools/list` from that process, validates the schema with a JSON Schema
validator (the supported keyword subset is explicit, and any *unknown* keyword
fails the test rather than being ignored), and then drives `tools/call`:

* a schema-conformant EXTERNAL `goal_start` with `write_scope` succeeds;
* the same payload without `max_bytes` is refused by both schema and runtime;
* an undeclared `write_scope` field is refused by both;
* `write_scope` on CODEX is refused by the runtime;
* only the accepted Goal exists on disk afterwards.

Plus two contract audits: every published tool's schema is checked for
unsupported keywords and for *satisfiability* (a generated minimal instance
validates), and `goal_next`/`goal_submit` agree on the WorkItem fields.

### Isolation prerequisite (also H4-D)

`src/config.rs`: added `LOCAL_MCP_STATE_DIR`, a host-owned, absolute-path-only
override for the durable state root (the same trust level as the existing
`LOCAL_MCP_MANAGED_WORKTREE_ROOT`). It is read from the process environment
only — never from MCP input — and a relative path is refused. Without it no real
server process could run against a throwaway Goal store and authorization
ledger, so "real MCP contract test" and "real dogfooding" were both impossible.

Evidence: `tests/mcp_contract.rs::isolated_state_roots_do_not_share_sessions_goals_or_ledgers`
runs two servers on two roots and shows the second cannot resolve the first's
session and never sees its Goals.

## H4-B — independent authorization review

An independent model (Codex, `gpt-6.1-sol`, xhigh) reviewed the fixed commit
`072fa15` read-only. It found **no Critical and no High** defect and reported one
MEDIUM and one LOW finding. Both were re-checked against the source (not taken on
trust) and both are fixed here.

### MEDIUM — a stale grant generation could clear a newer generation's usage floor

`record_anchor_grant` reset the anchor's monotonic usage floor whenever the grant
id differed, and only afterwards let `record_grant` refuse a stale epoch. A
rolled-back ledger whose next epoch was *lower* therefore produced
"newer grant identity, zero usage floor": spent budget came back, which defeats
the exact defense the anchor exists for.

Fix: the floor is reset only for a **strictly newer** epoch than the anchor's
`highest_epoch`, so the reset and the identity update now agree. Same-grant
re-anchoring still preserves the floor; a conflicting id at an equal epoch is not
provably newer and also preserves it.

Test: `at_gw_034_a_stale_generation_cannot_clear_a_newer_usage_floor`.
**Negative control was run**: with the old `new_generation` expression the test
fails (`usage_operations` 0 vs expected 1); with the fix it passes.

### LOW — `REVOKE_REQUESTED` was projected as `REVOKED`

Both states deny publication, so this is not an authorization bypass, but the API
claimed a finalization write that had not happened. `AuthorizationView::from_ledger`
now derives the reported state from the durable grant record while leaving the
permission decision untouched.

Tests: `at_gw_035_...projected_as_requested_not_revoked` (also asserts that a
requested revocation already denies new operations) and
`at_gw_036_...projected_as_revoked` (the finalized state is still `REVOKED`).

## H4-C — publication and revocation reliability

### R2-M3 — the publication helper had no hard deadline

`publish_workspace_write` → `sandbox::run` → `terminate_and_capture_bounded`
waited for the leader with no deadline, so an unresponsive helper held the
publication fence (a 30 s-bound lock the helper itself was inside), the Goal CAS,
and the MCP request open indefinitely.

Change: the *capture loop* now races an optional deadline (not the whole future,
because dropping the future would tear down without confirming anything). On
expiry the process group is terminated, the leader's exit is confirmed with a
bounded cleanup wait, and a typed `CaptureFailure::DeadlineExceeded` is returned.
Only `publish_workspace_write` passes a deadline (`PUBLICATION_HELPER_TIMEOUT`,
60 s); every other caller keeps the exact previous unbounded behaviour.

Semantics: the caller maps the error to `WRITER_PUBLICATION_UNKNOWN` and the
journal stays `APPLYING`. A timeout is never evidence that nothing was written.

Test: `sandbox::deadline_tests::deadline_stops_the_group_and_confirms_the_leader_exited`
(spawns a real 30 s child, asserts the deadline fires, the leader's exit is
confirmed, and the total wait is far below the child's lifetime) and
`…::without_a_deadline_a_short_command_completes`.

### I-6 — the 30 s fence timeout had no direct test

Change: the fence wait is now injectable
(`acquire_ledger_lock_within`, `revoke_active_within`), so the timeout *branch*
is exercised in milliseconds while the production constant stays the only
production value.

Tests:

* `at_gw_031` — the shipped constant is exactly 30 s;
* `at_gw_032` — a held fence produces `FenceTimeout` / `AUTHORIZATION_FENCE_BUSY`,
  the ledger bytes are unchanged, and the fence is acquirable again after release;
* `at_gw_033` — a revocation that cannot take the fence reports
  `REVOCATION_DEFERRED` (never `WRITE_AUTHORIZATION_REVOKED`), the durable grant
  is still `NotRevoked`, the anchor records no revocation, and the retry after
  release finalizes for real.

### Residual (documented, not closed)

A deadline proves the *leader* exited. A descendant that detached from the job /
process group is not proven gone by test; no publication helper spawns children
today, so this is a construction argument, not a measurement.
