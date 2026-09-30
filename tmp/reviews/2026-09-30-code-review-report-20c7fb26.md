# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20260930-20c7fb26`
- Review chain ID: `rc-20260930-7316d6e7`
- Review generation: `0`
- Review trigger: `initial`
- Parent review report ID: `None`
- Parent review report path: `None`
- Parent resolution ID: `None`
- Parent resolution path: `None`
- Generated at: `2026-09-30T00:21:55Z`
- Report path: `tmp/reviews/2026-09-30-code-review-report-20c7fb26.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:0a0f47521b0dac64a160c5e0786b8897bc25fb6f3ca44a10d517dfd33bf8fed1`

## Scope

- Review date: `2026-09-30`
- Scope kind: `working tree`
- Scope description: Phase 2 read-only managed-worktree discovery, Git/filesystem observation, eligibility and reconciliation, tests, and `src/main.rs` module/test wiring.
- Scope mode: `full frozen scope`
- Baseline: `35f70ef2c40ccb7b81ad09814ca8dc798cd86b9f`
- Target: `working tree on managed-worktrees/v1-phase1-2`
- Changed paths: `4`
- Diff size: `+3407 / -0`
- Completion: `Complete within reviewed scope`
- Requirements consulted: User mission; `docs/MANAGED_WORKTREES_V1_DESIGN.md` §§6, 8, 11, 19, 20, 23, 25, 26; `docs/GOAL_TASK_ORCHESTRATOR_V1_DESIGN.md`; `SECURITY.md`; Phase 2 module contracts and tests.
- Prior resolution consulted: `None`
- Assumptions: Phase 2 is required to remain read-only and is not yet wired into Goal lifecycle production paths; later phases must consume classifications conservatively.
- Excluded as unrelated: Phase 1 durable schema changes already committed at the baseline; Phase 3 creation authority and all later execution/cleanup phases.

## Review Orchestration

- Assessment subagent: `Coordinator assessment - discovery and reconciliation have distinct, high-risk failure modes, so independent bounded passes improve coverage.`
- Orchestration decision: `Parallel specialists`
- Decision confidence: `high`
- Decision rationale: The 3,407-line addition crosses untrusted machine-readable parsing, host Git/filesystem observation, persistence-bound ownership classification, and retry eligibility. Independent observation/security and reconciliation/state passes materially reduce the chance of missing a fail-open edge.
- Coordinator override: `None`
- Context or tool limits: `Review and local tests ran on macOS only; GitHub Actions matrix had not yet been run at report time.`

### Risk Dimensions

- `Read-only host boundary: allowed Git commands can consult repository configuration and inherited environment, and marker observation reads repository metadata.`
- `Recovery safety: an ambiguous or inconsistent observation must never become a retryable no-side-effect result.`
- `Cross-platform path behavior: Windows uses different absolute-path and case/path identity semantics and is part of the required compatibility matrix.`

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `R1` | Parser, observation, and host-side-effect security | `src/managed_worktree_discovery.rs` porcelain parser; `src/managed_worktree_observe.rs`; parser/observer tests | Worktree inventory trust; status NUL format; exact Git allowlist; marker path type and bounds; read-only execution | `Complete` |
| `R2` | Ownership, eligibility, lifecycle, and retry classification | `src/managed_worktree_discovery.rs` observation trust and classifiers; relevant tests | Durable identity binding; lifecycle outcomes; missing/unknown refs and paths; retry predicate; retained branch contract | `Complete` |
| `Coordinator` | Integration and independent verification | All four changed paths and frozen contracts | Re-read every candidate; compare report to current tests and actual diff | `Complete` |

### Synthesis Statement

The coordinator independently checked every retained candidate against code and the approved design. R1 and R2 independently identified the global unknown-attribute trust gap; it is merged as one finding. Other retained candidates were confirmed from their respective paths. No production creation or Goal lifecycle wiring is present in this scope. Cross-platform execution and a hostile Git-config runtime reproduction remain limitations, recorded as evidence limits rather than claims of verified platform behavior.

## Review Snapshot

- Recommendation: `Changes requested`
- Completion: `Complete within reviewed scope`
- Why now: Several fail-closed contracts are not yet upheld by the discovery model, and the Windows unit fixtures are not portable.
- Must-review now:
  1. `F1` Unknown worktree attributes bypass the repository-wide trust gate
  2. `F2` Malformed nonempty status output can satisfy the clean-primary check
  3. `F3` Host Git observation can execute configured helpers or write inherited traces
