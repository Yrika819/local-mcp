# H5-E — PR stack integration decision packet

Audience: the repository Owner. Every number here was produced by `git` and `gh`
against `origin/main = e6950fdc3bab72982285888a81cce822743fbba5` at the time of
writing.

---

## 1. What the stack actually is

The 21 open drafts are **one linear chain**, not 21 independent changes:

```
main (e6950fd)
 └─ integration/external-agent-v1-e3-base (9b8f6c1)
     = main + #30 + #31 + #32 + #33 + #34 merged in (5 merge commits)
      └─ #35  feat/external-model-work-item-v1        8084dc0b
          └─ #36  feat/external-goal-mcp-api-v1       0a4cdb5c
              └─ #37 … #51  (feat/h4-a-mcp-contract-v1, HEAD e633d9d)
```

Verified facts:

| Fact | Command | Result |
| --- | --- | --- |
| PR #51 contains the E3 integration base | `git merge-base --is-ancestor origin/integration/external-agent-v1-e3-base origin/feat/h4-a-mcp-contract-v1` | **true** |
| The merge-base with `main` is `main` itself | `git merge-base origin/main origin/feat/h4-a-mcp-contract-v1` | `e6950fd…` (fast-forwardable) |
| Commits above `main` on the stack HEAD | `git rev-list --count origin/main..origin/feat/h4-a-mcp-contract-v1` | **60** |
| Files changed by the stack | `git diff --stat origin/main origin/feat/h4-a-mcp-contract-v1` | **74 files, +33803 / −224** |
| PR #35's base already contains #30–#34 | `git log --oneline e6950fd..9b8f6c1` | 5 merge commits + 8 content commits |

**Consequence.** There is no way to land PR #35 (or #51) alone: landing either
lands #30, #31, #32, #33 and #34 with it. This is the structural problem H5-E had
to make decidable.

## 2. What #30–#34 actually contain

| PR | Content | Files | Delivered value | Duplicate of `main`? | Disposition |
| --- | --- | --- | --- | --- | --- |
| #30 | Architecture proposal for the External Agent Backend V1 (+519) | `docs/EXTERNAL_AGENT_BACKEND_V1_ARCHITECTURE.md` | Design record the ADRs are numbered against | No (new doc) | **Keep** — documentation, zero runtime risk |
| #31 | Test plan, threat model, API schema draft (+616) | 3 docs | The threat model the H4/H5 reviews cite (TM-05, TM-10, B-07/B-08) | No | **Keep** — documentation |
| #32 | Windows authority audit + fail-closed approval regression tests (+476) | `docs/WINDOWS_AUTHORITY_AUDIT_G0A.md`, `src/approvals.rs`, `src/main.rs`, `src/windows_authority_audit_tests.rs` | Real behaviour change in `approvals.rs` + 195 lines of regression tests | No | **Keep, but review the `src/approvals.rs` hunk separately** — it is the only production-code change among #30–#34 and later PRs (#48–#51) also modify `approvals.rs` |
| #33 | G0-B Goal orchestration regressions + additive `result_kind` (+862) | `src/goal_api.rs`, `src/goal_orchestration_regression_tests.rs`, docs, `src/main.rs` | Additive `result_kind` in the Goal API projection + 720 lines of regression tests | No | **Keep** — additive field, and H4/H5 depend on `result_kind` |
| #34 | E2 model accounting foundation (codex spawn / role calls / outcome metrics) (+507) | `src/model_accounting.rs`, `src/agent.rs`, `src/main.rs` | The zero-Codex-spawn assertion the external backend tests rely on | No | **Keep** — required by every later external test |

Nothing in #30–#34 duplicates `main`: `main` at `e6950fd` contains none of those
files (`git ls-tree -r --name-only e6950fd | grep -iE 'account|threat'` returns
only `tmp/` review reports). So the 12-commit pre-advance of #35's base is
necessary content, not accidental drift — but it is 5 PRs the Owner never merged
individually, which is why #35 should not be merged as if it were self-contained.

## 3. Options compared

