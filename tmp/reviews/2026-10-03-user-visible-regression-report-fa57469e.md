# User-Visible Regression Audit

## Scope

- Review date: `2026-10-03`
- Requested outcome: `review only`
- Continuation: `report only`
- Scope reviewed: `git diff origin/main...HEAD` (branch diff), branch `hardening/writer-integrity-v1`
- Baseline: `1c23f2279a4183e33fc3de3d1a9b646adda2a749` (`origin/main`)
- Completion: `Complete within reviewed scope`
- Assumptions: Host is macOS x86_64 (`x86_64-apple-darwin`). Windows runtime behavior was
  verified by **compile-checking the changed commit code for `x86_64-pc-windows-msvc` in an
  isolated scratch crate** and by static reading; no Windows execution was possible. The
  repository has an established review convention at `tmp/reviews/`, which this report follows.

## Gate Snapshot

- Recommendation: `Block`
- Completion: `Complete within reviewed scope`
- Why now: Two findings hard-break the primary user journey (every `write_file` and every
  Writer commit) for documented install layouts, and one finding leaves a multi-file Goal
  permanently un-resumable. These are shipping-blockers, not caveats.
- Must-review now:
  1. `F1` `write_file` and every Writer commit fail outright when `atomic-publish` is not a sibling of `local-mcp` — which is exactly what the macOS and Windows install docs instruct.
  2. `F2` A commit-time preimage refusal leaves the Goal `BLOCKED` with no reconciliation rule that can clear it.
  3. `F3` The Windows `write_file` approval gate is now requested but the refusal it guards is not what fails first.
- Findings count: `Block 2 | Discuss 2 | Watch 2 | Intentional 5`
- Coverage confidence: `medium-high` for Unix/macOS (executed), `medium` for Windows (compile + read only)
- Behavior graph coverage: `built for 6 surfaces` (MCP `write_file`, Writer commit, helper
  resolution, release packaging, durable recovery, reviewer continuation)
- Biggest blind spot: No Windows runtime execution. `MoveFileExW` no-clobber/replace behavior,
  the `ERROR_FILE_EXISTS`/`ERROR_ALREADY_EXISTS` mapping, and the Windows approval ordering
  are unverified at runtime.

## Complete Findings Index

| ID | Action | Surface | User-visible outcome | Confidence |
| --- | --- | --- | --- | --- |
| `F1` | `Block` | MCP `write_file` + Writer commit, macOS/Windows install layout | Every file write fails with "atomic publication helper is missing"; install docs instruct exactly this layout | high |
| `F2` | `Block` | Goal/Task orchestrator, multi-file Writer task | Goal is permanently `BLOCKED` with an unreconciled mutation intent; `goal_resume` cannot clear it | medium-high |
| `F3` | `Discuss` | MCP `write_file`, Windows | Windows approval prompt still fires and can be denied, then the write fails anyway for a different reason — the gate is now decorative on the common failure path | medium |
| `F4` | `Discuss` | MCP `write_file`, Unix file modes | A new file published via `link()` inherits the staging file's mode rather than a predictable umask-derived mode in some cases; mode handling for pre-existing files is correct | low-medium |
| `F5` | `Watch` | MCP `write_file` activity notification | The "Edited … (+N -M)" activity is still emitted on refusal paths, with the error only in `detail` | high |
| `F6` | `Watch` | Unix metadata | Ownership, ACLs, xattrs, and macOS resource forks are lost on every Writer/`write_file` replacement | high |
| `I1` | `Intentional` | Writer commit | Atomic publication, no truncated files | high |
| `I2` | `Intentional` | Writer commit | Commit-time preimage revalidation refuses to overwrite a changed file | high |
| `I3` | `Intentional` | MCP `write_file` | Read errors and non-UTF-8 existing files now fail instead of silently overwriting | high |
| `I4` | `Intentional` | Unix metadata | setuid/setgid/sticky no longer preserved across replacement | high |
| `I5` | `Intentional` | Release packaging | `atomic-publish` must ship next to `local-mcp` | high |

## Block

### F1 Block - Every file write fails on the documented macOS and Windows install layout

