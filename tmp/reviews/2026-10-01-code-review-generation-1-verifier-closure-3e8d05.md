# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261001-3e8d05`
- Review chain ID: `rc-20261001-7c1d4e`
- Review generation: `1`
- Review trigger: `post-implementation`
- Parent review report ID: `cr-20261001-7c1d4e`
- Parent review report path: `tmp/reviews/2026-10-01-code-review-report-verifier-closure-7c1d4e.md`
- Parent resolution ID: `rr-20261001-9ab2f1`
- Parent resolution path: `tmp/reviews/2026-10-01-receiving-code-review-resolution-verifier-closure-9ab2f1.md`
- Generated at: `2026-10-01T00:00:00Z`
- Report path: `tmp/reviews/2026-10-01-code-review-generation-1-verifier-closure-3e8d05.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:7f0a1c2b9d4e5a6f8c7b3d2e1f0a9b8c7d6e5f4a3b2c1d0e9f8a7b6c5d4e3f2`

Treat this completed report as the fixed review input for downstream work. Do not rewrite it during receiving or implementation; record dispositions, challenges, code changes, and verification in a separate `receiving-code-review` resolution report that references this Report ID.

## Scope

- Review date: 2026-10-01
- Scope kind: `working tree`
- Scope description: The implementation delta that answers the generation-0 findings and test gaps, and the execution chains it can affect: the verifier command-exit branch, the command authority table, the git argv classifier, the observation seam, the side-effect state model, and the three changed test surfaces.
- Scope mode: `implementation delta plus affected execution chains`
- Baseline: `f899ad31033540d666389d2520fd040fe267cbc7` plus the generation-0 reviewed working tree
- Target: `working tree` on `security/verifier-classifier-closure-v1`
- Changed paths: 8
- Diff size: 1091 insertions and 205 deletions
- Completion: `Complete within reviewed scope`
- Requirements consulted: the parent review and resolution, the task brief sections 0 through 27, `SECURITY.md`, and the frozen design documents cited by the parent.
- Prior resolution consulted: `rr-20261001-9ab2f1` at `tmp/reviews/2026-10-01-receiving-code-review-resolution-verifier-closure-9ab2f1.md`
- Assumptions: the resolution's dispositions are claims to verify, not facts; the coordinator re-read each changed hunk rather than trusting the resolution's summary.
- Excluded as unrelated: the pre-existing untracked `tmp/reviews/*` artifacts and the brief's out-of-scope audit items.

## Review Orchestration

- Assessment subagent: `Coordinator assessment - the delta is thirteen findings answered inside three files plus three test surfaces, every disposition is individually checkable, and a specialist pass would re-read the same small context without adding independent evidence`
- Orchestration decision: `Single reviewer`
- Decision confidence: `high`
- Decision rationale: the generation-0 report already established the risk model and the three specialists already covered the surfaces; the delta is narrow and every finding has a concrete, independently checkable expected behavior.
- Coordinator override: `None`
- Context or tool limits: no windows or linux execution.

### Risk Dimensions

- A fix that removes the finding but reopens another, in particular the two directions of git classification.
- A test that asserts the right property for the wrong reason, which was the parent's own failure mode.
- A behavioral change that is intentional and must be labelled as such rather than re-reported as a defect.

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `Coordinator` | Delta correctness and re-review of the fixed chains | every hunk changed by the resolution | the parent's cross-checks still hold; no new unclassified shape appears; no fix relies on a weakened assertion | Complete |

### Synthesis Statement

Each of the thirteen findings and three test gaps was re-verified by re-reading the changed code and, where the finding depended on git semantics, by comparing the new table against the reproduction already performed. Two dispositions were deliberately not followed in code: the open product question, which the brief instructs to report rather than guess at, and the pre-existing parser weakness, which is baseline behavior and would change verification semantics. No specialist was launched for this generation.

## Review Snapshot

- Recommendation: `Discuss`
- Completion: `Complete within reviewed scope`
- Why now: every actionable finding is closed in code, the delta introduces no new unclassified shape, and the two remaining items are a product question and a platform leg that this host cannot execute, so the review returns them to the owner rather than claiming a pass.
- Must-review now:
  1. `F1` the executable substitution must be on every path that reaches execution
  2. `F2` the classifier must still fail closed for unknown rendering flags
  3. `F14` the open product question must be reported, not coded around
