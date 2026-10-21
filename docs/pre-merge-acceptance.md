# GoalLatch Pre-Merge Final Acceptance

## Evidence Integrity Audit (soak-2h.json)
- Original file: `C:\Users\Yuta\AppData\Local\Temp\opencode\soak-2h.json`; 188,306 bytes; SHA256 `16a0b9ed106769f2166f1ab33aefafd5fb059d7ba95ad14742e56821f85c5597`; observed mtime `2026-10-18T10:32:55.106731+00:00`. Original preserved and read directly.
- The JSON itself contains no binary SHA, Git SHA, wall-clock start time, send-attempt count, send-success count, or shutdown-recovery count. The prior campaign note records binary SHA `5c37e4bc884aa671d98ac62c229c25a8de21c67a682ab2ee10a918654d7adff9` and source SHA `5ec4294e1608c2dedb1513173123b80bdf3d49f2`; the binary hash was independently recomputed from `gl-integration/target/release/local-mcp.exe` and matched. That does not add missing request-level logs.
- Recorded duration: requested 7,200s; elapsed 7,200.0s; `completed_full_duration=true`; child return code 0; early error null; stderr tail empty. This supports a continuous 2-hour process run with clean process exit.
- `requests=1,029,243` means parseable JSON lines whose IDs matched the pending map when the legacy reader processed them; it is not the number of raw responses, requests attempted, or successfully written. The old reader did not validate the full JSON-RPC response shape. `success=1,029,243` and `error_responses=0` therefore describe those matched records only (with no `error` member), not proven valid normal responses. Their ratio is 100% of the old harness's matched records; the rate over successfully-sent requests is unknown because its denominator was not recorded.
- `inflight_at_exit=21` is an outstanding pending-map count after process wait and a fixed 0.5s sleep. The old reader thread was not joined, and EOF observation was not synchronized with the count. The 21 may include write failures because the old harness inserted IDs into `pending` before attempting the write; they may also be unanswered requests, responses still queued or being processed, or reader timing effects. With no send/receive log, joined reader, or per-ID audit trail they cannot be further classified. They are not evidence of shutdown recovery; the old JSON had no such counter. Full-request completion is unproven.
- `response_bytes_read=6,392,186,024`; p50/p95/p99/max = `2.03/16.20/31.11/57746.62 ms`; server working set start/end/max = `8416/3600/9176 KB`; all 24 (elapsed seconds, KB) samples: `[[0.0,8416],[300.1,4268],[602.6,4044],[902.6,4280],[1202.7,5072],[1502.7,4320],[1802.8,4044],[2102.8,4380],[2403.0,4380],[2703.1,4076],[3003.2,4228],[3303.2,3840],[3603.2,4104],[3903.3,4168],[4203.4,3152],[4503.4,4112],[4803.5,4280],[5103.7,3784],[5403.8,3180],[5703.9,4144],[6004.0,4104],[6304.0,4256],[6609.2,4912],[6909.3,4256]]`; 4,194 slow events. These describe old harness observations, not client RSS (`mem_samples` are server working-set measurements). The recorded empty stderr tail is what the harness captured; its stderr reader was not joined at shutdown. Slow-event clustering does not by itself prove host contention attribution.
- Old harness command: `python scripts/soak/soak.py --binary target/release/local-mcp.exe --duration 7200 --concurrency 8 --state-dir <isolated> --out <path>`.

## Corrected 60-second harness run (2026-10-21)
- Improved harness: `scripts/soak/soak.py`; focused regression tests: `python -B -m unittest discover -s scripts/soak -p "test_*.py" -v` (17 passed, including subprocess fixtures and protection against overwriting existing evidence/state).
- Binary: `gl-integration/target/release/local-mcp.exe`, SHA256 `5c37e4bc884aa671d98ac62c229c25a8de21c67a682ab2ee10a918654d7adff9`. Start `2026-10-21T12:07:58.548710+00:00`, end `2026-10-21T12:08:58.569393+00:00`; requested 60.0s, elapsed 60.031s, concurrency/inflight limit 8.
- Attempted 3,485; write+flush succeeded 3,485; normal responses 3,485; JSON-RPC errors, invalid JSON, invalid responses, duplicates, unknown IDs, timeouts, send failures and final unanswered = 0. Seven in flight at shutdown start; all 7 recovered during shutdown; final inflight 0; child exit 0; shutdown timeout false; stdout EOF observed; stderr empty.
- Success rate is `normal_responses / requests_send_succeeded = 3485/3485 = 100%`; p50/p95/p99/max = 125/250/593/1375 ms; 21,579,999 response bytes; recorded server working set sample 7,628 KB. Client ID-state tracking used 10,455 bytes (latency reservoir also capped at 100,000 samples). The exact summary is in worktree-local ignored file `.local/soak-final-20261021.json`; the run is a harness correctness smoke, not a substitute for the old 2-hour observation.