User impact: After following the shipped installation guide verbatim on macOS or Windows,
the MCP `write_file` tool and every Writer file commit fail with
`atomic publication helper is missing; looked in …`. Nothing writes. The install appears to
succeed and only fails when the user actually asks for a file change.

Review reason: The release pipeline now correctly ships `atomic-publish` on all platforms,
but the user-facing installation instructions were not updated, so the most likely real-world
install produces a completely non-functional file-writing tool with no fallback.

Surface: MCP `write_file` tool; Writer commit path; macOS and Windows installation

Confidence: high

Look here first:
- [execution.rs](/Users/yuta/local-mcp-connector-parity/src/execution.rs#L60) — helper resolution, single candidate in production, `bail!` on miss
- [INSTALL.md](/Users/yuta/local-mcp-connector-parity/docs/release/INSTALL.md#L118) — macOS "One executable, `local-mcp`. There is no helper to install"

Behavior delta:
- Before: On macOS the Writer and `write_file` used `sh -c 'cat > "$1"'` inside the
  Seatbelt sandbox; on Windows they used in-process `tokio::fs::write`. Neither needed a
  second binary. A single-binary install was fully functional.
- After: Both paths call `publish_workspace_write` → `atomic_publish_helper()`, which looks
  only at `current_exe().parent()/atomic-publish` in a non-test build and hard-fails when it
  is absent. A single-binary install is completely non-functional for file writes.

Evidence:
- `scripts/release/targets.py` and `.github/workflows/release.yml` were both updated to ship
  `atomic-publish` on all six targets, and `python3 scripts/release/test_release_scripts.py`
  passes 19/19 including `test_every_platform_ships_the_writer_commit_helper`. So the
  *archive* is correct.
- `docs/release/INSTALL.md:122` still says macOS has "One executable, `local-mcp`. There is
  no helper to install". `INSTALL.md:157` still says Windows has "One executable,
  `local-mcp.exe`". The macOS `install -m 0755 …/local-mcp /usr/local/bin/local-mcp` command
  copies only one file, and the Windows `Copy-Item … local-mcp.exe C:\tools\local-mcp.exe`
  copies only one file. `README.md:322` likewise still says "On macOS, only `local-mcp` is
  needed".
- Reproduced the resolution logic in isolation: `atomic_publish_helper()` builds a
  single-element `candidates` vec in non-test builds (the `deps` parent fallback is
  `#[cfg(test)]` only), and `bail!`s with the looked-in path list.
- Independently confirmed with a scratch crate that `cargo run` does **not** rebuild a
  sibling `[[bin]]` after it has been deleted, so even a developer's `cargo run` checkout
  that once had `target/debug/atomic-publish` removed will not self-heal.

Reviewer action:
Block until `docs/release/INSTALL.md` (macOS and Windows archive contents, install
commands, and the "no helper to install" statements), `README.md:322` and `README.md:134`,
and the `CHANGELOG.md` Unreleased section all state that `atomic-publish` is a required
sibling on every platform, and that omitting it disables all file writes.

## Discuss

### F2 Discuss - A commit-time preimage refusal leaves the Goal permanently BLOCKED

User impact: In a multi-file Writer task, if an external edit (the user's editor, another
tool, a formatter) lands on operation N's target between materialization and commit, the
Goal transitions to `BLOCKED` with blocker `WRITER_PREIMAGE_CHANGED`. `goal_resume` does not
clear it. The user is left with a Goal that reports `BLOCKED` indefinitely and can only be
escaped by `goal_cancel`, losing the rest of the plan.

Review reason: This is the newly added refusal path, and it introduces a durable state that
no existing reconciliation rule can resolve. It is a genuinely new user-visible outcome, so
it needs an explicit decision rather than a silent merge.

Surface: Goal/Task orchestrator, multi-file Writer task, `goal_resume`

Confidence: medium-high

Look here first:
- [writer.rs](/Users/yuta/local-mcp-connector-parity/src/writer.rs#L686) — refusal records `WRITER_PREIMAGE_CHANGED` and transitions the task to `Blocked`
- [goal_api.rs](/Users/yuta/local-mcp-connector-parity/src/goal_api.rs#L1262) — the auto-unblock allowlist does not contain the new blocker code

Behavior delta:
- Before: A stale preimage detected during `materialize_operations` was mapped to
  `finish_valid_non_mutating_result` with `NeedsReplan` — a non-mutating outcome the
  replanner absorbs, with no durable intent created. The Goal stayed live and replanned.
- After: The commit-time gate runs *after* `task_prepare_latest_mutation_intent` has already
  persisted a durable `MutationIntent`. On refusal the task is moved to `Blocked` with a new
  blocker code. `reconcile_safe_writer_blocked_tasks` only auto-clears blockers in
  `{WRITER_BLOCKED, WRITER_BACKEND_ERROR, WRITER_OUTPUT_REJECTED, WRITER_OPERATION_REJECTED}`
  and additionally requires `latest.operation_id().is_none()` — which is false here because
  the intent is bound. `reconcile_legacy_writer_blocked_tasks` requires the blocker code to be
  exactly `WRITER_BLOCKED` and `attempts.len() == 1`. Neither matches.

Evidence:
- Static trace: `src/writer.rs:686-698` (refusal), `src/goal_api.rs:1262-1315`
  (allowlist + `operation_id().is_none()` requirement), `src/goal_api.rs:1317-1344`
  (legacy path requires `WRITER_BLOCKED`), `src/mutation_recovery.rs:85` (reconciliation only
  considers `TaskStatus::Running` tasks, and the task is now `Blocked`).
- Confirmed by the branch's own test
  `writer::tests::a_commit_time_preimage_change_is_recorded_not_swallowed_by_a_revision_conflict`,
  which asserts the task ends `Blocked` with a `WRITER_PREIMAGE_CHANGED` blocker and never
  exercises a subsequent resume.
- Design doc §4 states the refusal "returns `PreimageMismatch`, which the existing caller maps
  to `finish_valid_non_mutating_result` with `NeedsReplan`. No durable intent exists". The
  implementation does neither: a durable intent *does* exist, and the caller is
  `block_before_mutation`, not `finish_valid_non_mutating_result`. The implementation and the
  frozen design disagree.
- Not reproduced as a full end-to-end stall: I did not drive `goal_resume` against a
  `BLOCKED` Goal carrying this exact blocker through the public MCP surface.

Reviewer action:
Raise in review and confirm intent: either route the commit-time refusal through
`finish_valid_non_mutating_result`/`NeedsReplan` as §4 specifies, or add
`WRITER_PREIMAGE_CHANGED` to the resume reconciliation rules. Also reconcile the §4/§11 text,
which currently describes a state the code does not produce.

## Watch

### F3 Watch - The Windows write_file approval gate no longer gates the failing operation

User impact: On Windows the user is still prompted to approve `write_file_host_native`, but
on the common failure paths (missing helper, refused preimage) the write fails for a reason
the approval had nothing to do with. The prompt implies a decision that no longer changes the
outcome.

Review reason: The gate itself is preserved, which is good, but its relationship to the new
failure surface changed and is worth an explicit decision.

Surface: MCP `write_file`, Windows

Confidence: medium

Look here first:
- [mcp.rs](/Users/yuta/local-mcp-connector-parity/src/mcp.rs#L576) — approval requested before the read and publish
- [execution.rs](/Users/yuta/local-mcp-connector-parity/src/execution.rs#L60) — helper resolution happens after the prompt

Behavior delta:
- Before: On Windows, approval gated the only failure mode of an in-process
  `tokio::fs::write`. Denying approval was the meaningful stop.
- After: Approval still gates, but the operation can now also fail on helper resolution,
  preimage refusal, and staging failures that approval does not cover.

Evidence:
- Static trace of `src/mcp.rs:576-630`. The approval block is unchanged and still precedes the
  publish. The new failure modes are all downstream of it.
- Windows runtime not executed; ordering is read from source only.

Reviewer action:
Approve with caveat; confirm the Windows approval story is still coherent with the new
refusal taxonomy before shipping.

## Intentional Changes

- `I1` Atomic Writer publication replaces in-place truncation - matches design §6 -
  [workspace_publish.rs](/Users/yuta/local-mcp-connector-parity/src/workspace_publish.rs#L369)
- `I2` Commit-time preimage revalidation refuses to overwrite a changed target - matches design §4 -
  [writer.rs](/Users/yuta/local-mcp-connector-parity/src/writer.rs#L1645)
- `I3` MCP `write_file` fails on unreadable / non-UTF-8 existing files instead of silently overwriting - matches design §12 -
  [mcp.rs](/Users/yuta/local-mcp-connector-parity/src/mcp.rs#L596)
- `I4` setuid/setgid/sticky intentionally dropped across replacement - matches design §8 -
  [workspace_publish.rs](/Users/yuta/local-mcp-connector-parity/src/workspace_publish.rs#L255)
- `I5` `atomic-publish` ships next to `local-mcp` on every platform - matches design §3/§13a -
  [targets.py](/Users/yuta/local-mcp-connector-parity/scripts/release/targets.py#L162)

## Coverage Ledger

| Surface / path | Touched files or entry points | Status | Result | Evidence |
| --- | --- | --- | --- | --- |
| MCP `write_file` — existing UTF-8 file | [mcp.rs](/Users/yuta/local-mcp-connector-parity/src/mcp.rs#L559) | Finding `F1` | Publishes via helper; fails if helper absent | test `write_file_updates_an_existing_utf8_file` passes; helper-absent path traced statically |
| MCP `write_file` — new file | same | Finding `F1` | Uses `link()` no-clobber path | test `write_file_creates_a_new_file` passes |
| MCP `write_file` — read error | same | `Intentional I3` | Fails before mutation | test `write_file_refuses_a_target_that_became_a_directory` passes |
| MCP `write_file` — non-UTF-8 | same | `Intentional I3` | Fails before mutation, bytes preserved | test `write_file_refuses_a_non_utf8_existing_file_without_mutating_it` passes |
| MCP `write_file` — concurrent edit | same | `Finding F5` | Refused; activity title still says "Edited" | static trace of `mcp.rs:631-642` |
| MCP `write_file` — Windows approval gate | [mcp.rs](/Users/yuta/local-mcp-connector-parity/src/mcp.rs#L576) | Finding `F3` | Preserved, but decoupled from new refusals | static trace; Windows not executed |
| Writer ordinary small task → Verifying | [writer.rs](/Users/yuta/local-mcp-connector-parity/src/writer.rs#L463) | Reviewed - no regression found | Still reaches `Verifying` | test `valid_write_utf8_uses_host_authority_and_stops_in_verifying` passes |
| Writer multi-file task | [writer.rs](/Users/yuta/local-mcp-connector-parity/src/writer.rs#L676) | Finding `F2` | Each file independently atomic; refusal blocks the Goal | test `a_multi_file_attempt_publishes_each_operation_independently` passes; stall path traced statically |
| Writer commit-time revalidation | [writer.rs](/Users/yuta/local-mcp-connector-parity/src/writer.rs#L1645) | `Intentional I2` | Refuses before `BeginOperation` | tests `commit_time_revalidation_refuses_*` (3) pass |
| Writer durable mutation state machine | [mutation.rs](/Users/yuta/local-mcp-connector-parity/src/mutation.rs#L238) | Reviewed - no regression found | `PREPARED/APPLYING/APPLIED` preserved; order matches design §7 | tests `a_commit_time_preimage_change_is_recorded_not_swallowed_by_a_revision_conflict`, `successful_write_before_checkpoint_reconciles_as_performed` pass |
| Reviewer continuation flow | [writer.rs](/Users/yuta/local-mcp-connector-parity/src/writer.rs#L947) | Reviewed - no regression found | `resume_writer_reviewer` untouched by the diff | `git diff` shows zero hunks touching it; related tests pass |
| Mutation recovery after crash | [mutation_recovery.rs](/Users/yuta/local-mcp-connector-parity/src/mutation_recovery.rs#L21) | Reviewed - no regression found | `classify_observations` untouched; design §11 table matches | file not in diff |
| PRIMARY mode | [planner.rs](/Users/yuta/local-mcp-connector-parity/src/planner.rs#L426) | Reviewed - no regression found | `execution_root_for_goal` re-used identically by the new gate | static trace of `revalidate_before_commit` |
| Managed-worktree mode | [writer.rs](/Users/yuta/local-mcp-connector-parity/src/writer.rs#L1662) | Reviewed - no regression found | Gate uses `execution_root_for_goal`, so it follows the managed root | static trace; not executed end-to-end |
| Windows `MoveFileExW` commit path | [workspace_publish.rs](/Users/yuta/local-mcp-connector-parity/src/workspace_publish.rs#L591) | Reviewed - no regression found | Compiles clean for `x86_64-pc-windows-msvc` in isolation | isolated `cargo check --target x86_64-pc-windows-msvc` on the two changed modules: Finished, 0 errors |
| Windows `sandbox::run` | [sandbox.rs](/Users/yuta/local-mcp-connector-parity/src/sandbox.rs#L586) | Reviewed - no regression found | `#[cfg(not(unix))]` shim forwards to `run_tracked`; no new containment claimed | static read |
| Helper resolution / dev builds | [execution.rs](/Users/yuta/local-mcp-connector-parity/src/execution.rs#L60) | Finding `F1` | `#[cfg(test)]` fallback only; `cargo run` does not rebuild a removed sibling bin | scratch-crate experiment |
| Release packaging | [targets.py](/Users/yuta/local-mcp-connector-parity/scripts/release/targets.py#L162), [release.yml](/Users/yuta/local-mcp-connector-parity/.github/workflows/release.yml#L165) | `Intentional I5` | All six targets ship the helper | `test_release_scripts.py` 19/19 pass |
| Installation docs | [INSTALL.md](/Users/yuta/local-mcp-connector-parity/docs/release/INSTALL.md#L118), [README.md](/Users/yuta/local-mcp-connector-parity/README.md#L322) | Finding `F1` | Docs still describe single-binary installs | direct read |
| Unix file modes (executable bit) | [workspace_publish.rs](/Users/yuta/local-mcp-connector-parity/src/workspace_publish.rs#L255) | Reviewed - no regression found | `mode & 0o777` inherited and applied pre-publication | tests `an_executable_file_keeps_its_executable_bit`, `an_ordinary_non_executable_file_stays_non_executable`, `an_executable_target_keeps_its_mode_across_the_sandboxed_commit` pass |
| Verifier security closure | [verifier.rs](/Users/yuta/local-mcp-connector-parity/src/verifier.rs) | Reviewed - no regression found | Not touched by the diff | file absent from `git diff --stat` |
| MCP response shape | [mcp.rs](/Users/yuta/local-mcp-connector-parity/src/mcp.rs#L1106) | Reviewed - no regression found | `text_result(render_output(...))` unchanged | static trace |
| Path identity encoding | [mutation.rs](/Users/yuta/local-mcp-connector-parity/src/mutation.rs#L346) | Reviewed - no regression found | Lossless bytes; only new intents get the new value | tests `a_non_utf8_path_still_yields_a_lossless_identity`, `scope_identity_*` pass |
| Scratch `.staged` debris visibility | [workspace_publish.rs](/Users/yuta/local-mcp-connector-parity/src/workspace_publish.rs#L300) | Reviewed - no user-visible regression found | Named for the durable request id; cleaned on failure | tests `creates_an_absent_file_and_leaves_no_staging_debris`, `cleanup_refuses_paths_that_are_not_our_staging_shape` pass |

## Evidence Appendix

### Behavior Graph Deltas

| ID | Surface | Baseline path | After-change path | Delta | Ledger / finding link |
| --- | --- | --- | --- | --- | --- |
| `B1` | MCP `write_file` | Entry `write_file` → Input path/content → Guards `validate_path_authority` (+Windows approval) → Transform `read_to_string().unwrap_or_default()` → Effect `sh -c cat` (Unix) / `tokio::fs::write` (Windows) | Entry `write_file` → Input path/content → Guards `validate_path_authority` (+Windows approval) → Transform `tokio::fs::read` + UTF-8 check → Effect `atomic-publish` helper via `sandbox::run` | Transform changed (read errors now fatal); Output changed (helper dependency added) | `F1`, `I3` |
| `B2` | Writer commit | Entry `run_writer_attempt` → Input model ops → Guards `materialize_operations` → Transform durable PREPARED intent → Effect `BeginOperation` → `sh -c cat` → postimage | Entry `run_writer_attempt` → Input model ops → Guards `materialize_operations` **+ new `revalidate_before_commit`** → Transform durable PREPARED intent → Effect `BeginOperation` → helper → postimage | New guard inserted between durable prepare and `BeginOperation`; new refusal branch blocks the task | `F2`, `I2` |
| `B3` | Helper resolution | n/a | Entry `publish_workspace_write` → Input `current_exe().parent()` → Guard `candidate.is_file()` → Effect `bail!` | New hard dependency; no fallback path exists | `F1`, `I5` |
| `B4` | Durable recovery | Entry `goal_resume` → Input blocked task → Guards reconciliation allowlists → Effect Ready/Replan | same entry, new blocker code absent from every allowlist | Guard missing for the new state | `F2` |
| `B5` | Release packaging | Entry build → Input targets → Guards `targets.py` → Effect archive contents | same, `required_helpers` extended on all six targets | Output changed on macOS + Windows | `I5`, `F1` |
| `B6` | Reviewer continuation | Entry `resume_writer_reviewer` → … → reviewer verdict | unchanged (no hunks) | none | Reviewed |

### Diff Inventory

| File or area | Classification | User-visible path considered |
| --- | --- | --- |
| `src/writer.rs` | surface | Writer commit, reviewer continuation, durable intent ordering |
| `src/execution.rs` | surface | MCP `write_file`, Writer commit mechanism, helper resolution |
| `src/mcp.rs` | surface | MCP `write_file` tool and its response/notification |
| `src/sandbox.rs` | surface | Windows `run` shim; sandbox containment boundary |
| `src/mutation.rs` | surface | Durable mutation identity and dedup |
| `src/workspace_publish.rs` | surface | Atomic commit, file modes, durability, cleanup |
| `src/bin/atomic-publish.rs` | surface | The shipped helper's entry point and error tokens |
| `src/atomic_publish_frame.rs` | dependency | Wire format shared by host and helper |
| `src/atomic_publish_tests.rs` | test-only | Sandbox containment of the helper |
| `scripts/release/targets.py` | config | Archive contents per platform |
| `scripts/release/test_release_scripts.py` | test-only | Archive layout assertions |
| `.github/workflows/release.yml` | config | Build + archive + smoke assertions |
| `docs/WRITER_INTEGRITY_V1_DESIGN.md` | docs-only | Frozen intent used as the regression yardstick |
| `docs/release/INSTALL.md`, `README.md`, `CHANGELOG.md` | docs-only | Install journey — **not updated** (see `F1`) |

### Candidate Sweep Log

| Candidate | Decision | Reason |
| --- | --- | --- |
| Ordinary small Writer task stops reaching `Verifying` | dismissed | `valid_write_utf8_uses_host_authority_and_stops_in_verifying` passes |
| Reviewer continuation broken | dismissed | `resume_writer_reviewer` has zero diff hunks |
| Verifier security closure weakened | dismissed | verifier files absent from the diff |
| Multi-file Writer task is treated as a transaction | dismissed | Loop remains sequential per-operation; no rollback added, matching design §10 |
| Unix executable bit lost | dismissed | Three dedicated tests pass, including through the real sandbox |
| Failed publication wrongly reports success in the Writer path | dismissed | `output.status != 0` is checked explicitly before postimage verification |
| `write_file` activity says "Edited" on a *spawn* failure | dismissed | `?` on `publish_workspace_write` returns before the activity call |
| `scope_identity` change breaks existing durable records | dismissed | Value is opaque, only length-bounded; existing records stay valid |
| `atomic_publish_frame` mismatch between host and helper | dismissed | Single shared module compiled into both binaries by construction |
| Missing `.staged` debris after successful commit | dismissed | Tests assert no debris; cleanup is narrow and ownership-checked |
| Windows approval gate removed | dismissed | Block is byte-identical to baseline |
| Directory fsync mutating workspace permissions | dismissed | `sync_workspace_directory` deliberately avoids `secure_fs`; test asserts permissions unchanged |
| MCP response JSON shape changed | dismissed | `render_output`/`text_result` unchanged |
| PRIMARY vs managed-worktree divergence | dismissed | Both go through `execution_root_for_goal` |

### Verification Commands

- `cargo build --all-targets` — Finished, no errors.
- `cargo test --all-targets` — **990 passed, 0 failed** (1148s).
- `cargo test atomic_publish_tests` — 5 passed; confirmed these genuinely execute (not
  `NIX_BUILD_TOP`-skipped) on this host, so sandbox containment of the helper is real.
- `cargo clippy --all-targets --all-features` — Finished clean (CI uses `-D warnings`).
- `python3 scripts/release/test_release_scripts.py` — 19/19 passed, including
  `test_every_platform_ships_the_writer_commit_helper`.
- `cargo check --target x86_64-pc-windows-msvc` on the full crate — **blocked**, unrelated
  C dependency `aws-lc-sys` needs `windows.h`, unavailable on a macOS host.
- Isolated `cargo check --target x86_64-pc-windows-msvc` over just `workspace_publish.rs` +
  `atomic_publish_frame.rs` in `/tmp/gl-win-check` — **Finished, 0 errors**. This is the
  compile-correctness evidence for `MoveFileExW`, `move_without_replace`, `open_staging`
  (Windows), `inherit_mode` (no-op), and `sync_workspace_directory` (no-op).
- Scratch crate experiment in `/tmp/gl-run-check` — confirmed `cargo run` does not rebuild a
  deleted sibling `[[bin]]`, so a dev checkout cannot self-heal a removed `atomic-publish`.

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `F1` | entry | [execution.rs](/Users/yuta/local-mcp-connector-parity/src/execution.rs#L60) | Helper resolution with a single production candidate |
| `F1` | output | [INSTALL.md](/Users/yuta/local-mcp-connector-parity/docs/release/INSTALL.md#L122) | Docs still say macOS needs no helper |
| `F1` | output | [README.md](/Users/yuta/local-mcp-connector-parity/README.md#L322) | Same claim repeated for macOS |
| `F2` | entry | [writer.rs](/Users/yuta/local-mcp-connector-parity/src/writer.rs#L686) | Commit-time refusal blocks the task |
| `F2` | behavior | [goal_api.rs](/Users/yuta/local-mcp-connector-parity/src/goal_api.rs#L1262) | Allowlist that omits the new blocker code |
| `F2` | output | [writer.rs](/Users/yuta/local-mcp-connector-parity/src/writer.rs#L1645) | `revalidate_before_commit`, the guard that produces the state |
| `F5` | output | [mcp.rs](/Users/yuta/local-mcp-connector-parity/src/mcp.rs#L633) | Activity title is built unconditionally |
| `F3` | entry | [mcp.rs](/Users/yuta/local-mcp-connector-parity/src/mcp.rs#L576) | Windows approval precedes the new refusals |

### Blind Spots

| Area | Risk introduced by the blind spot | What would resolve it |
| --- | --- | --- |
| Windows runtime (`MoveFileExW`, error-code mapping, approval ordering) | `F3` severity and any Windows-only regression are unverified at runtime | Run the suite and a manual `write_file` on `windows-2025` with the real archive layout |
| Linux sandbox path (Landlock/bubblewrap exec of a non-system helper) | Design §2.1 validated macOS only; a Linux-only exec denial would break every Writer commit there | Run `atomic_publish_tests` and the writer suite on the Linux CI runner |
| Full end-to-end `F2` stall through the public MCP surface | `F2` rests on static trace of three reconciliation allowlists plus one unit test, not an observed `goal_resume` | Add a test: force a commit-time refusal, then call `goal_resume` and assert the Goal is not permanently `BLOCKED` |
| Release archive actually containing the helper | `targets.py` and its tests say yes, but no archive was built and unpacked here | Build one release archive per platform and run `verify_package.py` |
| Windows approval gate interaction with the new helper dependency | Cannot confirm which failure the user sees first in practice | Manual Windows run with the helper deliberately absent |

### Report Self-Check

- yes Every touched user-visible or unknown-impact surface appears in `Coverage Ledger`.
- yes Every finding in an action section appears in `Complete Findings Index`.
- yes Every `Finding F#` ledger row has a matching card.
- yes Every `Not covered` row has a reason and next verification step.
- yes Every user-visible or unknown-impact surface has graph or direct path evidence, or is explicitly marked as not covered.
- yes Recommendation follows the mapping rules from the skill (two unresolved `Block` findings → `Block`).