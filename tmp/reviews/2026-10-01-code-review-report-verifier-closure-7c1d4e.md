# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261001-7c1d4e`
- Review chain ID: `rc-20261001-7c1d4e`
- Review generation: `0`
- Review trigger: `initial`
- Parent review report ID: `None`
- Parent review report path: `None`
- Parent resolution ID: `None`
- Parent resolution path: `None`
- Generated at: `2026-10-01T00:00:00Z`
- Report path: `tmp/reviews/2026-10-01-code-review-report-verifier-closure-7c1d4e.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:2ded103215d81587f4be3126698b08e607b345d73d7d382a5a047806fb883b62`

Treat this completed report as the fixed review input for downstream work. Do not rewrite it during receiving or implementation; record dispositions, challenges, code changes, and verification in a separate `receiving-code-review` resolution report that references this Report ID.

## Scope

- Review date: 2026-10-01
- Scope kind: `working tree`
- Scope description: The uncommitted security closure for the Verifier command-authority, Git side-effect-classification, requested-command-lifecycle, side-effect-state, and failure-classification defects, on branch `security/verifier-classifier-closure-v1`.
- Scope mode: `full frozen scope`
- Baseline: `f899ad31033540d666389d2520fd040fe267cbc7`, the completed managed worktrees phase 4 head
- Target: `working tree` on `security/verifier-classifier-closure-v1`
- Changed paths: 8
- Diff size: 912 insertions and 237 deletions at the generation-0 freeze
- Completion: `Complete within reviewed scope`
- Requirements consulted: the task brief sections 0 through 27, `SECURITY.md`, `README.md` lines 190 through 299, `docs/GOAL_TASK_ORCHESTRATOR_V1_DESIGN.md` sections 11.5, 13.2, 13.3, 14.5 and 27.1, and `docs/MANAGED_WORKTREES_V1_DESIGN.md` sections 13, 15, 19, 23, 26 and 27.
- Prior resolution consulted: `None`
- Assumptions: no phase 5 work is in scope; the brief's decision that a command exit verification is pure observation only is authoritative; the repository's own model that an ordinary git status may write index stat metadata is accepted as the hazard definition.
- Excluded as unrelated: the pre-existing untracked `tmp/reviews/*` artifacts, and the brief's out-of-scope audit items covering replanner prompt compaction, superseded active-quota accounting, status pagination, writer time-of-check repair, writer atomic replacement, generic command output limits, job retention, protocol frame limits, orphan recovery, approval timeout, and migration display.

## Review Orchestration

- Assessment subagent: `Coordinator assessment - the semantic scope partitions cleanly along four independent risk dimensions and the coordinator already held the full defect list, so a delegated assessor would have re-sent context without adding independent evidence`
- Orchestration decision: `Parallel specialists`
- Decision confidence: `high`
- Decision rationale: four genuinely independent risk surfaces, on a security-boundary change where a missed class of defect is the entire point of the review.
- Coordinator override: `None`
- Context or tool limits: the argv specialist's first return truncated after a single progress line and was re-run with a narrower, explicitly prioritized prompt. No subagent ran the build; all build, test, and lint evidence was produced by the coordinator.

### Risk Dimensions

- Verifier command authority: a model-authored argv must not become host execution authority.
- Git argv semantics: a shape reported as side-effect free that actually mutates is the critical defect class.
- Requested-command lifecycle evidence: wrapper completion must never stand in for command completion.
- Side-effect state and budget: an unknown effect must not be recorded as a proof in either direction.
- Platform parity: Windows approval and compilation differ structurally from Unix.

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `R1` | Security and trust boundaries | command authority, observation seam, command execution and write authority | unknown side effects are never permission; the classifier never grants execution; the windows gate is not widened on unix | Complete |
| `R2` | Correctness, contracts, state semantics | git argv classification, side-effect state, budget model, failure classification | both failure directions of the classifier; no lost pre-change classification; the payload key matches its emitter | Complete |
| `R3` | Test adequacy, reliability, platform parity | verifier tests, managed worktree tests, all platform-conditional code, the phase 4 freeze list | windows approval responder semantics; the required negative-test matrix; the deliberate fixture removal | Complete |

### Synthesis Statement

The coordinator independently reproduced every accepted candidate. The three highest-severity claims were re-tested by the coordinator against the real git binary in a throwaway temporary repository: a branch created with a sort key, a branch created with a column option, a tag created with a format option, and a show-ref dereference that left the branch in place. Claims that did not reproduce were dismissed rather than carried. The trust-boundary and argv specialists overlapped on the allowlist question and agreed; the trust-boundary and test specialists overlapped on the windows approval question and did not contradict each other. Remaining blind spots are the windows and linux legs, which are structurally unverifiable on a macos host, and one pre-existing parser weakness that is recorded as out of scope rather than fixed.

## Review Snapshot

- Recommendation: `Block`
- Completion: `Complete within reviewed scope`
- Why now: the closure is the intended fix for the eight named defects, and the review found a caller-chosen executable and two ref-creation shapes reported as side-effect free, which would each have defeated the closure's central claim.
- Must-review now:
  1. `F1` a caller-chosen executable reached execution
  2. `F2` branch and tag rendering flags with a name were side-effect free
  3. `F3` two tests would fail the windows job
- Findings count: `Blocker 4 | Major 4 | Minor 5 | Question 1`
- Standalone test gaps: `Blocker 0 | Major 1 | Minor 2`
- Coverage confidence: `high` for unix semantics, `medium` for the windows and linux legs
- Biggest blind spot: no windows or linux execution in this review, so the windows approval and compilation behavior rests on code traces.

## Complete Findings Index

