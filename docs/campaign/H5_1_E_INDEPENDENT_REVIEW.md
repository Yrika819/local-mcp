# H5.1-E — Independent security review and remediation

## 1. What "independent" means here

* **Audited range:** `90b28e0..7cce959` (the H5.1-A/B/C/D stack).
* **Reviewer:** Codex `gpt-6.1-sol`, reasoning effort `high`, run as a separate
  app-owned read-only child task. The campaign implementer did not author the
  review and did not review its own remediation.
* **Mode:** read-only. The reviewer reported: *"No files were changed and no
  builds or tests were run."* Every reference was commit-pinned.
* **Verdict delivered:** `H5.1_GATE: FAIL`, blocking finding **H51-01 (High)**,
  plus H51-02 (Medium), H51-03 (Medium), H51-04 (Low), H51-05 (Info).

## 2. Findings and disposition

| id | Severity | Finding | Disposition |
| --- | --- | --- | --- |
| H51-01 | **High** | The schema-6 migration scan used `?` on `attempt.get("mutation_intent")`, so an earlier intent-less readonly/unsuccessful attempt returned from the whole scan and a later `APPLYING` Writer was missed. The quarantine became `NOT_STARTED` and R2-1 reopened. | **FIXED.** The scan continues past attempts without an intent and fails closed on an `APPLYING` intent whose identity is unusable. Regression: `schema_six_applying_intent_after_an_intentless_attempt_stays_quarantined` places the intent-less attempt first and a target file that exactly matches the intended postimage; recovery reconciles the journal to `ReconciledPerformed` and the quarantine must still be `UNPROVEN`. |
| H51-02 | Medium | `legacy_applying` produced an `UNPROVEN` record with no `lock_path` or `token_sha256`, but validation required both, so every migrated applying Goal was unreadable. | **FIXED.** A witness-less legacy `UNPROVEN` record is accepted only in that exact shape. `record_termination` refuses it (no probe exists to justify quiescence), `activate` refuses it (unresolved), and `reconcile` still never clears it. The existing recovery test now calls `validate()` and asserts the refusal. |
| H51-03 | Medium | A retry could accept `ABORTED` bytes whose earlier synchronization failed: readable page-cache contents stood in for durable evidence. | **FIXED.** The `ABORTED` branch re-synchronizes while holding the lock and returns `Unproven` if that fails. A thread-local test-only fault hook proves the failed-sync/retry path (`a_retry_cannot_accept_an_abort_whose_sync_failed`). Production always calls `sync_all`. |
| H51-04 | Low | Schema-1 decoding silently overwrote a supplied publication quarantine. | **FIXED.** A schema-1 document that already carries the field is refused, matching the sibling migrations. Regression: `legacy_schema_one_cannot_carry_a_publication_quarantine`. |
| H51-05 | Info | The negative controls prove kernel-lock behavior in-process, not a helper surviving host death or claiming late. | **OPEN / DEFERRED to H5.2-D.** This is a coverage gap, not a code defect. H5.2-D is the real-process crash/recovery leg and is the correct place for subprocess host-death, delayed-claim, and late-abort controls. It will not be claimed as verified before then. |

## 3. Additional race found by the reviewer's H51-01 chase

The full-suite CI run exposed `at_e5_044_two_clients_publish_exactly_once`
failing with `["BLOCKED", "BLOCKED"]` on Linux and macOS. The cause:
`advance_external_step` probed the publication lock at the start of every
step, including while a concurrent step was publishing an active WorkItem. The
lock is armed just before the helper spawns, so the probe could acquire the
still-free lock, write `ABORTED`, and make the real helper refuse. That is a
real bug, not a test flake.

**Fix:** `advance_external_step` does not probe while an active WorkItem
exists. The active publication path already re-checks the exact CAS and
refuses a concurrent publisher. Recovery probing remains in explicit
`goal_resume` and in steps with no active WorkItem. The test passes 40/40
consecutive runs and the concurrent revoke/resume control 100/100.

## 4. Verified-safe summary (from the review, retained)

* For valid schema-7 records, production callers clear an unresolved quarantine
  only through the lock probe (`goal_api`, `external_publish`), never through
  mutation observations.
* Revocation and Goal finalization refuse while the quarantine is unresolved.
* Lock arm → quarantine/BeginOperation CAS → helper spawn ordering is correct;
  a failed termination CAS leaves `ACTIVE`.
* The helper refuses missing/unarmed/aborted/mismatched tokens; an empty lock
  path is an intentional CODEX/direct-tool bypass and EXTERNAL always supplies
  one.
* R2-3 ledger/anchor projection, R2-5 fixture lock/`matches_goal`, R2-10
  effective usage, and F3's atomic Task transition are present.
* Crash execution itself and cross-platform CI remain `NOT_VERIFIED` until the
  final HEAD's CI and the H5.2-D subprocess controls.

## 5. Re-review

The remediation HEAD is `7ceda08` (on top of the audited `7cce959`). Because
code changed after the review, the final diff is **unreviewed** until a
re-review completes. The re-review is a separate read-only task against
`90b28e0..7ceda08`, with the same focus areas plus the four finding ids.

## 6. Gate

| Requirement | Result |
| --- | --- |
| 0 unresolved Critical | **PASS** — none found |
| 0 unresolved High | **PENDING re-review** — H51-01 fixed, fix unreviewed |
| Required CI 22/22 on final HEAD | **PENDING** — PR #55 running on `7ceda08` |
| Quarantine crash tolerance | **Local controls pass**; H5.2-D subprocess evidence pending |
| Grant / revocation consistency | **PASS** locally (R2-3/R2-5/R2-10/F3) |
| Independent review complete on final HEAD | **PENDING** |
