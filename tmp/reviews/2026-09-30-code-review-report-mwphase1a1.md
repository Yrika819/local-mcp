# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20260930-mwphase1a1`
- Review chain ID: `rc-20260930-mwphase1a1`
- Review generation: `0`
- Review trigger: `initial`
- Parent review report ID: `None`
- Parent review report path: `None`
- Parent resolution ID: `None`
- Parent resolution path: `None`
- Generated at: `2026-09-30T00:00:00Z`
- Report path: `tmp/reviews/2026-09-30-code-review-report-mwphase1a1.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:a983aff6a12b20e416b28d827143cc8afeb3d0a6d193c2c69b53f4e5e7342b38`

## Scope

- Review date: 2026-09-30
- Scope kind: `working tree`
- Scope description: Uncommitted working-tree changes on branch `managed-worktrees/v1-phase1-2` implementing Managed Worktrees V1 Phase 1 (schema plus pure state model): new `src/managed_worktree.rs` and `src/managed_worktree_tests.rs`, plus modifications to `src/goal.rs`, `src/task_store.rs`, `src/replanner.rs`, and `src/main.rs`.
- Scope mode: `full frozen scope`
- Baseline: `main` at `ec3477495eb62cda4d5cfb3e8273b5809b416e4a`
- Target: working tree (uncommitted)
- Changed paths: `6`
- Diff size: `+1330/-33`
- Completion: `Complete within reviewed scope`
- Requirements consulted: `docs/MANAGED_WORKTREES_V1_DESIGN.md` sections 2, 3, 4, 5, 7, 8, 9, 10, 11, 12, 13, 19, 20, 21, 25, 26, 27; `docs/GOAL_TASK_ORCHESTRATOR_V1_DESIGN.md` section 9.3 and the schema migration discussion; `SECURITY.md`; `.agents/skills/goallatch-maintainer/SKILL.md`; the overnight task brief.
- Prior resolution consulted: `None`
- Assumptions: `Phase 3 creation authority is out of scope, so no production caller yet supplies managed identity values. All verification was local; no network access was used or needed.`
- Excluded as unrelated: `target/`, `Cargo.lock` (unchanged), and CI workflows (untouched by this change).

## Review Orchestration

- Assessment subagent: `Coordinator assessment - the change is one new module plus four small edits, so the coordinator enumerated the risk dimensions directly and delegated only the review passes rather than the assessment.`
- Orchestration decision: `Parallel specialists`
- Decision confidence: `high`
- Decision rationale: The diff is small but authority-sensitive and spans two genuinely independent risk dimensions needing different evidence: authority and validation invariants in a new security-relevant module, and durable schema migration compatibility for an on-disk format read by every Goal. One pass risked a single lens dominating; two bounded specialists with disjoint ownership gave independent evidence at low context-sharing cost.
- Coordinator override: `None`
- Context or tool limits: `Reviewers were kept strictly read-only. No Windows or Linux cross-compile was available locally, so platform-dependent path semantics remain a CI-verified surface.`

### Risk Dimensions

- Authority-model regression: this change adds durable state that later becomes execution authority, so a validation gap could convert a tampered or corrupt file into filesystem or Git authority (design sections 2.7, 2.8, 13).
- Durable-format compatibility: `GOAL_SCHEMA_VERSION` advances 3 to 4, so a migration bug could corrupt, silently reclassify, or refuse to read every stored Goal (design section 9; orchestrator design section 9.3).
- Corruption-detection strength: existing `deny_unknown_fields` and `validate()` behavior is the primary defense against forged durable state and must not be weakened to gain compatibility.

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `R1` | authority-model and security invariants | `src/managed_worktree.rs` in full; `src/goal.rs` managed-workspace validation and `execution_root`; proof of zero Git and filesystem authority | confirm no worktree add, lock, remove, prune or ref mutation; no session-root widening; no model or caller supplied branch or path authority; `execution_root` returns `None` for every non-ACTIVE lifecycle | `Complete` |
| `R2` | migration, persistence, and backward compatibility | `src/task_store.rs` migration chain; `src/goal.rs` serde attributes and schema version; round-trip and legacy fixtures | confirm stored v1/v2/v3 Goals still load; PRIMARY serialization unchanged; migration never invents ownership; no stale hardcoded version strings; tests are not vacuous | `Complete` |
| `Coordinator` | cross-cutting integration, adjudication, and verification | whole diff | independently re-verify every accepted candidate; run format, lint, full test, and whitespace checks | `Complete` |

### Synthesis Statement

The coordinator independently re-read the implementation and re-ran the verification commands for every candidate proposed by `R1` and `R2`. Both proposed Major findings were confirmed by direct code reading and then fixed with regression tests. Several candidates were downgraded after adjudication: `R1-C1` was reduced from Major to a documentation-accuracy Minor because the design's actual requirement is that no untrusted caller can supply these values and no public MCP surface exposes one; `R1-C6` was downgraded to a Question because the error-variant split matches the pre-existing `Goal::validate()` convention. Two test-quality candidates were reclassified as standalone test gaps rather than findings. Remaining blind spots are cross-platform path and reparse semantics (CI-owned) and the Phase 3 host derivation path (not yet written).

