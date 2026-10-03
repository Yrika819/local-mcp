# Receiving Code Review Resolution

## Report Contract

- Report type: `receiving-code-review`
- Report ID: `rr-20261001-9ab2f1`
- Resolution ID: `rr-20261001-9ab2f1`
- Review chain ID: `rc-20261001-7c1d4e`
- Review generation being received: `0`
- Source report ID: `cr-20261001-7c1d4e`
- Source review report ID: `cr-20261001-7c1d4e`
- Source review report path: `tmp/reviews/2026-10-01-code-review-report-verifier-closure-7c1d4e.md`
- Generated at: `2026-10-01T00:00:00Z`
- Report path: `tmp/reviews/2026-10-01-receiving-code-review-resolution-verifier-closure-9ab2f1.md`
- Git mutation during receiving: the implementation commits recorded below, all on `security/verifier-classifier-closure-v1`; no other branch, tag, or worktree was touched
- Status: `Resolution complete`

## Scope and Authorization

- Authorization basis: the task brief authorizes continuing into the fixes, a bounded re-review, the regression review, the push, and the full matrix after the code review.
- Baseline at freeze: `f899ad31033540d666389d2520fd040fe267cbc7`
- Branch: `security/verifier-classifier-closure-v1`

## Dispositions

| ID | Severity | Disposition | Change made | Verification |
| --- | --- | --- | --- | --- |
| `F1` | Blocker | Fixed | Added `host_git_argv` in `src/verifier.rs`, which replaces the proposal's first argument with `execution::host_git_path()` before the command reaches the execution layer. The authority module now documents that it decides the argv only. | New negative test `unapproved_verification_commands_are_refused_before_spawn` and the existing approved-observation test both run through the substituted argv; `cargo test --locked --all-targets` green. |
| `F2` | Blocker | Fixed | Moved `--format`, `--sort`, `--column`, and `--no-column` from the selector table to the display table for both `branch` and `tag` in `src/git_command_class.rs`, and made `has_flag` match the `--flag=value` spelling so an attached value cannot drop a mutation claim. | Coordinator reproduced all three shapes against git 2.50.1 before the change; `a_display_flag_alone_does_not_turn_a_branch_name_into_a_pattern` now asserts twelve mutation shapes and four observation shapes. |
| `F3` | Blocker | Fixed | Restored `#[cfg(not(windows))]` on the command exit lifecycle test and simplified the assertion to the single reachable outcome, with a comment explaining why the windows leg is covered by the managed integration test instead. | `cargo test --locked --all-targets` green on this host; the windows leg is delegated to the matrix. |
| `F4` | Blocker | Fixed | Added `test_observe_git_with_policy` to `src/verifier.rs` and switched the managed execution-root test to an explicit policy; switched the three seam tests in `src/verifier_git_observation.rs` to `observe_with_policy(..., false)`. | Same. |
| `F5` | Major | Fixed | Removed the `status` and `diff` tails from `OBSERVATION_TAILS` and rewrote the documentation to say why, rather than narrowing the table to one optional-global spelling. | `mutating_and_ambiguous_git_shapes_are_refused_before_spawn` now covers both; the invariant test asserts every approved tail is side-effect free. |
| `F6` | Major | Fixed | Closed by the same removal: `status` and `diff` were the only approved tails that consult a filesystem monitor, a conversion filter, or an external diff driver, and they also lacked the suppression the host seam applies. | `host_owned_observation_never_runs_a_repository_chosen_program` proves the host seam is immune to a repository-chosen monitor. |
| `F7` | Major | Fixed | Added an `owns_root` flag so a caller-supplied workspace survives the fixture, added an explicit `TaskScope` parameter, pre-created a sentinel in every destination, and covered the task-scope-forbidden leg and file alteration in addition to creation. | `a_pure_verifier_command_cannot_write_anywhere_it_is_permitted_to_see` green. |
| `F8` | Major | Fixed | Rewrote the removal note to name the actual replacement test, to state the real reason, and to name the narrower property that is now uncovered. | Documentation-only; verified by reading the test names in the tree. |
| `F9` | Minor | Fixed | Corrected the `show_ref` table: `-d` is a read, `--delete` is not a git option and now falls through to the unknown class. | `show_ref_reads_verify_and_dereferences_but_never_deletes` green; reproduced against git 2.50.1. |
| `F10` | Minor | Fixed | The detached-value branch now advances for a long option or a bare short option, not only one-character short options. | `the_detached_value_of_a_long_global_option_is_not_read_as_a_subcommand` green. |
| `F11` | Minor | Fixed | Rewrote the `resolve_git_path` documentation to state what is refused and to record that a symlink is followed and fails closed downstream rather than being rejected. | Documentation-only. |
| `F12` | Minor | Fixed | `src/verifier.rs` now delegates to the single implementation in `src/verifier_git_observation.rs` and maps the error. | `git_scope_and_forbidden_changes_are_host_observed` and the windows path-spelling regression both still exercise it. |
| `F13` | Minor | Fixed | Rewrote the module documentation to state accurately that the observation is not sandboxed, why the host will still only run that one narrow shape, and which tests hold the envelope honest. | Documentation-only, cross-referenced from two named tests. |
| `T1` | Major | Fixed | Added `host_owned_observation_writes_nothing_anywhere`, which snapshots an outside sentinel, the index bytes, the index modification time, and every worktree entry, then asserts all are unchanged after an observation; added the hostile-monitor test. | Both green. |
| `T2` | Minor | Fixed | The denial test now asserts the failure text is specifically the approval gate, which ties the platform policy constant to a real approval request rather than a constant comparison. | `an_unapproved_observation_performs_no_git_invocation` green. |
| `T3` | Minor | Fixed | Extended the classifier matrix with the rendering-flag mutation shapes, the paired observation shapes, the dereference, and the detached global option. | `git_command_class` module: 17 tests green. |
| `F14` | Question | Not resolved in code | No code change. The brief instructs reporting rather than guessing, and resolving it either way requires a product decision or new execution authority. | Reported in the final handoff with the full lifecycle trace and a settlement criterion. |

## R1-C6 Disposition (dismissed, recorded as out of scope)

The porcelain status parser strips a three-byte status prefix from any token whose third byte is a space, which can corrupt a rename source path. The coordinator confirmed the identical logic exists at the baseline commit, so this is pre-existing rather than introduced. Fixing it would change the changed-path set produced for renames, which changes `GIT_SCOPE` and `NO_FORBIDDEN_CHANGES` semantics and needs its own review. It is recorded in the final handoff as a follow-up, not fixed here.

## R3-C6 Disposition (deferred)

The new observation tests create temporary directories without a drop guard and assume the system temporary directory is not inside a Git worktree. Both match the existing test convention in this crate. Deferred.

## Verification After Receiving

- `cargo fmt --all` then `cargo fmt --check` -> clean.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> clean.
- `cargo test --locked --all-targets` -> 912 passed, 0 failed.
- `cargo test --locked --all-targets managed_worktree` -> 186 passed, 0 failed.
- `git diff --check` -> clean.

## Remaining Items Returned to the Owner

- `F14`: whether a command exit verification is expected to complete on any platform, and whether the kind should be refused at plan materialization.
- The `R1-C6` porcelain rename-source parser weakness, as a separate bounded change.
- The platform-conditional blind spots `A12`, `A13`, and `A19`, which the eleven-job matrix is the first real check for.
