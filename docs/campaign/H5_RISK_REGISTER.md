# H5-0 — Baseline and risk reassessment

Campaign: **GoalLatch H5 — External Model Execution Closure / Security
Verification / Integration Readiness**
Worktree: `.local/h5-a` (isolated, created from PR #51 HEAD)
Branch: `feat/h5-a-security-remediation-v1`

Everything in this file was re-derived at the start of H5 from GitHub, from the
working tree, and from the source at the audited commit. Nothing is carried over
from an H4 claim without re-checking it.

---

## 0. Baseline captured at start (observed, not assumed)

| Item | Observed value | How |
| --- | --- | --- |
| `origin/main` | `e6950fdc3bab72982285888a81cce822743fbba5` | `git rev-parse origin/main` after `git fetch --prune` |
| PR #51 HEAD | `e633d9d8416cf8e4ff0bef1167945585128e3c0e` | `git rev-parse origin/feat/h4-a-mcp-contract-v1`; `gh pr view 51` |
| PR #51 base | `feat/goal-write-grant-d` (PR #50 head `072fa156…`) | `gh pr view 51` |
| PR #51 state | OPEN / Draft / `MERGEABLE` | `gh pr view 51` |
| PR #51 CI | 22/22 `SUCCESS` (2 runs × quality, test ×4, compat ×7) | `gh pr checks 51` |
| PR #51 GitHub formal reviews | **0** | `gh pr view 51 --json reviews` → `[]` |
| Open drafts in the stack | #31–#51 (21 PRs), #26, #29, #30 | `gh pr list --state all` |
| Worktree used for H5 | `.local/h5-a` at `e633d9d`, clean at creation | `git worktree add .local/h5-a -b feat/h5-a-security-remediation-v1 e633d9d` |
| Free space on C: | 46 GB (152 GB used of 198 GB) | `df -h /c` |
| Toolchain | cargo/rustc 1.96.0 (`1.96.0-x86_64-pc-windows-msvc`) | `cargo --version` |
| Cargo target dir for H5 | `.local/h5-a-target` (separate, so no existing worktree build cache is touched) | — |
| Independent reviewer available | Codex `gpt-6.1-sol` (different model from the campaign agent) | `orchestrator_capabilities` |

Protected assets were confirmed present and **not modified**: the primary
checkout `local-mcp-connector-parity`, `.local/gl-h4-a` (PR #51 worktree),
`.local/gl-auth-a0` (PR #50), `.local/gl-e5-hardening`,
`.local/gl-e5-h3-reliability`, `.local/gl-external-e{3,4-h1,5}`,
`.local/gl-campaign-20261106`, and `tmp/longrun-v2-resume-20261016`.

H5 used exactly **one** new worktree and **one** new target directory. No
existing worktree, state root, or build cache was deleted or rebuilt in place.

---

## 1. Classification key

* **CONFIRMED_DEFECT** — a failure path established from source and (where it
  matters) reproduced by a test.
* **COVERAGE_GAP** — the code may be right, but nothing proves it.
* **DESIGN_CONSTRAINT** — architectural; not fixable inside this program without
  an OS-level trust boundary.
* **VERIFIED_SAFE** — the guarantee exists and names its enforcing code and test.
* **NOT_VERIFIED** — not executed, not observed; reported as such.

---

## 2. Risk register

### 2.1 Carried over from H4, re-evaluated at `e633d9d`

| id | Item | Class | Status after H5-0 | Evidence |
| --- | --- | --- | --- | --- |
| H5-0-R1 | PR #51 had no independent review | COVERAGE_GAP | **Closed by H5-A.** Read-only audit by Codex `gpt-6.1-sol` of `e633d9d`; 2 High, 3 Medium, 2 Medium coverage gaps, 3 Low, 2 Info/design, 7 VERIFIED_SAFE | `docs/campaign/H5_A_INDEPENDENT_REVIEW.md` |
| H5-0-R2 | H4-D-F1: `response_required_fields` published field *names* only, no enum or bound vocabulary | CONFIRMED_DEFECT | **Closed by H5-B.** Per-role JSON Schema + digest + conditions + decision vocabulary, published on every `goal_next`, single-sourced with the validators | `docs/campaign/H5_B_RESPONSE_CONTRACT.md`, `src/external_response_contract.rs`, `tests/mcp_contract.rs` |
| H5-0-R3 | Real-model Scenario B/C/D not executed | NOT_VERIFIED | **Still open.** H5-C provides the isolated grant machinery; H5-D Scenario B/C/D were not executed in this campaign (see §2.4) | `docs/campaign/H5_D_REAL_MODEL_EVIDENCE.md` |
| H5-0-R4 | Production Owner approval vs test grant not separated | DESIGN_CONSTRAINT → CONFIRMED_DEFECT (missing mechanism) | **Mechanism added by H5-C.** `TEST_FIXTURE_V1` attestation, disposable-root marker, feature-gated issuance, `resolve_grant` refusal outside the fixture root | `docs/campaign/H5_C_TEST_GRANT_HARNESS.md`, `src/test_grant_fixture.rs`, `src/test_grant_fixture_tests.rs` |
| H5-0-R5 | Crash during publication and UNKNOWN outcome | VERIFIED_SAFE (journal) / High defect in the *lifetime* half | **High defect found and fixed by H5-A F1.** Revocation no longer finalizes over an unresolved publication; the durable state is `REVOKE_REQUESTED` and a new code reports the refusal | `src/authorization_store.rs` (`revoke_active_within`), `src/goal.rs` (`has_unresolved_publication`), `src/external_writer_failure_tests.rs::at_e5_050` |
| H5-0-R6 | Detached descendant processes can outlive the publication fence | DESIGN_CONSTRAINT → narrowed, and the harm is now blocked | **Partly closed.** The Windows Job Object already contains the tree; the POSIX `setsid` escape remains a design constraint (D1), but a surviving helper can no longer be paired with a *finalized* revocation | H5-A F1/F2 record, `docs/campaign/H5_A_INDEPENDENT_REVIEW.md` §5 |
| H5-0-R7 | C3 / R-1: same-OS-user Owner impersonation | DESIGN_CONSTRAINT | **Unchanged and explicitly not claimed as solved.** H5 adds no OS boundary; it adds an attestation value that says honestly what a fixture grant is | `docs/security/C3_0_WINDOWS_TRUST_BOUNDARY.md` |
| H5-0-R8 | PR #35's base contains unmerged #30–#34 | CONFIRMED_DEFECT (integration/structure) | **Quantified by H5-E.** `origin/feat/h4-a-mcp-contract-v1` contains `integration/external-agent-v1-e3-base`; merging #51 to `main` therefore lands #30, #31, #32, #33 and #34 in one step | `docs/campaign/H5_E_INTEGRATION_DECISION_PACKET.md` |
| H5-0-R9 | Serea execution-contract mismatch (`host.goal.run` vs `goal_next`/`goal_submit`) | DESIGN_CONSTRAINT | **Still open.** H5 does not change Serea or its frozen protocol; the ADR-2 `host.goal.work` direction is re-evaluated and recommended, not implemented | `docs/campaign/H5_E_INTEGRATION_DECISION_PACKET.md` §5, `docs/integration/SEREA_S0_0_CONTRACT_ASSESSMENT.md` |

### 2.2 New findings from the H5-A independent review, and their disposition

| id | Severity | Finding | Disposition |
| --- | --- | --- | --- |
| F1 | **High** | Publication cleanup could finish without observing the helper's exit, and revocation could then finalize while a helper might still write. Kill results were discarded and lifecycle evidence was stripped before the caller saw it. | **Fixed.** `Goal::has_unresolved_publication()` + a revocation barrier that denies the grant (`REVOKE_REQUESTED`) but refuses to finalize while any operation intent is `APPLYING`. New code `REVOCATION_BLOCKED_BY_UNRESOLVED_PUBLICATION`. Regression `at_e5_050` covers the block, the durable denial, the refusal of a new charge, and the successful finalization after reconciliation. |
| F2 | **High** | POSIX abort (SIGKILL of the host) bypasses `Drop`, so a surviving helper can outlive the ledger lock; a restarted host could then finalize revocation. | **Mitigated, not eliminated.** The same durable barrier applies after restart: the `APPLYING` intent survives, so the restarted host refuses to finalize until recovery reconciles it. The surviving process itself cannot be stopped retroactively on POSIX — that remains D1 and is stated as such, not claimed closed. On Windows the Job Object with kill-on-close already covers the tree. |
| F3 | Medium | A response that trips the size/depth bound records a receipt, drops the WorkItem, and leaves the Task `RUNNING` with no WorkItem to answer. | **Open, documented.** Not fixed in H5 (it needs a role-specific atomic Task transition and touches the submit pipeline well beyond the H5 scope). Recorded with a reproduction sketch in `H5_A_INDEPENDENT_REVIEW.md`. |
| F4 | Medium | The EXTERNAL Codex-fallback gate converted a state-store read error into "no active Goal" and allowed fallback. | **Fixed.** `CodexFallbackAuthority::{Established,Unavailable}`; `Unavailable` fails closed. Regression `an_unreadable_state_root_refuses_the_codex_fallback_gate`. |
| F5 | Low | Published tool schemas and runtime parsers still drift in places (`null` for optional fields, integer bounds, an unpublished fallback `mode`, broader runtime enums, undocumented semantic limits). | **Open, documented.** H5-B removes this class of drift for the five `ModelWorkItem` response contracts only. |
| F6 | Low | Enforcement used effective (anchored) usage while the API/console reported raw ledger usage. | **Fixed.** `AuthorizationView` now reports `max(ledger, anchor)` per component, matching enforcement. |
| F7 | Low | A replayed *rejected* receipt was labelled `REPLAY_OF_ACCEPTED_SUBMISSION`. | **Fixed.** `REPLAY_OF_STORED_RECEIPT` for a non-accepted receipt; `REPLAY_OF_ACCEPTED_SUBMISSION` only for `ACCEPTED_FOR_HOST_PROCESSING` / `COMMITTED`. |
| D1 | Info | POSIX process groups are not containment against a deliberate `setsid`; leader exit is not tree exit; Windows teardown does not verify an empty Job. | **Design constraint, restated.** The fixed publication helper spawns no descendants today, which bounds it; a helper that may spawn children needs a supervisor. |
| D2 | Info | The rollback anchor protects against ledger-only rollback, not coordinated restoration of both documents or same-principal forgery. | **Design constraint, restated.** No external monotonic witness exists inside one OS user's state. |
| G1 | Medium | The H4 MCP contract suite validated its own generated instances and never submitted; initialisation was abbreviated. | **Partially closed.** H5 adds a real `goal_start` → `goal_next` → `goal_submit` loop against a live server, including a refused out-of-contract answer and an accepted contract-valid answer. Initialisation and the remaining tool-surface drift stay open. |
| G2 | Medium | H3's "real boundary" test writes in-process; no termination-failure or late-write witness exists. | **Open.** H5-D crash injection was not executed (see §2.4). |

### 2.3 VERIFIED_SAFE, re-derived at `e633d9d`

| Invariant | Enforcing code | Evidence |
| --- | --- | --- |
| A scope is never authority | `goal_api::goal_start` refuses `write_scope` for CODEX; EXTERNAL persists a *proposal* | `tests/mcp_contract.rs`, `src/goal_api.rs` |
| Owner approval needs a live nonce | `approvals` → `authorization_store::approve_pending` → `build_grant` nonce check | `authorization_tests` AT-GW-005 |
| No publication without a grant | `external_publish` → `acquire_publication_fence` → `usable_grant_for_operation` | `external_writer_*` tests |
| Ledger-only rollback cannot refund budget | `record_anchor_grant` resets the floor only for a strictly newer epoch; `charge_operation_with_floor` uses the componentwise max | `at_gw_021b`, `at_gw_021c`, `at_gw_019c` |
| Revocation cannot be bypassed mid-operation | fence held across Begin → write → postimage → Complete | `external_writer_race_tests` |
| UNKNOWN is never retried blindly | journal stays `APPLYING`; `mutation_recovery` classifies; finalizer rejects unresolved side effects | `external_writer_failure_tests` |
| Reviewer cannot reach COMPLETED | reviewer counts findings; completion needs Task Verifier + Goal Verifier + Finalizer | `goal_finalizer_tests` |
| Model output is data | role validators are the only path from a submission to a Task transition | `external_*_tests` |

### 2.4 What H5 did **not** execute

* Real-model Scenario B (Writer → Reviewer → Finalizer), C (Replanner) and D
  (crash injection) were **not executed**. H5-C delivers the isolation mechanism
  they need, but wiring a live Writer loop plus a real model plus a crash
  schedule is a separate campaign leg. They are recorded as `NOT_VERIFIED`, not
  as passing.
* No Linux/macOS execution of any H5 change: all H5 test evidence is Windows
  x64. CI (Linux/macOS/Windows) is the cross-platform evidence and is recorded in
  the PR.
* The Owner Control Surface was not driven interactively; no production grant was
  issued in H5.

---

## 3. Gate assessment (H5-0 → H5-A)

The H5-A gate requires 0 unresolved Critical/High code defects plus an
independent review of the final HEAD. At H5-0 the review did not exist yet, so
the gate was **not** passed at baseline; it was passed after the remediation in
§2.2 (F1, F2) and is recorded in `H5_A_INDEPENDENT_REVIEW.md`.