| ID | Severity | Surface | Review risk | Confidence | Origin | Verification | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `F1` | Blocker | verifier command authority | security | high | `R1-C2` | static trace | `behavior; entry=verifier command exit execution; contract=the executed executable is host-resolved and never chosen by the proposal; effect=a repository-planted executable runs with session-wide write authority` | `ifp-sha256:86c542616cbf608789c95dba15afb03d6fc6f8658e652d5df773bafca199a569` | `kind:requirement; strength:authoritative; evidence:task sections 6, 8, and 9 require a host-resolved git identity and no caller-provided git executable` |
| `F2` | Blocker | git classification | security | high | `R2-C1, R2-C2` | runtime | `behavior; entry=git argv side-effect classification; contract=a known ref-creation shape classifies as a local mutation; effect=a ref creation is reported as side-effect free` | `ifp-sha256:27958cd4fc3bbbeb62d01f806117ec831e4fdd7239fa1f6160170e4a8044b7a9` | `kind:requirement; strength:authoritative; evidence:task sections 11 and 17 require a known mutation shape to classify as a mutation and never an ambiguous shape as side-effect free` |
| `F3` | Blocker | verifier tests | availability | high | `R3-C1` | static trace | `behavior; entry=verifier command exit lifecycle test; contract=a test that drives approval-gated host-native execution is platform-gated or supplies an approver; effect=the windows job fails on a missing approval channel` | `ifp-sha256:0604e2f2e06ff0336a35a2944941bb49e452d3a57f40d643406f32afca98e8e8` | `kind:requirement; strength:authoritative; evidence:the host-native approval gate in the execution layer is a frozen platform policy and applies to the verification command path` |
| `F4` | Blocker | observation tests | availability | high | `R3-C2` | static trace | `behavior; entry=host-owned verifier git observation test; contract=a test that is not about the approval gate selects an explicit policy; effect=the windows job fails on a missing approval channel` | `ifp-sha256:1d3ff4f0ee495fbbf462e5c841d9564322a68c70ce8b503185c7225ad4e26b72` | `kind:requirement; strength:authoritative; evidence:the approval policy constant in the observation seam is selected by platform and cannot be exercised without a responder` |
| `F5` | Major | command authority allowlist | security | high | `R1-C1, R2-C4` | contract trace | `behavior; entry=verifier command exit authority allowlist; contract=an approved pure observation performs no index write; effect=a verification command may write index metadata` | `ifp-sha256:3c3892a18f37574348e4c1518685886bec27c6925b775c9621d43ec345b46f62` | `kind:owner-decision; strength:authoritative; evidence:task section 3 fixes command exit as pure observation only and section 11 fixes the status classification rule` |
| `F6` | Major | command execution environment | security | medium | `R1-C4, R1-C8` | contract trace | `behavior; entry=verifier git execution environment; contract=an approved observation cannot reach a repository or user configured helper; effect=a configured filesystem monitor or external diff driver runs under an approved argv` | `ifp-sha256:359c274ff6de8980651dec48825fb603a9701f7710e745ba448873f7cb0a8968` | `kind:requirement; strength:authoritative; evidence:task section 9 requires optional index writes, filesystem monitor, hooks, and external diff to be suppressed for observations` |
| `F7` | Major | verifier tests | contract | high | `R3-C3, R1-C3` | static trace | `behavior; entry=verifier write authority probe; contract=the probe keeps its workspace and covers allowed, forbidden, and other-session destinations; effect=refusal and no-write assertions pass for the wrong reason` | `ifp-sha256:bd23ca4149b40d77863b75fb03d73804173200d614f2088f669ae2b91dab5382` | `kind:requirement; strength:authoritative; evidence:task section 15 requires a probe covering a task-scope-allowed path, a task-scope-forbidden path, and another session-permitted path` |
| `F8` | Major | managed worktree tests | contract | high | `R3-C4` | static trace | `behavior; entry=phase4 integration plan fixture; contract=a coverage narrowing change is documented accurately; effect=a maintainer trusts a test that does not cover the claimed property` | `ifp-sha256:f39d99b1323f3aa802ac34af86ef29ac1f34df2972c3462cd5ae408bdcdfa80c` | `kind:requirement; strength:authoritative; evidence:task section 19 freezes the managed phase 4 execution-root behavior and its regression coverage` |
| `F9` | Minor | git classification | contract | high | `R2-C3` | runtime | `behavior; entry=git show-ref option semantics; contract=dereference is a read and delete is the mutating spelling; effect=a read is misclassified as a mutation` | `ifp-sha256:a3b7afc81609490ab03c8f6a6aabb88a911caf4eed88ac8cb5bcdbeccdf597d1` | `kind:requirement; strength:authoritative; evidence:task section 11 requires a known-safe read shape to classify as side-effect free` |
| `F10` | Minor | git global options | contract | medium | `R2-C5` | static trace | `behavior; entry=git global option parsing; contract=the detached value of a long authority-shaping global is consumed before the subcommand; effect=the value token is misread as the subcommand` | `ifp-sha256:61e60aa4f11f310d36607a00c2fee13f524f4159de2cf6a2217bca127db85b11` | `kind:requirement; strength:authoritative; evidence:task section 12 requires global git options to be handled or conservatively rejected` |
| `F11` | Minor | observation path resolution | contract | medium | `R1-C7` | static trace | `behavior; entry=verifier git path resolution documentation; contract=the documented containment property matches the implemented one; effect=a maintainer relies on a guarantee that is not enforced` | `ifp-sha256:7b9d3b508add0901754595c6efe2b0986c199db5cdd01bfce1458184bc0f0eda` | `kind:public-contract; strength:authoritative; evidence:the function's own documentation is the contract a caller and maintainer read` |
| `F12` | Minor | verifier path resolution | contract | high | `R3-C7` | static trace | `behavior; entry=verifier path canonicalization; contract=one helper resolves a path identically for spec paths and git paths; effect=two copies drift and only one is regression tested` | `ifp-sha256:9ed29daa4ef9552aa4268c497e76d9bde5113fc3afd319e1369a9213569d3689` | `kind:public-contract; strength:authoritative; evidence:both modules expose path resolution to the same durable task scope boundary` |
| `F13` | Minor | observation module docs | contract | high | `R1-C5` | static trace | `behavior; entry=host-owned git observation module documentation; contract=the documented platform confinement matches the implementation; effect=a maintainer believes the observation is sandboxed on unix` | `ifp-sha256:f751313ff6c8b944a97a63c2f58b627c841754ee7d31c4ae01dcdcb58eb5db2b` | `kind:public-contract; strength:authoritative; evidence:the module documentation states the platform confinement the seam is expected to provide` |
| `F14` | Question | verification specification | approval question | high | `Coordinator` | contract trace | `behavior; entry=verification command exit; contract=unconfirmed product expectation for command exit satisfiability; effect=approval question` | `ifp-sha256:79c97d156bc3ae58a060ea89a966e21579a9c7e532bf659deb4fbf750898dde0` | `kind:product-intent; strength:unavailable; evidence:no frozen document states whether a command exit verification is expected to complete on a sandboxed unix host` |

