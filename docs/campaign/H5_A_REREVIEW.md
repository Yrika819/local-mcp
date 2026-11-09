# H5-A round 2 — independent re-review of the remediation

* **Reviewer:** Codex `gpt-6.1-sol`, reasoning `high`, separate app-owned child task.
* **Range:** `e633d9d..8e62812` (the H5 remediation commit).
* **Mode:** read-only; the reviewer reported it made no filesystem changes and ran
  no tests.
* **Verdict delivered:** `F4 REMEDIATED`, `F7 REMEDIATED`,
  `F1 PARTIALLY_REMEDIATED`, `F2 PARTIALLY_REMEDIATED`, `F6 PARTIALLY_REMEDIATED`;
  one new **conditional High** defect (R2-4) in fixture confinement and one
  inherited **High** still open (R2-1); plus a set of Medium/Low contract defects
  in the new H5-B surface.

This document records the disposition of every round-2 finding. Nothing in it is
a claim that a finding was closed unless the closing change and its test are
named.

## 1. Findings and disposition

| id | Sev | Finding | Disposition |
| --- | --- | --- | --- |
| R2-1 | **High** | Recovery converts `Applying` → `Unknown`/`Partial`/`ReconciledPerformed` from **file observations**, then `has_unresolved_publication()` returns false and revocation finalizes. File reconciliation is not process-quiescence evidence. | **OPEN — not fixed.** This is correct and it is the reason H5-A's gate is not claimed as passed. The fix needs a durable quiescence fact that recovery cannot clear (a publication quarantine cleared only by a proven teardown or an explicit Owner decision), plus tests for each recovery outcome. Recorded as the top item of the next campaign leg. |
| R2-2 | Medium | The already-`REVOKED` compatibility branch returned success before checking the barrier. | **FIXED** — the branch now loads the Goal and refuses with `REVOCATION_BLOCKED_BY_UNRESOLVED_PUBLICATION` when an `APPLYING` intent exists. |
| R2-3 | Medium | An anchor-only crash window can still project `REVOKED` (anchor denies, ledger still `NotRevoked`) while an `Applying` intent exists. | **OPEN** — needs the same quarantine mechanism as R2-1 so the projection is derived from one durable state instead of two. |
| R2-4 | **High (conditional)** | Disposable confinement was enforced only in the `cfg(test)` marker *writer*; the *reader* accepted a marker in the default production root and did not check the workspace. | **FIXED** — `config::test_grant_fixture_id_in` now refuses the default state root, refuses a root under a Git working tree, and requires the declared workspace to be a sibling of the state root under the same disposable parent. The vacuous `… \|\| true` assertion the reviewer flagged was removed (the default-build test now only asserts command absence). |
| R2-5 | Medium | Fixture issuance did not take the ledger lock or call `matches_goal`, so concurrent issuances could leave the anchor and ledger naming different grant ids. | **OPEN** — the fixture is test-only and single-threaded in practice, but the production discipline should be reused. Fix: expose a locked fixture-issuance entry point that calls the same `acquire_ledger_lock` + `matches_goal` sequence as `approve_pending`. |
| R2-6 | Medium | Replanner `add_tasks` reused the Planner shape, so `dependencies` were strings while the real `ReplanTaskProposal` needs tagged task refs. | **FIXED** — a separate `replan_task` definition publishes `{"ref_kind": "EXISTING"\|"NEW", …}` items. Regression: `replanner_task_dependencies_are_published_as_typed_refs`. |
| R2-7 | Medium | The contract required serde-defaulted fields (`dependencies`, `forbidden_paths`, every Replanner additive array) and rejected explicit `null`. | **FIXED** for the required-ness half: those fields are now optional. Explicit `pristine_plan_supersession: null` remains schema-invalid while the host accepts it; because the field is optional, a client that omits it is unaffected (the schema is stricter, never more permissive). Regression: `serde_defaulted_replanner_fields_are_optional_in_the_contract`. |
| R2-8 | Medium | Per-field validation is stricter than the schema in several places (blank strings, proposal-id characters, hash length, scope path bounds, `REVIEW_GATE` ceiling, `blocking_findings` above `u32`). | **PARTIAL** — the reviewer's direction is the safe direction (schema more permissive than the validator), so no response the host accepts is unexpressible. The concrete over-permissiveness that matters — an integer above `u32::MAX` for `blocking_findings` — is now bounded by publishing `maximum` on integer fields. The remaining semantic strictness is published in `conditions[]` and documented rather than encoded as keywords. |
| R2-9 | Medium | `COMMAND_EXIT` was published although `validate_and_normalize_verification` refuses it unconditionally. | **FIXED** — removed from the published `oneOf`; the vocabulary test now asserts exactly one enum kind is withheld and that the withheld kind is refused end to end. |
| R2-10 | Low | `/grant status` still printed raw ledger usage. | **OPEN** — small, but in `approvals.rs` console formatting; not fixed in this leg. |
| R2-11 | Medium | The equivalence tests did not cover typed Replanner dependencies, optional omissions, or numeric upper bounds. | **PARTIAL** — new tests cover R2-6 and R2-7; numeric `maximum` is published and asserted for integers. Full at-limit/over-limit parser probes exist for READONLY and WRITER only. |
| R2-12 | Low | An unknown persisted `output_contract` silently received the role's current schema. | **FIXED** — the projection now publishes `contract_resolved: false` alongside the verbatim persisted string, so a client can tell that `role`/`schema` describe the current contract rather than the persisted one. |
| R2-13/14/15/16 | Info | F4 verified safe; F7 verified safe; fixture compatibility verified safe; projection adds nothing to persisted identity. | **ACCEPTED** |

## 2. What this means for the H5-A gate

The round-2 reviewer found no Critical defect and confirmed F4 and F7. Two High
items are **not** closed:

* **R2-1** (inherited F1/F2): recovery can erase the publication barrier without
  proving execution stopped.
* **R2-4** was conditional-High and is now fixed, but a gate cannot be declared
  passed on the back of an unreviewed fix in the same document that records it.

Therefore:

> **H5-A gate: NOT PASSED.** Critical: 0. High unresolved: **R2-1** (and R2-3 as
> its projection half). F1/F2 remain `PARTIALLY_REMEDIATED`.

That is the honest state, and it is why this campaign reports
`NOT_READY_FOR_OWNER_REVIEW` rather than claiming closure. The remaining work is
narrow and named:

1. A durable publication quarantine (a Goal-level field written before the helper
   starts and cleared only by a proven teardown or an explicit Owner decision),
   not a predicate derived from journal states that recovery rewrites.
2. Recovery must not clear the quarantine; `Unknown`/`Partial`/reconciled
   outcomes must leave it set and remain non-finalizable.
3. `AuthorizationView` must derive `REVOKED` from the quarantine-aware state
   (R2-3).
4. Fixture issuance must reuse the production ledger lock + `matches_goal`
   (R2-5).
5. `/grant status` must report effective usage (R2-10).
