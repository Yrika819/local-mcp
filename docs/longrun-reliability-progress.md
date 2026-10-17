# GoalLatch Long-Run Reliability Campaign Progress

## Current phase
Phase 4 soak in progress (30-min verified run). Phase 5/6/7 pending.

## Branches
- main: 48acd8e5d8e89a26406686debf42118ba2986b71
- PR #12 hardening/execution-ownership-v1: d08ede4a4926bca75d813e41da3d114fef444048 (DRAFT)
- PR #13 hardening/durable-lock-v1: e7e931917eb1f4a826a278d43d3c575b9ed3f5ee (DRAFT)
- PR #14 hardening/runtime-responsiveness-v1: 80ff6b1... (DRAFT, depends on #12)
- PR #15 integration/longrun-audit-v1: 4113260... (DRAFT, verification-only; includes PR #12+#13+#14+#16)
- PR #16 fix/windows-snapshot-job-timeout-v1: 8b07e1442284e848706d2d48d6da1c794762b456 (DRAFT)

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
exited, so this is a latency-tail phenomenon, not a transport failure. Candidate
causes are client-side (Python GIL / OS scheduling / 1.58GB of stdout parsing) or
host memory pressure rather than a server defect (server working set was flat).
Needs time-clustered sampling to attribute.

## 30-min failure root cause (harness, not product)
The original harness retained every full response object (including multi-KB `tools/list`
payloads) in memory. Over ~161k requests at ~1MB/s of response bytes this accumulated to
gigabytes in the Python client, causing its reader thread to stall. The server then hit its
by-design `MCP_WRITE_TIMEOUT` (10s) writing to a client that stopped draining stdout and
terminated the transport explicitly (returncode 1). Fixed by retaining only per-request
latencies and adding child working-set sampling. A 120s validation run completed at 100%
success with the server working set flat (~12MB), confirming no server-side memory leak.

## Pending
- 30-min verified soak with the fixed harness (running).
- 2-hour extended soak.
- Release dry-run on integrated SHA (4113260).
- Independent re-audit.
- PR #15 body update to note PR #16 inclusion.

## Cautions
- Do not disturb live local-mcp processes started 2026-10-15 (production GoalLatch/Tunnel).
