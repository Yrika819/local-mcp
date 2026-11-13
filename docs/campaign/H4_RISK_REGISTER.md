# H4-0 — Baseline & Risk Register

Scope: `local-mcp` External Agent Backend V1 + Authorization B, as of PR #50
head `072fa1564072b86364330cd46560141118d210d8`.
Classification: **CONFIRMED_DEFECT**, **COVERAGE_GAP**, **DESIGN_CONSTRAINT**,
**VERIFIED_SAFE**, **NOT_VERIFIED**.

Every row below was re-derived from the source at that commit. A row marked
VERIFIED_SAFE names the code that makes the guarantee *and* the test that
exercises it; a row marked CONFIRMED_DEFECT names the line and the reproduction.

---

## 1. CONFIRMED_DEFECT

| id | Defect | Evidence | Acceptance condition |
| --- | --- | --- | --- |
| H4-A-1 | `goal_start` published an `inputSchema` without `write_scope`, while `GoalStartRequest` parses it. With `additionalProperties:false`, no schema-validating MCP client could submit a write scope, so the whole EXTERNAL Writer path was unreachable through the documented contract. | `src/mcp.rs` `tools()` `goal_start` schema (no `write_scope`); parser at `src/goal_api.rs:257-260`; scope parsing at `src/goal_api.rs:548-562`. | Published schema contains `write_scope`; the shape mirrors `RequestedWriteScopeV1` (`allowed_paths`/`allowed_operations`/`max_operations`/`max_bytes`/`expires_in_seconds`, all five required, `additionalProperties:false`); a real `tools/call` built from that schema is accepted; an out-of-schema payload is refused by both. Asserted in `tests/mcp_contract.rs`. |
| H4-C-1 (R2-M3) | The publication helper had **no hard execution deadline**. `publish_workspace_write` called `sandbox::run`, and `terminate_and_capture_bounded` waited for the leader with no deadline at all, so a helper that never exits held the publication fence, the Goal CAS and the MCP request open indefinitely. | `src/execution.rs` `publish_workspace_write`; `src/sandbox.rs` `terminate_and_capture_bounded` ("There is no command deadline"). | A deadline exists for the publication helper only; expiry terminates the process group **and** confirms the leader's exit; the caller maps it to `WRITER_PUBLICATION_UNKNOWN` (never to "not performed"); every other caller keeps the previous unbounded wait. Asserted in `src/sandbox.rs` `deadline_tests`. |
| H4-C-2 (I-6) | The 30 s publication/revocation fence bound had **no direct test**. Only the constant existed. | `src/authorization.rs:66`; `src/authorization_store.rs` `acquire_ledger_lock`. | The constant is asserted directly *and* the same code path is exercised with an injected wait, including that a deferred revocation is reported as `REVOCATION_DEFERRED` and never as `REVOKED`. Asserted by `at_gw_031`/`at_gw_032`/`at_gw_033`. |
| H4-D-1 | The durable state root could not be redirected, so no real `local-mcp mcp` server process could be run against a throwaway Goal store / authorization ledger. Every test shared the operator's real state root and was isolated by Session ID only. | `src/config.rs` `state_dir()` (no override); callers `TaskStore::new`, `authorization_store::ledger_path`, `config::session_path`. | A host-owned, absolute-path-only override exists; a relative path is refused; nothing about it is reachable from MCP input. Asserted in `src/config.rs` tests and used by `tests/mcp_contract.rs`. |

## 2. COVERAGE_GAP

| id | Gap | Evidence | Disposition in H4 |
| --- | --- | --- | --- |
| H4-A-2 | No test asserted the *published* schema against the *runtime parser* — only in-process JSON shapes. A field could be added to one and not the other (which is exactly H4-A-1). | `src/phase0_mcp_tests.rs` asserts JSON shapes; nothing spawns the server. | Closed by `tests/mcp_contract.rs` (real process, real `tools/list`, validator, normal + invalid `tools/call`). |
| H4-C-3 | No test covered the eight publication failure shapes as *distinct* states (pre-spawn failure, crash right after spawn, mid-first-file, after partial publication, unresponsive helper, missing receipt after exit, owner cancel while waiting for the fence, OS-level orphan child). | `src/external_writer_failure_tests.rs` covers preimage refusal, journal states and postimage mismatch; there is no "helper does not exit" case. | Partially closed: the unresponsive-helper case is now a real test (`deadline_tests`). The OS-orphan case is asserted only *by construction* (Job Object / process group teardown on drop); see the residual note in §5. |
| H4-D-2 | No test proved that two state roots cannot see each other's Goals/ledgers, because there was only ever one root. | — | Closed by `isolated_state_roots_do_not_share_sessions_goals_or_ledgers`. |
| H4-D-3 | There is no packaged, runnable real-model dogfooding harness. The existing E5 Scenario A–D coverage is fixture-driven (`src/external_flow_tests.rs`, `src/external_dag_tests.rs`). | `docs/EXTERNAL_AGENT_BACKEND_V1_E5_DOGFOODING.md` describes fixtures as the evidence source. | **Not closed by code.** Recorded as `NOT_VERIFIED` unless a real model answers a real ModelWorkItem; see the H4-D section of the final report. |