| | A. Merge #31–#34 individually, then the rest in order | B. Reconstruct only the needed diff on a new branch | C. One new integration Draft PR from `main`, keep the old stack as audit evidence |
| --- | --- | --- | --- |
| Merges required | 21 | 1 | 1 |
| CI runs required | 21 × 22 checks ≈ 462 | 1 × 22 | 1 × 22 |
| History | Preserved per PR | Flattened/cherry-picked, provenance lost | Preserved on the audited branches; one merge commit on `main` |
| Reviewability | Highest per change, lowest per result | Poor — one unreviewable diff, and re-review of already-reviewed commits is wasted | The 21 PRs *are* the review segments; the integration PR is the final acceptance |
| Branch protection | 21 approvals/merge events | 1 | 1 |
| Risk of losing a fix | Low | **High** (cherry-pick of a 60-commit chain with conflicts in `main.rs`, `mcp.rs`, `approvals.rs`) | Low |
| Owner effort | 21 decisions | 1 decision, no re-review trail | 1 decision + a documented "already reviewed" index |
| Recommended | **No** — the ordering forces re-CI 21 times and #51 is the only HEAD that matters | **No** — the chain is linear and conflict-free with `main`, so rewriting it adds risk without reducing it | **Yes** |

**Recommendation: option C.** The chain is fast-forwardable onto `main`
(merge-base == `main`), every commit is already individually reviewed and
green, and the audited artifacts (the 21 branches) stay exactly as they are.
H5 also adds one Draft PR on top of the H5 branch, which must be merged with the
stack, not before it.

## 4. Authorization: why #49/#50/#51 are not separable

`#48` (design) → `#49` (`GoalWriteGrantV1`, ledger, OCS wiring) → `#50`
(managed worktree + host evidence + finalizer) → `#51` (MCP `write_scope`
contract, state-root isolation, publication deadline) plus the H5 commit.

Reasons they cannot ship separately, re-derived from the code:

1. **#49 without #50**: `GoalWriteGrantV1` is created and revoked, but there is no
   managed-workspace implementation or finalizer evidence path, so a grant could
   authorize an operation whose workspace identity does not exist. #50 introduces
   `GrantWorkspaceIdentity::from_goal` for the managed mode that #49's
   `resolve_grant` already refuses on mismatch.
2. **#50 without #49**: the managed worktree writer path calls
   `ensure_writer_publishable` and `usable_grant_for_operation`, which do not
   exist before #49. It would not compile.
3. **#51 without #49/#50**: `write_scope` publication is meaningless without a
   grant, and the publication deadline lives inside the external publication path
   that #50 wires. It also depends on #49's fence.
4. **All four without #48**: the Owner would be shipping an authorization system
   with no recorded threat model for the same-uid residual risk (C3/R-1), which is
   the one constraint the product must state honestly.
5. **H5 adds to #51**: `resolve_grant` gains the fixture-attestation gate and
   `revoke_active` gains the unresolved-publication barrier. Both are inside the
   #49/#51 authorization core, so the H5 commit is not separable either.

**Minimum safe integration unit:** `#30`–`#34` (integration base) + `#35`–`#51` +
the H5 commit, released as one integrated change. Nothing smaller is coherent.

## 5. Serea forward compatibility (re-evaluated, not implemented)

* Serea owns dialogue/task/policy/approval; GoalLatch owns PC execution,
  workspace, journal and host verification. Both approvals stay separate; a Goal
  ID ↔ Serea Task ID mapping must be explicit; an unknown side effect must not be
  retried by either side; real results must come from host evidence.
* The frozen Serea `host.goal.run` shape assumes a self-executing backend, which
  is the opposite of `goal_next` → model → `goal_submit` (design constraint DC-3).
  ADR-2's `host.goal.work` (a WorkItem-shaped exchange) remains the recommended
  adapter contract, because it preserves the host/agent split instead of hiding
  it. H5 does not change `goal_next`/`goal_submit`, so the mismatch is unchanged
  and the adapter work still needs its own Owner approval.
* H5-B materially lowers the cost of that adapter: the response contract is now
  machine readable per role, so a Serea-side adapter can validate model output
  without a Rust reader.

## 6. Owner decision required

1. Approve option **C** (one integration Draft PR built from the stack HEAD +
   the H5 commit, old stack retained as evidence), or name a different option.
2. Confirm the integration unit in §4 (nothing ships below it).
3. Decide whether #30–#34 are reviewed as part of the integration PR or want
   individual reads first — #32's `src/approvals.rs` hunk is the only production
   behaviour change in that group.
4. Confirm that C3/R-1 stays a documented deployment constraint (no unlimited
   Writer operation on a normal Windows profile) until an OS-level trust boundary
   exists.

---

# 7. H5.3 update (External Backend V1 closure)

