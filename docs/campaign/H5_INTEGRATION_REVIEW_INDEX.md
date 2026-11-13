# Integration review index — External Agent Backend V1

Base: `main` = `e6950fdc3bab72982285888a81cce822743fbba5`
Integration head (this branch): the commit this file was committed on top of
`cf66941` — see the PR body for the exact SHA and for the range.

This is a **reading guide**, not a summary of the review outcome. GitHub's diff
view is 123 files across 90 commits; reading it front to back is not how this
work should be checked. Each section below says what the code is responsible for,
which invariant it must keep, where the tests are, and what to read first.

Nothing here merges, releases or un-drafts anything. C3 / R-1 is unresolved and
is restated at the end.

---

## 0. Reading order for a 60-minute review

| Order | Read | Why |
| --- | --- | --- |
| 1 | `docs/campaign/H5_E_INTEGRATION_DECISION_PACKET.md` | The state of the whole stack, the risks, and what is not proven |
| 2 | `docs/EXTERNAL_AGENT_BACKEND_V1_THREAT_MODEL.md` | The trust boundary the code is written against |
| 3 | `docs/EXTERNAL_AGENT_BACKEND_V1_AUTHORIZATION_B.md` | The grant model the Writer depends on |
| 4 | `docs/security/C3_0_WINDOWS_TRUST_BOUNDARY.md` | Why a same-user agent is **not** stopped, and what C3 would add |
| 5 | `docs/campaign/H5_3_1_FINAL_CLOSURE.md` + `H5_3_EVIDENCE.md` §10 | The evidence harness, the eight review rounds, and what each round changed |
| 6 | `src/authorization.rs` → `src/authorization_store.rs` → `src/external_writer.rs` → `src/external_publish.rs` | The write path in the order it executes |
| 7 | `src/mcp.rs` (`goal_next` / `goal_submit` / `goal_status` / `goal_result` / `goal_resume`) → `src/external_goal_api.rs` → `src/external_model_store.rs` | The external contract |
| 8 | `src/publication_lock.rs`, `src/publication_quarantine.rs`, `src/mutation.rs` | Crash and recovery behaviour |

## 1. Where each capability lives

### 1.1 Goal API and durable model work (E3)

| Item | Value |
| --- | --- |
| Entry points | `src/goal_api.rs`, `src/external_goal_api.rs`, `src/external_model_work.rs`, `src/external_model_store.rs` |
| Commits | `7aa5b7f`, `8084dc0`, `2834af4`, `0a4cdb5`, `3fb9094`, `86fa92a`, `5df8a29` |
| Invariants | A `ModelWorkItem` is issued once per `(goal, revision, role)`; `goal_submit` is idempotent on `request_digest`; a refused submission does not consume the role; every response is validated against the published contract before it is stored |
| Tests | `src/external_goal_api_tests.rs`, `src/external_goal_tests.rs`, `src/external_flow_tests.rs`, `src/external_dag_tests.rs`, `tests/mcp_contract.rs` |
| Watch for | the strict-shape refusals (`additionalProperties: false`) and the `RESPONSE_TOO_LARGE` bound |

### 1.2 Planner, Readonly worker, Reviewer, Replanner (E4 / E5)

| Item | Value |
| --- | --- |
| Entry points | `src/planner.rs`, `src/readonly_worker.rs`, `src/replanner.rs`, `src/external_replanner.rs`, `src/scheduler.rs`, `src/worker_capability.rs` |
| Commits | `afbce72`, `e018860`, `24cc80e`, `b4101a0`, `844dab2`, `36a45fb`, `08f3edf` |
| Invariants | EXTERNAL roles never spawn a Codex worker; a Readonly task cannot publish; the Replanner applies a plan as a single commit and must `resolve_needs_replan` before `add_dependencies`; a review leg can resume without re-running the Writer |
| Tests | `src/external_replanner_tests.rs`, `src/external_response_contract_tests.rs`, `src/external_reviewer_recovery_tests.rs`, `src/replanner_*_tests.rs`, `src/goal_orchestration_regression_tests.rs` |
| Watch for | the published condition text and the validator agreeing (there is a drift test that fails if they diverge) |