- Findings count: `Blocker 0 | Major 5 | Minor 2 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 1`
- Coverage confidence: `high`
- Biggest blind spot: Windows CI and host Git configurations were not exercised in this macOS review.

## Complete Findings Index

| ID | Severity | Surface | Review risk | Confidence | Origin | Verification | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `F1` | `Major` | Repository observation trust gate | Unknown attributes on primary or unrelated registrations can still lead to eligible/retryable classifications. | `high` | `R1, R2, Coordinator` | Independent source trace through `is_trustworthy`, eligibility, and reconciliation; existing tests cover only unknown attributes on the expected managed entry. | `behavior; entry=repository eligibility and reconciliation; contract=any unrecognized registered worktree attribute makes inventory ambiguous; effect=incomplete inventory is accepted as eligible or retryable` | `ifp-sha256:79307a23a9dd485aeefe00d88ddd540ae7f9dfbfa17c7dc93d15f68746c46ad2` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §§11, 20 and parser trust comment` |
| `F2` | `Major` | Primary status parser | NUL-only malformed status output is interpreted as a clean workspace. | `high` | `R1, Coordinator` | Source trace from `parse_primary_status` to `PrimaryWorkspaceStatus::is_clean` and eligibility; valid clean status is empty output. | `behavior; entry=primary status observation; contract=malformed nonempty NUL status output is rejected rather than treated clean; effect=dirty-primary gate can accept an invalid observation` | `ifp-sha256:4a585823823e0bfd9dfc79801afdf103c89198604cc6469bfdefb2d1bb27dc38` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §6` |
| `F3` | `Major` | `HostGit::run` and `git status` observation | Exact argv allowlisting does not prevent repository-configured `core.fsmonitor` helpers or inherited Git tracing from causing process/filesystem side effects. | `high` | `R1, R2, Coordinator` | Static trace confirms inherited process environment and Git configuration remain active; the helper path is conditional on host/repository configuration and was not runtime-reproduced. | `behavior; entry=host read-only Git observation; contract=observation commands cannot invoke configured helpers or write trace data; effect=Phase 2 read-only boundary performs an unrequested external side effect` | `ifp-sha256:0268c5a8363c3bbe414d0215e6a2ca8c8201b76ed030816b6017b07bb7bb11ec` | `kind:hard-invariant; strength:authoritative; evidence:user mission Phase 2 read-only boundary and docs/MANAGED_WORKTREES_V1_DESIGN.md §26` |
| `F4` | `Minor` | Sequencer marker reader | A dangling `sequencer/todo` symlink can be treated as absence; a special or oversized file can block or consume unbounded resources. | `medium` | `R1, Coordinator` | Source trace verifies direct unbounded `fs::read` and `NotFound` => absent; tests cover only normal sequencer content. | `behavior; entry=sequencer marker observation; contract=operation markers are bounded and nonregular/symlink markers fail closed; effect=observation can hang, exhaust memory, or miss operation state` | `ifp-sha256:e9b6450e70105313cca689bf4af044023dba0140cbd67ef0bccf5989955f61eb` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §§6, 11, 20` |
| `F5` | `Major` | Eligibility classifier | `Blocked`, `Removed`, `Active`, or `CleanupEligible` durable states can receive `Eligible` when repository observations otherwise match. | `high` | `R2, Coordinator` | Independent trace confirms `expected.lifecycle` is carried but never read by `classify_eligibility`; durable model marks `Removed` terminal. | `behavior; entry=managed-worktree eligibility; contract=terminal or blocked lifecycle is never classified as eligible for new creation; effect=stale Goal state is presented as creation-eligible` | `ifp-sha256:8162e5a7aa9dcd94503f3d1be706a951dc00bd85bd3a3452b22a576c93a59baa` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §§5, 11, 20 and src/managed_worktree.rs lifecycle contract` |
| `F6` | `Minor` | `REMOVED` reconciliation | Normal cleanup retaining the local branch is classified as `BranchOnlySideEffect` instead of `RemovedExact`. | `high` | `R2, Coordinator` | Source order and design cleanup contract confirm ref existence is classified as a partial creation side effect before `Removed` lifecycle is considered. | `behavior; entry=removed worktree reconciliation; contract=REMOVED permits the design retained branch when no worktree/path remains; effect=normal removal is classified as partial creation side effect` | `ifp-sha256:ef939a4a1ce08fc3ec8dac59976fc3183206ca843f4ac5b3c788d7bc3fdc2bc1` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §§19–20` |
| `F7` | `Major` | Windows pure/integration test fixtures | Unix-rooted fixture paths are not absolute Windows paths and record construction unwraps validation, making Phase 2 tests fail on Windows. | `high` | `R2, Coordinator` | Static trace from `/repo/...` constants through `Fixture::new` to `validate_canonical_absolute`; no Windows runner was available locally. | `behavior; entry=Windows Phase 2 pure tests; contract=fixtures use absolute Windows paths; effect=Windows all-target test suite fails while constructing records` | `ifp-sha256:5146ea7271be2a66cb312d3503d889bdcd58ed18e0ac7d0b45e6cbbd30a377d6` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §25I` |

