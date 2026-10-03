# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261001-a81f92c1`
- Review chain ID: `rc-20261001-a81f92c1`
- Review generation: `0`
- Review trigger: `initial`
- Parent review report ID: `None`
- Parent review report path: `None`
- Parent resolution ID: `None`
- Parent resolution path: `None`
- Generated at: `2026-10-01T02:28:27Z`
- Report path: `tmp/reviews/2026-10-01-code-review-report-winpaths-a81f.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:75d7769c6b882d4e6f2c5f29cb2649eca913e8a7f0d6c73e02ce3b187fd5a0c1`

Treat this completed report as the fixed review input for downstream work. Do not rewrite it during receiving or implementation; record dispositions, challenges, code changes, and verification in a separate `receiving-code-review` resolution report that references this Report ID.

## Scope

- Review date: `2026-10-01`
- Scope kind: `working tree`
- Scope description: `Read-only review of the uncommitted portability changes in src/config.rs, src/planner.rs, and src/verifier.rs: canonical spelling selection, Planner path normalization, Verifier execution/Git root handling, PRIMARY compatibility, symlink/reparse/UNC behavior, exact managed authority, and TaskScope path comparison. The supplied baseline was 8ae7c23b8575590c159063971c4c4f93fb3ca60c; actual repository HEAD was checked and is later.`
- Scope mode: `full frozen scope`
- Baseline: `HEAD c5eef763ddea2774729847a963289bc945621b66 (the supplied 8ae7c23b8575590c159063971c4c4f93fb3ca60c is an ancestor)`
- Target: `working tree, limited to src/config.rs, src/planner.rs, and src/verifier.rs`
- Changed paths: `3`
- Diff size: `20 additions / 17 deletions`
- Completion: `Complete within reviewed scope`
- Requirements consulted: `User-provided CI outcome for run 36802607148; docs/MANAGED_WORKTREES_V1_DESIGN.md §13 and managed-worktree authority invariants; code-level contracts in config, Planner, and Verifier.`
- Prior resolution consulted: `None`
- Assumptions: `The reported CI outcome is accepted as user-provided evidence and was not independently fetched. Windows behavior is reviewed statically on macOS.`
- Excluded as unrelated: `src/approvals.rs and pre-existing tmp/reviews artifacts; all Phase 5 work.`

## Review Orchestration

- Assessment subagent: `Coordinator assessment - the patch is a small, cohesive path-spelling change with coupled filesystem-security and TaskScope semantics; one reviewer can trace the full control/data flow without duplicative context.`
- Orchestration decision: `Single reviewer`
- Decision confidence: `high`
- Decision rationale: `All changed behavior shares one invariant: canonical paths must retain the intended root spelling while still referring to the same resolved path. The focused scope is three Rust modules; parallel reviewers would repeatedly need the same root and verifier path context.`
- Coordinator override: `None`
- Context or tool limits: `No Windows runtime was available locally. The supplied Windows CI result reports FileExists, FileDigest, CommandExit, and GitScope passing and TASK_SCOPE_GATE failing.`

### Risk Dimensions

- `Filesystem containment and authority: path spelling changes must not bypass canonical resolution, symlink/reparse handling, or exact managed-root authorization.`
- `Cross-platform compatibility: PRIMARY must retain its established Windows verbatim spelling, while managed roots and all compared paths must be consistently represented.`
- `TaskScope enforcement: changed Git paths must be compared to the durable compact managed allowed-path representation without false blocking or scope widening.`

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `Coordinator` | Path correctness, filesystem security, and behavior contract | `src/config.rs::canonical_path_like`; `src/planner.rs::normalize_planner_path`; `src/verifier.rs::prepare`, `observe_git`, `resolve_git_path`, `TASK_SCOPE_GATE` | PRIMARY spelling; managed exact authority; symlink/reparse and UNC behavior; changed-path comparison; Phase 5 exclusion | `Complete` |

### Synthesis Statement

The coordinator traced path spelling from validated execution-root selection through Planner TaskScope normalization and Verifier Git observation to the durable TaskScope gate. `canonical_path_like` preserves PRIMARY’s verbatim form and compact managed-drive spelling; canonicalization still resolves existing filesystem indirections, and `validated_execution_root` still checks Session coverage plus exact managed ownership. However, `resolve_git_path` uses a separate `fs::canonicalize`-based helper and therefore does not adopt the managed root spelling before the gate performs lexical containment. This leaves the reported Windows `TASK_SCOPE_GATE` failure in the reviewed path. No Phase 5 behavior is present or requested.

