# H5.1 campaign checkpoint — Security & CI recovery

Campaign: **GoalLatch H5.1 — Security Closure → CI Recovery**
Role: Lead Rust Engineer, Security Engineer, Reliability Engineer
Worktree: `.local/h51b`
Target dir: `.local/h51-target`

This is the resume point. Re-derive every row from GitHub, the working tree, and
the real CI before trusting it.

---

## 1. Baseline (verified at H5.1-0)

| Item | Observed value |
| --- | --- |
| `origin/main` | `e6950fdc3bab72982285888a81cce822743fbba5` |
| PR #51 HEAD | `e633d9d8416cf8e4ff0bef1167945585128e3c0e` |
| PR #52 HEAD | `90b28e0b8cf8d2bb324f5b5ed31a133163195375` (OPEN, Draft, base PR #51) |
| PR #52 CI | 14 success / 8 failure (two runs) |
| Local branch | `feat/h5-1-state-consistency-hygiene-v1` |
| Free space at start | 24 GB on C: |

The H5.1-0 classification remains:

* **CONFIRMED_DEFECT**: R2-1 (recovery cleared the publication barrier from
  file observations), R2-3 (anchor-only revocation projection), R2-5 (fixture
  issuance bypassed the ledger lock), R2-10 (raw ledger usage in `/grant
  status`), F3 (bounded refusal left the Task `RUNNING`), CRLF/autocrlf CI
  failures, Windows byte-comparison artifact failure.
* **COVERAGE_GAP**: R2-11 (some boundary-value probes), F5 (non-WorkItem
  schema↔parser drift), R2-12 (compatibility projection, partially fixed).
* **DESIGN_CONSTRAINT**: C3 / R-1 same-OS-user impersonation; POSIX detached
  descendants (D1).
* **VERIFIED_SAFE**: grant epoch/anchor floors, publication fence exclusivity,
  revocation state machine, receipt idempotency, CODEX writer bounds.

## 2. Phase status

| Phase | Status | Evidence |
| --- | --- | --- |
| H5.1-0 Baseline / failure inventory | **DONE** | §1; CI logs for PR #52 |
| H5.1-A CI / artifact recovery | **DONE** | PR #53: `.gitattributes` LF policy, CRLF renormalization, schema artifact test passes, macOS flake fixed in the stack |
| H5.1-B Durable publication quarantine | **DONE (implementation)** | PR #54; R2-1 fix + negative controls |
| H5.1-C State consistency | **DONE for R2-3/R2-5/R2-10/F3** | PR #54 + PR #55 |
| H5.1-D Repository hygiene | **DONE for the branch tip** | `docs/campaign/H5_1_D_EVIDENCE_SECURITY.md`; `tmp/h5-d` untracked from the tip (still in public history) |
| H5.1-E Independent security review | **ROUND 1 FAIL / REMEDIATED / RE-REVIEW RUNNING** | review of `90b28e0..7cce959` found H51-01 High + H51-02/03/04; fixed in `7ceda08`, recorded in `H5_1_E_INDEPENDENT_REVIEW.md`, re-review of `5158122` running |
| H5.2 Real-model validation | **BLOCKED by H5.1 gate** | not started |

## 3. Stacked Draft PRs

| PR | Base | Head | Scope |
| --- | --- | --- | --- |
| #53 | PR #52 (`feat/h5-a-security-remediation-v1`) | `0d9ea59` | LF checkout policy and CRLF renormalization; CI/artifact recovery |
| #54 | PR #53 | `b2f3267` | R2-1 root fix: durable publication quarantine, process-liveness latch, R2-3/R2-5/F3 |
| #55 | PR #54 | `5158122` | R2-10 effective usage, repo hygiene, additional negative controls, H51-01..04 remediation, at_e5_044 race fix |

PR #54's first CI run exposed a real race: two concurrent publishers both
observed `BLOCKED` because an ACTIVE lock probe wrote an identical quarantine
transition and bumped the Goal revision, breaking the winner's termination CAS.
PR #55 fixed the identical-transition part; the remaining window was the
driver's recovery probe acquiring the still-free armed lock before the helper
claimed it. `advance_external_step` now does not probe while an active WorkItem
exists. The final H5.1 HEAD is PR #55's `5158122` (`7ceda08` code + review doc).

Round-1 independent review (`90b28e0..7cce959`) returned FAIL on H51-01 (High).
`7ceda08` fixes H51-01 (migration scan continues past intent-less attempts),
H51-02 (witness-less legacy UNPROVEN validates but can never be promoted),
H51-03 (ABORTED retry re-syncs durably), and H51-04 (schema-1 refuses a carried
quarantine). The re-review of `5158122` is running.

## 4. R2-1 implementation

* New `src/publication_lock.rs`: host-armed `ARMED <token>` file; helper claims
  the exclusive OS lock and verifies the token; host proves quiescence by
  acquiring the lock and writing `ABORTED`; a held lock is `Active`; a missing
  or unreadable lock is `Unproven`, never quiescent.
* New `src/publication_quarantine.rs`: schema-7 `PublicationQuarantine` with
  `NOT_STARTED` / `ACTIVE` / `UNPROVEN` / `QUIESCENT_VERIFIED`, bounded history,
  activation/termination/recovery evidence, and validation.
* `Goal` schema 7 persists the quarantine. Schema 6 → 7 migration maps an
  `APPLYING` intent to `UNPROVEN` and otherwise `NOT_STARTED`.
