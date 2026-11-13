# H5.2 campaign checkpoint — Real-model validation

Campaign: **GoalLatch H5.2 — Real-Model Validation**
Branch: `feat/h5-2-real-model-validation-v1`
Base: H5.1 final HEAD `31540b7` (H5.1 Gate PASS)
Worktree: `.local/h52`

Re-derive every row from the working tree, `%TEMP%\goallatch-h5d-*` evidence, and
the H5.2 PR CI before trusting it.

---

## 1. Baseline

| Item | Value |
| --- | --- |
| H5.1 final HEAD | `31540b7fe3fc82cdcc4333678399dd1d4ad642c9` |
| H5.1 CI | 22/22 SUCCESS |
| H5.1 independent review | `gpt-6.1-sol` round 2 PASS; `gpt-6.1-terra` final audit PASS (0 Critical / 0 High) |
| Codex policy | explicit `gpt-6.1-terra`, reasoning `high`; no silent fallback |
| Model binary | debug `local-mcp` + `test-grant-fixture`, SHA-256 `859b1bac…` |

## 2. Phase status

| Phase | Status | Evidence |
| --- | --- | --- |
| H5.2-0 Test harness integration | **DONE** | `scripts/h5-d/real_model_scenario_{a,b,c}.py`; isolated marker/grant; production paths shared |
| H5.2-A Scenario A | **PASS** | `COMPLETED`, 2 calls, negative control `REJECTED_SCHEMA`, workspace unchanged |
| H5.2-B Writer/Reviewer | **PASS** | `COMPLETED`, 3 calls, `TEST_FIXTURE_V1` grant, only `src/main.rs` changed, exact content |
| H5.2-C Replanner | **PARTIAL** | run 1 `REPLAN_APPLIED` + one replacement task COMPLETED + one BLOCKED (model said inventory.txt missing); run 2 6× `REPLAN_SCHEMA_VIOLATION` |
| H5.2-D Crash/recovery | **PARTIAL** | real subprocess lock-holder PASS; full host-kill publication NOT_VERIFIED |
| H5.2 final independent review | **NOT STARTED** | — |

## 3. Test-grant isolation

* `test-grant` exists only under the non-default `test-grant-fixture` feature.
* The run uses a disposable `%TEMP%\goallatch-h5d-*` state root and workspace.
* Marker: canonical absolute state root, not the default root, disposable parent
  without `.git`, workspace sibling under the same parent.
* Grant attestation `TEST_FIXTURE_V1` + fixture id; `resolve_grant` refuses it
  outside a matching marker.
* No production state root, no Owner approval automation, no MCP tool change.

## 4. Model budget actually used

| Run | Calls | Model |
| --- | --- | --- |
| Scenario A | 2 | `gpt-6.1-terra` |
| Scenario B | 3 (first attempt) + 3 (accepted) | `gpt-6.1-terra` |
| Scenario C | 5 (run 1) + 8 (run 2, rejected replans) | `gpt-6.1-terra` |

No additional Codex subagent review was launched in H5.2. The H5.2 final review
will be a separate read-only task when the implementation is frozen.

## 5. Local suite (Windows x64, rustc 1.96.0)

| Run | Result |
| --- | --- |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | clean |
| `cargo test --locked` | **1688 passed, 0 failed, 2 ignored** |
| `cargo test --locked --features test-grant-fixture` | **1691 passed, 0 failed, 2 ignored** |
| `a_real_subprocess_lock_holder_blocks_quiescence_until_it_dies` | PASS |

## 6. H5.2 gate

| Gate | Result |
| --- | --- |
| Scenario A real model | PASS |
| Scenario B real model | PASS |
| Scenario C real model | PARTIAL |
| Scenario D real process | PARTIAL |
| Critical / High | 0 known |
| Required CI | pending H5.2 PR |
| Independent review of H5.2 HEAD | not yet |

**H5.2 = PARTIAL.** Do not claim READY_FOR_OWNER_REVIEW for H5.2 until Scenario C
completes with a real model and the full host-kill publication crash is proven.

## 7. Next work

1. Push the H5.2 branch and open a Draft PR based on the H5.1 branch.
2. Make Scenario C deterministic: either (a) improve the Replanner prompt to
   include the mandatory `add_dependencies` prerequisite, or (b) accept the
   host-path PASS and record the model-quality PARTIAL. Do not weaken the
   Replanner validator.
3. Add a test-only detached-helper / supervisor harness for the full host-kill
   publication crash (H5.2-D).
4. Run the H5.2 independent review on the frozen HEAD.
5. Only then reassess H5.2 PASS.

## 8. Owner decisions / not claimed

* C3 / R-1 is unresolved.
* H51-05 remains an informational coverage gap; the subprocess lock-holder
  closes the in-process-only part.
* Scenario C is PARTIAL because the real model did not complete the Goal; the
  host NeedsReplan/Replanner path itself was applied.
* No main merge, Draft clearance, Release, Tag, or S force-push.
