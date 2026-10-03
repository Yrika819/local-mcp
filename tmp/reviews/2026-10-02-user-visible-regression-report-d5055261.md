# User-Visible Regression Review

## Scope

- Comparison: working tree against `HEAD 591145d52d1f9b2602593ba2a58c4f56e2c3edd4` on `security/verifier-classifier-closure-v1`.
- Target: the 12 changed/deleted `src/` paths in the porcelain-path-integrity and COMMAND_EXIT-contract follow-up.
- Review mode: complete static path review plus existing focused/full test evidence from the implementation run; no new production source changes during review.
- Explicit scope exclusions: Phase 5, unrelated audit items, and untracked `tmp/reviews/` artifacts.

## Gate Snapshot

- Recommendation: `Pass`
- Completion: `Complete within reviewed scope`, including the complete cross-platform CI matrix.
- Why now: the stricter COMMAND_EXIT contract is intentional, and the host-owned Git observation, mechanically evaluated verification paths, legacy durable handling, and Windows matrix completed without an identified accidental regression.
- Must-review now: none; see Intentional Changes for the command-verification behavior change.
- Findings count: `Block 0 | Discuss 0 | Watch 0 | Intentional 1`
- Coverage confidence: high for reviewed source paths, local tests, and the completed authoritative CI matrix.
- Behavior graph coverage: command proposal/materialization and durable legacy verification traced; host Git observation traced from query selection through raw path parsing to TaskScope gate.
- Biggest blind spot: no remaining material blind spot identified within this scoped diff.

## Complete Findings Index

No accidental user-visible regressions identified. The deliberate COMMAND_EXIT contract change is recorded under Intentional Changes below.

## Block

None.

## Discuss

No findings; the cross-platform CI matrix completed successfully.

## Watch

None.

## Intentional Changes

### I1 - New and legacy model-authored COMMAND_EXIT is unsupported

- User impact: newly proposed arbitrary command checks now fail plan/replan materialization with a deterministic schema/authority error; a legacy durable COMMAND_EXIT is not spawned and is durably moved to Blocked.
- Before: some exact verifier-approved command shapes could be materialized, and legacy COMMAND_EXIT could reach verification execution.
- After: the planner/replanner reject every new COMMAND_EXIT. Legacy durable values remain decodable, but Verifier returns a blocked check without spawning; the durable transition removes the task from repeated Verifying selection. Users must use mechanically evaluated verification specifications such as FILE_EXISTS, FILE_DIGEST, GIT_SCOPE, NO_FORBIDDEN_CHANGES, and supported structured evidence.
- Intent basis: explicit product decision in the security follow-up: model-authored COMMAND_EXIT is currently unsupported; do not add BUILD_VERIFICATION or widen execution authority.
- Evidence: shared plan validator rejects the variant; replanner routes through that validator; the legacy Verifier branch is no-spawn and records Blocked; tests cover planner/replanner rejection, legacy durable state and scheduler re-selection.
- This intentionally does not restore build/test/script command verification. A future write-capable build-verification design is outside this scope.

## Coverage Ledger

| Surface | Paths | Result | Evidence / gap |
| --- | --- | --- | --- |
| Planner and replanner verification contract | `src/planner.rs`, `src/replanner.rs` | Intentional I1 | Shared validator rejects every new CommandExit; replanner test asserts durable Goal bytes stay unchanged on schema rejection. |
| Model-facing planner/replanner instructions | `src/goal_backends.rs`, `src/goal_backends_tests.rs` | Intentional I1 | Prompt now says legacy decoding only and directs to mechanically evaluated specs; prompt contract regression exists. |
| Legacy verification and scheduler progress | `src/verifier.rs`, `src/verifier_tests.rs`, `src/scheduler.rs` callers | Intentional I1; reviewed, no loop regression found | Legacy command returns a blocked check without process spawn; durable status/revision assertions and repeated scheduler-selection assertions cover it. |
| Host-owned Git query path and approval | `src/verifier_git_observation.rs` | Reviewed - no accidental regression found | Exact query enum, Session cwd authority check, Windows approval request and activity remain; the completed Windows-x64 and compatibility jobs passed. |
| Porcelain status and staged-path interpretation | `src/verifier_git_observation.rs`, `src/verifier_tests.rs` | Reviewed - no accidental regression found | Raw NUL records; R/C consumes one following source path; both endpoints reach production scope checks; real Git rename fixture covers `ab forbidden.txt`. |
| Git raw output and command resource handling | `src/sandbox.rs`, `src/phase0_sandbox_tests.rs` | Reviewed - no accidental regression found | Raw API uses shared bounded process capture/timeout/cleanup; non-UTF-8 stdout test verifies bytes are retained. |
| Managed execution-root integration | `src/managed_worktree_creation_tests.rs` | Reviewed - no accidental regression found | Phase 4 integration remains on host-owned Git observation; managed-worktree test suite recorded 186 passing. |
| Test module wiring and removed command authority | `src/main.rs`, `src/verifier_command_authority.rs` | Not user-visible directly; reviewed | Obsolete allowlist module is removed with its call paths; durable enum decoding remains. |
| Cross-platform Windows path and approval behavior | `src/verifier_git_observation.rs`, Windows CI jobs | Reviewed - no accidental regression found | Complete CI run `36886569747` passed Windows-x64 tests and Windows-2022/Windows-arm64 compatibility jobs; Windows approval/path behavior retained by source inspection and available tests. |

## Evidence Appendix

### Behavior traces

**New plan/replan path**

`model proposal -> Planner/Replanner shared verification normalization -> COMMAND_EXIT schema/authority refusal -> no new durable Task materialized`

**Legacy durable path**

`durable COMMAND_EXIT -> Verifier legacy branch -> no process spawn + Blocked result -> committed Goal revision -> scheduler no longer selects Task as Verifying`

**Host-owned Git path**

`host-selected GitQuery -> Session execution-cwd authority check -> Windows approval when policy requires -> bounded clean raw Git runner -> lossless NUL/path parsing -> existing TaskScope / forbidden-path predicate`

**Porcelain rename delta**

Before: independently interpreting each NUL token as an XY-prefixed status record could strip bytes from a rename source path such as `ab forbidden.txt`.

After: parser validates the head record and consumes exactly one subsequent raw path token for R/C; both source and destination are resolved and included in the changed-path set.

### Verification evidence reviewed

- `cargo fmt --all -- --check` — passed.
- `cargo test --locked --all-targets managed_worktree -- --quiet` — 186 passed, 0 failed.
- `cargo test --locked --all-targets --quiet` — 911 passed, 0 failed.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` — passed.
- `git diff --check` — clean.
- Focused parser, real Git rename, forbidden source/destination scope, raw-byte, Planner/Replanner, and legacy scheduler tests — passed per implementation run.
- CI run `36886569747` — all 11 jobs completed successfully: test (ubuntu-x64), test (macos-intel), test (macos-arm64), test (windows-x64), compat (ubuntu-22.04), compat (ubuntu-26.04), compat (ubuntu-arm64), compat (macos-26), compat (windows-2022), compat (windows-arm64), and quality (ubuntu-24.04).