Re-derived on the H5.3 branch `feat/h5-3-external-backend-closure-v1`
(base `0dcab4a` = PR #56 HEAD) after the H5.3 campaign. `main` is still
`e6950fdc3bab72982285888a81cce822743fbba5`.

## 7.1 Stack shape after H5.3

| Fact | Value |
| --- | --- |
| Open Draft PRs | **30** (#26–#56 plus #15, #25, #29) |
| PR #56 state | OPEN / Draft / MERGEABLE, CI 22/22 SUCCESS at `0dcab4a` |
| H5.3 branch commits above `main` | 76 (the same linear chain plus H5.3) |
| `main..H5.3` diff | **121 files, +47 568 / −268** |
| H5.3 diff (`0dcab4a..H5.3 HEAD`) | 21 files, +2 744 / −75 |
| H5.3 adds to the stack | one Draft PR based on `feat/h5-2-real-model-validation-v1` |

The H5.3 branch remains fast-forwardable onto `main` (`merge-base == main`), so
option C is unchanged and still the only recommended shape. **No main-based
integration PR was created and none should be created without the Owner's
explicit approval** — the campaign brief forbids it, and this packet only
prepares the decision.

## 7.2 What H5.3 changes about the integration decision

1. **The integration unit is unchanged** (#30–#34 base + #35–#56 + H5.3), but
   H5.3 adds no new authorization surface. It changes one path-resolution
   function, one error-code mapping, one published contract text, two test-only
   seams and the test harnesses.
2. **H5.3 closed contract/validator drift** in the Replanner's
   `resolve_needs_replan` requirement, and published the conditional
   readonly-reassessment barrier. A Serea-side adapter can now rely on the
   published conditions instead of reverse-engineering the validator.
3. **H5.3 removed a data-loss hazard in the test harness.** The real-model driver
   previously deleted an operator-supplied `--work-dir`; it now only creates.
   This matters for the Owner because *running the supplied evidence drivers* was
   itself hazardous before this change.
4. **Two production defects were found by running the stack for real**, and are
   fixed here: a `WRITE_UTF8` with `expected_preimage: ABSENT` could never
   publish (the published capability was unreachable), and a durable
   `PENDING -> READY` refusal was reported as `GOAL_PERSISTENCE_ERROR`.
5. **A platform constraint is now recorded, not hidden.** On Windows,
   `codex exec --sandbox read-only` makes the workspace unreadable to the model.
   Any real-model "read-only investigation" evidence produced with that sandbox
   is evidence about the model's reasoning, not about the workspace. H5.3's C2
   run uses `--sandbox workspace-write` and proves, per call, that the model
   wrote nothing.

## 7.3 Intermediate CI status the Owner should expect

The PRs below H5.3 keep whatever CI state they had when they were opened; they
are **not** re-run by pushing H5.3. Only two HEADs matter for acceptance:

| HEAD | Meaning | CI |
| --- | --- | --- |
| `0dcab4a` (PR #56) | H5.2 PARTIAL, the H5.3 base | 22/22 SUCCESS |
| H5.3 PR HEAD | the integrated technical result | see the H5.3 PR |

A red CI on an intermediate PR in the chain (for example #53, which was opened to
fix CRLF before the later renormalisation landed) is **not** evidence that the
integrated HEAD is red. The integration PR is the only place where a single CI
result describes the whole change.

## 7.4 Residual constraints carried into the Owner decision

| Constraint | Effect on integration |
| --- | --- |
| **C3 / R-1** same-OS-user impersonation | Unresolved. Landing the stack does not make an unrestricted EXTERNAL Writer safe against production data on a normal Windows account. Must ship as a documented deployment constraint. |
| POSIX detached descendants | `NOT_VERIFIED`. The Windows Job Object guarantee does not generalise to Linux/macOS process groups. |
| `codex --sandbox read-only` on Windows | Broken for reads (H53-4). Blocks real-model read-only dogfooding, not the product. |
| Core file tools publish no `additionalProperties` | Recorded residual (F5), pinned by a test so the set cannot grow silently. |
| R2-11 boundary probes | Partially covered; unchanged from H5.2. |

## 7.5 Owner decisions (unchanged, restated)

1. Approve option **C** and name the integration PR owner.
2. Confirm the integration unit (§4) — nothing ships below it.
3. Decide whether #30–#34 are reviewed inside the integration PR.
4. Confirm C3/R-1 ships as a documented deployment constraint.
5. Separately decide main merge, Draft clearance, Release/Tag and V1 Beta GO.
   None of these is requested or performed here.
