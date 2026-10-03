# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261002-f50a6040`
- Review chain ID: `rc-20261002-f50a6040`
- Review generation: `0`
- Review trigger: `initial`
- Parent review report ID: `None`
- Parent review report path: `None`
- Parent resolution ID: `None`
- Parent resolution path: `None`
- Generated at: `2026-10-02T00:00:00Z`
- Report path: `tmp/reviews/2026-10-02-code-review-report-f50a6040.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:d54fb72f3a47d74045c386bfdf610cc4fb031137695c85f4536eeddeb4896096`

Treat this completed report as the fixed review input for downstream work. Do not rewrite it during receiving or implementation; record any later dispositions and verification in a separate resolution report.

## Scope

- Review date: `2026-10-02`
- Scope kind: `working tree`
- Scope description: Follow-up closure diff against `HEAD 591145d52d1f9b2602593ba2a58c4f56e2c3edd4`, covering raw Git path framing, rename/copy source handling, bounded raw host-Git output, Windows/Unix path conversion, TaskScope reconciliation, and removal of newly proposed model-authored `COMMAND_EXIT` with durable legacy handling.
- Scope mode: `full frozen scope`
- Baseline: `HEAD 591145d52d1f9b2602593ba2a58c4f56e2c3edd4`
- Target: working tree on `security/verifier-classifier-closure-v1`, diff SHA256 `d54fb72f3a47d74045c386bfdf610cc4fb031137695c85f4536eeddeb4896096`
- Changed paths: `12`
- Diff size: `587 additions / 830 deletions`
- Completion: `Complete within reviewed scope`
- Requirements consulted: current security follow-up contract; `SECURITY.md`; Goal/Task and Managed Worktrees frozen designs.
- Prior resolution consulted: `None`
- Assumptions: no host Git observation query is a model-authored `COMMAND_EXIT`; Windows non-UTF-8 Git path bytes are unsupported and must fail closed.
- Excluded as unrelated: existing untracked `tmp/reviews/` artifacts (not included in diff, changed, or staged); all unrelated audit items and Managed Worktrees Phase 5.

## Review Orchestration

- Assessment subagent: `Coordinator assessment - two independent high-risk paths (Git byte/path integrity and durable command capability retirement) benefit from disjoint specialist passes`
- Orchestration decision: `Parallel specialists`
- Decision confidence: `high`
- Decision rationale: porcelain framing/path conversion and planner/verifier/scheduler compatibility have distinct primary failure modes; independent review reduces the risk of missing a filesystem escape or durable-state loop. The coordinator re-read and adjudicated each candidate.
- Coordinator override: `None`
- Context or tool limits: Windows runtime/CI was not available locally; Windows-specific path rejection was statically checked and a Windows-only test was added. Full CI remains pending.

### Risk Dimensions

- NUL-framed Git paths are authority inputs; an incorrectly consumed source record can hide a forbidden path.
- Lossy conversion, root-relative or drive-relative paths can make filesystem containment decisions disagree with the emitted Git path.
- Host Git runs without an OS sandbox; the byte-preserving seam must preserve bounded output, timeout, process-group cleanup, clean environment, approval and Session authority.
- Durable legacy `COMMAND_EXIT` must make one terminal progress transition without spawning or being selected indefinitely.

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `R1` | Git porcelain framing, raw bytes, path containment | `src/verifier_git_observation.rs`, `src/sandbox.rs`, managed parser contracts | Both rename paths, lossless Unix conversion, Windows prefix/root rejection, bounds, approval and Session checks | Complete |
| `R2` | COMMAND_EXIT lifecycle and durable scheduling | `src/planner.rs`, `src/replanner.rs`, `src/verifier.rs`, scheduler callers | New-plan rejection, legacy decoding, durable blocked transition, repeated selection, no generic spawn | Complete |
| `Coordinator` | Integrated contract and final candidate adjudication | all changed paths and affected callers | Production TaskScope predicate, Phase 4 execution root, Windows policy, final tests | Complete |

### Synthesis Statement

