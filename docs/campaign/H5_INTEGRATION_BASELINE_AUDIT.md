# Integration baseline audit (方式C, phase II)

Audience: the Owner, and the reviewer of the integration Draft PR.
Every fact below was produced with `git`/`gh` against this repository on
2026-11-14. Commands are given so the numbers can be re-derived.

| Item | Value |
| --- | --- |
| `main` | `e6950fdc3bab72982285888a81cce822743fbba5` |
| Integration head | see the PR: `integration/external-agent-backend-v1-owner-review` |
| Reviewed code head | `cf66941` (round 13: `VERDICT READY`, no findings) |
| Ancestry | `git merge-base --is-ancestor origin/main <head>` → **true**, merge-base = `main` |
| Distance | `git rev-list --left-right --count origin/main...<head>` → `0 90` |
| Diff | 90 commits, 124 files, `+50348 / −268` (63 source files `+34580/−268`, 42 docs `+8691`) |
| Merge commits | 5, all in `integration/external-agent-v1-e3-base` (merging #30–#34) |

---

## 1. Commit-graph verification

| Check | Command | Result |
| --- | --- | --- |
| `main` is an ancestor | `git merge-base --is-ancestor origin/main HEAD` | **true** |
| Nothing is behind | `git rev-list --left-right --count origin/main...HEAD` | `0 90` (0 behind) |
| Every stacked tip is contained | `git merge-base --is-ancestor origin/<tip> HEAD` for all 18 tips | **true for all 18** |
| No duplicated commit subject in the range | `git log --format='%s' origin/main..HEAD \| sort \| uniq -d` | empty |
| Merge commits are confined to the E3 base | `git log --merges origin/main..HEAD` | 5, all `*into integration/external-agent-v1-e3-base` |
| No history was rewritten | `git reflog` of the stack branches is untouched; no `--force` push in this campaign | the stack branches still point at `b85355b`, `31540b7`, … as before |
| No foreign branch content | every changed path is under `src/`, `docs/`, `scripts/`, `tests/`, `.github/`, or is `Cargo.toml`/`Cargo.lock`/`.gitattributes`/`.gitignore`/`CHANGELOG.md` | 124 paths, all accounted for |

The 18 tips verified as contained:

```
5df8a29 feat/external-e3-hardening-v1        0a4cdb5 feat/external-goal-mcp-api-v1
86fa92a feat/external-recovery-compat-v1     afbce72 feat/external-e4a-planner-v1
e018860 feat/external-e4b-readonly-v1        36a45fb feat/e5-b2-reviewer-recovery
844dab2 feat/e5-c-replanner                  08f3edf feat/e5-h3-reliability-closure
3320dcd feat/goal-write-grant-a0             327b461 feat/goal-write-grant-a1
072fa15 feat/goal-write-grant-d              e633d9d feat/h4-a-mcp-contract-v1
90b28e0 feat/h5-a-security-remediation-v1    0d9ea59 feat/h5-1-ci-artifact-recovery-v1
b2f3267 feat/h5-1-publication-quarantine-v1  31540b7 feat/h5-1-state-consistency-hygiene-v1
0dcab4a feat/h5-2-real-model-validation-v1   b85355b feat/h5-3-external-backend-closure-v1
```

**Conclusion.** The integration head is the audited stack head plus the H5.3.1
commits; `main` fast-forwards into it, and nothing outside the stack's own
branches was pulled in.

## 2. PR #30–#34, audited individually

Owner's approval of 方式C was an approval of the *method*, not a blanket safety
certification of these five changes. Each is audited below against the final
implementation.

### #30 — architecture proposal (`ee231d1`, docs only)

| Item | Finding |
| --- | --- |
| Content | `docs/EXTERNAL_AGENT_BACKEND_V1_ARCHITECTURE.md`; the numbering the later ADRs cite |
| Runtime risk | none (documentation) |
| Consistency with the final code | **drift, documentation-only.** The document describes the backend as a design with the Writer "not yet implemented"; the merged stack now implements Writer issuance, staging, helper publication, quarantine and recovery. `docs/campaign/H5_INTEGRATION_REVIEW_INDEX.md` is the current map. The architecture document is kept as the design record, not as a description of the current build |
| Disposition | keep |

### #31 — test plan, threat model, API schema draft (`6fcaa65`, docs only)

| Item | Finding |
| --- | --- |
| Content | `EXTERNAL_AGENT_BACKEND_V1_TEST_PLAN.md`, `…_THREAT_MODEL.md`, `…_API_SCHEMA_DRAFT.md` |
| Consistency | **the threat model is still the one the code is written against** (its TM-05 / TM-10 / B-07 / B-08 items are the ones H4/H5 review rounds cite). The API schema **draft** was superseded: the published contract is now generated and pinned (`docs/schemas/`, plus the drift test). The draft is not imported by any code path |
| Drift | the draft schema is not the published schema; a reader must use `docs/schemas/*` and `tests/mcp_contract.rs` |
| Disposition | keep, with the review index pointing at the published schema |

### #32 — Windows authority audit + fail-closed approval regressions (`8c1aa3c`)

This is the only production-code change among #30–#34, and the brief asks for it
specifically.

| Item | Finding |
| --- | --- |
| Change | `src/approvals.rs` (+79), `src/main.rs`, `src/windows_authority_audit_tests.rs` (+195), `docs/WINDOWS_AUTHORITY_AUDIT_G0A.md` |
| Fail-closed behaviour | `request_grant_from_owner` returns `OwnerGrantOutcome::Unavailable` on any transport failure (no socket path, connect failure, serialise failure, write timeout, shutdown failure, read timeout/EOF). `approvals::request` returns `Err` for an invalid reply and `Ok(false)` for an explicit deny; every caller refuses the operation in both cases. Audited in the merged code, not only in the PR description |
| Windows authority decision | `primary_execution_mode()` is `HostNative` on Windows and `primary_execution_requires_approval(HostNative) == true`, so the approval happens before any spawn; on non-Windows the sandbox substitutes. There is no host-native mutation path that skips `approvals::request` — the call-site inventory in `docs/WINDOWS_AUTHORITY_AUDIT_G0A.md` §2.1 matches the merged tree |
| System privilege handling | the approval pipe is created with an **explicit current-user-only DACL** (`src/pipe_security.rs`: `O:<sid>G:<sid>D:P(A;;FA;;;<sid>)`), deliberately omitting `SYSTEM`/`Administrators`; Tokio's default (broad DACL) is explicitly rejected. A null SECURITY_ATTRIBUTES is never passed |
| CODEX vs EXTERNAL difference | CODEX goals use the same approval gate and are refused a writer grant (`resolve_grant` refuses a CODEX goal, `WRITE_AUTHORIZATION_UNAVAILABLE`); EXTERNAL goals additionally need a `GoalWriteGrantV1`. **No contradiction with Authorization B / C3**: the approval gate answers "may this host-native action run at all", the grant answers "may this Writer publish into this scope, for how much, until when". They compose rather than overlap |
| Drift | the audit document predates Authorization B and does not mention grants. That is a documentation gap, closed here rather than by editing an audit record that must stay as it was written |
| Disposition | keep |

### #33 — Goal orchestration regressions + additive `result_kind` (`ec057a7`)

| Item | Finding |
| --- | --- |
| Change | `src/goal_api.rs`, `src/goal_orchestration_regression_tests.rs`, `src/main.rs`, docs |
| Consistency | the additive `result_kind` is used by the current projections and by the external tests; `CriterionId` and `READONLY_BLOCKED` behaviour are pinned by the tests that ship |
| Disposition | keep |

### #34 — E2 model accounting (`3e03ac0`, `ea8f166`, `f48ca15`)

| Item | Finding |
| --- | --- |
| Change | `src/model_accounting.rs`, `src/agent.rs`, `src/main.rs` |
| Why it matters now | the external backend tests assert **zero Codex spawns for EXTERNAL goals**; the counters are what makes that assertion checkable rather than claimed |
| Consistency | the per-test-thread registry mirror (`f48ca15`) keeps parallel tests deterministic; the counters are read by the external tests in the merged tree |
| Disposition | keep |

## 3. Baseline facts the audit depends on

| Fact | Value |
| --- | --- |
| `cargo test --locked` on the integration head (Windows host) | 1753 passed, 0 failed, 9 ignored |
| `cargo test --locked --features test-grant-fixture` | 1700 passed, 0 failed, 2 ignored (lib) + integration binaries |
| `cargo fmt/check/clippy` | clean |
| Python fixture safety suite | 43 tests, 2 POSIX-only skips on Windows |
| Real-model runs | H5.2 Scenario A/B/C, H5.3 C1/C2, D1/D2 — see `H5_2_REAL_MODEL_EVIDENCE.md`, `H5_3_EVIDENCE.md` |
| C3 / R-1 | **unresolved**, unchanged by this integration |

## 4. Audit findings summary

| Severity | Count | Item |
| --- | --- | --- |
| Critical | 0 | — |
| High | 0 | — |
| Medium | 0 | — |
| Low (documentation drift) | 2 | #30's architecture text still reads as a proposal; #32's G0-A audit predates Authorization B and does not mention grants |
| Structural note | 1 | five PRs (#30–#34) reach `main` only as part of this integration, because #35's base already contained them (H5-E §1). Not a defect; a fact the Owner should see when choosing how to tidy the old PRs |

No code change was made for the two low findings: #30 and #32 are dated records of
what was true when they were reviewed. Editing them would destroy the audit trail;
the review index is where the current truth is stated.
