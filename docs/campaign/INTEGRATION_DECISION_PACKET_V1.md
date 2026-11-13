# Integration Decision Packet — External Agent Backend V1

Audience: the repository Owner. This is the decision document for the integration
Draft PR. It is deliberately short on prose and long on facts that can be
re-derived with `git`/`gh`/`cargo`.

**It does not approve anything.** Merge, Draft clearance, Release, Tag, Beta GO
and any operational use of an unrestricted EXTERNAL Writer remain the Owner's
decisions (section E).

---

## A. What is being integrated

| Item | Value |
| --- | --- |
| `main` | `e6950fdc3bab72982285888a81cce822743fbba5` (2026-11-06) |
| Integration head | `integration/external-agent-backend-v1-owner-review`, PR **#58** (Draft) |
| Reviewed code head | `cf66941` — independent review round 13: `VERDICT READY`, **no findings** |
| Commits after the reviewed head | documentation only; `git diff --stat cf66941..head` lists `docs/` only |
| Ancestry | `main` is an ancestor; `rev-list --left-right --count` = `0 90` |
| Size | 90 commits, 124 files, `+50348 / −268`; 63 source files (`+34580/−268`), 42 docs (`+8691`) |
| Merge commits | 5, all inside `integration/external-agent-v1-e3-base` |
| Stacked PRs included | #30–#57 (30 open drafts), all still open, none modified by this campaign |

### A.1 Diff classification

| Area | Files | What it is |
| --- | --- | --- |
| Authorization | 6 | `GoalWriteGrantV1` model, store/CAS, IPC gate, tests |
| External contract | 9 | `goal_next`/`goal_submit`/`goal_status`/`goal_result`/`goal_resume`, response contract, published schemas |
| Writer / publication | 11 | staging, helper frame, `atomic-publish`, mutation journal, postimage |
| Recovery / quarantine | 6 | publication lock, durable quarantine, crash-recovery tests |
| Planner / Readonly / Reviewer / Replanner | 9 | role implementations and their contract tests |
| Managed workspace | 8 | create/prepare/observe/writer paths, snapshots, evidence binding |
| Verification / finalizer | 5 | host verification, Git observation, finalization |
| Windows authority / IPC | 3 | approvals, pipe security, the G0-A regression suite |
| Model accounting | 2 | spawn/role/outcome counters (the zero-Codex-spawn assertion) |
| Evidence harness (test-only) | 9 | `scripts/h5-d/`, never shipped |
| Docs / schemas / CI | 55 | design records, campaign evidence, published schemas, LF policy |

`docs/campaign/H5_INTEGRATION_REVIEW_INDEX.md` maps each area to its entry points,
invariants and tests.

## B. Security

### B.1 Open code defects found by review

| Class | Count | Note |
| --- | --- | --- |
| Critical | 0 | across 13 rounds of H5.3.1 review and the whole-diff review (§B.4) |
| High | 0 | rounds 6–11 found 12 High findings; all were remediated and re-reviewed, none waived |
| Dependency of this answer | the fixture harness findings were about a **test-only** harness; the product's own review rounds were H5-A/H5.1/H5.3 |

### B.2 C3 / R-1 — the unresolved deployment constraint

| Item | State |
| --- | --- |
| What it is | the Owner Control Surface, the MCP server and any agent process run as **the same Windows user**; a same-user process (and any administrator) can act with the Owner's authority |
| Visible to the Owner | yes — `AuthorizationView.same_uid_process_resistance: false` is reported to the caller, not hidden |
| Does this integration fix it | **No** |
| What would | `docs/security/C3_0_WINDOWS_TRUST_BOUNDARY.md` recommends option A (a Broker as a separate local principal) + D (Windows Hello as the local consent factor) + E (phone-signed challenge) later |
| Owner decision needed | whether EXTERNAL Writer is allowed in production before C3 exists (section E) |

### B.3 What the authority chain does guarantee

| Property | Where | Evidence |
| --- | --- | --- |
| An EXTERNAL goal cannot obtain a grant through MCP | `src/mcp.rs` publishes no grant-issuing tool; `resolve_grant` is the only gate | `src/authorization_tests.rs`, `docs/EXTERNAL_AGENT_BACKEND_V1_AUTHORIZATION_B.md` |
| A grant is bound to scope, budget, expiry, writer identity and generation | `src/authorization.rs` | the same suite, plus the eight remediation rounds |
| Revocation fences later publication | `src/authorization_store.rs`, `src/external_publish.rs` | revocation tests, H5.1-B/c |
| Only the helper publishes; the Writer stages | `src/external_writer.rs`, `src/bin/atomic-publish.rs` | writer failure/race matrices |
| A crashed publication is quarantined, not retried | `src/publication_quarantine.rs` | `publication_lock_subprocess_tests.rs`, `external_h3_reliability_tests.rs` |
| A CODEX goal can never receive an EXTERNAL grant | `src/authorization.rs` | `resolve_grant` refusal test |
| Host-native actions fail closed without an approval channel | `src/approvals.rs` | `windows_authority_audit_tests.rs` |
| A `TEST_FIXTURE_V1` grant is inert outside a marked disposable root | `src/config.rs`, `src/test_grant_fixture.rs` | `test_grant_fixture_tests.rs`, feature-gated |

### B.4 Default behaviour from a normal install

This was re-checked because a README warning is not a control.