## Review Snapshot

- Recommendation: `Changes requested`
- Completion: `Complete within reviewed scope`
- Why now: Two authority and contract defects were confirmed against the frozen design and fixed in the working tree; the disposition record is required before the Phase 1 commit.
- Must-review now:
  1. `F1` `managed execution root may overlap the primary workspace`
  2. `F2` `schema-2 migration bypasses the shared normalization step`
  3. `F5` `lifecycle edge table is documentation rather than enforcement`
- Findings count: `Blocker 0 | Major 2 | Minor 6 | Question 1`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 2`
- Coverage confidence: `high` for local behavior; `medium` for cross-platform path semantics
- Biggest blind spot: `No Phase 3 caller exists, so host derivation of the managed identity values cannot be reviewed yet`

## Complete Findings Index

| ID | Severity | Surface | Review risk | Confidence | Origin | Verification | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `F1` | `Major` | durable record validation | a corrupt record could relocate managed execution into the primary workspace and its Git internals | `high` | `R1` | `static trace` | `behavior; entry=managed-worktree-record-validation; contract=managed-execution-root-must-not-overlap-primary-workspace; effect=corrupt-durable-record-relocates-execution-into-primary-workspace` | `ifp-sha256:54feacc75e8c21e6a7bf95883207df0daa6c83d4abf4c36b65f2863b83dae653` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md sections 2.7, 2.8, and 13` |
| `F2` | `Major` | schema-2 migration branch | schema-2 Goals were normalized by a serde default rather than by migration, and the comment described control flow that did not exist | `high` | `R2` | `static trace` | `behavior; entry=goal-schema-2-migration; contract=every-pre-managed-migration-normalizes-through-shared-step; effect=schema-2-goal-normalized-by-serde-default-not-migration` | `ifp-sha256:f1a24beff3b701376e18997475993aff7fd3eaf22a5b4a8ee97f87b94526be31` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md section 9` |
| `F3` | `Minor` | new module documentation | module doc claimed uniform host derivation for all persisted values, overstating what the code enforces | `high` | `R1` | `static trace` | `behavior; entry=managed-worktree-module-contract-documentation; contract=documentation-states-mechanically-enforced-derivation; effect=doc-claims-stronger-derivation-than-code-enforces` | `ifp-sha256:0ca0523f07f5a476b3ad01f176dff246a652da70c89ca75b005f2faaad260579` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md section 2.3` |
| `F4` | `Minor` | informational ref validation | `source_ref` accepted refs that Git itself rejects | `high` | `R1` | `static trace` | `behavior; entry=informational-ref-validation; contract=source-ref-must-satisfy-git-ref-name-rules; effect=source-ref-accepted-though-git-rejects-it` | `ifp-sha256:06ea06a1474ea7b1b7886b7c236809a12d3db3d2c57d6b02c8194a38c3e10c66` | `kind:public-contract; strength:authoritative; evidence:Git ref-name rules already enforced by the same validator for dot and lock components` |
| `F5` | `Minor` | lifecycle model documentation | the doc implied the lifecycle edge table is enforced in production when Phase 1 has no transition call site | `high` | `R1` | `static trace` | `behavior; entry=managed-worktree-lifecycle-edge-table; contract=transition-enforcement-claim-matches-implemented-enforcement; effect=doc-implies-edge-table-enforced-in-production` | `ifp-sha256:f3065ef351e3fac5a796d4483eac3cbe7216f6c4ccce31a5ca2e861c507d04bb` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md section 11` |
| `F6` | `Minor` | migration-chain version guard | a constant-folded guard was unreachable and misreported a code-side skew as user data corruption | `high` | `R2` | `static trace` | `behavior; entry=goal-schema-migration-chain-guard; contract=version-skew-detected-at-compile-time; effect=constant-folded-runtime-guard-misreports-codeskew-as-datacorruption` | `ifp-sha256:47785cc152d38cea14d126e6bbb78620ec893ee49744a5d075cbad598d3a7fa9` | `kind:hard-invariant; strength:authoritative; evidence:a migration chain must never silently route a newer durable document through an older step` |
| `F7` | `Minor` | user-visible error output | a stale schema reference survived the version bump that this change generalized everywhere else | `high` | `R2` | `static trace` | `behavior; entry=replan-authority-violation-message; contract=user-visible-message-names-current-schema; effect=stale-schema-wording-after-version-bump` | `ifp-sha256:b7daf87191a1908c5d7c8c4e3ba33c4306849e211d40597c3cfe65e4768287dc` | `kind:requirement; strength:authoritative; evidence:this change advances GOAL_SCHEMA_VERSION, so version wording must track it` |
| `F8` | `Minor` | execution-root derivation | the authority accessor's precondition was undocumented | `medium` | `R1` | `static trace` | `behavior; entry=goal-execution-root-accessor; contract=authority-accessor-documents-precondition; effect=accessor-safety-relies-on-caller-validation` | `ifp-sha256:c6938ee6a632cc1010bcc50440fa14c0a379f71593bd6f5aa320d52020b4d487` | `kind:hard-invariant; strength:authoritative; evidence:durable authority in this codebase is only reached after Goal validation` |
| `F9` | `Question` | durable validation error taxonomy | corruption surfaces as two different error variants | `low` | `R1` | `static trace` | `behavior; entry=managed-worktree-validation-error-taxonomy; contract=corruption-surfaces-consistently; effect=callers-must-match-two-error-variants` | `ifp-sha256:2fab996b217c0949a1885d3ff7ee4e2c84ae28a9cd533f0111a28b97f842ce07` | `kind:product-intent; strength:unavailable; evidence:no frozen clause specifies a managed-workspace corruption error taxonomy` |

## Blocker

`None.`

## Major

### F1 Major - Managed execution root may overlap the primary workspace

Impact: security and authority. A durable managed-worktree record whose `worktree_root` is nested inside the primary workspace, or contains it, would relocate managed execution into the primary workspace and its Git administrative internals once Phase 4 routes execution through `execution_root()`.

Review reason: This directly contradicts frozen design invariants that managed work must never gain write access to the primary workspace and that Git administrative internals remain forbidden targets. The check is pure and cheap, so deferring it buys nothing.

Surface: durable state validation, security boundary.

Issue key: `behavior; entry=managed-worktree-record-validation; contract=managed-execution-root-must-not-overlap-primary-workspace; effect=corrupt-durable-record-relocates-execution-into-primary-workspace`

Issue fingerprint: `ifp-sha256:54feacc75e8c21e6a7bf95883207df0daa6c83d4abf4c36b65f2863b83dae653`

Expected basis: `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md sections 2.7, 2.8, and 13`

Confidence: `high`

Origin: `R1` candidate `R1-C2`

Coordinator verification: Re-read `ManagedWorktreeRecord::validate` and confirmed only exact equality between the two roots was rejected, with no containment check. Confirmed `Goal::execution_root` returns the worktree root verbatim for `Active` records. Confirmed the new path test table covered relative, dot, dot-dot, and exact-equal cases but no containment case.

Look here first:
- `src/managed_worktree.rs` (`ManagedWorktreeRecord::validate`)
- `src/goal.rs` (`Goal::execution_root`)

Failure mode:
- Expected: a managed execution root that is inside the primary workspace, inside its Git administrative internals, or that contains the primary workspace is rejected purely from durable data.
- Current: only exact equality was rejected, so the primary Git directory, a primary subdirectory, and the primary parent all validated.

Evidence: static trace of `validate`; the path rejection test table had no containment case.

Assumptions and limits: `The broader policy question of where a host may place a managed root is a separate product decision; only the unambiguous overlap cases were fixed.`

Reviewer action: `request fix`

### F2 Major - Schema-2 migration bypasses the shared normalization step

Impact: data and contract. Stored schema-2 Goals were normalized to `PRIMARY` by a serde default rather than by migration, and the adjacent comment described a fall-through to the managed-worktree step that never occurred. A future change to the field default or the migration chain would silently diverge schema-2 from schema-3 Goals.

Review reason: The migration chain is the single mechanism that decides what a legacy Goal becomes. A migration that is correct only by coincidence, and documented incorrectly, is a latent durability defect.

Surface: persistence, schema migration.

Issue key: `behavior; entry=goal-schema-2-migration; contract=every-pre-managed-migration-normalizes-through-shared-step; effect=schema-2-goal-normalized-by-serde-default-not-migration`

Issue fingerprint: `ifp-sha256:f1a24beff3b701376e18997475993aff7fd3eaf22a5b4a8ee97f87b94526be31`

Expected basis: `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md section 9`

Confidence: `high`

Origin: `R2` candidate `R2-C1`

Coordinator verification: Traced `load_path_unlocked` and confirmed the schema-2 branch returned the shared finalizer directly and that the finalizer never inserts the workspace mode; confirmed the only insert site was unreachable from schema 2; confirmed the existing schema-2 test asserted only the schema version, so the divergence was untested.

Look here first:
- `src/task_store.rs` (`load_path_unlocked` schema-2 branch)
- `src/task_store.rs` (`decode_pre_managed_schema`)

Failure mode:
- Expected: every pre-schema-4 stored Goal is normalized by the same explicit migration code.
- Current: schema-2 input relied on the serde default to produce the primary mode, and the adjacent comment claimed control flow that did not exist.

Evidence: static trace; no test asserted the workspace mode of a migrated schema-2 Goal.

Assumptions and limits: `None.`

Reviewer action: `request fix`

## Minor

### F3 Minor - Module doc overstates mechanical derivation

Impact: maintainability. The module documentation stated that every persisted value is host-derived, which the code mechanically enforces only for the branch ref and the lock reason.

Review reason: A future Phase 3 author may trust the documentation instead of the validator and introduce an unverified path source.

Surface: documentation, new module.

Issue key: `behavior; entry=managed-worktree-module-contract-documentation; contract=documentation-states-mechanically-enforced-derivation; effect=doc-claims-stronger-derivation-than-code-enforces`

Issue fingerprint: `ifp-sha256:0ca0523f07f5a476b3ad01f176dff246a652da70c89ca75b005f2faaad260579`

Expected basis: `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md section 2.3`

Confidence: `high`

Origin: `R1` candidate `R1-C1`, downgraded from `Major` by the coordinator

Coordinator verification: Confirmed no public MCP request type exposes a managed field, so no untrusted caller can supply these values and the design's requirement holds structurally. The defect is documentation accuracy only.

Look here first:
- `src/managed_worktree.rs` (module documentation)

Failure mode:
- Expected: the documentation distinguishes values that are mechanically unforgeable from values the host supplies and validation only shape-checks.
- Current: it claimed uniform host derivation for all persisted values.

Evidence: static trace of the module documentation against the record and intent constructor signatures.

Assumptions and limits: `None.`

Reviewer action: `approve with caveat`

### F4 Minor - Informational ref accepted refs Git rejects

Impact: contract. The informational source ref accepted a reflog selector and components ending in a dot, both of which Git rejects as ref names.

Review reason: The field is informational in Phase 1, but it is validated by the same Git ref rules already enforced for neighbouring cases, so leaving gaps is inconsistent and becomes a real surface if it ever reaches a Git argument vector.

Surface: validation, ref parsing.

Issue key: `behavior; entry=informational-ref-validation; contract=source-ref-must-satisfy-git-ref-name-rules; effect=source-ref-accepted-though-git-rejects-it`

Issue fingerprint: `ifp-sha256:06ea06a1474ea7b1b7886b7c236809a12d3db3d2c57d6b02c8194a38c3e10c66`

Expected basis: `kind:public-contract; strength:authoritative; evidence:Git ref-name rules already enforced by the same validator for dot and lock components`

Confidence: `high`

Origin: `R1` candidate `R1-C4`

Coordinator verification: Re-read the informational ref validator and confirmed a reflog selector and a trailing-dot component were not rejected while the lock suffix and leading dot were.

Look here first:
- `src/managed_worktree.rs` (`validate_informational_ref`)

Failure mode:
- Expected: the validator rejects a reflog selector and any component ending in a dot.
- Current: both were accepted.

Evidence: static trace; the hostile-input test table lacked both cases.

Assumptions and limits: `None.`

Reviewer action: `approve with caveat`

### F5 Minor - Lifecycle edge table is documentation rather than enforcement

Impact: contract. The lifecycle transition predicate has no production caller, so a direct jump from requested to active is not currently prevented; the documentation implied otherwise.

Review reason: Phase 3 must add the transition call site, and the documentation should not overstate the current guarantee.

Surface: lifecycle model, documentation.

Issue key: `behavior; entry=managed-worktree-lifecycle-edge-table; contract=transition-enforcement-claim-matches-implemented-enforcement; effect=doc-implies-edge-table-enforced-in-production`

Issue fingerprint: `ifp-sha256:f3065ef351e3fac5a796d4483eac3cbe7216f6c4ccce31a5ca2e861c507d04bb`

Expected basis: `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md section 11`

Confidence: `high`

Origin: `R1` candidate `R1-C5`

Coordinator verification: Confirmed no production caller of the transition predicate, and confirmed that the reconciliation-observation check inside `validate` is a shape check rather than a transition check.

Look here first:
- `src/managed_worktree.rs` (`ManagedWorktreeLifecycle::can_transition_to`)

Failure mode:
- Expected: the documentation states the edge set is frozen for Phase 1 and enforced at the Phase 3 transition site.
- Current: it implied the ordering is enforced now.

Evidence: static trace; a search for the transition predicate outside the module and its test returned no production matches.

Assumptions and limits: `No transition code exists yet, so this is a documentation defect rather than a runtime one.`

Reviewer action: `approve with caveat`

### F6 Minor - Migration-chain version guard was unreachable and mis-typed

Impact: maintainability and diagnosability. A constant-versus-constant comparison always folded to false, and the message described constants rather than the loaded document while reporting corruption, which callers surface as user data corruption.

Review reason: The guard's only value is to catch a future schema bump; as written it could never fire and, if it did, would misdirect diagnosis toward user data.

Surface: persistence, migration.

Issue key: `behavior; entry=goal-schema-migration-chain-guard; contract=version-skew-detected-at-compile-time; effect=constant-folded-runtime-guard-misreports-codeskew-as-datacorruption`

Issue fingerprint: `ifp-sha256:47785cc152d38cea14d126e6bbb78620ec893ee49744a5d075cbad598d3a7fa9`

Expected basis: `kind:hard-invariant; strength:authoritative; evidence:a migration chain must never silently route a newer durable document through an older step`

Confidence: `high`

Origin: `R2` candidate `R2-C2`

Coordinator verification: Confirmed both operands were compile-time constants equal to four, so the branch is provably unreachable, and confirmed a clean rebuild emits no dead-code diagnostic.

Look here first:
- `src/task_store.rs` (`decode_migrated_goal`)

Failure mode:
- Expected: schema-chain skew between the current schema and the migration chain is a compile error.
- Current: a constant-folded runtime check that is unreachable and, if reached, reports a code-side skew as durable-state corruption.

Evidence: static trace of both constant declarations; clean rebuild produced no warning.

Assumptions and limits: `None.`

Reviewer action: `approve with caveat`

### F7 Minor - Stale schema wording in replanner error

Impact: user. The replan authority-violation message still named a superseded schema after the bump, and this change generalized the analogous goal messages but missed this one.

Review reason: Minor, but directly caused by the version bump in this change, so it belongs here.

Surface: error output, replanner.

Issue key: `behavior; entry=replan-authority-violation-message; contract=user-visible-message-names-current-schema; effect=stale-schema-wording-after-version-bump`

Issue fingerprint: `ifp-sha256:b7daf87191a1908c5d7c8c4e3ba33c4306849e211d40597c3cfe65e4768287dc`

Expected basis: `kind:requirement; strength:authoritative; evidence:this change advances GOAL_SCHEMA_VERSION, so version wording must track it`

Confidence: `high`

Origin: `R2` candidate `R2-C3`

Coordinator verification: Read the message and confirmed it renders through the error display implementation on a live path.

Look here first:
- `src/replanner.rs` (replan authority-violation message)

Failure mode:
- Expected: the message names no specific schema version, or names the current one.
- Current: it still named the superseded schema.

Evidence: static trace; no test asserts this string.

Assumptions and limits: `Did not audit documentation or CI workflows for message-text coupling.`

Reviewer action: `approve with caveat`

### F8 Minor - Execution-root accessor precondition undocumented

Impact: maintainability. The execution-root accessor does not re-validate its input, so its safety depends on callers having validated the Goal first.

Review reason: This is the function Phase 4 will wire into Planner, writer, and verifier, so its precondition should be explicit now.

Surface: execution-root derivation.

Issue key: `behavior; entry=goal-execution-root-accessor; contract=authority-accessor-documents-precondition; effect=accessor-safety-relies-on-caller-validation`

Issue fingerprint: `ifp-sha256:c6938ee6a632cc1010bcc50440fa14c0a379f71593bd6f5aa320d52020b4d487`

Expected basis: `kind:hard-invariant; strength:authoritative; evidence:durable authority in this codebase is only reached after Goal validation`

Confidence: `medium`

Origin: `R1` candidate `R1-C3`

Coordinator verification: Confirmed every current construction and durable load path validates first, so the accessor is safe today; the defect is the undocumented precondition.

Look here first:
- `src/goal.rs` (`Goal::execution_root`)

Failure mode:
- Expected: the documentation states the accessor requires an already-validated Goal.
- Current: the precondition was unstated.

Evidence: static trace of the Goal constructor and the durable store decode and commit call sites.

Assumptions and limits: `Assumes no future path deserializes a Goal and calls this accessor without validating.`

Reviewer action: `approve with caveat`

## Questions

### F9 Question - Managed-workspace corruption surfaces as two error variants

Approval impact: Cannot be classified as a defect without a product decision on the corruption error taxonomy. No current caller depends on the split, so it does not block the Phase 1 commit.

Needed context: Whether a future corruption-triage or repair path should distinguish identifier corruption from state-shape corruption, or treat all managed-workspace corruption uniformly.

Surface: error taxonomy, durable validation.

Issue key: `behavior; entry=managed-worktree-validation-error-taxonomy; contract=corruption-surfaces-consistently; effect=callers-must-match-two-error-variants`

Issue fingerprint: `ifp-sha256:2fab996b217c0949a1885d3ff7ee4e2c84ae28a9cd533f0111a28b97f842ce07`

Expected basis: `kind:product-intent; strength:unavailable; evidence:no frozen clause specifies a managed-workspace corruption error taxonomy`

Confidence: `low`

Origin: `R1` candidate `R1-C6`

Coordinator verification: Confirmed record and intent shape failures return durable-state corruption while identifier failures propagate an unsafe-identifier error. Kept as a question because this matches the pre-existing Goal validation convention, so the split introduces no new inconsistency.

Look here first:
- `src/orchestrator_error.rs` (error variant definitions)

Evidence: static trace; existing tests assert the per-case variant, so behavior is pinned.

Settlement criterion: `An explicit product decision on whether a managed-workspace corruption triage path should be able to distinguish the two variants.`

Reviewer action: `ask owner`

## Test Gaps

| ID | Severity | Surface | Missing coverage | Risk | Origin | Evidence | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `T1` | `Minor` | managed-worktree validation purity | the determinism test re-ran a pure function and could never fail; no assertion covered that validation leaves the state it inspects unchanged | validation that normalizes or defaults fields could ship undetected | `R2` | the loop body only re-asserted a previously computed boolean | `test-gap; entry=managed-worktree-validation-purity-assertions; contract=assertions-must-be-falsifiable; gap=vacuous-repeat-loop-cannot-fail` | `ifp-sha256:9e340d9b83cc8ac701c1dab417a778cc77ddd804228d3194b52816a34c9a755d` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md section 25 test plan freeze` |
| `T2` | `Minor` | PRIMARY-with-managed-state rejection | the fixture built its record from a different Goal, so the test could pass for an unrelated ownership-mismatch reason | the PRIMARY guard could be weakened while the test stayed green | `R2` | the fixture constructed the record from a second Goal with a different identity | `test-gap; entry=primary-goal-rejects-managed-state-fixture; contract=test-fixture-isolates-intended-rule; gap=cross-goal-record-fixture-passes-for-unrelated-reason` | `ifp-sha256:c35b23f12e2a4553eba8428015c93677325449a7cec4dc9a56a1745c6c1fb84f` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md section 25 opt-in and regression group` |

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | Durable record validation | `src/managed_worktree.rs` record validate | `R1` | contract trace | `Finding F1` | Containment gap confirmed against design sections 2.7, 2.8, 13 and fixed with new rejection coverage. |
| `A2` | Host-derived branch ref and lock reason | `src/managed_worktree.rs` derivations and equality validators | `R1` | contract trace | `Reviewed - no issue found` | Both are re-derived and compared for exact equality; hostile substitutions are rejected by test. |
| `A3` | Identifier validation | `src/managed_worktree.rs` identifier implementations | `R1` | contract trace | `Reviewed - no issue found` | Non-canonical identifier spellings are rejected; the identifier-visibility widening is a pure predicate. |
| `A4` | Path canonicality validation | `src/managed_worktree.rs` absolute-path validator | `R1` | contract trace | `Reviewed - no issue found` | Component allowlist plus a raw-bytes rebuild comparison correctly defeats component-wise path equality. |
| `A5` | Object-id validation | `src/managed_worktree.rs` object-id validator | `R1` | contract trace | `Reviewed - no issue found` | Full lowercase hex only; abbreviated, uppercase, and non-hex values are rejected. |
| `A6` | Informational ref validation | `src/managed_worktree.rs` informational ref validator | `R1` | contract trace | `Finding F4` | Reflog selector and trailing-dot gaps confirmed and fixed. |
| `A7` | Lifecycle edges and observation pairing | `src/managed_worktree.rs` lifecycle type | `R1` | contract trace | `Finding F5` | Edge table is correct but unenforced in production; documentation corrected. |
| `A8` | Execution-root derivation | `src/goal.rs` execution-root accessor | `R1` | dependency trace | `Finding F8` | Returns none for every non-active managed lifecycle, verified by test; precondition documented. |
| `A9` | Goal-level managed validation and intent pairing | `src/goal.rs` managed-workspace validator | `R1` | contract trace | `Reviewed - no issue found` | The four-arm match is exhaustive over intent and lifecycle; all four rules are covered by tests. |
| `A10` | Proof of zero Git and filesystem authority | whole diff | `R1` | diff-only | `Reviewed - no issue found` | No process spawn, filesystem call, or Git vocabulary was added; the module imports only serialization, identifier, and path types. |
| `A11` | Proof of no session path-authority widening | `src/config.rs`, `src/goal_api.rs` | `R1` | diff-only | `Reviewed - no issue found` | No permitted-directories change; the public start request exposes no managed field. |
| `A12` | PRIMARY regression freeze and single-writer lease | `src/goal.rs`, `src/scheduler.rs`, `src/writer.rs` | `R1` | dependency trace | `Reviewed - no issue found` | No writer, lease, or scheduler code was touched; PRIMARY validation returns early. |
| `A13` | Schema migration chain | `src/task_store.rs` durable load and migration helpers | `R2` | contract trace | `Finding F2` | The schema-2 bypass of the shared step was confirmed and fixed with a new test. |
| `A14` | Migration never invents managed ownership | `src/task_store.rs` migration helpers | `R2` | contract trace | `Reviewed - no issue found` | Constant primary insert; managed keys on legacy documents are rejected; covered by two tests. |
| `A15` | PRIMARY serialization byte-compatibility | `src/goal.rs` serde attributes | `R2` | contract trace | `Reviewed - no issue found` | All three new fields are skipped for primary Goals; key absence is asserted by test. |
| `A16` | Round-trip fidelity of managed state | `src/managed_worktree_tests.rs` round-trip test | `R2` | contract trace | `Reviewed - no issue found` | Serialize, deserialize, and validate preserve the record exactly. |
| `A17` | Stale schema-version strings | `src/replanner.rs`, `src/goal.rs` | `R2` | diff-only | `Finding F7` | One stale user-visible string was found and fixed; all test-side literals are intentional fixtures. |
| `A18` | Migration-chain version-skew guard | `src/task_store.rs` migration finalizer | `R2` | contract trace | `Finding F6` | Constant-folded and mis-typed; replaced with a compile-time assertion. |
| `A19` | Legacy schema-1 terminal reader coexistence | `src/goal.rs` legacy terminal validator | `R2` | dependency trace | `Reviewed - no issue found` | The managed-state check was added there too; legacy fixtures still load because they carry no managed fields. |
| `A20` | Managed-workspace validation purity | `src/managed_worktree_tests.rs` purity tests | `R2` | contract trace | `Reviewed - no issue found` | Purity behavior is correct and asserted; a test-quality gap in the same area is tracked as `T1`. |
| `A21` | PRIMARY-with-managed-state rejection behavior | `src/goal.rs` primary guard | `R2` | contract trace | `Reviewed - no issue found` | The behavior is correctly rejected; a test-isolation gap in the same area is tracked as `T2`. |
| `A22` | Cross-platform path and reparse semantics | `src/managed_worktree.rs` path validation | `Coordinator` | contract trace | `Not covered` | Platform-dependent behavior is not exercised on this host. | Windows prefix handling and reparse or junction behavior need the GitHub Actions matrix legs for windows-x64, compat windows-2022, and compat windows-arm64. |
| `A23` | Phase 3 host derivation of managed identity values | future callers of the record and intent constructors | `Coordinator` | contract trace | `Not covered` | No production caller exists yet, so host derivation cannot be traced. | Review the Phase 3 creation-authority diff when it is authorized. |
| `A24` | Byte-level diff of a stored pre-change Goal against a current primary emit | `src/task_store.rs` durable format | `Coordinator` | contract trace | `Not covered` | Byte compatibility was inferred from skip behavior rather than an observed diff. | Commit a pre-change Goal fixture and diff it against a current primary emit. |

## Subagent Candidate Adjudication

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `R1-C1` | `R1` | accepted, downgraded | `F3` | confirmed no public field exposes managed identity and confirmed the doc overclaimed | The design requires that the model or caller cannot choose these values, which holds structurally; the residual defect is documentation accuracy. |
| `R1-C2` | `R1` | accepted | `F1` | re-read the record validator and confirmed only equality was rejected, and that the execution root passes the field through | A pure, cheap invariant directly supported by design sections 2.7, 2.8, and 13. |
| `R1-C3` | `R1` | accepted | `F8` | confirmed every construction and load path validates first | The accessor is safe today; the precondition should be explicit for Phase 4. |
| `R1-C4` | `R1` | accepted | `F4` | confirmed a reflog selector and trailing dot were unrejected while sibling rules were enforced | Inconsistent with the same function's other Git ref rules. |
| `R1-C5` | `R1` | accepted | `F5` | confirmed zero production callers of the transition predicate | Documentation overstated the guarantee; no runtime path exists to break. |
| `R1-C6` | `R1` | downgraded to question | `F9` | compared against pre-existing identifier handling in Goal validation | The variant split matches the established convention, so it introduces no new inconsistency. |
| `R2-C1` | `R2` | accepted | `F2` | traced both migration helpers and confirmed the workspace-mode insert is unreachable from schema 2 | Correct only by coincidence, with an inaccurate comment. |
| `R2-C2` | `R2` | accepted | `F6` | confirmed both operands are constants and a clean rebuild emits no diagnostic | An unreachable guard with a misleading error type. |
| `R2-C3` | `R2` | accepted | `F7` | confirmed the message text and its display path | Directly caused by the version bump in this change. |
| `R2-C4` | `R2` | reclassified as test gap | `T1` | confirmed the loop re-asserted a cached boolean over a pure function | It provides no regression signal, so it is a coverage gap rather than a behavior finding. |
| `R2-C5` | `R2` | reclassified as test gap | `T2` | confirmed the fixture built a record from a second Goal with a different identity | The test could pass for an unrelated reason. |
| Schema-4 breaks existing Goals | `R1` | dismissed | `None` | confirmed all three fields default on read and are skipped when primary | Backward compatibility achieved without weakening the container. |
| A legacy Goal can smuggle managed state | `R1` | dismissed | `None` | confirmed both the migration guard and the primary guard reject it independently | Two independent layers, both tested. |
| Field order breaks primary bytes | `R2` | dismissed | `None` | confirmed all three fields are skipped for primary Goals | Serialization emits nothing for skipped fields. |

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `src/managed_worktree.rs` | surface | authority validation, durable state model, identity derivation |
| `src/managed_worktree_tests.rs` | test-only | invariant coverage, migration and rejection cases |
| `src/goal.rs` | surface | schema version, serialization compatibility, Goal-level validation, execution root |
| `src/task_store.rs` | surface | migration chain, durable compatibility, corruption detection |
| `src/replanner.rs` | surface | user-visible error output |
| `src/main.rs` | config | module registration only |

### Verification Commands

- `cargo fmt --check` -> clean, no diff
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> clean, no warnings
- `cargo test --locked --all-targets` -> 723 passed, 0 failed (684 pre-existing baseline plus 39 new)
- `cargo test --locked --all-targets managed_worktree` -> 36 passed, 0 failed
- `git diff --check` -> clean, exit 0
- `git rev-parse HEAD` -> `ec3477495eb62cda4d5cfb3e8273b5809b416e4a`, unchanged by the review
- `git worktree list --porcelain` -> a single primary worktree; no worktree was created, removed, or modified

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `F1` | entry | `src/managed_worktree.rs` record validate | where durable identity is checked before it can become authority |
| `F1` | risk | `src/goal.rs` execution-root accessor | hands the worktree root to future execution routing verbatim |
| `F1` | test | `src/managed_worktree_tests.rs` overlap rejection test | regression coverage for the containment invariant |
| `F2` | entry | `src/task_store.rs` durable load dispatch | the branch point for every stored schema version |
| `F2` | risk | `src/task_store.rs` pre-managed migration step | the only site that establishes the primary mode during migration |
| `F2` | test | `src/task_store.rs` schema-2 shared-step test | proves schema-2 takes the shared path |

### Dismissed Coordinator Candidates

| Candidate | Decision | Evidence |
| --- | --- | --- |
| The schema-2 migration might be missing a field added in schema 3 or 4 | dismissed | The schema-2 branch inserts the supersession history itself and the shared step then applies the workspace mode; no field is dropped. |
| Adding the managed check to the schema-1 legacy reader could reject valid legacy Goals | dismissed | Legacy Goals carry no managed fields, so the function returns early and the legacy fixture still loads. |
| The unknown-field guard on Goal might reject the new fields | dismissed | The fields are declared members of Goal, so they are known; unknown fields are still rejected and are now also rejected inside both new nested structs. |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A22` | Windows prefix handling, reparse points, and path canonicalization semantics are platform-dependent and untested on this host | a managed record could validate on macOS but behave differently on Windows | the GitHub Actions matrix legs for windows-x64, compat windows-2022, and compat windows-arm64 |
| `A23` | No Phase 3 caller exists, so host derivation of the worktree root, base commit, and common directory cannot be reviewed | an unverified value could reach the record through future host code | review the Phase 3 creation-authority diff when it is authorized |
| `A24` | Primary byte-compatibility was not proven by an observed golden-file diff of a real stored Goal | a subtle serialization change could be missed | commit a pre-change Goal fixture and diff it against a current primary emit |