- Findings count: `Blocker 0 | Major 0 | Minor 0 | Question 1`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 0`
- Coverage confidence: `high` for unix semantics, `medium` for the windows and linux legs
- Biggest blind spot: still no windows or linux execution; the matrix is the first real check.

## Complete Findings Index

| ID | Severity | Surface | Review risk | Confidence | Origin | Verification | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `F14` | Question | verification specification | approval question | high | `Coordinator` | contract trace | `behavior; entry=verification command exit; contract=unconfirmed product expectation for command exit satisfiability; effect=approval question` | `ifp-sha256:79c97d156bc3ae58a060ea89a966e21579a9c7e532bf659deb4fbf750898dde0` | `kind:product-intent; strength:unavailable; evidence:no frozen document states whether a command exit verification is expected to complete on a sandboxed unix host` |

## Blocker

`None.`

## Major

`None.`

## Minor

`None.`

## Questions

### F14 Question - A command exit verification is satisfiable on no platform today

Approval impact: a shipped verification kind that can never pass is a product decision; this review cannot close it, and shipping without an explicit decision means every plan that uses it blocks its task
Needed context: whether a command exit verification is expected to complete on a sandboxed unix host, and if not, whether the kind should be refused at plan materialization
Surface: verification specification
Issue key: `behavior; entry=verification command exit; contract=unconfirmed product expectation for command exit satisfiability; effect=approval question`
Issue fingerprint: `ifp-sha256:79c97d156bc3ae58a060ea89a966e21579a9c7e532bf659deb4fbf750898dde0`
Expected basis: `kind:product-intent; strength:unavailable; evidence:no frozen document states whether a command exit verification is expected to complete on a sandboxed unix host`
Confidence: high
Origin: `Coordinator`
Coordinator verification: re-traced the chain after the delta; the substitution of the host executable does not change the lifecycle, and the platform gate remains for the command path

Look here first:
- [sandbox lifecycle](/Users/yuta/local-mcp-connector-parity/src/sandbox.rs#L218)
- [pass rule](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L558)

Evidence:
- the wrapper reports an unproven start on linux and macos, the command-finished flag derives from it, and the pass rule requires it
- the delta narrowed what may be proposed but cannot change what the sandbox can prove

Settlement criterion:
- an explicit product decision either that the kind is inert and should be refused at plan materialization, or that a host-owned observation seam is authorized for model-authored argv, which would be new execution authority and needs its own design

Reviewer action:
`ask owner`

## Test Gaps

`None.`

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | executable identity on every execution path | [command exit branch](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L513) | `Coordinator` | dependency trace | `Reviewed - no issue found` | re-read the branch: classification, then the host substitution, then the single call into the runner; no other path spawns a model-authored argv |
| `A2` | allowlist consistency with the classifier | [allowlist](/Users/yuta/local-mcp-connector-parity/src/verifier_command_authority.rs#L134) | `Coordinator` | contract trace | `Reviewed - no issue found` | every remaining approved tail classifies as the read class, asserted by the invariant test; the two divergent tails were removed rather than narrowed |
| `A3` | classifier polarity after the table moves | [listing decision](/Users/yuta/local-mcp-connector-parity/src/git_command_class.rs#L244) | `Coordinator` | runtime verified | `Reviewed - no issue found` | the attached-value matching widens the mutation check and narrows nothing; an unlisted flag with an attached value still falls through |
| `A4` | global option parsing | [detached value branch](/Users/yuta/local-mcp-connector-parity/src/git_command_class.rs#L131) | `Coordinator` | contract trace | `Reviewed - no issue found` | the new condition is a strict superset of the old one, so no previously placed shape became unplaced |
| `A5` | observation seam envelope | [observation seam](/Users/yuta/local-mcp-connector-parity/src/verifier_git_observation.rs#L1) | `Coordinator` | runtime verified | `Reviewed - no issue found` | two new tests now prove the envelope the documentation claims; the module documentation matches the executor |
| `A6` | side effect state and budget | [fallback](/Users/yuta/local-mcp-connector-parity/src/fallback.rs#L1305) | `Coordinator` | contract trace | `Reviewed - no issue found` | unchanged by the delta; re-read the ordering and the lock gate |
| `A7` | failure classification | [fallback](/Users/yuta/local-mcp-connector-parity/src/fallback.rs#L604) | `Coordinator` | contract trace | `Reviewed - no issue found` | unchanged by the delta |
| `A8` | test surfaces | [verifier tests](/Users/yuta/local-mcp-connector-parity/src/verifier_tests.rs#L157), [managed tests](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_creation_tests.rs#L3192) | `Coordinator` | contract trace | `Reviewed - no issue found` | the write probe now keeps its workspace, covers three destinations, and asserts non-modification; the fixture note matches the tests that exist |
| `A9` | phase 4 freeze | [managed tests](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_creation_tests.rs#L3190) | `Coordinator` | contract trace | `Reviewed - no issue found` | the real integration path and the new execution-root observation test both green; the one uncovered property is named in the note and in the handoff |
| `A10` | windows and linux legs | [approval gate](/Users/yuta/local-mcp-connector-parity/src/execution.rs#L707) | `Coordinator` | contract trace | `Not covered` | `Not covered` | not executed on this host; the complete matrix is the next verification, with the windows jobs inspected first |

## Subagent Candidate Adjudication

`None - this generation used a single reviewer by decision, so no specialist candidates were produced.`

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `src/git_command_class.rs` | surface | classification, authority |
| `src/verifier_command_authority.rs` | surface | authorization, trust boundary |
| `src/verifier_git_observation.rs` | surface | authorization, filesystem effects, platform policy |
| `src/verifier.rs` | surface | authorization, lifecycle, path resolution |
| `src/fallback.rs` | surface | state model, failure classification |
| `src/main.rs` | config | none |
| `src/verifier_tests.rs` | test-only | tests |
| `src/managed_worktree_creation_tests.rs` | test-only | tests |

### Verification Commands

- `cargo fmt --all` then `cargo fmt --check` -> clean.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> clean.
- `cargo test --locked --all-targets` -> 912 passed and 0 failed.
- `cargo test --locked --all-targets managed_worktree` -> 186 passed and 0 failed.
- `cargo test --locked --all-targets verifier_tests` -> 46 passed and 0 failed.
- `cargo test --locked --all-targets git_command_class` -> 17 passed and 0 failed.
- `cargo test --locked --all-targets verifier_git_observation` -> 7 passed and 0 failed.
- `git diff --check` -> clean.

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `F1` | fixed | [host_git_argv](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L1000) | where the executable becomes host-owned |
| `F2` | fixed | [branch tables](/Users/yuta/local-mcp-connector-parity/src/git_command_class.rs#L262) | where the rendering flags moved |
| `F7` | fixed | [probe](/Users/yuta/local-mcp-connector-parity/src/verifier_tests.rs#L157) | where the write-authority proof is made real |
| `T1` | fixed | [observation envelope test](/Users/yuta/local-mcp-connector-parity/src/verifier_git_observation.rs#L427) | what holds the seam honest |

### Dismissed Coordinator Candidates

| Candidate | Decision | Evidence |
| --- | --- | --- |
| Removing the two tails from the allowlist loses a capability the brief requires | dismissed | the brief admits exact read-only git observation shapes and requires pure observation only; the real status and staged-name observation is performed by the host-owned seam, which is strictly better hardened |
| The `has_flag` change could turn a read into a mutation | dismissed | over-claiming a mutation is the fail-safe direction, and the change only widens the mutation match and the listed base-name match |
| The fix for the write probe weakened it further by using an explicit scope | dismissed | the scope is now asserted to contain an allowed and a forbidden path, which is what the brief requires; the previous version had an empty forbidden list |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A10` | no windows or linux execution | a platform-only compile or test failure could still land | the complete eleven-job matrix |
| `A10` | the pre-existing porcelain rename-source parser weakness is deliberately unfixed | a hostile rename source path could be misparsed in a forbidden-path check | a separate bounded change with its own review |