## Quality Gates
- Historical release-candidate HEAD 5ec4294: `cargo fmt`, check, clippy and the recorded 1,301-test run passed; CI at that integration candidate was green.
- Current source-tree checks: `cargo fmt --all -- --check` PASS; `cargo check --locked --all-targets` PASS (6m57s on retry after an initial 15m dependency-build timeout); `cargo clippy --locked --all-targets --all-features -- -D warnings` PASS (1m54s).
- `cargo test --locked --all-targets` was attempted twice locally and hit the 15m tool limit (first during test-target dependency compilation; the second reached tests but did not finish before timeout; no test assertion failure was printed). Do not report this as a local pass. The same product-source tree at PR #15 HEAD 33ce337 had all 11 CI jobs successful; final PR #15 CI will be required after the campaign commit.
- Soak harness regression suite: 17/17 passed locally; it will run in CI quality job.
- Squash-simulation `cargo check --locked --all-targets`: PASS (1m43s; shared build cache, source tree equivalent to normal merge simulation).

## Windows Test Audit (PR #16)
- Test: windows_timeout_terminates_job_contained_child_tree
- Timeout: 30s (was 10s) for PowerShell child PID witness publication
- Root cause: NOT fully determined. Hypothesis: CI load causes PowerShell startup delay (3-5s to publish PID)
- Guarantee: Job Object severs child tree on session drop; tasklist polling confirms descendant termination within 5s
- Residual: startup delay root cause undetermined; Windows-only test; CI passes consistently

## PR Status
| PR | HEAD | Base | Current CI |
|---|---|---|---|
| #12 | d08ede4a4926bca75d813e41da3d114fef444048 | main @ 48acd8e | Green on unchanged HEAD; current PR checks all SUCCESS |
| #13 | e7e931917eb1f4a826a278d43d3c575b9ed3f5ee | main @ 48acd8e | Green on unchanged HEAD; current PR checks all SUCCESS |
| #14 | ee06e29a7ddf51c7d6dc501fff11503cf68bffb4 | PR #12 @ d08ede4 | Green on this HEAD; run 39569615363 includes successful macOS Intel rerun |
| #16 | 7d46f66dfcbd058f388d835d193c7fd27017f2cc | main @ 48acd8e | Green; run 39596831970, 11/11 jobs including macOS Intel rerun |
| #15 | cd1feacee5962e74b5644abbbab045a76ba92d26 | main @ 48acd8e | Green; PR run 39601789967, 11/11 jobs (verification-only; never merge) |

## PR Dependency and Merge Simulations
- #12 alone changes execution/job ownership and MCP ownership tests. #13 alone changes bounded durable session-lock waits in `src/task_store.rs`; it is not in #14's ancestry. #16 alone changes the Windows PID-witness wait from 10s to 30s; Job Object termination assertions are unchanged.
- #14's review diff is now correctly based on PR #12 and contains only its Goal-store `spawn_blocking` work, result loading and progress note. Its head descends from #12, not #13; its runtime intent also relies on #13's bounded lock waits.
- Squash simulation: squash-equivalent #12, #13 and #16 applied cleanly. Applying only PR #14's unique delta (`d08ede4..ee06e29`) with three-way apply was clean; final source tree matches the existing integration candidate except one blank line in `src/task_store.rs`. All patch helpers are absent and the `spawn_blocking` Goal path remains. A naive merge of the *entire cumulative* PR #14 branch instead conflicted in `src/mcp.rs`; do not apply PR #14's cumulative #12 history as another patch.
- Normal-merge simulation order #12 -> #13 -> #16 -> #14 completed without conflicts. Final source tree matches the integration candidate except the same blank line; PR #14's `spawn_blocking` calls and the Windows 30s witness wait remain.
- Recommended actual method: normal merge commits, preserving #12 ancestry on #14. Keep #14 based on #12 for review; after #12 merges, retarget #14 to main and merge normally. #15 remains verification-only and must not be merged.
- Main is unchanged. Simulations are isolated branches/worktrees only.