The coordinator independently reproduced the `ab forbidden.txt` rename shape in a test-owned repository, traced both emitted paths through raw parsing and the production TaskScope predicate, and reviewed the bounded raw runner and approval/authority call path. The path reviewer found a Windows drive/root-relative escape candidate; the implementation was narrowed before this frozen review target to reject prefix/root components before joining and to require an absolute Git top-level path. The command reviewer found no alternate model-command spawn path or legacy scheduler loop. No code finding remains in the frozen target. Windows execution is explicitly left to the complete CI matrix.

## Review Snapshot

- Recommendation: `Discuss`
- Completion: `Complete within reviewed scope`
- Why now: no code defect remains in the reviewed target, but Windows-specific execution and the full matrix have not yet completed.
- Must-review now: `A10` Windows path/approval CI evidence
- Findings count: `Blocker 0 | Major 0 | Minor 0 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 0`
- Coverage confidence: `high` for macOS-host behavior and static cross-platform paths; `medium` overall
- Biggest blind spot: Windows-only path/approval execution and full remote CI.

## Complete Findings Index

No code-review findings identified in the reviewed scope.

## Blocker

None.

## Major

None.

## Minor

None.

## Questions

None.

## Test Gaps

None. The remaining Windows runtime item is tracked as uncovered platform verification in A10 rather than a missing source-level test; a Windows-only unit test now covers drive-relative and root-relative spellings.

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | Porcelain-v1 `-z` framing and R/C record state machine | `src/verifier_git_observation.rs:274-334` | `R1` | runtime + parser trace | `Reviewed - no issue found` | Requires termination, rejects empty/malformed tokens, validates XY and separator, consumes exactly one raw second path for R/C. Table-driven parser cases and real Git rename pass. | Source token is never parsed as an XY record. |
| `A2` | Raw bounded host-Git stdout | `src/sandbox.rs:629-865`, `src/verifier_git_observation.rs:250-271` | `R1` | implementation trace + runtime | `Reviewed - no issue found` | Raw API shares the same bounded capture/process-group/timeout/cleanup core; legacy string API remains a conversion wrapper. | Raw byte probe test passed; existing size/timeout/cleanup tests remain in full suite. |
| `A3` | Lossless path conversion and unsafe path rejection | `src/verifier_git_observation.rs:336-408` | `R1` | platform semantics + tests | `Reviewed - no issue found` | Unix uses `OsStringExt::from_vec`; Windows requires valid UTF-8 and rejects prefix/root/parent components before join. Top-level output is raw, framed, and required absolute. | Unix non-UTF8 observation passed. Windows-only rooted/drive-relative negative test is included but awaits Windows CI. |
| `A4` | Changed/staged Git paths into production scope checks | `src/verifier_git_observation.rs:300-334`, `src/verifier.rs:301-315,551-584,883-890`, `src/verifier_tests.rs` | `R1` + Coordinator | end-to-end runtime | `Reviewed - no issue found` | Both rename paths are returned; source-forbidden/destination-allowed and source-allowed/destination-forbidden tests assert the production TaskScope predicate rejects and Verifier blocks. | Real Git source case `ab forbidden.txt` reproduced. |
| `A5` | New plan and replan `COMMAND_EXIT` contract | `src/planner.rs:860-868`, `src/replanner.rs:1048,1168`, `src/goal_backends.rs` | `R2` | contract trace + focused tests | `Reviewed - no issue found` | Shared validator rejects every new `CommandExit` with deterministic mechanically-evaluated-spec guidance; prompt no longer advertises arbitrary command verification. | Planner/replanner tests pass and replan durable bytes remain unchanged. |
| `A6` | Legacy durable `COMMAND_EXIT` progress and scheduler behavior | `src/verifier.rs:503-508,673-740`, `src/scheduler.rs:215-229` | `R2` | persistence/control-flow trace + tests | `Reviewed - no issue found` | Legacy entry produces a blocked check without a command spawn; one revision-checked commit stores the indeterminate result and Blocked state. Scheduler no longer selects it as Verifying. | Tests repeat selection and prove no VerifyTask loop. |
| `A7` | Internal Git seam vs model command authority | `src/verifier_git_observation.rs:39-121,205-271`, `src/verifier.rs:503` | `R1` + `R2` | authority trace | `Reviewed - no issue found` | Host Git remains an exact query enum using resolved host Git; no generic argv route remains under COMMAND_EXIT. Windows approval policy/activity and Session cwd validation remain. | Observation envelope and Windows policy tests passed locally where applicable. |
| `A8` | Phase 4 managed execution-root regression | `src/managed_worktree_creation_tests.rs`, `src/verifier_git_observation.rs` | Coordinator | integration tests | `Reviewed - no issue found` | Managed integration still uses host-owned observation at the execution root; PRIMARY and managed path spelling contracts were not redesigned. | Managed suite: 186/186; full suite: 911/911. |
| `A9` | Durable state compatibility and model-facing contract text | `src/task.rs`, `src/goal_backends.rs`, `src/goal_backends_tests.rs` | `R2` | serialization/prompt trace | `Reviewed - no issue found` | Enum decoding remains present; only new plan materialization is rejected. Model prompt now says legacy decoding only and mechanically evaluated specs. | Prompt contract regression passed. |
| `A10` | Windows path and full cross-platform matrix | Windows-specific test and CI workflow | Coordinator | static platform trace | `Not covered` | Drive/root-relative forms are refused in code and have a Windows-only test, but the Windows runner and all remote CI jobs have not yet run. | Complete all 11 expected CI jobs; inspect Windows first. |

