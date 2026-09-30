# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20260930-74558c0d`
- Review chain ID: `rc-20260930-7316d6e7`
- Review generation: `1`
- Review trigger: `post-implementation`
- Parent review report ID: `cr-20260930-20c7fb26`
- Parent review report path: `tmp/reviews/2026-09-30-code-review-report-20c7fb26.md`
- Parent resolution ID: `rr-20260930-967f5c8a`
- Parent resolution path: `tmp/reviews/2026-09-30-receiving-resolution-967f5c8a.md`
- Generated at: `2026-09-30T00:55:10Z`
- Report path: `tmp/reviews/2026-09-30-code-review-generation-1-74558c0d.md`
- Source skill: `code-review`
- Status: `Review incomplete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:be9d714ad4331d20cb26919941c7173b0e646eb1c7c8b186dd525e940e39cadc`

## Scope

- Review date: `2026-09-30`
- Scope kind: `working tree`
- Scope description: Generation-1 re-review of the Phase 2 fixes adjudicated in `rr-20260930-967f5c8a`, plus directly affected status parsing, eligibility/reconciliation, Git observer, and test chains.
- Scope mode: `implementation delta plus affected execution chains`
- Baseline: `Generation-0 target identified by report cr-20260930-20c7fb26 and scope fingerprint sha256:0a0f47521b0dac64a160c5e0786b8897bc25fb6f3ca44a10d517dfd33bf8fed1`
- Target: `working tree on managed-worktrees/v1-phase1-2 at HEAD 35f70ef2c40ccb7b81ad09814ca8dc798cd86b9f`
- Changed paths: `3 implementation paths; src/managed_worktree_discovery.rs, src/managed_worktree_observe.rs, and src/managed_worktree_discovery_tests.rs`
- Diff size: `Unavailable - the generation-0 uncommitted source snapshot is frozen by report fingerprint rather than a Git object; this review is limited to the F1-F7/T1 resolution and its directly affected chains.`
- Completion: `Incomplete - native Windows execution/CI was unavailable; exact uncovered area A5 is recorded below.`
- Requirements consulted: `Parent report cr-20260930-20c7fb26; full parent resolution rr-20260930-967f5c8a; docs/MANAGED_WORKTREES_V1_DESIGN.md §§5, 6, 11, 19, 20, 23, 25, 26; SECURITY.md.`
- Prior resolution consulted: `rr-20260930-967f5c8a at tmp/reviews/2026-09-30-receiving-resolution-967f5c8a.md`
- Assumptions: `Phase 2 remains internal and read-only; later lifecycle wiring must treat its observations as evidence, not authority.`
- Excluded as unrelated: `Phase 1 schema implementation already present at baseline; Phase 3 creation and every later execution/cleanup phase.`

## Review Orchestration

- Assessment subagent: `Coordinator assessment - the bounded delta has independent parser/host-side-effect and lifecycle/platform risks, and both chains need fresh verification.`
- Orchestration decision: `Parallel specialists`
- Decision confidence: `high`
- Decision rationale: `R1 independently rechecked the read-only process and parsing fixes; R2 independently rechecked lifecycle, retry, removal, and cross-platform fixtures. The coordinator adjudicated all candidates against the frozen parent decisions.`
- Coordinator override: `None`
- Context or tool limits: `Native Windows execution and GitHub Actions were unavailable; agents remained read-only and did not run Git-mutating temporary-repository tests.`

### Risk Dimensions

- `Malformed host observation must not be interpreted as a clean primary workspace.`
- `Lifecycle/ref/path combinations must not turn blocked or removed state into a new creation verdict or retry.`
- `Windows path and test behavior needs native matrix evidence beyond static review.`

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `R1` | Parser and host observation | Status NUL parser, Git config/environment isolation, sequencer marker handling, related tests | F1-F4/T1 recheck; malformed status bytes; exact read-only boundary | `Complete` |
| `R2` | Lifecycle and platform tests | Eligibility lifecycle, RemovedExact, ref/path retry handling, portable test fixtures | F5-F7/T1 recheck; blocked/terminal outcomes; Windows path shape | `Complete` |
| `Coordinator` | Integration and issue adjudication | Both bounded review chains | Independently re-read each accepted candidate and parent disposition | `Complete` |

### Synthesis Statement