## Prior Resolution Reconciliation

`None - initial review generation.`

## Receiving Handoff

- Handoff status: `Ready for receiving-code-review`
- Automatic receiving permitted: `Yes`
- Source report ID: `cr-20260930-mwphase1a1`
- Scope fingerprint to recheck: `sha256:a983aff6a12b20e416b28d827143cc8afeb3d0a6d193c2c69b53f4e5e7342b38`
- Actionable finding IDs: `F1, F2, F3, F4, F5, F6, F7, F8`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `T1, T2`
- Deferred test-gap IDs: `None`
- Open question IDs: `F9`
- Open coverage area IDs: `A22, A23, A24`
- Highest-risk verification to repeat: `cargo test --locked --all-targets` plus the GitHub Actions matrix, because the cross-platform path surface is platform-owned and cannot be discharged locally.
- Suggested implementation boundaries: `src/managed_worktree.rs` validation, `src/task_store.rs` migration chain, `src/goal.rs` execution-root documentation, `src/replanner.rs` message text, and `src/managed_worktree_tests.rs`. Do not widen into Phase 2 discovery or Phase 3 creation authority.
- Re-review note: `Treat every finding as a claim to verify. Challenges require a counterclaim, argument, evidence, limits, and settlement criterion.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or product owner.`

### Disclosure