## Blocker

### F1 Blocker - A caller-chosen executable reached execution

Impact: security
Review reason: the closure's central claim is that only a host-approved pure observation can execute, and a proposal that merely names a program selected that program
Surface: command exit verification path
Issue key: `behavior; entry=verifier command exit execution; contract=the executed executable is host-resolved and never chosen by the proposal; effect=a repository-planted executable runs with session-wide write authority`
Issue fingerprint: `ifp-sha256:86c542616cbf608789c95dba15afb03d6fc6f8658e652d5df773bafca199a569`
Expected basis: `kind:requirement; strength:authoritative; evidence:task sections 6, 8, and 9 require a host-resolved git identity and no caller-provided git executable`
Confidence: high
Origin: `R1-C2`
Coordinator verification: call chain traced from the specification evaluation to the sandbox spawn; no host identity binding existed on that path

Look here first:
- [authority gate](/Users/yuta/local-mcp-connector-parity/src/verifier_command_authority.rs#L168)
- [execution hand-off](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L537)

Failure mode:
- Expected: the executed executable is the host-resolved git identity and a proposal never selects a program.
- Current: the authority compared the file name against git and the approved argv was forwarded with its own first entry.

Evidence:
- the authority reduced the first argument with the file-name helper and never consulted the host git identity
- the verifier passed the command unchanged to its runner
- the sandbox spawns the first argument verbatim
- the only executable-identity binding in the crate is scoped to the authorized staging mutation and is never reached by a verification command

Assumptions and limits:
- the exploit path was traced statically rather than executed, per the read-only mandate

Reviewer action:
`block until fixed`

### F2 Blocker - Branch and tag rendering flags with a name were classified side-effect free

Impact: security
Review reason: this is the exact defect class the closure exists to remove; a ref creation reported as side-effect free becomes a confirmed non-execution, leaves the budget unconsumed, and satisfies the reconciliation gates that treat that class as mechanically safe
Surface: git branch and git tag classification
Issue key: `behavior; entry=git argv side-effect classification; contract=a known ref-creation shape classifies as a local mutation; effect=a ref creation is reported as side-effect free`
Issue fingerprint: `ifp-sha256:27958cd4fc3bbbeb62d01f806117ec831e4fdd7239fa1f6160170e4a8044b7a9`
Expected basis: `kind:requirement; strength:authoritative; evidence:task sections 11 and 17 require a known mutation shape to classify as a mutation and never an ambiguous shape as side-effect free`
Confidence: high
Origin: `R2-C1, R2-C2`
Coordinator verification: reproduced against a real git binary in a throwaway repository; all three shapes created the named ref

Look here first:
- [selector decision](/Users/yuta/local-mcp-connector-parity/src/git_command_class.rs#L244)
- [branch selectors](/Users/yuta/local-mcp-connector-parity/src/git_command_class.rs#L262)

Failure mode:
- Expected: format, sort, and column render a listing and do not put git into listing mode, so a following positional is a ref to create.
- Current: all three sat in the selector table, and any selector short-circuited to the side-effect-free class regardless of positionals.

Evidence:
- the coordinator reproduced the branch creation with a sort key and with a column option, and the tag creation with a format option, in a throwaway repository that was then deleted
- contrast: a branch name after a quiet flag was already correct, because that flag sat in the display table, which proves the defect was table membership rather than the rule itself

Assumptions and limits:
- behavior was observed on one git version; other versions were not exercised

Reviewer action:
`block until fixed`

### F3 Blocker - The command exit lifecycle test would fail the windows job

Impact: availability
Review reason: the branch is not releasable with a test that panics on one matrix leg, and the failure would be misread as a security regression rather than a test-harness gap
Surface: verifier tests
Issue key: `behavior; entry=verifier command exit lifecycle test; contract=a test that drives approval-gated host-native execution is platform-gated or supplies an approver; effect=the windows job fails on a missing approval channel`
Issue fingerprint: `ifp-sha256:0604e2f2e06ff0336a35a2944941bb49e452d3a57f40d643406f32afca98e8e8`
Expected basis: `kind:requirement; strength:authoritative; evidence:the host-native approval gate in the execution layer is a frozen platform policy and applies to the verification command path`
Confidence: high
Origin: `R3-C1`
Coordinator verification: traced the removed platform gate against the approval-gated command path

Look here first:
- [lifecycle test](/Users/yuta/local-mcp-connector-parity/src/verifier_tests.rs#L868)
- [approval gate](/Users/yuta/local-mcp-connector-parity/src/execution.rs#L707)

Failure mode:
- Expected: a test that lets a command exit verification reach the command path is platform-gated or provides an approval responder.
- Current: neither, and the windows branch of the assertion was unreachable.

Evidence:
- the change removed the platform gate from both the test and its repository initializer
- the only approval responder in the crate belongs to the managed worktree integration test
- on windows the responder connection fails for an unbound pipe, so the request returns an error and the test unwraps it

Assumptions and limits:
- the panic itself was not executed; a windows run is the definitive check

Reviewer action:
`block until fixed`

### F4 Blocker - Two observation tests drove the production approval policy with no responder

Impact: availability
Review reason: both are new tests introduced by this change and both would fail the windows leg of the matrix
Surface: observation seam and managed worktree tests
Issue key: `behavior; entry=host-owned verifier git observation test; contract=a test that is not about the approval gate selects an explicit policy; effect=the windows job fails on a missing approval channel`
Issue fingerprint: `ifp-sha256:1d3ff4f0ee495fbbf462e5c841d9564322a68c70ce8b503185c7225ad4e26b72`
Expected basis: `kind:requirement; strength:authoritative; evidence:the approval policy constant in the observation seam is selected by platform and cannot be exercised without a responder`
Confidence: high
Origin: `R3-C2`
Coordinator verification: traced the policy selection in both tests

Look here first:
- [observation test](/Users/yuta/local-mcp-connector-parity/src/verifier_git_observation.rs#L407)
- [managed observation test](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_creation_tests.rs#L3590)

Failure mode:
- Expected: a test that is not about the approval gate selects an explicit policy so it is host-independent.
- Current: the production policy was used, making both tests windows-only failures.

Evidence:
- the seam selects the approval policy by platform and both tests call it with an unwrap
- the seam's explicit-policy entry point already existed and was unused by these tests
- a third test passed on windows for the wrong reason, because an unavailable approval looks like a non-repository

Assumptions and limits:
- the panic itself was not executed

Reviewer action:
`block until fixed`

## Major

### F5 Major - The allowlist admitted a status shape the classifier called unsafe

Impact: security
Review reason: two host-owned tables disagreed about one argv and the more permissive one gated execution, and an ordinary git status may refresh and write index stat metadata
Surface: command authority allowlist
Issue key: `behavior; entry=verifier command exit authority allowlist; contract=an approved pure observation performs no index write; effect=a verification command may write index metadata`
Issue fingerprint: `ifp-sha256:3c3892a18f37574348e4c1518685886bec27c6925b775c9621d43ec345b46f62`
Expected basis: `kind:owner-decision; strength:authoritative; evidence:task section 3 fixes command exit as pure observation only and section 11 fixes the status classification rule`
Confidence: high
Origin: `R1-C1, R2-C4`
Coordinator verification: contract trace of the allowlist against the classifier

Look here first:
- [allowlist](/Users/yuta/local-mcp-connector-parity/src/verifier_command_authority.rs#L134)
- [classifier rule](/Users/yuta/local-mcp-connector-parity/src/git_command_class.rs#L425)

Failure mode:
- Expected: an approved pure observation performs no index write and the host's own tables agree.
- Current: the authority approved an argv the classifier called ambiguous, and the documentation described a rule the code did not enforce.

Evidence:
- the allowlist contained the bare status tail and the global that would have made it safe was optional
- the classifier returns the unknown class for the identical argv without that global
- the invariant test had been narrowed to the prefixed form only, so it could not catch the divergence

Assumptions and limits:
- whether the index write actually lands inside the session's permitted roots was not measured

Reviewer action:
`request fix`

### F6 Major - An approved git argv ran without git configuration neutralization

Impact: security
Review reason: the model-authorized path used the reduced sandbox environment, so a user-level filesystem monitor or an attributes-driven conversion filter could turn an observation into program execution
Surface: command execution environment
Issue key: `behavior; entry=verifier git execution environment; contract=an approved observation cannot reach a repository or user configured helper; effect=a configured filesystem monitor or external diff driver runs under an approved argv`
Issue fingerprint: `ifp-sha256:359c274ff6de8980651dec48825fb603a9701f7710e745ba448873f7cb0a8968`
Expected basis: `kind:requirement; strength:authoritative; evidence:task section 9 requires optional index writes, filesystem monitor, hooks, and external diff to be suppressed for observations`
Confidence: medium
Origin: `R1-C4, R1-C8`
Coordinator verification: environment trace; no live hostile-repository experiment was run for the model path

Look here first:
- [allowlist](/Users/yuta/local-mcp-connector-parity/src/verifier_command_authority.rs#L134)
- [sandbox environment](/Users/yuta/local-mcp-connector-parity/src/sandbox.rs#L450)

Failure mode:
- Expected: an approved observation cannot reach a repository or user configured helper.
- Current: the approved status and diff shapes could, and they also lacked the external-diff suppression the host seam applies.

Evidence:
- the two environments differ: one neutralizes git configuration and the other does not
- status and diff were the only approved tails that consult those settings

Assumptions and limits:
- the filesystem monitor was not confirmed to be honored under the sandbox profile at runtime

Reviewer action:
`request fix`

### F7 Major - The write-authority probe was vacuous after its first iteration

Impact: contract
Review reason: the test claimed to prove the brief's write-authority property and proved much less, because the fixture deleted the shared workspace and the forbidden destination was never covered
Surface: verifier tests
Issue key: `behavior; entry=verifier write authority probe; contract=the probe keeps its workspace and covers allowed, forbidden, and other-session destinations; effect=refusal and no-write assertions pass for the wrong reason`
Issue fingerprint: `ifp-sha256:bd23ca4149b40d77863b75fb03d73804173200d614f2088f669ae2b91dab5382`
Expected basis: `kind:requirement; strength:authoritative; evidence:task section 15 requires a probe covering a task-scope-allowed path, a task-scope-forbidden path, and another session-permitted path`
Confidence: high
Origin: `R3-C3, R1-C3`
Coordinator verification: traced the fixture drop and the empty forbidden list

Look here first:
- [fixture drop](/Users/yuta/local-mcp-connector-parity/src/verifier_tests.rs#L29)
- [probe](/Users/yuta/local-mcp-connector-parity/src/verifier_tests.rs#L157)

Failure mode:
- Expected: the probe keeps its workspace and covers a task-scope-allowed path, a task-scope-forbidden path, and another session-permitted path.
- Current: the workspace was deleted after the first case and the forbidden leg did not exist.

Evidence:
- the fixture drop removed the caller-owned root and the builder stored it, so every later iteration failed on a missing directory
- the invalid-state assertion is satisfied by a missing directory as well as by a refusal, which is why the gap was invisible

Assumptions and limits:
- none

Reviewer action:
`request fix`

### F8 Major - The fixture-removal comment named a nonexistent test and overstated coverage

Impact: contract
Review reason: the phase 4 integration fixture lost its only managed-mode command-exit coverage and the note told a maintainer a replacement existed that did not
Surface: managed worktree tests
Issue key: `behavior; entry=phase4 integration plan fixture; contract=a coverage narrowing change is documented accurately; effect=a maintainer trusts a test that does not cover the claimed property`
Issue fingerprint: `ifp-sha256:f39d99b1323f3aa802ac34af86ef29ac1f34df2972c3462cd5ae408bdcdfa80c`
Expected basis: `kind:requirement; strength:authoritative; evidence:task section 19 freezes the managed phase 4 execution-root behavior and its regression coverage`
Confidence: high
Origin: `R3-C4`
Coordinator verification: compared the comment against the actual test

Look here first:
- [removal note](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_creation_tests.rs#L3192)
- [replacement](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_creation_tests.rs#L3573)

Failure mode:
- Expected: a coverage-narrowing change is documented accurately and the loss is named.
- Current: the note claimed coverage the replacement does not provide and cited a test name that does not exist.

Evidence:
- the comment names a test that is not in the tree
- the replacement never constructs a command exit verification
- the technical justification for the removal was independently confirmed correct

Assumptions and limits:
- the uncovered property cannot be covered in managed mode on unix by construction, which is a genuine coverage gap

Reviewer action:
`request fix`

## Minor

### F9 Minor - A show-ref dereference was classified as a mutation

Impact: contract
Review reason: it is an accuracy error in a table whose whole purpose is trustworthy classification
Surface: git show-ref classification
Issue key: `behavior; entry=git show-ref option semantics; contract=dereference is a read and delete is the mutating spelling; effect=a read is misclassified as a mutation`
Issue fingerprint: `ifp-sha256:a3b7afc81609490ab03c8f6a6aabb88a911caf4eed88ac8cb5bcdbeccdf597d1`
Expected basis: `kind:requirement; strength:authoritative; evidence:task section 11 requires a known-safe read shape to classify as side-effect free`
Confidence: high
Origin: `R2-C3`
Coordinator verification: reproduced against a real git binary

Look here first:
- [show_ref](/Users/yuta/local-mcp-connector-parity/src/git_command_class.rs#L453)

Failure mode:
- Expected: the short form is a dereference, which is a read.
- Current: the short form and a non-existent delete spelling were both in the mutation table.

Evidence:
- the coordinator reproduced the dereference: it printed the object id and the branch survived
- the delete spelling is rejected by git as an unknown option

Assumptions and limits:
- the direction was fail-safe, so this was never exploitable

Reviewer action:
`request fix`

### F10 Minor - A long global option's detached value was read as the subcommand

Impact: contract
Review reason: the parse was wrong even though the fail-closed gate masked the outcome, which makes it a trap for a later refactor
Surface: git global option parsing
Issue key: `behavior; entry=git global option parsing; contract=the detached value of a long authority-shaping global is consumed before the subcommand; effect=the value token is misread as the subcommand`
Issue fingerprint: `ifp-sha256:61e60aa4f11f310d36607a00c2fee13f524f4159de2cf6a2217bca127db85b11`
Expected basis: `kind:requirement; strength:authoritative; evidence:task section 12 requires global git options to be handled or conservatively rejected`
Confidence: medium
Origin: `R2-C5`
Coordinator verification: traced the detached-value branch

Look here first:
- [detached value branch](/Users/yuta/local-mcp-connector-parity/src/git_command_class.rs#L131)

Failure mode:
- Expected: the detached value is consumed before the subcommand is located.
- Current: the value token became the subcommand.

Evidence:
- the branch that consumes a detached value only advanced for one-character short options, so a long option's value was not consumed
- the trailing shaped-option gate still produced the unknown class, so the existing test passed for the wrong reason

Assumptions and limits:
- no unsafe outcome was reachable today

Reviewer action:
`request fix`

### F11 Minor - Path resolution documented a containment guarantee it does not enforce

Impact: contract
Review reason: a comment that overstates a security property is a future defect
Surface: git path resolution
Issue key: `behavior; entry=verifier git path resolution documentation; contract=the documented containment property matches the implemented one; effect=a maintainer relies on a guarantee that is not enforced`
Issue fingerprint: `ifp-sha256:7b9d3b508add0901754595c6efe2b0986c199db5cdd01bfce1458184bc0f0eda`
Expected basis: `kind:public-contract; strength:authoritative; evidence:the function's own documentation is the contract a caller and maintainer read`
Confidence: medium
Origin: `R1-C7`
Coordinator verification: traced the literal check and the downstream boundary comparison

Look here first:
- [resolve_git_path](/Users/yuta/local-mcp-connector-parity/src/verifier_git_observation.rs#L254)

Failure mode:
- Expected: the documented property matches the implemented one.
- Current: the documentation claimed an absolute guarantee while the code performs a literal check and relies on downstream fail-closed behavior.

Evidence:
- only literal absolute paths and parent components are refused, and canonicalization follows symlinks
- the outcome is fail-closed downstream because a resolved-outside path matches no allowed boundary

Assumptions and limits:
- not every consumer of the observation struct was enumerated

Reviewer action:
`request fix`

### F12 Minor - Path canonicalization was duplicated across two modules

Impact: contract
Review reason: two copies can drift and only one is regression-tested, so a future path-handling fix silently half-applies
Surface: verifier path resolution
Issue key: `behavior; entry=verifier path canonicalization; contract=one helper resolves a path identically for spec paths and git paths; effect=two copies drift and only one is regression tested`
Issue fingerprint: `ifp-sha256:9ed29daa4ef9552aa4268c497e76d9bde5113fc3afd319e1369a9213569d3689`
Expected basis: `kind:public-contract; strength:authoritative; evidence:both modules expose path resolution to the same durable task scope boundary`
Confidence: high
Origin: `R3-C7`
Coordinator verification: confirmed the verbatim duplicate

Look here first:
- [verifier copy](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L937)

Failure mode:
- Expected: one implementation resolves a path identically for specification paths and git-emitted paths.
- Current: two implementations existed.

Evidence:
- a verbatim duplicate apart from the error type
- the windows path-spelling regression test reaches only the new copy

Assumptions and limits:
- none

Reviewer action:
`request fix`

### F13 Minor - The observation seam documented a sandbox posture it does not have

Impact: contract
Review reason: the module documentation is the artifact a future maintainer relies on and it asserted the opposite of the code
Surface: observation seam documentation
Issue key: `behavior; entry=host-owned git observation module documentation; contract=the documented platform confinement matches the implementation; effect=a maintainer believes the observation is sandboxed on unix`
Issue fingerprint: `ifp-sha256:f751313ff6c8b944a97a63c2f58b627c841754ee7d31c4ae01dcdcb58eb5db2b`
Expected basis: `kind:public-contract; strength:authoritative; evidence:the module documentation states the platform confinement the seam is expected to provide`
Confidence: high
Origin: `R1-C5`
Coordinator verification: traced the module documentation against the executor

Look here first:
- [module doc](/Users/yuta/local-mcp-connector-parity/src/verifier_git_observation.rs#L1)

Failure mode:
- Expected: the documented confinement matches the implementation.
- Current: the documentation claimed a sandbox the path does not have.

Evidence:
- the seam calls the unsandboxed bounded executor on every platform
- the removal of the sandbox itself was independently reviewed as an accepted narrowing of what the host will run, and the pre-existing managed-worktree observation seam establishes the same pattern

Assumptions and limits:
- none

Reviewer action:
`request fix`

## Questions

### F14 Question - A command exit verification is satisfiable on no platform today

Approval impact: a shipped verification kind that can never pass is a product decision, not only a security fix; approving the closure means accepting that the kind is inert and that any plan using it will block its task
Needed context: whether a command exit verification is expected to complete on a sandboxed unix host, and if not, whether the kind should be refused at plan materialization rather than accepted and later blocked
Surface: verification specification
Issue key: `behavior; entry=verification command exit; contract=unconfirmed product expectation for command exit satisfiability; effect=approval question`
Issue fingerprint: `ifp-sha256:79c97d156bc3ae58a060ea89a966e21579a9c7e532bf659deb4fbf750898dde0`
Expected basis: `kind:product-intent; strength:unavailable; evidence:no frozen document states whether a command exit verification is expected to complete on a sandboxed unix host`
Confidence: high
Origin: `Coordinator`
Coordinator verification: traced the full lifecycle chain from the wrapper to the pass rule

Look here first:
- [sandbox lifecycle](/Users/yuta/local-mcp-connector-parity/src/sandbox.rs#L218)
- [pass rule](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L543)

Evidence:
- the lifecycle chain is deterministic: the wrapper reports an unproven start on linux and macos, the command-finished flag is derived from it, the payload carries that flag, and the verifier's pass rule requires it
- on windows the command path is approval-gated
- the brief's own black-box result shows a zero exit code without a proven start, so no shortcut from a zero wrapper exit is sound

Settlement criterion:
- an explicit product decision either that the kind is inert and should be refused at plan materialization, or that a host-owned observation seam is authorized for model-authored argv, which would be new execution authority and needs its own design

Reviewer action:
`ask owner`

## Test Gaps

| ID | Severity | Surface | Missing coverage | Risk | Origin | Evidence | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `T1` | Major | host-owned git observation | No mechanical proof that observing a worktree creates, alters, and executes nothing, although the path leaves the platform sandbox | A confinement regression in the seam would be silent | `R3-C4b` | the only new test asserted argv shape, never side effects | `test-gap; entry=host-owned verifier git observation confinement; contract=observing a worktree is proven to create, alter, and execute nothing; gap=no mechanical proof of the observation envelope` | `ifp-sha256:06c1abd440ac9bbaac376957da004365a5e288192633cd3172689bf5fa5aa394` | `kind:requirement; strength:authoritative; evidence:task section 9 requires the observation to be effectively read-only` |
| `T2` | Minor | verifier observation approval gate | The windows approval gate was pinned only by a constant equality, so deleting the request would pass every test | The gate could be removed silently | `R3-C5` | the non-windows denial test is compiled out on windows and the managed integration responder handle exits on a signal | `test-gap; entry=verifier observation approval gate; contract=removing the gate is detectable by a test; gap=only a tautological constant equality test exists` | `ifp-sha256:5ab9865b735d7e5fecc487c1aa9636d7706f938b6ba501fe98770a603c4a427d` | `kind:requirement; strength:authoritative; evidence:task section 10 requires the windows approval boundary to remain intact and observable` |
| `T3` | Minor | git argv classifier | The required matrix did not assert rendering-flag ref creation or show-ref dereference | The exact shapes that produced F2 and F9 were unasserted | `R2-C4` | matrix cross-check against the classifier test module | `test-gap; entry=git argv classifier matrix; contract=every required classification shape is asserted; gap=rendering-flag ref creation and show-ref dereference shapes were unasserted` | `ifp-sha256:8b52d7df3a445395c66a0c51e09163a3a927bc7cf7efaff199173e42e36e4247` | `kind:requirement; strength:authoritative; evidence:task section 17 enumerates the required classifier matrix` |

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | verifier command authority executable identity | [authority](/Users/yuta/local-mcp-connector-parity/src/verifier_command_authority.rs#L168), [verifier](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L513) | `R1` | dependency trace | `Finding F1` | `Finding F1` | call chain traced to the sandbox spawn; no host identity binding existed |
| `A2` | command authority allowlist consistency | [allowlist](/Users/yuta/local-mcp-connector-parity/src/verifier_command_authority.rs#L134) | `R1` | contract trace | `Finding F5` | `Finding F5` | the two host tables disagreed on one argv and the permissive one gated execution |
| `A3` | command execution environment | [sandbox environment](/Users/yuta/local-mcp-connector-parity/src/sandbox.rs#L450) | `R1` | contract trace | `Finding F6` | `Finding F6` | environment asymmetry traced; the two affected tails are the only ones that consult those settings |
| `A4` | verifier write authority probe | [probe](/Users/yuta/local-mcp-connector-parity/src/verifier_tests.rs#L157) | `R1` | contract trace | `Finding F7` | `Finding F7` | fixture drop deleted the shared workspace; the forbidden destination was absent |
| `A5` | phase 4 integration plan fixture | [removal note](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_creation_tests.rs#L3192) | `R1` | contract trace | `Finding F8` | `Finding F8` | comment named a nonexistent test and claimed coverage the replacement does not provide |
| `A6` | git branch and tag listing selectors | [selector decision](/Users/yuta/local-mcp-connector-parity/src/git_command_class.rs#L244) | `R2` | runtime verified | `Finding F2` | `Finding F2` | reproduced against a real git binary in a throwaway repository |
| `A7` | git show-ref option table | [show_ref](/Users/yuta/local-mcp-connector-parity/src/git_command_class.rs#L453) | `R2` | runtime verified | `Finding F9` | `Finding F9` | reproduced the dereference and confirmed no delete option exists |
| `A8` | git global option parsing | [detached value branch](/Users/yuta/local-mcp-connector-parity/src/git_command_class.rs#L131) | `R2` | contract trace | `Finding F10` | `Finding F10` | detached-value branch traced; masked today by the fail-closed gate |
| `A9` | git path resolution documentation | [resolve_git_path](/Users/yuta/local-mcp-connector-parity/src/verifier_git_observation.rs#L254) | `R2` | contract trace | `Finding F11` | `Finding F11` | literal check traced; downstream fail-closed behavior confirmed |
| `A10` | verifier path canonicalization | [verifier copy](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L937) | `R2` | static trace | `Finding F12` | `Finding F12` | verbatim duplicate confirmed apart from the error type |
| `A11` | observation module documentation | [module doc](/Users/yuta/local-mcp-connector-parity/src/verifier_git_observation.rs#L1) | `R2` | static trace | `Finding F13` | `Finding F13` | documentation asserted a sandbox the executor does not use |
| `A12` | windows command exit test | [lifecycle test](/Users/yuta/local-mcp-connector-parity/src/verifier_tests.rs#L868) | `R3` | contract trace | `Finding F3` | `Finding F3` | the platform gate was removed while the command path stays approval-gated |
| `A13` | windows observation tests | [observation test](/Users/yuta/local-mcp-connector-parity/src/verifier_git_observation.rs#L407) | `R3` | contract trace | `Finding F4` | `Finding F4` | production approval policy selected with no responder in either test |
| `A14` | command exit satisfiability | [pass rule](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L543) | `R3` | contract trace | `Finding F14` | `Finding F14` | full lifecycle chain traced from the wrapper to the pass rule |
| `A15` | side effect state and budget model | [fallback](/Users/yuta/local-mcp-connector-parity/src/fallback.rs#L1305) | `R2` | contract trace | `Reviewed - no issue found` | `Reviewed - no issue found` | unknown is reachable only through a refuted start; the lock gate closes the retry |
| `A16` | failure classification | [fallback](/Users/yuta/local-mcp-connector-parity/src/fallback.rs#L604) | `R2` | contract trace | `Reviewed - no issue found` | `Reviewed - no issue found` | tool-missing precedence and the removal of the bare launcher word verified; no remaining dependence on the bare match |
| `A17` | module registration | [main](/Users/yuta/local-mcp-connector-parity/src/main.rs) | `Coordinator` | diff-only | `Not review-relevant` | `Not review-relevant` | three module declarations, no behavior |
| `A18` | classifier consumers | [execution](/Users/yuta/local-mcp-connector-parity/src/execution.rs#L861), [goal api](/Users/yuta/local-mcp-connector-parity/src/goal_api.rs#L1119) | `R2` | dependency trace | `Reviewed - no issue found` | `Reviewed - no issue found` | stricter unknown results can only remove reconciliation matches; production binders hardcode their class |
| `A19` | linux sandbox denial text | [fallback](/Users/yuta/local-mcp-connector-parity/src/fallback.rs#L622) | `R2` | inferred | `Not covered` | `Not covered` | landlock denial text is inferred from the sandboxing dependency, not observed; a linux run of a denied command would confirm the exact string |

## Subagent Candidate Adjudication

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `R1-C1` | `R1` | accepted | `F5` | contract trace of the allowlist against the classifier | the two host tables disagreed and the documentation described an unenforced rule |
| `R1-C2` | `R1` | accepted | `F1` | full call chain traced to the sandbox spawn | a name is not an executable identity |
| `R1-C3` | `R1` | merged | `F7` | the forbidden leg was absent and the test asserted only rejection | same failure mode as the vacuity finding |
| `R1-C4` | `R1` | accepted | `F6` | environment trace | real asymmetry; closed by removing the two affected tails |
| `R1-C5` | `R1` | accepted | `F13` | documentation traced against the executor | documentation asserted the opposite of the code |
| `R1-C6` | `R1` | dismissed as an in-scope defect | `None` | confirmed the identical logic exists at the baseline commit | pre-existing and semantics-affecting; fixing it would change git-scope semantics for renames and needs its own review, so it is recorded in the handoff |
| `R1-C7` | `R1` | accepted | `F11` | code trace | documentation overstates a security property |
| `R1-C8` | `R1` | merged | `F6` | the model diff tails lacked the seam suppression | same root cause as the configuration asymmetry |
| `R2-C1` | `R2` | accepted | `F2` | coordinator reproduced the ref creation | a mutation reported as side-effect free |
| `R2-C2` | `R2` | merged | `F2` | coordinator reproduced the tag creation | same defect in the tag family |
| `R2-C3` | `R2` | accepted | `F9` | coordinator reproduced the dereference | the table mislabeled a read |
| `R2-C4` | `R2` | accepted | `T3` | matrix cross-check | the decisive shapes were unasserted |
| `R2-C5` | `R2` | accepted | `F10` | code trace of the detached-value branch | latent parse defect, masked by a fail-closed gate |
| `R3-C1` | `R3` | accepted | `F3` | code trace of the removed platform gate | the windows job would fail |
| `R3-C2` | `R3` | accepted | `F4` | code trace of the policy selection | the windows job would fail |
| `R3-C3` | `R3` | accepted | `F7` | code trace of the fixture drop | the proof was vacuous |
| `R3-C4` | `R3` | accepted | `F8` | name and claim comparison | documentation overstated coverage |
| `R3-C4b` | `R3` | accepted | `T1` | no side-effect assertion existed | the seam envelope was unproven |
| `R3-C5` | `R3` | accepted | `T2` | mutation test of the gate | the gate was a tautology |
| `R3-C6` | `R3` | deferred | `None` | temp-dir hygiene matches existing test convention in the crate | no defect; noted in the handoff |
| `R3-C7` | `R3` | accepted | `F12` | verbatim duplicate confirmed | drift risk |
| `R2` dismissed claims | `R2` | dismissed | `None` | nine claims individually traced | the state ordering, budget lock, reconciliation gates, sandbox marker removal, tool-missing precedence, payload key, and lost-classification concerns were each checked and none reproduced |
| `R3` dismissed claims | `R3` | dismissed | `None` | four claims individually traced | the windows approval count and cwd binding, the executable-suffix platform asymmetry, and a pre-existing duplicate assertion were each checked and none reproduced |

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `src/git_command_class.rs` | surface | classification, authority |
| `src/verifier_command_authority.rs` | surface | authorization, trust boundary |
| `src/verifier_git_observation.rs` | surface | authorization, filesystem effects, platform policy |
| `src/verifier.rs` | surface | authorization, lifecycle, filesystem effects |
| `src/fallback.rs` | surface | state model, failure classification |
| `src/main.rs` | config | none |
| `src/verifier_tests.rs` | test-only | tests |
| `src/managed_worktree_creation_tests.rs` | test-only | tests |

### Verification Commands

- `git --no-pager show f899ad3:src/fallback.rs` -> the pre-change classifier arms, compared against the new tables; no classification was lost.
- real-git experiments in a throwaway temporary repository -> a sort key and a column option create a branch, a format option creates a tag, a dereference leaves the ref in place, a two-reference symbolic form rewrites the current ref, and a line-count option lists rather than creates.
- `cargo test --locked --all-targets` -> 912 passed and 0 failed.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> clean.
- `cargo fmt --all` then `cargo fmt --check` -> clean.

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `F1` | entry | [verifier authority](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L513) | where the approved argv enters execution |
| `F1` | risk | [sandbox spawn](/Users/yuta/local-mcp-connector-parity/src/sandbox.rs#L438) | the first argument is spawned verbatim |
| `F2` | risk | [listing decision](/Users/yuta/local-mcp-connector-parity/src/git_command_class.rs#L244) | a selector short-circuits to the read class |
| `F3` | test | [verifier tests](/Users/yuta/local-mcp-connector-parity/src/verifier_tests.rs#L868) | the test that would fail on windows |
| `F7` | test | [probe](/Users/yuta/local-mcp-connector-parity/src/verifier_tests.rs#L157) | the vacuous write-authority proof |

### Dismissed Coordinator Candidates

| Candidate | Decision | Evidence |
| --- | --- | --- |
| The observation seam's approval request count changed on windows | dismissed | four queries before and after, one approval each, with the same operation label and working directory |
| Yolo mode short-circuits the new approval gate differently | dismissed | the request path has no yolo branch; yolo is session-process state |
| The stricter classifier could make reconciliation unsafe | dismissed | every reconciliation gate requires both the read class and a confirmed non-execution, and production binders hardcode their class |
| Removing the bare launcher substring loses genuine denials | dismissed | real seatbelt text contains both a specific runtime token and a standard permission error, and a focused test asserts it |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A19` | linux sandbox denial text was inferred, not observed | a genuine denial could classify as unknown instead of a permission failure; both are diagnose-only, so the blast radius is small | a linux run of a command denied by the sandbox |
| `A12` | no windows execution | a windows-only compile or test failure could still land | the windows jobs in the complete matrix, inspected first |
| `A13` | no linux execution | a linux-only failure could still land | the linux jobs in the complete matrix |
| `A18` | not every consumer of the observation struct was enumerated | a consumer might rely on containment that only holds fail-closed | a full read of the observation consumers |

## Prior Resolution Reconciliation

`None - initial review generation.`

## Receiving Handoff

- Handoff status: `Ready for receiving-code-review`
- Automatic receiving permitted: `Yes`
- Source report ID: `cr-20261001-7c1d4e`
- Scope fingerprint to recheck: `sha256:2ded103215d81587f4be3126698b08e607b345d73d7d382a5a047806fb883b62`
- Actionable finding IDs: `F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `T1, T2, T3`
- Deferred test-gap IDs: `None`
- Open question IDs: `F14`
- Open coverage area IDs: `A19`
- Highest-risk verification to repeat: `cargo test --locked --all-targets`, then the complete eleven-job matrix with the windows jobs inspected first.
- Suggested implementation boundaries: the command authority module and the command-exit branch of the verifier for the executable identity and the allowlist divergences; the git classification module alone for the argv and option-table findings and the matrix test gap; the three new or changed test surfaces for the platform and coverage findings and the confinement test gap; documentation and de-duplication edits for the remaining three.
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
- `yes` Generation `1` reconciles relevant parent terminal dispositions and records a reason for every reopened issue fingerprint. Not applicable at generation `0`.
- `yes` Every non-Question finding and standalone test gap appears exactly once in actionable or deferred handoff IDs; every Question and Not-covered area appears in its matching open list.
- `yes` Every meaningful subagent candidate has an adjudication.
- `yes` Every `Not covered` area has a reason and next step.
- `yes` Recommendation follows the skill mapping.
- `yes` The validator passes; generation `1` includes `--parent-report <generation-0-report> --parent-resolution <resolution-report>`.
- `yes` Git state was not mutated.