## Blocker

None.

## Major

### F1 Major - Unknown inventory attributes bypass the trust gate

Impact: A future caller can receive an eligible or retryable verdict from an inventory containing a registration whose semantics the parser intentionally does not understand.
Review reason: Unknown attributes are deliberately preserved so the implementation can fail closed, but the global trust predicate does not apply that rule to every inventory entry. In particular, an unknown primary attribute can leave the primary eligible; an unknown unrelated entry can coexist with `NoSideEffect` and a bounded retry.
Surface: Repository observation trust, eligibility, and reconciliation.
Issue key: `behavior; entry=repository eligibility and reconciliation; contract=any unrecognized registered worktree attribute makes inventory ambiguous; effect=incomplete inventory is accepted as eligible or retryable`
Issue fingerprint: `ifp-sha256:79307a23a9dd485aeefe00d88ddd540ae7f9dfbfa17c7dc93d15f68746c46ad2`
Expected basis: `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §§11, 20 and parser trust comment`
Confidence: `high`
Origin: `R1-C1, R2-C1; independently verified by Coordinator`
Coordinator verification: `Traced RepositoryObservation::is_trustworthy through classify_eligibility and classify_reconciliation; verified the existing regression test only puts an unknown attribute on the expected managed entry.`

Look here first:
- [`RepositoryObservation::is_trustworthy`](../../src/managed_worktree_discovery.rs#L678)
- [`classify_reconciliation`](../../src/managed_worktree_discovery.rs#L974)

Failure mode:
- Expected: Any unknown inventory attribute prevents an automatic safe/eligible/retryable conclusion.
- Current: Only bare entries and inventory contradictions are checked globally; unknown attributes are checked only on the expected registered record.

Evidence:
- The primary entry can carry an unknown attribute while passing the current global trust predicate. When the target is absent, the same untrusted inventory can reach the no-side-effect classification if the expected ref is observed absent.
- Existing `unknown_attributes_are_preserved_not_dropped` and managed-entry ambiguity tests do not cover primary or unrelated entries.

Assumptions and limits:
- The meaning of a future attribute is unknown; fail-closed handling is explicitly the code's stated policy, so no future Git semantics are assumed.

Reviewer action:
`block until fixed`

### F2 Major - Malformed status output can satisfy the clean-primary gate

Impact: A malformed but nonempty status observation can be classified as clean and satisfy the managed eligibility precondition.
Review reason: Clean-primary is a safety precondition; invalid output must not be treated as evidence of cleanliness.
Surface: `parse_primary_status` and `PrimaryWorkspaceStatus::is_clean`.
Issue key: `behavior; entry=primary status observation; contract=malformed nonempty NUL status output is rejected rather than treated clean; effect=dirty-primary gate can accept an invalid observation`
Issue fingerprint: `ifp-sha256:4a585823823e0bfd9dfc79801afdf103c89198604cc6469bfdefb2d1bb27dc38`
Expected basis: `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §6`
Confidence: `high`
Origin: `R1-C2; independently verified by Coordinator`
Coordinator verification: `Traced the empty-token skip in parse_primary_status into the default clean status; valid empty Git status output is the empty byte sequence.`

Look here first:
- [`parse_primary_status`](../../src/managed_worktree_observe.rs#L403)
- [`classify_eligibility`](../../src/managed_worktree_discovery.rs#L866)

Failure mode:
- Expected: Empty output means clean; malformed nonempty output, including NUL-only records, fails closed.
- Current: Every empty token is skipped, so NUL-only or repeated-separator output returns a default clean status.

Evidence:
- The status parser initializes `PrimaryWorkspaceStatus::default()` and returns it after skipping empty NUL-delimited records. Eligibility uses `is_clean()` for the primary workspace.

Assumptions and limits:
- Git itself should not emit this malformed output; the defect concerns failure handling if the output stream is corrupted or an execution seam returns malformed bytes.

Reviewer action:
`block until fixed`

### F3 Major - Git observation is not mechanically side-effect-free

Impact: A nominally read-only observation may launch a configured helper or write trace output outside the observer's explicit operation model.
Review reason: The argument allowlist constrains argv, but Git consults repository configuration and inherited environment; `git status` can invoke `core.fsmonitor`, and `GIT_TRACE*` settings can cause writes.
Surface: `HostGit::run` and its `status` call.
Issue key: `behavior; entry=host read-only Git observation; contract=observation commands cannot invoke configured helpers or write trace data; effect=Phase 2 read-only boundary performs an unrequested external side effect`
Issue fingerprint: `ifp-sha256:0268c5a8363c3bbe414d0215e6a2ca8c8201b76ed030816b6017b07bb7bb11ec`
Expected basis: `kind:hard-invariant; strength:authoritative; evidence:user mission Phase 2 read-only boundary and docs/MANAGED_WORKTREES_V1_DESIGN.md §26`
Confidence: `high`
Origin: `R1-C4, R2-C5; independently verified by Coordinator`
Coordinator verification: `Verified that Command inherits environment/configuration and only adds --no-optional-locks; no fsmonitor override or trace-environment isolation is applied. This is a static finding; helper execution was not reproduced.`

Look here first:
- [`HostGit::run`](../../src/managed_worktree_observe.rs#L108)
- [`observe_repository` status call](../../src/managed_worktree_observe.rs#L229)

Failure mode:
- Expected: Discovery executes only host-bounded read-only behavior and cannot trigger Git-configured external helpers or tracing writes.
- Current: Allowed `git status` inherits config/environment, so an enabled fsmonitor helper can execute and Git trace variables can write files.

Evidence:
- Exact argv validation is not a restriction on Git's helper/config/environment behavior. `--no-optional-locks` does not disable configured fsmonitor hooks or inherited trace output.

Assumptions and limits:
- Exploitation depends on configuration or environment being present. The module is not wired into Goal lifecycle code in this Phase 2 patch, but the observer's own contract is explicitly read-only.

Reviewer action:
`block until fixed`

### F5 Major - Eligibility ignores durable lifecycle

Impact: Terminal, blocked, or already-active durable state can be presented as eligible for new worktree creation.
Review reason: Eligibility is named and documented as the creation precondition, so it must not claim eligibility for a record whose lifecycle disallows creation.
Surface: `ExpectedWorktreeTarget.lifecycle` and `classify_eligibility`.
Issue key: `behavior; entry=managed-worktree eligibility; contract=terminal or blocked lifecycle is never classified as eligible for new creation; effect=stale Goal state is presented as creation-eligible`
Issue fingerprint: `ifp-sha256:8162e5a7aa9dcd94503f3d1be706a951dc00bd85bd3a3452b22a576c93a59baa`
Expected basis: `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §§5, 11, 20 and src/managed_worktree.rs lifecycle contract`
Confidence: `high`
Origin: `R2-C2; independently verified by Coordinator`
Coordinator verification: `Confirmed lifecycle is populated by ExpectedWorktreeTarget::from_record but never read in classify_eligibility. The durable state model makes Removed terminal, and Blocked requires explicit recovery.`

Look here first:
- [`ExpectedWorktreeTarget::from_record`](../../src/managed_worktree_discovery.rs#L716)
- [`classify_eligibility`](../../src/managed_worktree_discovery.rs#L827)

Failure mode:
- Expected: Only a lifecycle state permitted to enter/continue the creation eligibility path can return Eligible; Blocked and Removed must not be eligible for a new creation.
- Current: The classifier checks repository facts only and can return Eligible for all lifecycle variants.

Evidence:
- The classifier never accesses `expected.lifecycle`; existing tests only exercise `Requested`.

Assumptions and limits:
- There are no production callers yet. This is a Phase 2 contract/model defect that would otherwise be easy for Phase 3 callers to mistake for a complete creation gate.

Reviewer action:
`block until fixed`

### F7 Major - Windows test fixtures use non-absolute Unix paths

Impact: The all-target test suite is expected to fail on Windows before the discovery tests can validate behavior.
Review reason: The required compatibility matrix includes Windows, and the fixture path shape is not absolute under Windows path semantics.
Surface: Test constants and `Fixture::new`.
Issue key: `behavior; entry=Windows Phase 2 pure tests; contract=fixtures use absolute Windows paths; effect=Windows all-target test suite fails while constructing records`
Issue fingerprint: `ifp-sha256:5146ea7271be2a66cb312d3503d889bdcd58ed18e0ac7d0b45e6cbbd30a377d6`
Expected basis: `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §25I`
Confidence: `high`
Origin: `R2-C4; independently verified by Coordinator`
Coordinator verification: `Traced Unix-rooted constants into ManagedWorktreeRecord::requested and its canonical-absolute-path validator. Windows requires a drive or UNC prefix for an absolute path; no Windows execution was available locally.`

Look here first:
- [Phase 2 path constants](../../src/managed_worktree_discovery_tests.rs#L30)
- [`Fixture::new`](../../src/managed_worktree_discovery_tests.rs#L104)

Failure mode:
- Expected: Pure fixtures use absolute, structurally canonical paths on each supported platform.
- Current: `/repo/primary` and `/managed/...` lack a Windows drive/UNC prefix; record construction calls `.unwrap()` after validation.

Evidence:
- Every `Fixture::new` used by the unconditional tests calls `ManagedWorktreeRecord::requested`, which rejects a path for which `Path::is_absolute()` is false.

Assumptions and limits:
- Windows behavior is inferred from Rust's Windows path model and the record validator; authoritative Windows CI remains necessary.

Reviewer action:
`block until fixed`

## Minor

### F4 Minor - Sequencer marker read follows links and is unbounded

Impact: Malformed sequencer metadata may be mistaken for absence, block observation, or consume excessive memory.
Review reason: The other operation markers use `symlink_metadata`; `sequencer/todo` is read directly, so it has a different and weaker failure boundary.
Surface: `observe_in_progress` reading `sequencer/todo`.
Issue key: `behavior; entry=sequencer marker observation; contract=operation markers are bounded and nonregular/symlink markers fail closed; effect=observation can hang, exhaust memory, or miss operation state`
Issue fingerprint: `ifp-sha256:e9b6450e70105313cca689bf4af044023dba0140cbd67ef0bccf5989955f61eb`
Expected basis: `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §§6, 11, 20`
Confidence: `medium`
Origin: `R1-C3; independently verified by Coordinator`
Coordinator verification: `Confirmed direct fs::read, unbounded contents, and treating ErrorKind::NotFound as no marker; no adversarial marker runtime test was run.`

Look here first:
- [`observe_in_progress`](../../src/managed_worktree_observe.rs#L348)
- [sequencer marker tests](../../src/managed_worktree_discovery_tests.rs#L452)

Failure mode:
- Expected: Operation metadata is inspected with bounded, fail-closed file-type handling; unreadable or ambiguous marker state cannot become no-operation.
- Current: A dangling symlink returns NotFound and is treated as absent; a FIFO can block `fs::read`; a large regular file is read without a bound.

Evidence:
- Marker path is obtained from `git rev-parse --git-path`, then passed directly to `fs::read`; the `NotFound` arm returns `Ok(())` without a prior `symlink_metadata` check.

Assumptions and limits:
- This requires malformed or concurrently changed Git administrative metadata. Other marker types are handled more conservatively.

Reviewer action:
`request focused verification`

### F6 Minor - Retained branch contradicts `RemovedExact` classification

Impact: A normal completed cleanup may be reported as a partial creation side effect, requiring explicit recovery despite no worktree remaining.
Review reason: The design explicitly retains the local branch by default after normal worktree removal.
Surface: The no-registration branch in `classify_reconciliation`.
Issue key: `behavior; entry=removed worktree reconciliation; contract=REMOVED permits the design retained branch when no worktree/path remains; effect=normal removal is classified as partial creation side effect`
Issue fingerprint: `ifp-sha256:ef939a4a1ce08fc3ec8dac59976fc3183206ca843f4ac5b3c788d7bc3fdc2bc1`
Expected basis: `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §§19–20`
Confidence: `high`
Origin: `R2-C3; independently verified by Coordinator`
Coordinator verification: `Verified the branch-ref existence check returns BranchOnlySideEffect before the Removed lifecycle case. Design §19 says the branch is retained by default and branch deletion is separate.`

Look here first:
- [`classify_reconciliation` no-registration branch](../../src/managed_worktree_discovery.rs#L1008)
- [Cleanup branch-retention contract](../../docs/MANAGED_WORKTREES_V1_DESIGN.md#L575)

Failure mode:
- Expected: `REMOVED` with no registration and an absent exact worktree path classifies as `RemovedExact` whether the local branch is retained or separately deleted, provided no worktree owns that branch.
- Current: An observed retained branch is classified as `BranchOnlySideEffect`; `RemovedExact` is reached only when the ref is absent.

Evidence:
- The classifier examines branch-ref existence before matching lifecycle. The `RemovedExact` variant comment also currently says “No registration and no branch,” contrary to design §19's default.

Assumptions and limits:
- This does not authorize branch deletion or any Git mutation; it concerns a read-only durable state classification only.

Reviewer action:
`request fix`

## Questions

None.

## Test Gaps

| ID | Severity | Surface | Missing coverage | Risk | Origin | Evidence | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `T1` | `Minor` | Eligibility with unobserved expected ref | No test proves an unqueried expected branch ref remains unknown and ineligible; the shared test helper turns `None` into `false`. | An accidental change from fail-closed to absent-ref behavior could escape the eligibility tests. | `R2, Coordinator` | Test helper `classify_eligibility` uses `unwrap_or(false)` at `src/managed_worktree_discovery_tests.rs:224-228`; production classifier handles `None`, but focused direct eligibility coverage is missing. | `test-gap; entry=eligibility ref observation; contract=unqueried expected branch ref remains unknown and is asserted ineligibility; gap=test helper coerces missing ref observation to absent` | `ifp-sha256:110068b4e924d98e9e81c0a90effd7b06730b1ae1b5d354d588d4589b5b32c49` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §§6, 11` |

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | Module/test wiring | `src/main.rs` | Coordinator | `dependency trace` | `Reviewed - no issue found` | Only module declarations and the test module are added; no lifecycle behavior is wired. | Full diff and Phase 1 baseline inspected. |
| `A2` | Worktree porcelain parser and inventory consistency | `src/managed_worktree_discovery.rs` parser/inventory | R1, Coordinator | `contract trace` | `Finding F1` | Parser preserves unknown attributes, but the global trust gate does not reject them across all inventory entries. | Parser tests include NUL handling, malformed attributes, duplicates, and unknown attributes on an expected managed entry. |
| `A3` | Host Git and repository observation | `src/managed_worktree_observe.rs` `HostGit`, identity, refs | R1, Coordinator | `contract trace` | `Finding F3` | Exact argv list is insufficient while Git config and inherited environment can trigger helpers/traces. | Requires host-side environment/config isolation and focused tests. |
| `A4` | Git operation-marker observation | `src/managed_worktree_observe.rs` marker readers | R1, Coordinator | `dependency trace` | `Finding F4` | Sequencer file is read without type/size bounds and dangling links become absence. | Add adversarial marker fixtures. |
| `A5` | Primary status parsing and cleanliness | `src/managed_worktree_observe.rs` `parse_primary_status` | R1, Coordinator | `contract trace` | `Finding F2` | Empty tokens in nonempty status output are silently ignored. | Add malformed NUL-only and delimiter tests. |
| `A6` | Eligibility and durable identity binding | `src/managed_worktree_discovery.rs` `ExpectedWorktreeTarget`, `classify_eligibility` | R2, Coordinator | `contract trace` | `Finding F5` | Durable lifecycle is carried but not enforced by eligibility. | Add Requested/Prepared/Blocked/Removed lifecycle tests. |
| `A7` | Stale/recovery classification and bounded retry | `src/managed_worktree_discovery.rs` `classify_reconciliation` | R2, Coordinator | `contract trace` | `Finding F6` | Removed state conflicts with the design's retained-branch cleanup outcome. | Add retained-branch `REMOVED_EXACT` regression test. |
| `A8` | Cross-platform fixtures and retry-negative coverage | `src/managed_worktree_discovery_tests.rs` | R2, Coordinator | `runtime verified` | `Finding F7` | macOS focused/full tests ran; Windows fixture path semantics are not portable. T1 separately records the missing direct unknown-ref eligibility assertion. | Run Windows matrix after implementation. |

## Subagent Candidate Adjudication

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `R1-C1` | R1 | `merged` | F1 | `RepositoryObservation::is_trustworthy` and both classifiers | R2 independently found the same unknown-attribute trust bypass. |
| `R1-C2` | R1 | `accepted` | F2 | `parse_primary_status` and clean status contract | NUL-only nonempty input is currently accepted as clean. |
| `R1-C3` | R1 | `accepted` | F4 | Direct `fs::read` and NotFound branch | File type/size handling differs from all other operation markers. |
| `R1-C4` | R1 | `accepted` | F3 | `HostGit::run` inherits config/environment | Exact argv allowlisting does not constrain Git helper/environment behavior. |
| `R1-C5` | R1 | `dismissed` | None | `parse_worktree_list_porcelain_z` and observer primary registration check | Permissive empty tokens do not independently create a false safe classification; malformed trailing truncation is rejected and inventory consistency remains required. |
| `R1-C6` | R1 | `dismissed` | None | `same_path_identity` and fail-closed path matching | Windows Unicode case mismatch may deny a valid path but does not falsely prove ownership; retain as a cross-platform test limitation, not a current unsafe-acceptance finding. |
| `R2-C1` | R2 | `merged` | F1 | Same trust-gate trace as R1-C1 | Duplicate of R1-C1. |
| `R2-C2` | R2 | `accepted` | F5 | `ExpectedWorktreeTarget::from_record` and `classify_eligibility` | The lifecycle is never checked by the eligibility verdict. |
| `R2-C3` | R2 | `accepted` | F6 | `classify_reconciliation` compared with design §19 | Normal retained branch is misclassified for Removed lifecycle. |
| `R2-C4` | R2 | `accepted` | F7 | Constants and record validation path | Unix-rooted paths are not absolute on Windows. |
| `R2-C5` | R2 | `merged` | F3 | `HostGit::run` static trace | Same helper/config concern as R1-C4; no separate finding. |
| `R2-D1` | R2 | `dismissed` | None | `Reconciliation::permits_bounded_retry` and direct unknown-ref test | Production reconciliation keeps `None` ambiguous; test helper masking is separately recorded as T1. |
| `R2-D2` | R2 | `dismissed` | None | `src/main.rs` diff and module contracts | No PRIMARY execution behavior is wired or changed in this Phase 2 diff. |

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `src/main.rs` | surface | Module/test wiring only; PRIMARY execution path unchanged. |
| `src/managed_worktree_discovery.rs` | surface | Porcelain parsing, normalized inventory, path identity, trust, eligibility, retry and stale-state classifications. |
| `src/managed_worktree_observe.rs` | surface | Git allowlist/process environment, repository identity, status, operation markers, refs, filesystem path observation. |
| `src/managed_worktree_discovery_tests.rs` | test-only | Parser boundaries, negative authority cases, classification, temporary-repository integration and platform fixtures. |

### Verification Commands

- `cargo fmt --check` -> passed before code-review findings were addressed.
- `cargo test --locked --all-targets managed_worktree_discovery` -> 53 passed, 723 filtered; passed at the review snapshot.
- `cargo test --locked --all-targets` -> 776 passed; passed at the review snapshot.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> initially reported lint issues; focused lint fixes were applied before the review snapshot, then the command passed.
- `git diff --check` -> passed for tracked diff; untracked-file whitespace is included in the final staged/commit verification, not claimed as checked by this invocation.
- GitHub Actions -> not run at review time.

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| F1 | trust gate | [`RepositoryObservation::is_trustworthy`](../../src/managed_worktree_discovery.rs#L678) | Global observation trust fails to inspect unknown attributes. |
| F1 | retry path | [`classify_reconciliation`](../../src/managed_worktree_discovery.rs#L974) | The trust verdict guards all automatic classifications. |
| F2 | status parser | [`parse_primary_status`](../../src/managed_worktree_observe.rs#L403) | Empty tokens are skipped regardless of nonempty malformed input. |
| F3 | process boundary | [`HostGit::run`](../../src/managed_worktree_observe.rs#L108) | Git inherits configuration/environment and can run helpers. |
| F4 | metadata reader | [`observe_in_progress`](../../src/managed_worktree_observe.rs#L348) | Sequencer todo is read directly and without a bound. |
| F5 | policy classifier | [`classify_eligibility`](../../src/managed_worktree_discovery.rs#L827) | Lifecycle is carried by expected target but not considered. |
| F6 | stale state | [`classify_reconciliation`](../../src/managed_worktree_discovery.rs#L1008) | Branch existence is handled before the Removed lifecycle state. |
| F7 | Windows test fixture | [`Fixture::new`](../../src/managed_worktree_discovery_tests.rs#L104) | Absolute-path validation is called with Unix-rooted constants. |
| T1 | test helper | [eligibility test helper](../../src/managed_worktree_discovery_tests.rs#L218) | Missing ref observation is converted to absence in tests. |

### Dismissed Coordinator Candidates

| Candidate | Decision | Evidence |
| --- | --- | --- |
| Extra empty separators in worktree-list parser | `dismissed` | They do not independently turn a missing/contradictory primary registration into a trusted inventory; observer requires the primary top-level exactly once. |
| Windows Unicode case-path mismatch | `dismissed` as unsafe-acceptance finding | Current comparison could conservatively refuse a real path but cannot prove an incorrect path identity; retain as a Windows verification limitation. |
| Non-UTF-8 worktree-list bytes | `dismissed` | Observer rejects invalid UTF-8 as unavailable instead of lossy-decoding or classifying. |
| Unknown-ref retry behavior in production reconciliation | `dismissed` | The direct unknown-ref test and classifier show `None` returns `Ambiguous`; only the shared test helper masks it, captured by T1. |
| PRIMARY regression | `dismissed` | The tracked diff only registers modules and tests; no production caller or Goal behavior is wired. |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A3` | No runtime reproduction with repository `core.fsmonitor` configuration or inherited `GIT_TRACE*` environment. | Confirms the conditional side-effect path and verifies its mitigation. | Add a temporary-repository test with a harmless helper and trace environment after isolating the Git process. |
| `A8` | No Windows/macOS/Linux CI matrix run at report time. | Confirms platform path fixture and Git behavior. | Push the bounded branch and observe the authoritative Actions matrix. |

## Prior Resolution Reconciliation

None - initial review generation.

## Receiving Handoff

- Handoff status: `Ready for receiving-code-review`
- Automatic receiving permitted: `Yes`
- Source report ID: `cr-20260930-20c7fb26`
- Scope fingerprint to recheck: `sha256:0a0f47521b0dac64a160c5e0786b8897bc25fb6f3ca44a10d517dfd33bf8fed1`
- Actionable finding IDs: `F1, F2, F3, F4, F5, F6, F7`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `T1`
- Deferred test-gap IDs: `None`
- Open question IDs: `None`
- Open coverage area IDs: `None`
- Highest-risk verification to repeat: `Prove host Git calls cannot invoke configured helpers or inherited trace output, and run Windows all-target tests.`
- Suggested implementation boundaries: `Only the four Phase 2 paths; no Goal lifecycle or Git mutation authority.`
- Re-review note: `Treat every finding as a claim to verify. Challenges require a counterclaim, argument, evidence, limits, and settlement criterion.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or product owner.`

## Report Self-Check

- `yes` Actual assessment mode and rationale are recorded: coordinator assessment plus two bounded read-only specialists.
- `yes` Every changed review-relevant or unknown-impact area appears once in `Review Coverage Ledger`.
- `yes` Every final finding appears once in the index and once as a matching card.
- `yes` Every `Finding F#` area references an existing finding.
- `yes` Every standalone test gap has a stable ID and severity.
- `yes` Every `F#` and `T#` has a unique semantic issue fingerprint and an authoritative expected basis, or the item is an explicit `Question` for unconfirmed intent.
- `yes` Generation, trigger, parent resolution, scope mode, and receiving handoff satisfy the bounded chain contract.
- `yes` Generation `1` reconciliation is not applicable to this initial report.
- `yes` Every non-Question finding and standalone test gap is actionable; there are no open questions or Not-covered areas.
- `yes` Every meaningful subagent candidate has an adjudication.
- `yes` Every `Not covered` area has a reason and next step; there are no Not-covered rows.
- `yes` Recommendation follows the skill mapping: unresolved Major findings produce `Changes requested`.
- `pending` The validator must pass before this report is considered complete.
- `yes` No Git state was mutated during review.
