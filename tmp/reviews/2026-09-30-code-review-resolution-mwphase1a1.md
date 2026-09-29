# Receiving Code Review Resolution

## Resolution Contract

- Report type: `receiving-code-review`
- Resolution ID: `rr-20260930-mwphase1a1`
- Review chain ID: `rc-20260930-mwphase1a1`
- Source report ID: `cr-20260930-mwphase1a1`
- Source report path: `tmp/reviews/2026-09-30-code-review-report-mwphase1a1.md`
- Resolved at: `2026-09-30T00:00:00Z`
- Git mutation during resolution: `None`
- Baseline: `main` at `ec3477495eb62cda4d5cfb3e8273b5809b416e4a`
- Target: working tree (uncommitted)

## Summary

Every finding and test gap raised by `cr-20260930-mwphase1a1` was independently verified in the coordinator, found to be real and in scope, and fixed. No finding was dismissed, and no finding was deferred. Two defects were genuinely new: the missing primary-workspace overlap invariant (`F1`) and the schema-2 migration bypass (`F2`). One real validation gap was found and fixed during resolution of the same class: `validate_canonical_absolute` compared paths with `Path`'s `PartialEq`, which compares components rather than bytes, so a non-canonical path containing a current-directory segment was accepted. That was fixed before the review passes ran and is recorded here for continuity.

The single open question (`F9`, corruption error taxonomy) has no authoritative basis in the frozen design, so it is carried forward to the handoff rather than resolved by invention.

## Disposition Ledger

| ID | Fingerprint | Severity | Disposition | Change applied | Verification |
| --- | --- | --- | --- | --- | --- |
| `F1` | `ifp-sha256:54feacc75e8c21e6a7bf95883207df0daa6c83d4abf4c36b65f2863b83dae653` | `Major` | Fixed | Added a pure overlap invariant rejecting a `worktree_root` that is inside the primary workspace or contains it, in `ManagedWorktreeRecord::validate`, with a comment tying it to design sections 2.7, 2.8, and 13. | New test `record_rejects_worktree_root_overlapping_the_primary_workspace` asserts rejection for the primary Git directory, a Git worktree admin path, a primary subdirectory, a deep nested path, the primary parent, and the filesystem root, and asserts the rejection reason. A companion test proves a legitimately placed sibling root still validates. Full suite green. |
| `F2` | `ifp-sha256:f1a24beff3b701376e18997475993aff7fd3eaf22a5b4a8ee97f87b94526be31` | `Major` | Fixed | The schema-2 branch now calls `decode_pre_managed_schema` instead of the finalizer, so the workspace mode is established explicitly by the same code as schema 3. The incorrect fall-through comment was replaced with one describing the real control flow. | New test `schema_two_migrates_to_primary_through_the_shared_step` asserts a stored schema-2 Goal loads as primary at the current schema with the expected execution root. The existing schema-2 non-rewrite test still passes. |
| `F3` | `ifp-sha256:0ca0523f07f5a476b3ad01f176dff246a652da70c89ca75b005f2faaad260579` | `Minor` | Fixed | Rewrote the module documentation to state that no public MCP request type exposes a managed field, that `branch_ref` and `lock_reason` are mechanically unforgeable, and that the remaining identity values are host-supplied and shape-validated, with observation genuineness deferred to the reconciliation phase. | Documentation-only change; `cargo fmt --check` and clippy clean. |
| `F4` | `ifp-sha256:06ea06a1474ea7b1b7886b7c236809a12d3db3d2c57d6b02c8194a38c3e10c66` | `Minor` | Fixed | `validate_informational_ref` now rejects a reflog selector and any ref component ending in a dot, matching the Git ref rules the same function already enforced for the lock suffix and leading dot. | Extended the existing hostile-input table in `record_rejects_unsafe_source_refs`; the full managed-worktree test filter passes. |
| `F5` | `ifp-sha256:f3065ef351e3fac5a796d4483eac3cbe7216f6c4ccce31a5ca2e861c507d04bb` | `Minor` | Fixed | The lifecycle documentation now states that Phase 1 freezes the edge set and that the transition site consulting it belongs to Phase 3 creation authority, which is not authorized here. | Documentation-only change. The existing exhaustive edge-table test still passes unchanged. |
| `F6` | `ifp-sha256:47785cc152d38cea14d126e6bbb78620ec893ee49744a5d075cbad598d3a7fa9` | `Minor` | Fixed | Replaced the constant-folded runtime guard, which could never fire and would have misreported a code-side skew as durable-state corruption, with a module-level compile-time assertion that the current schema equals the version the migration chain targets. | `cargo build` and `cargo clippy --locked --all-targets --all-features -- -D warnings` both clean, confirming the assertion holds and produces no dead-code warning. |
| `F7` | `ifp-sha256:b7daf87191a1908c5d7c8c4e3ba33c4306849e211d40597c3cfe65e4768287dc` | `Minor` | Fixed | Removed the stale schema reference from the replan authority-violation message, matching the generalization this change already applied to the analogous messages in `src/goal.rs`. | Full suite green; no test asserts the string, confirmed by search. |
| `F8` | `ifp-sha256:c6938ee6a632cc1010bcc50440fa14c0a379f71593bd6f5aa320d52020b4d487` | `Minor` | Fixed | Documented on `Goal::execution_root` that it is a pure accessor that does not re-validate and must only be reached from an already-validated Goal, noting that every constructor and durable load path validates first. | Documentation-only change. |
| `F9` | `ifp-sha256:2fab996b217c0949a1885d3ff7ee4e2c84ae28a9cd533f0111a28b97f842ce07` | `Question` | Carried forward | No change. The split between durable-state corruption and unsafe-identifier errors matches the pre-existing `Goal::validate()` convention, and the frozen design specifies no corruption error taxonomy. | Left as an open question for the product owner; it does not block the Phase 1 commit. |
| `T1` | `ifp-sha256:9e340d9b83cc8ac701c1dab417a778cc77ddd804228d3194b52816a34c9a755d` | `Minor` | Fixed | Replaced the vacuous determinism loop with a falsifiable assertion that validation leaves every field of the record byte-identical, so validation that normalizes or defaults fields now fails the test. | Test `validation_does_not_mutate_the_record_it_checks` passes; confirmed falsifiable because it compares serialized state before and after. |
| `T2` | `ifp-sha256:c35b23f12e2a4553eba8428015c93677325449a7cec4dc9a56a1745c6c1fb84f` | `Minor` | Fixed | Rebuilt the fixture from a single Goal so the record identity is consistent, and tightened the assertion to require the specific primary-workspace rejection reason rather than any corruption error. | Test `primary_goal_rejects_managed_state` now asserts the exact rejection reason, so it can no longer pass for an unrelated ownership-mismatch reason. |

