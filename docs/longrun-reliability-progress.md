# GoalLatch Long-Run Reliability Campaign Progress

## Current phase
Phase 4 soak complete (5/10/30-min). Phase 5 audit complete. Phase 6 release
dry-run complete. Phase 7 final report pending.

## Branches (all CI green)
- main: 48acd8e5d8e89a26406686debf42118ba2986b71
- PR #12 hardening/execution-ownership-v1: d08ede4a4926bca75d813e41da3d114fef444048 (DRAFT, CI success 38931681909)
- PR #13 hardening/durable-lock-v1: e7e931917eb1f4a826a278d43d3c575b9ed3f5ee (DRAFT, CI success 38925308079)
- PR #14 hardening/runtime-responsiveness-v1: 629097c5427f0f2a71a7f099a19490bd0f5a420f (DRAFT, depends on #12, CI success 39052256039)
- PR #15 integration/longrun-audit-v1: 744630a71c979c2cc24dc41bb69bab04de81c7e1 (DRAFT, verification-only; includes PR #12+#13+#14+#16, CI success 39053003971 + 39053007921)
- PR #16 fix/windows-snapshot-job-timeout-v1: 7db93c35530749ad17eea32f6cbffb239a3ec102 (DRAFT, CI success 39052144878)

## Implemented
- PR #14: moved Goal store operations (goal_start/status/pause/resume/cancel and goal_result loading) onto spawn_blocking.
- PR #16: Windows job containment startup timeout 10s -> 30s (CI cold start robustness). Merged into PR #15.
- Soak harness scripts/soak/soak.py added and hardened.

## Verification
- PR #14: cargo check/clippy/fmt Green; mcp_contract 33 passed; goal_result 7 passed.
- Integration: cargo build Green; mcp_contract 33 passed; task_store 25 passed.
- PR #15 integration CI: all jobs green on 4113260; only `compat (windows-2022)` post-checkout step hung on the runner after all functional steps succeeded.
- Smoke soak 5 min: 23612 requests, 100% success, p50 3.0ms, p95 8.0ms, p99 11.0ms, max 451.8ms.
- Stress soak 5 min @ concurrency 8: 47016 requests, 100% success, p50 5.0ms, p95 12.0ms, p99 15.0ms, max 878.4ms.
- Soak 10 min @ concurrency 8: 93800 requests, 100% success, p50 5.0ms, p95 12.1ms, p99 16.0ms, max 1713.4ms.
- 30-min soak attempt (first): server exited code 1 after 1082s with stderr
  `response writer transport failure: client unreachable or stalled`, 161182 requests all
  successful, max latency 48831ms.
- 30-min soak (fixed harness, concurrency 8): 1800.0s full duration, returncode 0,
  no stderr, 254911 requests, 100% success, 0 error responses, 1.58GB responses read,
  server working set flat 11.6MB -> ~6-7MB, p50 5.0ms, p95 14.0ms, p99 2784ms,
  max 28413ms.
- Release dry-run on integrated SHA (4113260), run 39049854348: plan + 6 platform
  builds (linux x86_64/aarch64, windows x86_64/aarch64, macos x86_64/aarch64) +
  checksums all succeeded; `publish` skipped as designed for workflow_dispatch.

## Residual risk: soak tail latency
The 30-min soak's p99 (2.78s) and max (28.4s) are far above the 5-/10-min runs
(p99 15-16ms, max 0.9-1.7s). All requests still succeeded and the server never
exited, so this is a latency-tail phenomenon, not a transport failure.

Attribution (time-clustered slow-event sampling + controlled runs):
- Clean 180s run with no concurrent host activity: slow events (>100ms) = 0,
  p50 5.0ms, p95 12.5ms, p99 16.0ms, max 36ms, 28504 requests, 100% success.
- 600s probe run while the host was concurrently doing heavy git diff/build/gh
  work: throughput halved (~71 vs ~142 req/s), slow_event_count hit the 5000 cap,
  p95 7945ms, p99 25624ms, max 37325ms. Slow events arrived in bursts (responses
  queued during multi-second client starvation, then drained), the signature of a
  starved reader rather than server slowness.
- Server working set stayed flat in every run (7-20MB), never exited, and
  returned 0 error responses, so there is no server-side resource leak or
  correctness defect. The tail is host/client contention.

Conclusion: verified reliable on the core contract (no crash, no hang, no failed
or errored request). Tail latency must be re-measured on a release build and a
dedicated host; this 4-vCPU/8GB shared machine starves the soak client under load.

## Line-ending hygiene
`main` stores Rust sources with LF. Edits made through the Windows editor had
introduced CRLF into `src/goal_api.rs`, `src/task_store.rs`, and
`src/managed_worktree_snapshot_index_blob.rs`, which rewrote those files
wholesale in `git diff`. Normalized back to LF on
`integration/longrun-audit-v1`, `hardening/runtime-responsiveness-v1` (PR #14),
and `fix/windows-snapshot-job-timeout-v1` (PR #16). Diffs vs `main` are now the
real changes only (PR #14: 653 insertions; PR #16: 1 line).

## 30-min failure root cause (harness, not product)
The original harness retained every full response object (including multi-KB `tools/list`
payloads) in memory. Over ~161k requests at ~1MB/s of response bytes this accumulated to
gigabytes in the Python client, causing its reader thread to stall. The server then hit its
by-design `MCP_WRITE_TIMEOUT` (10s) writing to a client that stopped draining stdout and
terminated the transport explicitly (returncode 1). Fixed by retaining only per-request
latencies and adding child working-set sampling. A 120s validation run completed at 100%
success with the server working set flat (~12MB), confirming no server-side memory leak.

## Pending
- 2-hour extended soak (not run; host contention makes it low-signal here).
- PR #15 body update to note PR #16 inclusion (done via gh).
- Final Japanese report (Phase 7).

## Cautions
- Do not disturb live local-mcp processes started 2026-10-15 (production GoalLatch/Tunnel).
