# H4 campaign checkpoint

Campaign: **GoalLatch H4 — External Backend V1 Readiness Closure**
Checkpoint file: `docs/campaign/H4_CAMPAIGN_CHECKPOINT.md`
Updated: 2026-11-09 (session 1)

This file is the resume point for the long-running campaign. It records what was
*observed*, not what was claimed. On resume, re-derive every row from GitHub and
the working tree before trusting it.

---

## 0. Baseline captured at start (verified, not assumed)

| Item | Observed value | How |
| --- | --- | --- |
| `origin/main` | `e6950fdc3bab72982285888a81cce822743fbba5` | `git rev-parse origin/main` after `git fetch` |
| PR #50 head | `072fa1564072b86364330cd46560141118d210d8` | `gh pr view 50` |
| PR #50 base | `327b461be8e3bd0c649adffeab3547bfa352102c` (PR #49 head) | `gh pr view 50` |
| PR #50 state | OPEN / Draft / `MERGEABLE` / `CLEAN` | `gh pr view 50` |
| PR #50 CI | 22/22 `SUCCESS` (quality, test ×4 OS, compat ×7) | `gh pr view 50 --json statusCheckRollup` |
| Worktree for PR #50 | `.local/gl-auth-a0` at `072fa15`, branch `feat/goal-write-grant-d`, clean | `git worktree list` |
| Free space on C: | 54 GB | `df -h /c` |
| Toolchain | cargo/rustc 1.96.0 | `cargo --version` |

No H4 work existed before this campaign: the newest PR was #50 and no branch
matched `h4`.

Protected assets confirmed present and **not modified**:
`.local/gl-e5-hardening`, `.local/gl-e5-h3-reliability`, `.local/gl-auth-a0`,
`.local/gl-external-e5`, `.local/gl-external-e4-h1`, `.local/gl-external-e3`,
`.local/gl-campaign-20261106`, `tmp/longrun-v2-resume-20261016`, and the primary
checkout.

State-root fact established for H4-D (code evidence, `src/config.rs`):
before this campaign there was **no** way to redirect the durable state root, so
a test could only be isolated by Session ID. `LOCAL_MCP_STATE_DIR` (host-owned,
absolute-path-only) is added in H4-A to make real out-of-process MCP testing and
real-model dogfooding isolatable.

---

## 1. Phase status

| Phase | Status | Evidence |
| --- | --- | --- |
| H4-0 Baseline / Risk Audit | DONE | §0 above, `docs/campaign/H4_RISK_REGISTER.md` |
| H4-A MCP Contract Correctness | DONE | PR #51; `tests/mcp_contract.rs` drives the real binary |
| H4-B Authorization final audit | DONE (findings closed) | independent review of `072fa15`: 0 Critical / 0 High; 1 MEDIUM + 1 LOW fixed with tests, negative control run |
| H4-C Publication / revocation reliability | DONE | deadline in `sandbox.rs` + `at_gw_031..033`; residual orphan-descendant note |
| H4-D Real-model dogfooding | PARTIAL | Scenario A executed with a real model end to end; B/C/D `NOT_VERIFIED` (no non-interactive grant path) |
| H4-E Integration readiness | DONE | `docs/campaign/H4_E_INTEGRATION_READINESS.md` |
| C3-0 Windows trust boundary | DESIGN COMPLETE | `docs/security/C3_0_WINDOWS_TRUST_BOUNDARY.md` |
| S0-0 Serea contract assessment | DESIGN COMPLETE | `docs/integration/SEREA_S0_0_CONTRACT_ASSESSMENT.md` |

## 2. H4-A findings so far

* `goal_start.inputSchema` omitted `write_scope` while `GoalStartRequest` parses
  it (`src/goal_api.rs:260`). With `additionalProperties:false` this made the
  field unreachable for any schema-validating MCP client. Fixed by publishing the
  schema that mirrors `RequestedWriteScopeV1` exactly.
* `goal_resume.inputSchema` already publishes `failed_task_replan_requests` via
  the post-processing loop (`src/mcp.rs`), so it was **not** a second gap.
* No other published tool currently declares a field the parser refuses, nor
  accepts a field the schema hides — asserted by the new real-process contract
  test rather than by inspection.

## 3. Open Critical / High

* C3 / R-1 (same-OS-user Owner impersonation) — **open by design**. Not a code
  defect that H4 can close; see the C3-0 document.
* Independent review of PR #50 head `072fa15` by Codex `gpt-6.1-sol` (xhigh):
  **0 Critical, 0 High**. One MEDIUM (stale-generation usage-floor reset) and one
  LOW (`REVOKE_REQUESTED` projected as `REVOKED`) — both confirmed against the
  source, both fixed in this PR with regression tests, and the MEDIUM fix has a
  negative control that fails without it.
* The H4 diff itself (#51) has **not** been independently reviewed. That is
  recorded as a remaining gate, not as a pass.

## 4. H4-D result (see `docs/campaign/H4_D_REAL_MODEL_EVIDENCE.md`)

Scenario A executed for real: isolated state root, real server process, real
model output, Goal `f0c44a92-…` reached `COMPLETED` (revision 11) with final
verification `PASSED`, after one genuine `READONLY_OUTPUT_INVALID` rejection of a
model answer. Replay of an accepted submission returns
`REPLAY_OF_ACCEPTED_SUBMISSION` across processes.

Scenario B (Writer/Reviewer), C (Replanner) and the crash-injection half of D are
`NOT_VERIFIED`: a `GoalWriteGrantV1` can only be issued from the interactive
Owner Control Surface, and no non-interactive grant path exists in the shipped
binary. The test-only grant harness is not reachable from an out-of-process run,
and hand-writing a ledger record would bypass the boundary H4-B just reviewed.

### Original blocker note

Real-model dogfooding needs (a) an isolated state root, (b) an isolated
workspace, (c) a real model to answer the ModelWorkItem, and (d) an Owner
approval harness for the Writer grant. (a) and (b) are now available; (c)
requires a model call budget the campaign may or may not have; (d) exists in
test code but is not packaged as a runnable harness. Whatever is not executed is
reported as `NOT_VERIFIED` — never as a PASS and never replaced by a fixture
result presented as a real-model run.

## 5. Next actions on resume

1. Re-read PR #50 and #51+# heads from GitHub; compare with this file.
2. Re-run `cargo fmt --all -- --check`, `cargo check --locked --all-targets`,
   `cargo clippy --locked --all-targets --all-features -- -D warnings`,
   `cargo test --locked` in the H4 worktree.
3. Continue from the first IN PROGRESS phase whose gate is not yet green.