## Review Snapshot

- Recommendation: `Changes requested`
- Completion: `Complete within reviewed scope`
- Why now: `The changed-path normalization remains inconsistent with compact managed TaskScope paths, so valid Windows managed-worktree changes are still blocked at reconciliation.`
- Must-review now: `F1 Windows Git changed paths do not match managed TaskScope spelling`
  1. `F1` `Windows Git changed paths do not match managed TaskScope spelling`
- Findings count: `Blocker 0 | Major 1 | Minor 0 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 0`
- Coverage confidence: `high for static path-flow and authority analysis; medium for Windows runtime`
- Biggest blind spot: `No local Windows execution; the CI outcome is user-provided and was not independently queried.`

## Complete Findings Index

| ID | Severity | Surface | Review risk | Confidence | Origin | Verification | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `F1` | `Major` | `Verifier managed TaskScope Git reconciliation` | `Valid in-scope Windows changes are reported outside durable TaskScope and verification becomes BLOCKED.` | `high` | `Coordinator` | `Static trace of Planner normalization, Git-path canonicalization, and TASK_SCOPE_GATE; consistent with the user-reported CI result.` | `behavior; entry=managed-worktree task verification on Windows; contract=Git-observed changed paths within durable TaskScope are accepted; effect=TASK_SCOPE_GATE blocks valid in-scope changes` | `ifp-sha256:db9a1b92a67544fd18c2677af331554a9f4c1fafbb85a95c1d549cac362d5c16` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §13 requires TaskScope paths to be rooted at execution_root; Verifier TASK_SCOPE_GATE enforces durable TaskScope.` |

## Blocker

None.

## Major

### F1 Major - Windows Git changed paths do not match managed TaskScope spelling

Impact: `Managed Windows tasks with valid in-scope changes are blocked at verification despite passing their individual FileExists, FileDigest, CommandExit, and GitScope checks, as reported for CI run 36802607148.`

Review reason: `The portability change makes managed allowed paths and the Git root compact, but changed paths are still canonicalized independently to Windows fs::canonicalize spelling before lexical TaskScope comparison. This leaves the targeted approval/reconciliation workflow failing closed.`

Surface: `Verifier Git observation and durable TaskScope containment for managed execution.`

Issue key: `behavior; entry=managed-worktree task verification on Windows; contract=Git-observed changed paths within durable TaskScope are accepted; effect=TASK_SCOPE_GATE blocks valid in-scope changes`

Issue fingerprint: `ifp-sha256:db9a1b92a67544fd18c2677af331554a9f4c1fafbb85a95c1d549cac362d5c16`

Expected basis: `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §13 requires TaskScope paths to be rooted at execution_root; Verifier TASK_SCOPE_GATE enforces durable TaskScope.`

Confidence: `high`

Origin: `Coordinator`

Coordinator verification: `Independently traced Planner's canonical_path_like normalization to a compact managed path, then traced observe_git's root spelling and resolve_git_path -> canonicalize_existing_prefix -> fs::canonicalize to Git changed paths. in_allowed/in_boundaries compare those PathBufs directly; no spelling normalization is applied to changed paths.`

Look here first:
- [`resolve_git_path` and changed-path canonicalization](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L1189)
- [`TASK_SCOPE_GATE` direct allowed-path comparison](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L313)

Failure mode:
- Expected: `Git-observed changed paths under the validated managed execution root should compare within the compact, durable TaskScope paths produced by Planner.`
- Current: `Git path resolution canonicalizes each existing path with fs::canonicalize (via canonicalize_existing_prefix), whereas managed TaskScope normalization uses canonical_path_like and preserves compact drive spelling. The verifier then performs direct Path equality/starts_with containment; a verbatim-prefix mismatch causes the scope gate to fail and the task to become BLOCKED.`

Evidence:
- `src/planner.rs::normalize_planner_path` calls canonical_path_like for the execution root and existing path prefix; compact managed root spelling is retained in resolved TaskScope paths.
- `src/verifier.rs::observe_git` applies canonical_path_like to Git's top-level root, but parse_status_paths/parse_nul_paths call resolve_git_path, which calls canonicalize_existing_prefix; that helper uses fs::canonicalize directly for the existing prefix.
- `src/verifier.rs::evaluate` compares each Git changed path to durable allowed and forbidden paths with in_allowed/in_boundaries without converting spelling.
- `src/config.rs::canonical_path_like` handles spelling based on the validated reference root, but it is not used in Git changed-path resolution.
- `validated_execution_root` still verifies current Session authority, `same_path_identity` against the durable worktree root, and exact active Git reconciliation before returning the managed root. This finding is not an authority expansion.
- User reports CI run 36802607148 passes FileExists, FileDigest, CommandExit, and GitScope but returns BLOCKED at TASK_SCOPE_GATE due to verbatim Git paths versus compact TaskScope paths; this is consistent with the code trace and not independently fetched.

Assumptions and limits:
- `Windows fs::canonicalize` returns the extended `\\?\` spelling described in the config.rs canonical_path documentation and reported by the user for this CI run.
- The existing Git-observation path remains the relevant source of changed paths for this mutating TaskScope gate.

Reviewer action:
`request fix`

## Minor

None.

## Questions

None.

## Test Gaps

None as standalone items. The reported Windows CI failure is represented by F1; a passing Windows regression run should be required to verify the correction.

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | Canonical helper behavior, PRIMARY spelling, UNC handling | `src/config.rs::canonical_path_like`, `canonical_path`, `strip_verbatim_prefix` | `Coordinator` | `contract trace; diff review` | `Reviewed - no issue found` | `PRIMARY verbatim paths stay verbatim; compact drive stripping is limited to VerbatimDisk, so UNC spelling is not shortened. canonicalization remains in the helper.` | `Diff and config path helper lines 138-198.` |
| `A2` | Planner normalization and containment | `src/planner.rs::normalize_planner_path`, `canonicalize_existing_prefix` | `Coordinator` | `dependency trace` | `Reviewed - no issue found` | `Paths are canonicalized before starts_with containment and the root-relative suffix is appended only after resolving the deepest existing prefix. The spelling is chosen from the execution root.` | `src/planner.rs lines 776-858; helper still canonicalizes existing prefixes.` |
| `A3` | Verifier Git-root and changed-path comparison | `src/verifier.rs::observe_git`, `resolve_git_path`, `TASK_SCOPE_GATE` | `Coordinator` | `dependency trace; CI evidence supplied by user` | `Finding F1` | `Git root follows root spelling, but individual changed paths do not; direct comparison to TaskScope therefore fails for the reported managed Windows spelling mismatch.` | `src/verifier.rs lines 313-327 and 1103-1131, 1189-1197.` |
| `A4` | Exact managed authority and execution root | `src/managed_worktree_prepare.rs::validated_execution_root` (supporting dependency) | `Coordinator` | `contract trace` | `Reviewed - no issue found` | `Current Session path authority and exact durable root identity are checked; the changed spelling behavior does not bypass this gate.` | `src/managed_worktree_prepare.rs lines 173-229.` |
| `A5` | Windows runtime and regression evidence | `canonical_path_like`, Planner/Verifier Windows flow | `Coordinator` | `static trace; external runtime not independently executed` | `Reviewed - no issue found` | `User provided CI run 36802607148 as evidence of the failing gate. No local Windows runtime was available; repeat that platform run after correction.` | `The user-provided result is consistent with F1; a passing Windows run is the next verification.` |

## Subagent Candidate Adjudication

No subagents were available or used; the coordinator conducted the single-reviewer assessment and review. The finding above was directly verified against the relevant code path.

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `src/config.rs` | `surface` | Canonical path spelling, Windows verbatim prefix handling, PRIMARY compatibility, UNC behavior |
| `src/planner.rs` | `surface` | Path normalization, TaskScope allowed-path spelling, containment after canonicalization |
| `src/verifier.rs` | `surface` | Snapshot execution root, Git root normalization, changed-path resolution, TaskScope gate |
| `src/managed_worktree_prepare.rs` | `dependency` | Current authority and durable managed-root validation |
| `docs/MANAGED_WORKTREES_V1_DESIGN.md` | `dependency` | Managed execution-root and TaskScope contract |

### Verification Commands

- `git --no-optional-locks status --short && git rev-parse HEAD && git diff --stat HEAD && git diff -- src/config.rs src/planner.rs src/verifier.rs` -> `Confirmed actual HEAD c5eef763ddea2774729847a963289bc945621b66, scoped three-file diff, and unrelated/pre-existing dirty paths.`
- `git diff --check HEAD -- src/config.rs src/planner.rs src/verifier.rs` -> `Passed.`
- `git diff HEAD -- src/config.rs src/planner.rs src/verifier.rs | shasum -a 256` -> `75d7769c6b882d4e6f2c5f29cb2649eca913e8a7f0d6c73e02ce3b187fd5a0c1.`
- `Windows runtime tests` -> `Not run locally; CI run 36802607148 outcome was supplied by the user and not independently fetched.`

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `F1` | `entry` | [`TASK_SCOPE_GATE`](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L313) | `Shows the direct comparison between observed Git paths and stored TaskScope.` |
| `F1` | `risk` | [`resolve_git_path`](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L1189) | `Shows changed paths flow into canonicalize_existing_prefix rather than canonical_path_like.` |
| `F1` | `contract` | [`Execution-root binding`](/Users/yuta/local-mcp-connector-parity/docs/MANAGED_WORKTREES_V1_DESIGN.md#L381) | `Defines execution_root as the basis for Planner paths and TaskScope.` |

### Dismissed Coordinator Candidates

| Candidate | Decision | Evidence |
| --- | --- | --- |
| `canonical_path_like` bypasses symlink/reparse resolution | `dismissed` | `The helper calls fs::canonicalize before changing spelling; changing a VerbatimDisk prefix does not undo canonical resolution.` |
| `canonical_path_like` changes UNC identity | `dismissed` | `strip_verbatim_prefix removes only Prefix::VerbatimDisk; UNC paths are not shortened.` |
| `managed root spelling accepts a different/unowned path` | `dismissed` | `validated_execution_root resolves the durable managed root under current Session authority and requires same_path_identity equality before returning it.` |
| `PRIMARY Windows behavior regresses due to retained root spelling` | `dismissed` | `PRIMARY execution root is fs::canonicalize-derived and starts with \\\\?\\; canonical_path_like retains fs::canonicalize's verbatim form for that root.` |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A5` | `Windows runtime was not executed locally; CI details were not independently retrieved.` | `No uncertainty about the static mismatch; limits independent confirmation of exact runtime forms and regression behavior.` | `Run/fetch the Windows CI result after a focused fix and confirm changed paths compare inside managed TaskScope while out-of-scope paths remain blocked.` |