### 1.3 Authorization B — `GoalWriteGrantV1` (E5 / H4)

| Item | Value |
| --- | --- |
| Entry points | `src/authorization.rs` (model), `src/authorization_store.rs` (durable CAS + lock), `src/approvals.rs` (Owner Control Surface IPC) |
| Commits | `3320dcd`, `2f574ce`, `327b461`, `1323823`, `e74e174`, `35ad119`, `a573ef3`, `cd03938`, `fa734df`, `1e633f3`, `cc55c5d`, `05eeb99`, `072fa15` |
| Invariants | a grant is bound to `(session, goal, writer_id, epoch, scope, budget, expiry)`; charge/begin/write are durable and idempotent; only a **new generation** resets the usage floor; revocation is fenced and refuses a later publication; a CODEX goal can never receive an EXTERNAL grant |
| Tests | `src/authorization_tests.rs` (+ the seven remediation rounds' tests inside it) |
| Watch for | `resolve_grant` being the single gate used by issuance, stage, publication and revocation; the refusal classes (`GOAL_TASK_TRANSITION_REFUSED`, `WRITE_AUTHORIZATION_UNAVAILABLE`) |

### 1.4 Writer publication and the helper

| Item | Value |
| --- | --- |
| Entry points | `src/external_writer.rs`, `src/writer_stage.rs`, `src/external_publish.rs`, `src/bin/atomic-publish.rs`, `src/atomic_publish_frame.rs`, `src/mutation.rs`, `src/workspace_publish.rs` |
| Commits | `225907d`, `aac075d`, `ee4e8f8`, `12afe6e`, `933211c`, `267d6a8`, `24b6231` |
| Invariants | the Writer **stages**; only the helper **publishes**; `expected_preimage` is enforced (including `ABSENT`); a stage is candidate-unique and immutable; nothing is published for a cancelled Goal or a revoked grant; postimage verification happens after publication |
| Tests | `src/external_writer_tests.rs`, `src/external_writer_failure_tests.rs`, `src/external_writer_race_tests.rs`, `src/atomic_publish_tests.rs`, `src/writer_stress_tests.rs` |
| Watch for | H53-3: `expected_preimage: ABSENT` resolves through the parent directory |

### 1.5 Crash recovery, quarantine, publication lock

| Item | Value |
| --- | --- |
| Entry points | `src/publication_lock.rs`, `src/publication_quarantine.rs`, `src/mutation.rs` (recovery paths), `src/external_goal_driver.rs` |
| Commits | `b2f3267`, `bd6a317`, `7181c96`, `7cce959`, `7ceda08` |
| Invariants | an ACTIVE probe does not bump the Goal revision; a crashed publication is quarantined durably and is not blind-retried; the lock has a liveness witness and the host refuses to proceed when it cannot prove quiescence |
| Tests | `src/publication_lock_subprocess_tests.rs`, `src/external_goal_recovery_tests.rs`, `src/external_h3_reliability_tests.rs`, `src/mutation_recovery` tests |
| Watch for | the difference between "lock file exists" and "lock holder is alive" |

### 1.6 Managed workspace (H4 / E5-D)

| Item | Value |
| --- | --- |
| Entry points | `src/managed_worktree_create.rs`, `src/managed_worktree_prepare.rs`, `src/managed_worktree_observe.rs`, `src/managed_worktree_writer_paths.rs`, `src/workspace_snapshot.rs` |
| Commits | `4931520`, `1122b43`, plus the H4-A contract commit `267d6a8` |
| Invariants | creation is approved through the session IPC; writer paths are derived from durable mutations, not from the model; snapshots are re-observed before completion and bound to the Goal |
| Tests | `src/managed_worktree_*_tests.rs`, `src/managed_workspace_verification_evidence_tests.rs` |
| Watch for | the execute-path acceptance being `Unix`-only for the production driver, and what that means for Windows |

### 1.7 Host verification and finalization

| Item | Value |
| --- | --- |
| Entry points | `src/goal_verifier.rs`, `src/verifier.rs`, `src/verifier_git_observation.rs`, `src/goal_finalizer.rs` |
| Invariants | the host verifies the postimage itself; the Finalizer records evidence, not model claims; a verifier observation is a Git observation, not a model statement |
| Tests | `src/goal_verifier_tests.rs`, `src/verifier_tests.rs`, `src/verifier_git_observation.rs`, `src/goal_finalizer_tests.rs` |

### 1.8 Windows authority and IPC (G0-A)

| Item | Value |
| --- | --- |
| Entry points | `src/approvals.rs`, `src/pipe_security.rs`, `src/windows_authority_audit_tests.rs` |
| Commits | `8c1aa3c`, `ea8f166` |
| Invariants | every host-native mutation requests approval first and fails closed on a missing channel, an invalid reply, a disconnect or a denial; the approval pipe DACL is current-user-only; `yolo` is stdin-local and cannot be set through the pipe |
| Audit record | `docs/WINDOWS_AUTHORITY_AUDIT_G0A.md` (0 High, 1 Medium, 4 Low/Info) |
| Watch for | `Modify` vs `Full control` on the pipe DACL, and the fact that `pipe_security.rs` deliberately omits `SYSTEM`/`Administrators` while the *test harness* allows them (different objects, different policy — see `H5_3_1_FINAL_CLOSURE.md` §3) |

### 1.9 Test-grant isolation (H5-C)

| Item | Value |
| --- | --- |
| Entry points | `src/test_grant_fixture.rs`, `src/config.rs` (`test_grant_fixture_id`) |
| Invariants | `test-grant` exists only under the non-default `test-grant-fixture` feature; issuance refuses an unmarked state root; `resolve_grant` refuses a `TEST_FIXTURE_V1` grant whose fixture id does not match a marker in the running state root |
| Tests | `src/test_grant_fixture_tests.rs`, plus `cargo test --features test-grant-fixture` |

### 1.10 Evidence harness (H5.3 / H5.3.1)

| Item | Value |
| --- | --- |
| Entry points | `scripts/h5-d/real_model_scenario_a.py` (fixture + driver), `private_root.py` (root privacy), `crash_host_kill.py`, `test_fixture_safety.py` |
| Invariants | create-only, never delete; the root is created **with** an owner-only descriptor and verified by reading it back; every component of the path must be non-replaceable by an untrusted principal; whole-tree verification before seeding and on reuse; one live run per root; per-call digest comparison |
| Evidence | `docs/campaign/H5_3_0_DRIVER_SAFETY.md` §7, `H5_3_1_FINAL_CLOSURE.md`, `H5_3_EVIDENCE.md` §10 (13 review rounds) |
| Watch for | the harness is test-only and is **not** shipped; a matching digest is not OS-level read-only isolation |

### 1.11 CI and repository hygiene

| Item | Value |
| --- | --- |
| Commits | `0d9ea59` (LF checkout policy, CRLF renormalisation), `7366280` (Linux clippy) |
| Files | `.gitattributes`, `.github/workflows/ci.yml` |
| Watch for | the `quality` job running the Python safety suite, and the Windows/Linux/macOS test legs |

## 2. What this integration does *not* claim

| Claim | Status |
| --- | --- |
| The Owner decision surface is protected from a same-user agent | **No.** C3 / R-1 is unresolved; `AuthorizationView.same_uid_process_resistance` reports `false` |
| An unrestricted EXTERNAL Writer is safe on a normal Windows account | **No.** Nothing here changes that; the Broker (C3) is the missing piece |
| Publication is crash-safe | **Yes, with a quarantine**: a crashed publication is durably quarantined and requires an explicit decision, not a blind retry |
| The fixture harness is isolated at the OS level | **No.** It is owner-only by DACL and refuses replaceable paths; a same-user or administrator process is not stopped |
| Linux/macOS were exercised like Windows | **No.** See the platform matrix in the decision packet |

## 3. Quick verification (from a clean checkout of this branch)

```
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
cargo test --locked --features test-grant-fixture
python scripts/h5-d/test_fixture_safety.py
python scripts/release/test_workflow_policy.py
python scripts/release/test_release_scripts.py
python scripts/release/test_install_contract.py
```

The Python fixture tests create their bases under `~/.goallatch-h5d-test-fixtures`
and never delete them. The driver itself refuses the OS temporary directory as a
base when another local principal can rename it.