The coordinator re-read the status parser and directly confirmed that a record containing two blank XY status bytes plus a path is accepted as clean. R1's finding reopens parent F2 under the same semantic fingerprint. Parent F1, F3, F4, F5, F6, F7, and T1 are otherwise closed by the implementation and focused tests. R2 identified two narrow missing assertions; the current code paths appear correct, but those changed boundaries are not directly protected. Native Windows behavior remains unverified, so this report is incomplete for that platform surface. This generation is terminal.

## Review Snapshot

- Recommendation: `Changes requested`
- Completion: `Incomplete - native Windows execution unavailable; A5 remains not covered.`
- Why now: `A malformed status record can still pass the clean-primary gate, and two changed reconciliation/lifecycle boundaries lack direct tests.`
- Must-review now:
  1. `F1` Blank XY status records still classify as clean
  2. `T1` RemovedExact ref-presence matrix is not fully tested
  3. `A5` Native Windows execution remains outstanding
- Findings count: `Blocker 0 | Major 1 | Minor 0 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 2`
- Coverage confidence: `medium`
- Biggest blind spot: `Native Windows all-target execution and authoritative GitHub Actions matrix.`

## Complete Findings Index

| ID | Severity | Surface | Review risk | Confidence | Origin | Verification | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `F1` | `Major` | Primary status parser | A malformed record with blank XY status bytes is treated as clean and can pass the primary-clean gate. | `high` | `R1, Coordinator` | Re-read exact parser branch; independently traced all-space XY record through default status to eligibility. | `behavior; entry=primary status observation; contract=malformed nonempty NUL status output is rejected rather than treated clean; effect=dirty-primary gate can accept an invalid observation` | `ifp-sha256:4a585823823e0bfd9dfc79801afdf103c89198604cc6469bfdefb2d1bb27dc38` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §6` |

## Blocker

None.

## Major

### F1 Major - Blank XY status record still passes as clean

Impact: An invalid nonempty status observation can still satisfy the clean-primary precondition for managed eligibility.
Review reason: The parser now validates NUL framing and record length, but it still accepts an impossible all-space status pair and returns no dirty flags.
Surface: `parse_primary_status` and its eligibility consumer.
Issue key: `behavior; entry=primary status observation; contract=malformed nonempty NUL status output is rejected rather than treated clean; effect=dirty-primary gate can accept an invalid observation`
Issue fingerprint: `ifp-sha256:4a585823823e0bfd9dfc79801afdf103c89198604cc6469bfdefb2d1bb27dc38`
Expected basis: `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §6`
Confidence: `high`
Origin: `R1-C1; independently verified by Coordinator`
Coordinator verification: `Confirmed record b"   filename\0" meets current length/separator checks; both status bytes equal spaces, so no status flags are set and is_clean() remains true.`

Look here first:
- [`parse_primary_status`](../../src/managed_worktree_observe.rs#L445)
- [`classify_eligibility` clean-primary check](../../src/managed_worktree_discovery.rs#L889)

Failure mode:
- Expected: Any malformed nonempty status record is rejected; Git omits clean files rather than emitting an all-space XY record.
- Current: A record with `record[0] == b' '` and `record[1] == b' '` passes shape validation and is ignored by the flag-setting branch.

Evidence:
- Existing malformed-output tests cover NUL-only, repeated NUL, and missing terminator but not a blank XY prefix.
- The parser only validates record length and the third-byte separator before interpreting XY values.

Assumptions and limits:
- Git does not normally emit this record. The defect concerns fail-closed handling of malformed output supplied to the observation parser.

Reviewer action:
`block until fixed`

## Minor

None.

## Questions

None.

## Test Gaps

| ID | Severity | Surface | Missing coverage | Risk | Origin | Evidence | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `T1` | `Minor` | `REMOVED` reconciliation | Add direct tests for the expected ref observed absent and unobserved cases, in addition to retained/present. | The changed ref-observation boundary could regress while the current test remains green. | `R2` | `removed_worktree_is_exact_when_the_design_retains_its_branch` only covers a present ref; classifier has distinct `Some(_)` and `None` branches. | `test-gap; entry=removed worktree reconciliation; contract=retained branch may be present, absent, or unobserved; gap=only retained and observed-present ref case is tested` | `ifp-sha256:cd245d02958465617383a2d7f4fca1b5323c69509af631a2e4d4c386bd2de81c` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §§19–20` |
| `T2` | `Minor` | Blocked record eligibility | Add a test proving a `BLOCKED` record remains ineligible even when `ExpectedWorktreeTarget` is created from a valid durable intent. | A future lifecycle-gate refactor could accidentally let an intent override the explicit recovery-only boundary. | `R2` | Current lifecycle loop tests `BLOCKED` without an intent; `from_record_and_intent` permits a blocked record for exact reconciliation. | `test-gap; entry=blocked worktree eligibility; contract=valid prepared intent does not permit Blocked lifecycle eligibility; gap=blocked record is only tested without an attached intent` | `ifp-sha256:85f6400a333e1b48bf6dfce8f5bbe76e1c3648e7486e4fe9c72858d66f9d2687` | `kind:approved-design; strength:authoritative; evidence:docs/MANAGED_WORKTREES_V1_DESIGN.md §§5, 11, 20` |

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | Status NUL parsing | `src/managed_worktree_observe.rs::parse_primary_status`; observer malformed-output tests | R1, Coordinator | `contract trace` | `Finding F1` | NUL framing is checked, but blank XY bytes still return a clean status. | Add blank-XY negative test and reject impossible status pairs. |
| `A2` | Host Git read-only invocation | `src/managed_worktree_observe.rs::HostGit::run`; fsmonitor/environment tests | R1, Coordinator | `runtime verified` | `Reviewed - no issue found` | Configured fsmonitor is disabled, inherited GIT_* overrides are removed, and the isolated helper test verifies no invocation. | Host PATH is assumed to be the trusted application environment; no PATH threat is asserted by the contract. |
| `A3` | Sequencer operation marker | `src/managed_worktree_observe.rs::observe_in_progress` | R1, Coordinator | `dependency trace` | `Reviewed - no issue found` | Stable symlink/nonregular/oversized marker states fail closed and reads are bounded. Same-user concurrent filesystem races remain a general observation limit. | Phase 2 never treats an observation as mutation authority. |
| `A4` | Eligibility and RemovedExact lifecycle/retry | `src/managed_worktree_discovery.rs` classifiers; focused tests | R2, Coordinator | `contract trace` | `Reviewed - no issue found` | Lifecycle gate rejects blocked/terminal states; RemovedExact handles retained/absent refs and unknown refs conservatively. | Add T1/T2 assertions to lock changed boundaries. |
| `A5` | Windows test/runtime behavior | Platform-specific fixture constants and discovery tests | R2, Coordinator | `diff-only` | `Not covered` | Static review finds drive-qualified fixture roots, but no native Windows run was available. | Run Windows all-target tests and CI matrix before declaring cross-platform verification complete. |
| `A6` | Module wiring / phase boundary | `src/main.rs`; discovery/observe call graph | Coordinator | `dependency trace` | `Reviewed - no issue found` | Modules are registered, but no Goal lifecycle or production mutation call site is wired. | Preserve Phase 3 boundary. |