## 3. DESIGN_CONSTRAINT (architectural, not a bug)

| id | Constraint |
| --- | --- |
| DC-1 | **C3 / R-1 — same-OS-user Owner impersonation.** The Owner Control Surface, the MCP server and an arbitrary agent process run as the same Windows user. Everything the OCS relies on (console input, the pending-request nonce digest, the ledger files) is reachable by anything running as that user. No amount of extra MCP-argument validation fixes this; it needs an OS trust boundary. The product states this honestly in the authorization projection (`owner_attestation`, `same_uid_process_resistance`), and H4 must not present it as solved. Design in `docs/security/C3_0_WINDOWS_TRUST_BOUNDARY.md`. |
| DC-2 | Windows has no application sandbox in this program. `sandbox::run` on Windows is argv execution plus a restricted environment, not a filesystem boundary. Every Windows publication is therefore bounded by *path authority and the grant*, not by an OS sandbox. |
| DC-3 | The external backend's model work is performed by the **client**. `goal_next` hands out a `ModelWorkItem` and refuses to proceed until `goal_submit`. This is the opposite of a self-executing backend, and it is the single largest constraint on the Serea integration (§S0-0). |
| DC-4 | `goal_cancel` cancels durable Goal authority; it does not stop an in-flight legacy job, and it does not undo already-published mutations. That is deliberate (`goal_cancel` description) and matches "cancellation is not an inverse". |
| DC-5 | A cancelled/blocked Writer Task is not revived by a later Owner grant (`record_writer_authorization_block`); recovering needs a new plan. Deliberate fail-closed choice. |

## 4. VERIFIED_SAFE (with evidence)

| Invariant | Code | Test |
| --- | --- | --- |
| EXTERNAL never spawns Codex | `external_goal_api::goal_run_backend_decision`, `external_goal_run_outcome` returns `codex_invoked:false` and `EXTERNAL_MODEL_REQUIRED` | `src/external_goal_api_tests.rs:794` |
| Model output is data, never authority | `external_publish` validates a submission against the same role validators as Codex and only records evidence; reviewer cannot publish | `src/external_writer_tests.rs`, `src/external_flow_tests.rs` |
| Host decides session/workspace | `config::validate_path_authority`, `ManagedRootSource::resolve`, `GrantWorkspaceIdentity::from_goal` | `src/phase0_config_tests.rs`, `src/authorization_tests.rs` |
| No publication without a grant | `publish_prepared_writer_intent` → `intent.authorization()` else `GrantRefusal::Required`; `acquire_publication_fence` → `usable_grant_for_operation` | `src/authorization_tests.rs` (AT-GW-*), `src/external_writer_tests.rs` |
| Revocation cannot be bypassed mid-operation | fence held across Begin → write → postimage → Complete; `ensure_writer_publishable` re-checked *inside* the Begin CAS | `src/external_writer_race_tests.rs`, `src/authorization_tests.rs` |
| UNKNOWN is never retried blindly | `WRITER_PUBLICATION_UNKNOWN`, `WRITER_COMPLETION_NOT_JOURNALED`, `WRITER_POSTIMAGE_UNKNOWN` all leave the journal APPLYING for recovery | `src/external_writer_failure_tests.rs`, `src/mutation_recovery.rs` |
| No duplicate writer publication | `published_intent_awaiting_completion` completes bookkeeping without a second write; `already_advanced` → `WRITER_PUBLICATION_CONFLICT` | `src/external_h3_reliability_tests.rs`, `src/external_writer_race_tests.rs` |
| Reviewer cannot reach COMPLETED | reviewer only counts findings; completion needs Task Verifier + Goal Verifier + Finalizer | `src/goal_finalizer_tests.rs`, `src/verifier_tests.rs` |
| Durable CAS / receipts / journal idempotency | `mutate_goal_snapshot` revision CAS; `(work_id, submission_id, payload)` receipt identity | `src/external_goal_api_tests.rs`, `src/task_store.rs` tests |
| CODEX path unchanged | `goal_start` defaults to CODEX; `write_scope` on CODEX is refused; `model_backend` is immutable after creation | `src/external_goal_tests.rs:124-215` |

## 5. Residual notes H4 does not close

1. **Orphan descendants on deadline expiry.** The deadline path asks the group to
   stop and confirms the *leader* exited. On Windows the Job Object kills the
   whole tree when the group is dropped; on Unix the process group is signalled.
   What is *not* proven by test is that a grandchild which detached from the
   group (`setsid`, `CREATE_NEW_PROCESS_GROUP` + detach) is gone. No publication
   helper spawns children today, so the risk is bounded by the helper being a
   single-purpose binary — but it is a construction argument, not a test.
2. **`tokio::time::timeout` alone is not a stop.** This is why the implementation
   races the *capture loop* rather than wrapping the whole future: dropping the
   future would drop `ProcessGroup` (which does terminate), but it would give no
   exit confirmation and no bounded drain, so the caller could not distinguish
   "stopped" from "still running".
3. **Windows dogfooding of the Writer path stays `NOT_VERIFIED`** until a real
   model and an isolated state root are used together (§H4-D).
