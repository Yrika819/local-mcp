# Platform Runtime Closure V1 — Bounded Stress Audit

- Date: 2026-10-07
- Branch: `hardening/platform-runtime-closure-v1`
- Code HEAD when run: `155c544c9e046f439b7798c3c244c1616a95f404`
- CI run establishing the cross-platform baseline: `37526497166`, completed success 11/11 at the same HEAD.
- Audit type: bounded, test-owned real-process stress; no production code or authority changes.

## Runs and outcomes

1. `process_group_stress_tests::many_short_lived_groups_in_parallel_leave_no_residue` — 5 consecutive passes. Each pass drives 16 rounds × 8 parallel owned groups: **640 total rapid process-group launches/terminations**. Every pass also verifies its unrelated group remains intact and no owned group/witness residue remains. Durations: 16.78s, 16.33s, 15.70s, 14.59s, 14.70s.
2. `platform_runtime_tests::unix::a_bounded_blocking_command_kills_a_descendant_that_outlives_its_leader` — 5/5 passes (1.05–1.16s each). Leader exits before the descendant; FIFO EOF is the descendant witness.
3. `platform_runtime_tests::unix::a_bounded_blocking_command_terminates_on_output_overflow` — 5/5 passes (1.20–1.52s each). The output cap terminates the owned group and returns incomplete/overflow evidence.
4. Earlier reliability gate for `a_timed_out_blocking_command_leaves_no_descendant_running` — 20/20 focused passes after the witness fixture was changed so only the intended child opens the FIFO writer; four concurrent copies plus process-runtime, ownership, and stress sibling suites also passed.
5. Full local default-parallel suite — two consecutive passes, 1125/1125 main test-target tests each.
6. Cross-platform CI — Windows owner-death/descendant runtime tests, Linux Bubblewrap/helper, macOS runtime, and all compatibility jobs passed in `37526497166`.

## Findings

- No orphan survivor, stale PGID signal, wrong-process termination, bounded-cleanup hang, or output-overflow misclassification was observed in these runs.
- The Windows CI witness used real nested helper processes and passed in the complete Windows x64 test target; CI provides the runtime proof for this platform, not this macOS stress host.
- No source changes resulted from this audit.

## Coverage limits

This bounded run did not add a real-descendant process specifically to the batch `release_all_jobs` cancellation test, nor did it stress managed-worktree mutation cancellation. Those remain the review’s test gaps T2 and T1 respectively; they are not inferred closed by the process-group repetitions. Unix descendants that deliberately escape their process group remain outside the documented containment guarantee.

## Protected audit handling

The pre-existing Resource Bounds stress audit `tmp/reviews/2026-10-05-resource-bounds-v1-stress-audit.md` was not opened, modified, staged, or copied. Its SHA256 remains `fcbdd96c7ef94ed425b7ed7e54d900563a9b71af53481e46af37eeedcd61acc0`.