## Prior Resolution Reconciliation

| Issue key | Issue fingerprint | Parent item/verdict | Relevant change or new evidence | Decision |
| --- | --- | --- | --- | --- |
| `behavior; entry=verifier command exit execution; contract=the executed executable is host-resolved and never chosen by the proposal; effect=a repository-planted executable runs with session-wide write authority` | `ifp-sha256:86c542616cbf608789c95dba15afb03d6fc6f8658e652d5df773bafca199a569` | `F1` Fixed | `kind:code; ref:src/verifier.rs host_git_argv; change:the proposal's first argument is replaced with the host-resolved git identity before the runner is called` | kept closed |
| `behavior; entry=git argv side-effect classification; contract=a known ref-creation shape classifies as a local mutation; effect=a ref creation is reported as side-effect free` | `ifp-sha256:27958cd4fc3bbbeb62d01f806117ec831e4fdd7239fa1f6160170e4a8044b7a9` | `F2` Fixed | `kind:code; ref:src/git_command_class.rs listing_command tables; change:rendering flags moved from the selector table to the display table and attached-value matching added` | kept closed |
| `behavior; entry=verifier command exit lifecycle test; contract=a test that drives approval-gated host-native execution is platform-gated or supplies an approver; effect=the windows job fails on a missing approval channel` | `ifp-sha256:0604e2f2e06ff0336a35a2944941bb49e452d3a57f40d643406f32afca98e8e8` | `F3` Fixed | `kind:code; ref:src/verifier_tests.rs platform gate; change:the test is platform-gated again with the single reachable outcome asserted` | kept closed |
| `behavior; entry=host-owned verifier git observation test; contract=a test that is not about the approval gate selects an explicit policy; effect=the windows job fails on a missing approval channel` | `ifp-sha256:1d3ff4f0ee495fbbf462e5c841d9564322a68c70ce8b503185c7225ad4e26b72` | `F4` Fixed | `kind:code; ref:src/verifier_git_observation.rs and src/managed_worktree_creation_tests.rs; change:all four tests select an explicit policy` | kept closed |
| `behavior; entry=verifier command exit authority allowlist; contract=an approved pure observation performs no index write; effect=a verification command may write index metadata` | `ifp-sha256:3c3892a18f37574348e4c1518685886bec27c6925b775c9621d43ec345b46f62` | `F5` Fixed | `kind:code; ref:src/verifier_command_authority.rs OBSERVATION_TAILS; change:the status and diff tails were removed rather than narrowed` | kept closed |
| `behavior; entry=verifier git execution environment; contract=an approved observation cannot reach a repository or user configured helper; effect=a configured filesystem monitor or external diff driver runs under an approved argv` | `ifp-sha256:359c274ff6de8980651dec48825fb603a9701f7710e745ba448873f7cb0a8968` | `F6` Fixed | `kind:code; ref:src/verifier_command_authority.rs OBSERVATION_TAILS; change:the only two tails that consult those settings no longer exist` | kept closed |
| `behavior; entry=verifier write authority probe; contract=the probe keeps its workspace and covers allowed, forbidden, and other-session destinations; effect=refusal and no-write assertions pass for the wrong reason` | `ifp-sha256:bd23ca4149b40d77863b75fb03d73804173200d614f2088f669ae2b91dab5382` | `F7` Fixed | `kind:code; ref:src/verifier_tests.rs Fixture ownership and probe; change:the caller keeps workspace ownership, the forbidden leg exists, and sentinels are asserted unmodified` | kept closed |
| `behavior; entry=phase4 integration plan fixture; contract=a coverage narrowing change is documented accurately; effect=a maintainer trusts a test that does not cover the claimed property` | `ifp-sha256:f39d99b1323f3aa802ac34af86ef29ac1f34df2972c3462cd5ae408bdcdfa80c` | `F8` Fixed | `kind:code; ref:src/managed_worktree_creation_tests.rs removal note; change:the note names the real test, the real reason, and the uncovered property` | kept closed |
| `behavior; entry=git show-ref option semantics; contract=dereference is a read and delete is the mutating spelling; effect=a read is misclassified as a mutation` | `ifp-sha256:a3b7afc81609490ab03c8f6a6aabb88a911caf4eed88ac8cb5bcdbeccdf597d1` | `F9` Fixed | `kind:code; ref:src/git_command_class.rs show_ref; change:the short form moved to the read table and the non-existent delete spelling falls through to the unknown class` | kept closed |
| `behavior; entry=git global option parsing; contract=the detached value of a long authority-shaping global is consumed before the subcommand; effect=the value token is misread as the subcommand` | `ifp-sha256:61e60aa4f11f310d36607a00c2fee13f524f4159de2cf6a2217bca127db85b11` | `F10` Fixed | `kind:code; ref:src/git_command_class.rs split_global_options; change:the detached value is consumed for long options and bare short options` | kept closed |
| `behavior; entry=verifier git path resolution documentation; contract=the documented containment property matches the implemented one; effect=a maintainer relies on a guarantee that is not enforced` | `ifp-sha256:7b9d3b508add0901754595c6efe2b0986c199db5cdd01bfce1458184bc0f0eda` | `F11` Fixed | `kind:code; ref:src/verifier_git_observation.rs resolve_git_path documentation; change:the documentation now states the literal check and the fail-closed downstream behavior` | kept closed |
| `behavior; entry=verifier path canonicalization; contract=one helper resolves a path identically for spec paths and git paths; effect=two copies drift and only one is regression tested` | `ifp-sha256:9ed29daa4ef9552aa4268c497e76d9bde5113fc3afd319e1369a9213569d3689` | `F12` Fixed | `kind:code; ref:src/verifier.rs canonicalize_existing_prefix; change:the duplicate was deleted and the verifier delegates to the single implementation` | kept closed |
| `behavior; entry=host-owned git observation module documentation; contract=the documented platform confinement matches the implementation; effect=a maintainer believes the observation is sandboxed on unix` | `ifp-sha256:f751313ff6c8b944a97a63c2f58b627c841754ee7d31c4ae01dcdcb58eb5db2b` | `F13` Fixed | `kind:code; ref:src/verifier_git_observation.rs module documentation; change:the documentation now states the observation is not sandboxed and names the tests that hold the envelope` | kept closed |
| `test-gap; entry=host-owned verifier git observation confinement; contract=observing a worktree is proven to create, alter, and execute nothing; gap=no mechanical proof of the observation envelope` | `ifp-sha256:06c1abd440ac9bbaac376957da004365a5e288192633cd3172689bf5fa5aa394` | `T1` Fixed | `kind:code; ref:src/verifier_git_observation.rs host_owned_observation_writes_nothing_anywhere; change:index bytes, index modification time, worktree entries, and an outside sentinel are all asserted unchanged` | kept closed |
| `test-gap; entry=verifier observation approval gate; contract=removing the gate is detectable by a test; gap=only a tautological constant equality test exists` | `ifp-sha256:5ab9865b735d7e5fecc487c1aa9636d7706f938b6ba501fe98770a603c4a427d` | `T2` Fixed | `kind:code; ref:src/verifier_git_observation.rs an_unapproved_observation_performs_no_git_invocation; change:the test now asserts the failure is specifically the approval gate` | kept closed |
| `test-gap; entry=git argv classifier matrix; contract=every required classification shape is asserted; gap=rendering-flag ref creation and show-ref dereference shapes were unasserted` | `ifp-sha256:8b52d7df3a445395c66a0c51e09163a3a927bc7cf7efaff199173e42e36e4247` | `T3` Fixed | `kind:code; ref:src/git_command_class.rs tests; change:twelve rendering-flag mutation shapes, four paired observation shapes, the dereference, and the detached global option are asserted` | kept closed |

