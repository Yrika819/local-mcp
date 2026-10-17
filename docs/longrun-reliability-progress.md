# GoalLatch Long-Run Reliability Campaign Progress

## Current phase
Phase B/C runtime responsiveness fix completed; Phase D harness prototyped; integration pending.

## Branches
- main: 48acd8e5d8e89a26406686debf42118ba2986b71
- PR #12 hardening/execution-ownership-v1: d08ede4a4926bca75d813e41da3d114fef444048 (DRAFT)
- PR #13 hardening/durable-lock-v1: e7e931917eb1f4a826a278d43d3c575b9ed3f5ee (DRAFT)
- PR #14 hardening/runtime-responsiveness-v1: cb1c0f93aa308cc8a569e7364cd28386284d49b4 (DRAFT, depends on #12)

## Implemented
- PR #14: moved synchronous Goal store operations (goal_start/status/pause/resume/cancel) onto tokio::task::spawn_blocking, including OS lock acquisition, so async workers are not occupied during lock waits.
- Verified cargo check, mcp_contract tests (33 passed), clippy, fmt on PR #14.

## PR #12 adversarial audit result
No production correctness defect found in typed ownership, PendingJobRegistration, DispatchOutcome, reclaim flow, session scoping, cancellation, or shutdown. Existing regressions cover lost poll response, cancelled registration, refusal, cross-session, writer rejection.

## PR #13 runtime starvation
Reproduced risk: with_session_lock used std::thread::sleep inside async dispatch. Mitigated by PR #14 spawn_blocking. Lock contention delay now bounded by PR #13 try_lock and isolated to blocking pool.

## Soak smoke
- 5 sequential ping requests through isolated MCP process: p50 ~0ms, max 34.5ms, 5/5 success.

## Pending
- Integration branch combining #12 + #13 + #14.
- Long-duration soak (30m/2h) with metrics.
- Release dry-run on integrated SHA.
PY