## Final Candidate CI (2026-10-21)
- PR #15 HEAD `cd1feacee5962e74b5644abbbab045a76ba92d26`: https://github.com/Yrika819/local-mcp/actions/runs/39601789967 — success, all 11 jobs, including quality (fmt/check/clippy/harness regression), Linux test, Windows test/compat, and macOS test/compat.
- PR #16 HEAD `7d46f66dfcbd058f388d835d193c7fd27017f2cc`: https://github.com/Yrika819/local-mcp/actions/runs/39596831970 — success, all 11 jobs; the cancelled duplicate job was rerun individually and succeeded.
- A duplicate push-triggered integration run `39601780835` was cancelled to conserve Actions capacity; the PR-triggered run above completed fully and successfully. The cancellation is not test evidence and is not used as a pass.
- PR #12/#13/#14 current check surfaces were queried after finalization and had no non-success checks; their heads did not change.

## Release Dry-Run
- Previous: run 39049854348 on c2966066 (success, 6 platforms + checksums)
- Run 39446430810 on 5ec4294: success; plan + six platform builds + checksums succeeded; publish skipped.
- No new dry-run requested for this campaign update: changed files are soak tooling/tests, verification docs, a CI test step, and an explanatory source comment. No runtime code behavior, build settings, or package layout changed; the release binary source tree remains the 5ec4294 product tree.

## Pre-Merge Checklist
- [x] Historical quality gates pass on HEAD 5ec4294; current-candidate full local `cargo test` remains timed out and must be covered by final CI.
- [x] All current source PR CIs green; integration candidate #15 HEAD cd1feace CI run 39601789967 is 11/11 successful. PR #16 HEAD 7d46f66 CI run 39596831970 is 11/11 successful.
- [ ] 2h process run completed, but all-successful-send completion is unproven: 21 pending; legacy send counters absent.
- [x] Release dry-run on c2966066 (success)
- [x] Release dry-run on 5ec4294 (run 39446430810, success, 6 platforms + checksums, publish skipped)
- [x] Independent code audit (Phase 5)
- [x] Windows test audit documented
- [x] No demonstrated P0/P1 defect after independent audit; shutdown semantics documented above.
- [x] Rollback plan: preserve main history and revert the exact integration commits in reverse applied order (#14, #16, #13, #12). For merge commits use `git revert -m 1 <merge-sha>`; for squash commits revert the single squash commit. Stop further merges, rerun CI, and reconcile any durable Goal/Task/session side effects separately—code revert does not roll back persisted user data. Never reset or force-push main.

## Permitted Performance Risks
- Historical 2h tail latency max was 57.7s; attribution to host contention is plausible from prior controlled runs but not proven by the preserved 2h JSON alone.
- 2h Soak is single run (no replication)
- Windows startup delay root cause undetermined
- Shared host (4vCPU/8GB) differs from production

## Independent Audit and Accepted Shutdown Semantics
- Independent review found no demonstrated P0/P1 defect. A proposed EOF cancellation issue was checked against the design: `goal_run` is durable foreground work, and `goal_cancel` is the explicit persisted cancellation path. Transport EOF releases legacy retained jobs; it is not specified to cancel a foreground Goal.
- Corrected the stale `src/mcp.rs` shutdown comment that incorrectly described long Goals as background jobs. Residual: a running `spawn_blocking` foreground Goal may delay Tokio process exit while it finishes; it is bounded by the Goal run policy but EOF itself does not cancel it. No test/prod behavior was changed for this accepted contract.
- Job ownership, Session isolation, response-loss cleanup, try-lock/CAS persistence, MCP admission/backpressure and shutdown, and Windows Job Object coverage were reviewed against source/tests and the existing cross-platform CI. Windows remains experimental per `SECURITY.md`; no parity claim is made.
- Independent review did not locally execute the Windows-only descendant process test or reproduce live model-backed Goal shutdown; CI is the cross-platform evidence.

## Unverified Items
- [x] Release dry-run on 5ec4294 (run 39446430810 succeeded; six builds/checksums; publish skipped).
- 2nd 2h Soak replication (not required per user)
- Dedicated host tail latency measurement (not required per user)