## Challenges to Source Review

`None. Every candidate proposed by the two specialists was independently re-verified in the coordinator before disposition. The two downgrades (R1-C1 from Major to Minor, and R1-C6 to a question) and the two reclassifications (R2-C4 and R2-C5 to test gaps) are recorded in the source report's adjudication table with the evidence for each.`

## Additional Defect Found During Resolution

`validate_canonical_absolute` originally compared the rebuilt path with the input using `Path`'s `PartialEq`, which compares components rather than raw bytes. Under that rule a non-canonical path containing a current-directory segment compares equal to its canonical form, so the canonicality check silently accepted it. The comparison now uses `as_os_str()`. This was found by a failing test, fixed at the root cause, and covered by a regression case asserting rejection of a non-canonical path containing a current-directory segment.`

## Post-Fix Verification

- `cargo fmt --check` -> clean
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> clean, no warnings
- `cargo test --locked --all-targets` -> 723 passed, 0 failed
- `cargo test --locked --all-targets managed_worktree` -> 36 passed, 0 failed
- `git diff --check` -> clean, exit 0

## Residual Authority Statement

After these fixes the Phase 1 change still adds no Managed Worktrees mutation authority. There is no worktree creation, branch creation, lock or unlock, removal, prune, or ref mutation; no Session permission-root change; no Planner execution against a managed root; and no `PREPARED` to `ACTIVE` transition through Git. Validation remains a pure function of durable data with no filesystem or Git access. The single-writer lease, the read-only model worker posture, and the `PRIMARY` execution path are unchanged.