## Subagent Candidate Adjudication

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `R1-C1` | `R1` | `dismissed` | `None` | Current resolver rejects `Prefix`, `RootDir`, and `ParentDir` before `root.join`; top-level Git output must be absolute; Windows-only tests cover `C:outside.txt` and `\\rooted.txt`. | The candidate was valid before the final narrowing; it does not remain in the frozen target. Windows CI is still required. |
| `R1-C2` | `R1` | `dismissed` | `None` | Parser uses a record index and increments once to consume raw source path for R/C; real Git output and source/destination tests pass. | No independent-token XY-prefix heuristic remains. |
| `R1-C3` | `R1` | `dismissed` | `None` | Raw observer paths do not pass through `String::from_utf8_lossy`; Unix uses raw OsString bytes and Windows rejects invalid UTF-8. | No lossy path authority decision found. |
| `R2-C1` | `R2` | `dismissed` | `None` | Verifier’s legacy arm returns Blocked; commit is durable; repeated scheduler selection excludes the task. | No infinite selection loop or generic command route remains. |
| `R2-C2` | `R2` | `dismissed` | `None` | Both planner and replanner route through the shared verification normalizer; the Task enum still decodes legacy records. | No materialization bypass or schema removal found. |

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `src/goal_backends.rs` | surface | Planner prompt contract |
| `src/goal_backends_tests.rs` | test-only | Prompt regression |
| `src/main.rs` | config | Removed obsolete model-command authority module |
| `src/managed_worktree_creation_tests.rs` | test-only | Phase 4 contract note |
| `src/phase0_sandbox_tests.rs` | test-only | Raw bounded stdout |
| `src/planner.rs` | surface | New plan verification validation |
| `src/replanner.rs` | surface/test | Replan verification validation |
| `src/sandbox.rs` | surface | Bounded raw output seam |
| `src/verifier.rs` | surface | Legacy block and production scope predicate |
| `src/verifier_command_authority.rs` | deleted | Removed now-unreachable model command allowlist |
| `src/verifier_git_observation.rs` | surface | Host-owned Git execution, bytes and path parsing |
| `src/verifier_tests.rs` | test-only | Durable command behavior and rename scope |

### Verification Commands

