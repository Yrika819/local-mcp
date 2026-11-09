# H5 campaign checkpoint

Campaign: **GoalLatch H5 — External Model Execution Closure / Security
Verification / Integration Readiness**
Branch: `feat/h5-a-security-remediation-v1`, based on PR #51 HEAD `e633d9d`
Worktree: `.local/h5-a` · target dir `.local/h5-a-target`

This is the resume point. Re-derive every row from GitHub and the working tree
before trusting it.

---

## 1. Baseline (verified at start)

| Item | Value |
| --- | --- |
| `origin/main` | `e6950fdc3bab72982285888a81cce822743fbba5` |
| PR #51 HEAD | `e633d9d8416cf8e4ff0bef1167945585128e3c0e` (OPEN, Draft, MERGEABLE) |
| PR #51 CI | 22/22 SUCCESS; GitHub formal reviews: **0** |
| Free space | 46 GB at start |
| Independent model available | Codex `gpt-6.1-sol` |

## 2. Phase status

| Phase | Status | Evidence |
| --- | --- | --- |
| H5-0 Baseline / risk reassessment | **DONE** | `H5_RISK_REGISTER.md` |
| H5-A Independent review of PR #51 HEAD | **DONE** (round 1) | `H5_A_INDEPENDENT_REVIEW.md` |
| H5-A Remediation of F1/F2/F4/F6/F7 | **PARTIAL** | `H5_A_REREVIEW.md` — F4, F7 remediated; F1/F2 **partially** remediated (R2-1 open) |
| H5-A round-2 re-review | **DONE** | `H5_A_REREVIEW.md` — no Critical; R2-1 High open |
| **H5-A gate** | **NOT PASSED** | 0 Critical, 1 High unresolved (R2-1) |
| H5-B Response Contract V2 | **DONE** | `H5_B_RESPONSE_CONTRACT.md`, `docs/schemas/external/*.v2.json` |
| H5-C Isolated grant harness | **DONE (mechanism) / PARTIAL (E2E)** | `H5_C_TEST_GRANT_HARNESS.md` |
| H5-D Scenario A (real model) | **DONE — PASSED** | `H5_D_REAL_MODEL_EVIDENCE.md` |
| H5-D Scenario B / C / D | **NOT_VERIFIED** | not executed |
| H5-E Integration decision packet | **DONE** | `H5_E_INTEGRATION_DECISION_PACKET.md` |
| C3 / Serea compatibility | **DONE (design only)** | `docs/security/C3_0_WINDOWS_TRUST_BOUNDARY.md`, `H5_E` §5 |

## 3. Code changes in this leg

| Area | Files |
| --- | --- |
| H5-A F1/F2 | `src/goal.rs` (`has_unresolved_publication`), `src/authorization_store.rs` (`revoke_active_within` barrier + already-`REVOKED` branch), `src/authorization.rs` (`RevocationBlockedByPublication`), `src/external_writer_failure_tests.rs` (`at_e5_050`) |
| H5-A F4 | `src/external_goal_api.rs` (`CodexFallbackAuthority`), `src/mcp.rs`, `src/external_goal_api_tests.rs` |
| H5-A F6/F7 | `src/authorization.rs` (`AuthorizationView` effective usage + attestation), `src/external_goal_api.rs` (replay label) |
| H5-B | `src/external_response_contract.rs` (new), `src/external_response_contract_tests.rs` (new), `src/external_goal_api.rs` (`ResponseContractView`), `src/external_model_work.rs` (`SubmissionDecision::ALL`), constant imports in `src/planner.rs`, `src/writer.rs`, `src/readonly_worker.rs`, `tests/mcp_contract.rs`, `docs/schemas/external/*` |
| H5-C | `Cargo.toml` (`test-grant-fixture` feature), `src/test_grant_fixture.rs` (new), `src/test_grant_fixture_tests.rs` (new), `src/config.rs` (marker reader/writer + test override), `src/authorization.rs` (`OwnerAttestation::TestFixtureV1`, `test_fixture_id`, `stamp_test_fixture`, `record_approved_with_attestation`, `resolve_grant` gate), `src/main.rs` (feature-gated command) |
| H5-D | `scripts/h5-d/real_model_scenario_a.py` (new) |
| Docs | `docs/campaign/H5_*.md`, `docs/schemas/external/*` |

## 4. Measured test evidence (Windows x64, rustc 1.96.0)

| Run | Command | Result |
| --- | --- | --- |
| Baseline (pre-H5 code) | `cargo check --locked --all-targets` | clean |
| Full suite, state `8e62812` | `cargo test --locked` | **1662 passed, 0 failed, 1 ignored** + 5 + 5 + 21 integration |
| Full suite, all features, H5 HEAD | `cargo test --locked --features test-grant-fixture` | **1669 passed, 0 failed, 1 ignored** + 5 + 5 + 21 |
| Contract tests | `cargo test --locked external_response_contract` | 24 passed, 1 ignored (regeneration) |
| MCP contract (real process) | `cargo test --locked --test mcp_contract` | 5 passed |
| H5-C isolation | `cargo test --locked --features test-grant-fixture test_grant_fixture` | 4 passed |
| F1 barrier | `cargo test --locked --features test-grant-fixture at_e5_050` | passed |
| Lints | `cargo fmt --all -- --check`; `cargo clippy --locked --all-targets --all-features -- -D warnings` | clean |
| Real model, Scenario A | `python scripts/h5-d/real_model_scenario_a.py --binary …` | **COMPLETED**, 2 model calls, negative control `REJECTED_SCHEMA`, workspace unchanged |

CI on the pushed branch is the cross-platform evidence (Linux/macOS); the runs
above are Windows-local only.

## 5. What must happen next (resume plan)

1. **H5-A gate blocker — durable publication quarantine (R2-1, R2-3).**
   Add a Goal-level quarantine written *before* the helper starts and cleared only
   by a proven teardown or an explicit Owner decision. Recovery must not clear it;
   `Unknown`/`Partial`/`ReconciledPerformed` must leave it set. Re-derive
   `AuthorizationView`'s `REVOKED` from it. Then re-run the round-2 audit.
2. **R2-5** — fixture issuance must take the ledger lock and call `matches_goal`.
3. **R2-10** — `/grant status` must print effective (anchored) usage.
4. **H5-D B/C/D** — wire `scripts/h5-d/` to the `test-grant` fixture, then run the
   Writer → Reviewer → Finalizer loop, a live Replanner, and process-kill crash
   injection at the eight publication points.
5. Only then re-assess `READY_FOR_OWNER_REVIEW`.

## 6. Re-entry notes

* Restore the environment: `CARGO_TARGET_DIR=.local/h5-a-target`, MSVC toolchain
  `1.96.0-x86_64-pc-windows-msvc`; do not touch the existing worktrees' caches.
* `docs/schemas/external/*.json` are generated: run
  `cargo test regenerate_response_contract_artifacts -- --ignored` after any
  contract change, then `cargo test external_response_contract`.
* The H5-D driver needs `codex` on `PATH` and a built `local-mcp.exe`; it never
  uses the operator's state root.