The overnight task authorized fixing in-scope review findings in the same step. All findings above were therefore fixed in the working tree after the read-only review passes completed, and the verification commands in this report were re-run against the fixed tree. Per the skill contract this report is the frozen record of what the reviewers found at review time; the applied changes and their dispositions are recorded in the separate resolution report `rr-20260930-mwphase1a1`.

## Report Self-Check

- `yes` Actual assessment mode and rationale are recorded: coordinator, delegated assessor, or unavailable fallback.
- `yes` Every changed review-relevant or unknown-impact area appears once in `Review Coverage Ledger`.
- `yes` Every final finding appears once in the index and once as a matching card.
- `yes` Every `Finding F#` area references an existing finding.
- `yes` Every standalone test gap has a stable ID and severity.
- `yes` Every `F#` and `T#` has a unique semantic issue fingerprint and an authoritative expected basis, or the item is an explicit `Question` for unconfirmed intent.
- `yes` Generation, trigger, parent resolution, scope mode, and receiving handoff satisfy the bounded chain contract.
- `yes` Generation `1` reconciliation is not applicable because this report is generation `0`.
- `yes` Every non-Question finding and standalone test gap appears exactly once in actionable or deferred handoff IDs; every Question and Not-covered area appears in its matching open list.
- `yes` Every meaningful subagent candidate has an adjudication.
- `yes` Every `Not covered` area has a reason and next step.
- `yes` Recommendation follows the skill mapping.
- `yes` The validator passes.
- `yes` Git state was not mutated.