## Receiving Handoff

- Handoff status: `Terminal post-review - return to user/owner`
- Automatic receiving permitted: `No`
- Source report ID: `cr-20261001-3e8d05`
- Scope fingerprint to recheck: `sha256:7f0a1c2b9d4e5a6f8c7b3d2e1f0a9b8c7d6e5f4a3b2c1d0e9f8a7b6c5d4e3f2`
- Actionable finding IDs: `None`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `None`
- Deferred test-gap IDs: `None`
- Open question IDs: `F14`
- Open coverage area IDs: `A10`
- Highest-risk verification to repeat: the complete eleven-job matrix, with the windows jobs inspected first.
- Suggested implementation boundaries: `None`
- Re-review note: `Treat every finding as a claim to verify. Challenges require a counterclaim, argument, evidence, limits, and settlement criterion.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or product owner.`

## Report Self-Check

- `yes` Actual assessment mode and rationale are recorded: coordinator, delegated assessor, or unavailable fallback.
- `yes` Every changed review-relevant or unknown-impact area appears once in `Review Coverage Ledger`.
- `yes` Every final finding appears once in the index and once as a matching card.
- `yes` Every `Finding F#` area references an existing finding.
- `yes` Every standalone test gap has a stable ID and severity.
- `yes` Every `F#` and `T#` has a unique semantic issue fingerprint and an authoritative expected basis, or the item is an explicit `Question` for unconfirmed intent.
- `yes` Generation, trigger, parent resolution, scope mode, and receiving handoff satisfy the bounded chain contract.
- `yes` Generation `1` reconciles relevant parent terminal dispositions and records a reason for every reopened issue fingerprint. Nothing was reopened; every parent finding is recorded as kept closed with a concrete code reference.
- `yes` Every non-Question finding and standalone test gap appears exactly once in actionable or deferred handoff IDs; every Question and Not-covered area appears in its matching open list.
- `yes` Every meaningful subagent candidate has an adjudication. Not applicable; single-reviewer generation.
- `yes` Every `Not covered` area has a reason and next step.
- `yes` Recommendation follows the skill mapping.
- `yes` The validator passes.
- `yes` Git state was not mutated.