## Subagent Candidate Adjudication

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `R1-C1` | R1 | `accepted` | F1 | `parse_primary_status` lines 445–489 and `PrimaryWorkspaceStatus::is_clean` | Blank XY record is malformed but reaches a default clean status. |
| `R1-C2` | R1 | `dismissed` | None | `HostGit::run` plus fsmonitor negative test | Parent F3 fix is present and runtime test confirms configured helper was not run. |
| `R1-C3` | R1 | `dismissed` | None | Marker `symlink_metadata`, file type/length guard, bounded read | Same-user replacement races remain general TOCTOU; stable malformed entries are rejected, and no lifecycle authority consumes this as a mutation grant. |
| `R1-C4` | R1 | `dismissed` | None | Host-owned executable/environment assumption and user contract | PATH-shim risk is not established as a defect within this bounded read-only model. |
| `R2-C1` | R2 | `represented by test gap` | T1 | `RemovedExact` match at `src/managed_worktree_discovery.rs:1047-1053` and test at `src/managed_worktree_discovery_tests.rs:1151-1168` | Absent and unknown-ref branches are distinct but only present-ref branch is tested. |
| `R2-C2` | R2 | `represented by test gap` | T2 | Lifecycle gate at `src/managed_worktree_discovery.rs:856-863`; blocked lifecycle test at `src/managed_worktree_discovery_tests.rs:1688-1739` | Test does not attach a valid intent to the blocked fixture. |
| `R2-C3` | R2 | `dismissed` | None | Platform-specific fixture roots and static code review | Parser-only Unix-form strings are not passed through durable record path validation. |

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `src/managed_worktree_discovery.rs` | surface | Observation trust, eligibility lifecycle, RemovedExact/no-side-effect reconciliation. |
| `src/managed_worktree_observe.rs` | surface | Host Git execution, status parsing, operation-marker reads. |
| `src/managed_worktree_discovery_tests.rs` | test-only | Negative status cases, helper/environment behavior, lifecycle/ref/path matrix, platform fixtures. |
| `src/main.rs` | affected dependency, unchanged in generation 1 delta | Module registration only; no production Goal lifecycle call site. |