| Question | Answer in the code |
| --- | --- |
| Does an EXTERNAL Writer start without a grant? | No. It receives `WRITE_AUTHORIZATION_UNAVAILABLE`; the H5.2 Scenario B run recorded exactly that |
| Can a model mint a grant? | No tool exists; issuance is an Owner-side action over the session IPC |
| Is the test-grant path compiled into a normal build? | No — `test-grant-fixture` is not a default feature, and even a feature-enabled build refuses an unmarked state root |
| Can a fixture grant be moved into a real state root? | No — `resolve_grant` requires the fixture id to match a marker in the running state root |
| Does an unapproved host-native action run? | No — approval is required before spawn on Windows and fails closed without a channel |
| What can a same-user process still do? | Everything the Owner can, including approving its own request. **That is C3/R-1** |

### B.5 Platform guarantee range (stated, not implied)

| Claim | Windows | Linux | macOS |
| --- | --- | --- | --- |
| Rust suite green | yes (local + CI) | CI | CI |
| Real-model Scenario A/B/C | yes (H5.2/H5.3) | not run | not run |
| Crash injection D1/D2 with real processes | yes | not run | not run |
| Managed-workspace production driver acceptance | unit/contract only | executed (Unix path) | not run |
| Descendant containment after a POSIX crash | — | **NOT_VERIFIED** | **NOT_VERIFIED** |
| Publication lock flake | — | — | known intermittent; mitigated by re-running CI, **root cause not fixed** |

Read-only evidence range: H5.3 observed that `codex exec --sandbox read-only`
cannot read the workspace on Windows, so the harness ran with `workspace-write`
and compared workspace digests before and after each call. **A matching digest
shows the files ended where they started; it is not proof that nothing was
written, and it is never presented as OS-level read-only isolation.**

## C. Quality

| Check | Result on the integration head |
| --- | --- |
| `cargo fmt --all -- --check` | clean |
| `cargo check --locked --all-targets` | clean |
| `cargo clippy --locked --all-targets --all-features -- -D warnings` | clean |
| `cargo test --locked` | 1753 passed, 0 failed, 9 ignored |
| `cargo test --locked --features test-grant-fixture` | 1700 passed, 0 failed, 2 ignored (lib) + integration binaries |
| `python scripts/h5-d/test_fixture_safety.py` | 43 tests, 0 failed (2 POSIX-only skips on Windows) |
| Release/workflow/install-contract self-tests | CI `quality` job |
| Required CI on PR #58 | see the PR checks; any failure is recorded in `docs/campaign/H5_3_EVIDENCE.md` §11 rather than described as green |
| Independent review | rounds 1–13 on H5.3/H5.3.1 (13 rounds, 0 Critical/High unresolved) + the whole-diff review recorded in §B.4/§11 |
| Real-model E2E | Scenario A (readonly DAG), B (no grant → refused), C (oversize refused) |
| Crash injection | D1 host kill / Job Object, D2 surviving lock holder |
| CODEX compatibility | the CODEX paths are unchanged in behaviour; the external backend is additive, and CODEX goals are refused writer grants |

## D. Not finished before a release

| # | Item | Why it blocks a *release* (not this Draft) |
| --- | --- | --- |
| D1 | **C3 Broker** (separate OS principal, Hello-signed consent) | Without it, the Owner's approval is only as strong as the user account |
| D2 | **OS permission separation** for the ledger and the helper | An agent that can run as the user can read the ledger |
| D3 | **POSIX descendant containment** after a crash | `NOT_VERIFIED`; a detached descendant could outlive the lock witness |
| D4 | **R2-11** boundary probes (grant TTL, quarantine schema, durable-JSON corruption) | Partial coverage; recorded as a coverage gap, not a defect |
| D5 | **F5** non-WorkItem schema drift | **Closed in H5.3** (every published tool asserts `additionalProperties: false` and the runtime refuses an undeclared field) |
| D6 | **Serea adapter contract** | Not in this stack; the wire contract stays inside `host.goal.*` approval |
| D7 | macOS publication-lock flake | Root cause unknown; re-running CI hides it |
| D8 | Linux/macOS real-process crash evidence | Not run; cannot be claimed from CI alone |

## E. Decisions requested from the Owner

| # | Decision | Options |
| --- | --- | --- |
| E1 | Is this integration Draft acceptable as the review unit? | accept as the review unit / ask for a different grouping |
| E2 | Merge to `main`? | merge now / merge after specific fixes / do not merge yet |
| E3 | C3 priority | C3 before any EXTERNAL Writer use in production / C3 next quarter / accept the same-user risk for personal use only |
| E4 | Writer capability in public vs private builds | full EXTERNAL Writer in both / readonly-only in public / Writer behind an explicit opt-in flag |
| E5 | Release / Beta GO | no release yet / release as readonly-only / beta with the Writer disabled by default |
| E6 | The 30 open draft PRs | leave them as the audit trail (proposal) / close them after this integration merges / keep them open until the C3 work lands |

**Proposal only, not executed:** keep #30–#57 open as the audit trail until this
integration is merged, then close them with a pointer to the integration commit.
No PR was closed, un-drafted, re-based or re-targeted in this campaign.

## F. How to verify this packet

```
gh pr view 58 --json number,isDraft,baseRefName,headRefName,mergeable
git rev-list --left-right --count origin/main origin/integration/external-agent-backend-v1-owner-review
git diff --shortstat origin/main origin/integration/external-agent-backend-v1-owner-review
gh pr checks 58
cargo fmt --all -- --check && cargo check --locked --all-targets
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
cargo test --locked --features test-grant-fixture
python scripts/h5-d/test_fixture_safety.py
```

Every claim in section B that is not a command above is a reference to a test or a
document, and those references are given inline.
