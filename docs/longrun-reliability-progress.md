# GoalLatch Long-Run Reliability Campaign Progress

Snapshot: 2026-10-21, after PR #16 merged and before PR #14 merge.

## Repository and PR state
- `main`: `500d353e4ceb5729b83ca78d18cb4ddbe1200841` (PR #12, #13, and #16 merged).
- PR #12 `d08ede4a4926bca75d813e41da3d114fef444048` merged as `ea3334efbdc194b757e564188deb79ac23e5c7e1`.
- PR #13 `e7e931917eb1f4a826a278d43d3c575b9ed3f5ee` merged as `ce3e0063b7808bbbbdd48f83617c3706b1c76c51`.
- PR #16 `7d46f66dfcbd058f388d835d193c7fd27017f2cc` merged as `500d353e4ceb5729b83ca78d18cb4ddbe1200841`.
- PR #14 `ee06e29a7ddf51c7d6dc501fff11503cf68bffb4` is still Draft; its base is now `main`. Its review diff consists of the Goal store `spawn_blocking` changes, this progress snapshot, and the `TaskStore: Clone` needed to move the store into the blocking closure.
- PR #15 remains verification-only and must not be merged.

## Implementation and verification
- PR #12 establishes typed ownership transfer for background jobs, session-scoped registry operations, response-loss cleanup, writer-failure ownership, and shutdown cleanup.
- PR #13 bounds OS-backed per-session Goal lock waits with `try_lock`, a 5-second deadline, 10ms retry interval, revision checks, and atomic persistence/recovery.
- PR #14 moves synchronous Goal store operations and foreground `goal_run` work off async workers with `tokio::task::spawn_blocking`. Goal Result loading is also moved off async workers before workspace observation. This changes scheduling, not Goal authority or cancellation semantics.
- Focused prior checks: MCP contract 33 passed, Job Registry 18 passed, Goal persistence 25 passed, Windows descendant-process test 1 passed. The full local all-targets test exceeded the local tool time limit; cross-platform CI is the authoritative complete matrix.
- Main CI after PR #12: run `39617516389`, success 11/11 on `ea3334efbdc194b757e564188deb79ac23e5c7e1`.
- Main CI after PR #13: run `39624348190`, success 11/11 on `ce3e0063b7808bbbbdd48f83617c3706b1c76c51`.
- Main CI after PR #16: run `39626713836`, success 11/11 on `500d353e4ceb5729b83ca78d18cb4ddbe1200841`.
- Integration candidate PR #15: run `39613695523`, success 11/11 on `59f6c6d5d8ef1ce8104c6b1f777ae75344c80a60`.

## Soak evidence
- The preserved 7,200-second run recorded 1,029,243 matched parsed responses, zero recorded JSON-RPC error members, child exit 0, and 21 IDs still inflight at shutdown. The old harness did not record send attempts/successes or a joined EOF drain; full-send success and classification of those 21 IDs remain unproven.
- The improved harness regression suite passed 17/17. Its separate 60-second run recorded 3,485 attempts, 3,485 successful writes, 3,485 normal responses, seven shutdown responses recovered, and zero final unanswered requests. This short run is not a replacement for the historical two-hour observation.

## Release evidence and remaining work
- Release dry-run `39446430810` succeeded on prior product SHA `5ec4294e1608c2dedb1513173123b80bdf3d49f2`: six platform builds and checksums passed; publish was skipped. This is not a dry-run of the post-merge main.
- Remaining at this snapshot: complete PR #14's latest-base review/CI, merge it by normal merge commit if safe, then validate the resulting main and run a dry-run against that exact main SHA.
- No P0/P1 defect was demonstrated in the independent audit. Windows remains Experimental; no security parity claim is made.