* `external_publish` arms the quarantine in the same Goal CAS as
  `BeginOperation`, before the helper spawns, and records termination evidence
  immediately after the write. A failed termination commit leaves `ACTIVE` and
  returns `WRITER_PUBLICATION_QUARANTINE_UNRECORDED`.
* `goal_resume` and the external driver reconcile from the lock probe only. An
  ACTIVE probe is a no-op (no revision bump).
* `revoke_active` and `goal_finalizer` refuse while the quarantine is
  unresolved.
* `has_unresolved_publication()` remains quarantine-aware and also keeps the
  journal `APPLYING` check as defence in depth.

## 5. Negative controls

| Test | What it proves |
| --- | --- |
| `r2_1_recovery_never_clears_a_quarantine_while_the_helper_holds_the_lock` | A real OS lock held across `goal_resume` and `revoke_active` keeps the quarantine `UNPROVEN`; releasing it is the only path to `QUIESCENT_VERIFIED` |
| `r2_1_a_partial_file_change_with_a_live_helper_keeps_the_quarantine` | A partial file with a live helper is `UNPROVEN`; recovery blocks the Task with `MUTATION_RECONCILIATION_REQUIRED` and never guesses `NOT_PERFORMED` |
| `r2_1_concurrent_revoke_and_resume_cannot_clear_a_held_quarantine` | Concurrent revocation/resume against a held lock leaves `REVOKE_REQUESTED` and `UNPROVEN` |
| `publication_quarantine::tests::recovery_probes_a_held_lock_and_keeps_the_quarantine` | The lock witness is a real kernel lock, not a flag |
| `publication_lock::tests::a_held_lock_is_active_and_a_late_helper_refuses_after_abort` | `ABORTED` is a one-way latch; a late helper refuses |

## 6. Measured local evidence (Windows x64, rustc 1.96.0)

| Run | Result |
| --- | --- |
| `cargo fmt --all -- --check` | clean |
| `cargo check --locked --all-targets` | clean |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | clean |
| `cargo test --locked` | **1684 passed, 0 failed, 1 ignored** |
| `cargo test --locked --features test-grant-fixture` | **1687 passed, 0 failed, 1 ignored** |
| `at_e5_044_two_clients_publish_exactly_once`, 30 consecutive runs | 30/30 pass |
| `external_writer_failure_tests` | 20/20 pass (incl. 3 R2-1 controls) |
| `publication_quarantine` / `publication_lock` | 8/8 and 6/6 pass |

CI on PR #55 is the cross-platform evidence and was still pending when this
checkpoint was written. Re-check it before claiming the H5.1 gate.

## 7. Independent review

* Target: `7cce959` (`feat/h5-1-state-consistency-hygiene-v1`), diff from
  `90b28e0`.
* Reviewer: Codex `gpt-6.1-sol`, reasoning `high`, separate app-owned read-only
  task.
* Status: **RUNNING**. The result and disposition must be recorded in
  `H5_1_E_INDEPENDENT_REVIEW.md` before any PASS claim.
* Rule: any code change after the review makes the final diff unreviewed and
  requires a re-review.

## 8. H5.1 gate

| Requirement | Result |
| --- | --- |
| R2-1 root fix | **Implemented and locally proven**; re-review pending |
| 0 unresolved Critical/High | **Round-1 High H51-01 fixed; re-review pending** |
| Required CI 22/22 on final HEAD | **Pending** (PR #55 running on `5158122`) |
| Quarantine crash tolerance proven | **Local negative controls pass** (incl. 40/40 at_e5_044, 100/100 concurrent revoke/resume); H5.2-D subprocess evidence pending |
| Grant state consistency | **Implemented** (R2-3, R2-5, R2-10, F3) |
| Known permanent-stop problems resolved | **F3 fixed** |
| Independent Security Review complete | **Round 1 complete (FAIL); re-review running** |
| Final HEAD evidence | **Pending CI + re-review** |

Current honest state: **PARTIAL / NOT READY FOR OWNER REVIEW** until PR #55 CI
is green and the re-review reports 0 Critical/High.

## 9. Next work (resume plan)

1. Wait for PR #55 CI on `5158122` and the H5.1-E re-review.
2. For every remaining Critical/High finding: fix on
   `feat/h5-1-state-consistency-hygiene-v1`, re-run the focused tests, and
   request another re-review of the new HEAD.
3. Re-run the full local suite and CI after any change.
4. Only when all H5.1 gate rows are PASS: update this checkpoint and start H5.2
   (Scenario A regression, then Writer/Reviewer, Replanner, Crash/Recovery).
5. If the gate is not met: keep H5.1 PARTIAL/BLOCKED and do not run a Writer
   dogfooding loop.

## 10. Owner decisions / not claimed

* C3 / R-1 is **not solved**. The quarantine is a process-liveness barrier, not
  an OS trust boundary. Unrestricted real-world EXTERNAL Writer operation on a
  normal Windows account must not be declared safe while C3 is open.
* `tmp/h5-d` remains in PR #52's public history. No secret was found; erasure
  was not attempted and is not claimed. Any invalidation decision is the
  Owner's.
* R2-11's remaining boundary probes and F5 non-WorkItem schema drift are
  documented coverage gaps, not claimed closed.
* No main merge, Draft clearance, Release, Tag, or S force-push was performed.