## Prior Resolution Reconciliation

None - initial review generation.

## Receiving Handoff

- Handoff status: `Ready for receiving-code-review`
- Automatic receiving permitted: `No`
- Source report ID: `cr-20261001-a81f92c1`
- Scope fingerprint to recheck: `sha256:75d7769c6b882d4e6f2c5f29cb2649eca913e8a7f0d6c73e02ce3b187fd5a0c1`
- Actionable finding IDs: `F1`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `None`
- Deferred test-gap IDs: `None`
- Open question IDs: `None`
- Open coverage area IDs: `None`
- Highest-risk verification to repeat: `On Windows, run the managed-worktree verification path with an allowed changed file and verify TASK_SCOPE_GATE passes; confirm a changed file outside TaskScope remains blocked.`
- Suggested implementation boundaries: `Normalize Git-observed changed paths using the same validated execution-root spelling convention as managed TaskScope, while preserving canonicalization and out-of-scope containment; do not change authority derivation or enter Phase 5.`
- Re-review note: `Treat every finding as a claim to verify. Challenges require a counterclaim, argument, evidence, limits, and settlement criterion.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or product owner.`

## Report Self-Check

- `yes` Actual assessment mode and rationale are recorded: coordinator single reviewer.
- `yes` Every changed review-relevant or unknown-impact area appears once in `Review Coverage Ledger`.
- `yes` Every final finding appears once in the index and once as a matching card.
- `yes` Every `Finding F#` area references an existing finding.
- `yes` Every standalone test gap has a stable ID and severity; none were standalone.
- `yes` Every `F#` has a semantic issue fingerprint and authoritative approved-design basis.
- `yes` Generation, trigger, parent resolution, scope mode, and receiving handoff satisfy the generation-0 review contract.
- `yes` Generation 1 reconciliation is not applicable to this initial review.
- `yes` Every non-Question finding appears once in actionable or deferred handoff IDs; no open Questions or Not-covered areas remain.
- `yes` No subagent candidates were produced; coordinator candidates and dismissals are recorded.
- `yes` Every Not-covered area would require a reason and next step; none are marked Not covered.
- `yes` Recommendation follows the skill mapping: one Major finding -> Changes requested.
- `yes` The report validator passed: `python3 /Users/yuta/.agents/skills/code-review/scripts/validate_review_report.py tmp/reviews/2026-10-01-code-review-report-winpaths-a81f.md`.
- `yes` Git metadata/index and source files were not mutated; only this standalone review artifact was created.
