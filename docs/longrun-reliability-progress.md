# GoalLatch Long-Run Reliability Campaign Progress

## Current phase
Phase 4 smoke completed; Phase 5/6 pending.

## Branches
- main: 48acd8e5d8e89a26406686debf42118ba2986b71
- PR #12 hardening/execution-ownership-v1: d08ede4a4926bca75d813e41da3d114fef444048 (DRAFT)
- PR #13 hardening/durable-lock-v1: e7e931917eb1f4a826a278d43d3c575b9ed3f5ee (DRAFT)
- PR #14 hardening/runtime-responsiveness-v1: 80ff6b1... (DRAFT, depends on #12)
- PR #15 integration/longrun-audit-v1: 9182771... (DRAFT, verification-only)
- PR #16 fix/windows-snapshot-job-timeout-v1: new

## Implemented
- PR #14: moved Goal store operations (goal_start/status/pause/resume/cancel and goal_result loading) onto spawn_blocking.
- PR #16: Windows job containment startup timeout 10s -> 30s (CI cold start robustness).
- Soak harness scripts/soak/soak.py added.

## Verification
- PR #14: cargo check/clippy/fmt Green; mcp_contract 33 passed; goal_result 7 passed.
- Integration: cargo build Green; mcp_contract 33 passed; task_store 25 passed.
- Smoke soak 5 min: 23612 requests, 100% success, p50 3.0ms, p95 8.0ms, p99 11.0ms, max 451.8ms.

## Pending
- 30-min stress and 2-hour extended soak.
- Release dry-run on integrated SHA.
- Independent re-audit.
PY