### Verification Commands

- `cargo fmt --check` -> passed after the generation-0 resolution.
- `cargo test --locked --all-targets managed_worktree_discovery` -> 61 passed, 723 filtered on macOS.
- `cargo test --locked --all-targets` -> 784 passed, 0 failed on macOS.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> passed.
- `git diff --check` -> passed for tracked diff at review time; final staged check must include new files.
- Generation-0 report validator -> passed for `cr-20260930-20c7fb26`.
- Generation-1 report validator -> pending.
- GitHub Actions/native Windows -> not run yet.

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| F1 | malformed status branch | [`parse_primary_status`](../../src/managed_worktree_observe.rs#L445) | Blank XY bytes are not rejected. |
| F1 | policy consumer | [`classify_eligibility`](../../src/managed_worktree_discovery.rs#L889) | Clean status is one creation-eligibility precondition. |
| T1 | removed ref branches | [`classify_reconciliation`](../../src/managed_worktree_discovery.rs#L1047) | Present/absent ref is accepted; unknown remains ambiguous. |
| T1 | current regression coverage | [RemovedExact test](../../src/managed_worktree_discovery_tests.rs#L1151) | Only observed-present ref currently has a direct test. |
| T2 | blocked lifecycle gate | [`classify_eligibility`](../../src/managed_worktree_discovery.rs#L856) | Valid intent must not override blocked lifecycle. |
| A5 | platform fixtures | [Path constants](../../src/managed_worktree_discovery_tests.rs#L31) | Static drive-qualified path review; native execution remains outstanding. |

### Dismissed Coordinator Candidates

| Candidate | Decision | Evidence |
| --- | --- | --- |
| Sequencer metadata replacement race | `dismissed for this bounded review` | Same-user concurrent filesystem mutation is an inherent observation TOCTOU; stable symlinks/nonregular files are rejected, reads are bounded, and no mutation authority is granted. Preserve as an explicit residual risk. |
| PATH can resolve a Git shim | `dismissed for this bounded review` | The host process environment is treated as host-owned; the model cannot alter PATH, and no contract requires arbitrary untrusted environment isolation. If that assumption changes, resolve host Git identity before production wiring. |
| Remaining parser-only slash-rooted fixture strings | `dismissed` | Those tests exercise porcelain parsing only; they do not pass those strings into durable record canonical-path validation. |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A5` | No native Windows all-target test or GitHub Actions matrix result was available. | Cannot claim the platform-specific fixtures and Git observer compile/run on Windows. | Push the branch and inspect `test windows-x64` and `compat windows-2022/windows-arm64` results. |

## Prior Resolution Reconciliation

| Issue key | Issue fingerprint | Parent item/verdict | Relevant change or new evidence | Decision |
| --- | --- | --- | --- | --- |
| `behavior; entry=repository eligibility and reconciliation; contract=any unrecognized registered worktree attribute makes inventory ambiguous; effect=incomplete inventory is accepted as eligible or retryable` | `ifp-sha256:79307a23a9dd485aeefe00d88ddd540ae7f9dfbfa17c7dc93d15f68746c46ad2` | `F1 Fixed` | `None` | `kept closed` |
| `behavior; entry=primary status observation; contract=malformed nonempty NUL status output is rejected rather than treated clean; effect=dirty-primary gate can accept an invalid observation` | `ifp-sha256:4a585823823e0bfd9dfc79801afdf103c89198604cc6469bfdefb2d1bb27dc38` | `F2 Fixed` | `kind:code; ref:src/managed_worktree_observe.rs:445-489; change:the parser still accepts a two-space XY prefix with a path and leaves cleanliness flags false` | `reopened as F1; omitted malformed-status boundary` |
| `behavior; entry=host read-only Git observation; contract=observation commands cannot invoke configured helpers or write trace data; effect=Phase 2 read-only boundary performs an unrequested external side effect` | `ifp-sha256:0268c5a8363c3bbe414d0215e6a2ca8c8201b76ed030816b6017b07bb7bb11ec` | `F3 Fixed` | `None` | `kept closed` |
| `behavior; entry=sequencer marker observation; contract=operation markers are bounded and nonregular/symlink markers fail closed; effect=observation can hang, exhaust memory, or miss operation state` | `ifp-sha256:e9b6450e70105313cca689bf4af044023dba0140cbd67ef0bccf5989955f61eb` | `F4 Fixed` | `None` | `kept closed; general same-user race documented as residual limit` |
| `behavior; entry=managed-worktree eligibility; contract=terminal or blocked lifecycle is never classified as eligible for new creation; effect=stale Goal state is presented as creation-eligible` | `ifp-sha256:8162e5a7aa9dcd94503f3d1be706a951dc00bd85bd3a3452b22a576c93a59baa` | `F5 Fixed` | `None` | `kept closed` |
| `behavior; entry=removed worktree reconciliation; contract=REMOVED permits the design retained branch when no worktree/path remains; effect=normal removal is classified as partial creation side effect` | `ifp-sha256:ef939a4a1ce08fc3ec8dac59976fc3183206ca843f4ac5b3c788d7bc3fdc2bc1` | `F6 Fixed` | `None` | `kept closed` |
| `behavior; entry=Windows Phase 2 pure tests; contract=fixtures use absolute Windows paths; effect=Windows all-target test suite fails while constructing records` | `ifp-sha256:5146ea7271be2a66cb312d3503d889bdcd58ed18e0ac7d0b45e6cbbd30a377d6` | `F7 Fixed` | `None` | `kept closed; native execution remains open coverage A5` |
| `test-gap; entry=eligibility ref observation; contract=unqueried expected branch ref remains unknown and is asserted ineligibility; gap=test helper coerces missing ref observation to absent` | `ifp-sha256:110068b4e924d98e9e81c0a90effd7b06730b1ae1b5d354d588d4589b5b32c49` | `T1 Fixed` | `None` | `kept closed` |

## Receiving Handoff

- Handoff status: `Terminal post-review - return to user/owner`
- Automatic receiving permitted: `No`
- Source report ID: `cr-20260930-74558c0d`
- Scope fingerprint to recheck: `sha256:be9d714ad4331d20cb26919941c7173b0e646eb1c7c8b186dd525e940e39cadc`
- Actionable finding IDs: `F1`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `T1, T2`
- Deferred test-gap IDs: `None`
- Open question IDs: `None`
- Open coverage area IDs: `A5`
- Highest-risk verification to repeat: `Add a negative parser test for a blank XY record, prove it fails closed, and run Windows all-target CI.`
- Suggested implementation boundaries: `The status parser and directly affected Phase 2 tests only; if work resumes after this generation-1 report, start a new review generation-0 chain.`
- Re-review note: `Generation 1 is terminal. Any later review must start a new generation-0 chain; do not automatically invoke receiving-code-review.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or product owner.`

## Report Self-Check

- `yes` Actual assessment mode and rationale are recorded: coordinator assessment plus two bounded specialists.
- `yes` Every changed review-relevant or unknown-impact area appears once in `Review Coverage Ledger`.
- `yes` Every final finding appears once in the index and once as a matching card.
- `yes` Every `Finding F#` area references an existing finding.
- `yes` Every standalone test gap has a stable ID and severity.
- `yes` Every `F#` and `T#` has a unique semantic issue fingerprint and authoritative expected-behavior basis.
- `yes` Generation, trigger, parent resolution, scope mode, and receiving handoff satisfy the bounded chain contract.
- `yes` Every overlapping parent resolution issue is reconciled; F2 reopens with a concrete code delta.
- `yes` Every non-Question finding and test gap is assigned exactly once; Not-covered area A5 is in the open coverage list.
- `yes` Every meaningful subagent candidate has an adjudication.
- `yes` The Not-covered area A5 has a reason and specific next verification.
- `yes` Recommendation follows the skill mapping: unresolved Major F1 produces `Changes requested`.
- `pending` The generation-1 validator must pass before this report is considered complete.
- `yes` No Git state was mutated during review.