- `cargo fmt --all -- --check` -> passed.
- `cargo test --locked --all-targets managed_worktree -- --quiet` -> 186 passed, 0 failed.
- `cargo test --locked --all-targets --quiet` -> 911 passed, 0 failed.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> passed.
- `cargo test --locked verifier_git_observation -- --nocapture` -> 11 passed, 0 failed.
- Focused Planner, replanner, legacy scheduler, raw-byte, and both forbidden rename endpoint tests -> passed.
- `git diff --check` -> clean.

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `A1` | parser | [`verifier_git_observation.rs`](/Users/yuta/local-mcp-connector-parity/src/verifier_git_observation.rs#L274) | Exact NUL framing and rename/copy consumption. |
| `A3` | path resolution | [`verifier_git_observation.rs`](/Users/yuta/local-mcp-connector-parity/src/verifier_git_observation.rs#L336) | Lossless conversion and prefix/root containment. |
| `A5` | planner gate | [`planner.rs`](/Users/yuta/local-mcp-connector-parity/src/planner.rs#L860) | Deterministic new-plan rejection. |
| `A6` | legacy behavior | [`verifier.rs`](/Users/yuta/local-mcp-connector-parity/src/verifier.rs#L503) | No-spawn durable legacy block. |
| `A10` | Windows runtime gap | [`verifier_git_observation.rs`](/Users/yuta/local-mcp-connector-parity/src/verifier_git_observation.rs#L790) | Windows-only negative test awaiting CI. |

### Dismissed Coordinator Candidates

| Candidate | Decision | Evidence |
| --- | --- | --- |
| Index writes from status | dismissed | Host query includes `--no-optional-locks`; argv test and no-write index snapshot test cover it. |
| Repository-configured hooks/fsmonitor/diff helpers | dismissed | Hooks are redirected, fsmonitor disabled, environment cleaned; staged diff uses `--no-ext-diff --no-textconv`; hostile monitor test passes. |
| Rename destination only entering scope | dismissed | Both endpoints asserted in actual Git observation, then the production TaskScope predicate is called for both endpoint directions. |
| Legacy task remains VERIFYING after unsupported command error | dismissed | Verifier no longer returns an error for the durable variant; blocked result is committed and scheduler re-selection test passes. |
| Windows drive-relative/root-relative path escape | resolved before freeze | Prefix/root component rejection and absolute top-level output check precede canonicalization/join; runtime remains pending. |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A10` | No Windows runner or remote CI result available at this point. | Windows `Path` semantics, approval integration, and platform compilation require the authoritative matrix. | Push branch and wait for all 11 jobs; inspect Windows-x64/Windows-arm64 first. |

## Prior Resolution Reconciliation

None - initial generation for this follow-up review chain. Deferred rename-parser and COMMAND_EXIT product items are explicitly included in the current user requirements and are now in scope.

## Receiving Handoff

- Handoff status: `Ready for receiving-code-review`
- Automatic receiving permitted: `Yes`
- Source report ID: `cr-20261002-f50a6040`
- Scope fingerprint to recheck: `sha256:d54fb72f3a47d74045c386bfdf610cc4fb031137695c85f4536eeddeb4896096`
- Actionable finding IDs: `None`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `None`
- Deferred test-gap IDs: `None`
- Open question IDs: `None`
- Open coverage area IDs: `A10`
- Highest-risk verification to repeat: complete 11-job CI matrix, particularly Windows-x64 and Windows-arm64.
- Suggested implementation boundaries: None unless Windows CI identifies a causal issue; do not widen command authority.
- Re-review note: Treat every finding as a claim to verify. Challenges require a counterclaim, argument, evidence, limits, and settlement criterion.
- Chain rule: Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or owner.

## Report Self-Check

- `yes` Actual assessment mode and rationale are recorded.
- `yes` Every changed review-relevant or unknown-impact area appears once in the ledger.
- `yes` There are no final findings; the index and finding cards reflect that.
- `yes` There are no standalone test gaps; Windows runtime is tracked as Not covered.
- `yes` Generation, trigger, parent state, scope mode, and handoff are consistent.
- `yes` Both reviewers' candidates were independently adjudicated.
- `yes` The uncovered platform area has a concrete next verification step.
- `yes` Recommendation follows the skill mapping: an uncovered cross-platform area yields Discuss.
- `yes` Git state was not mutated during this frozen review.
