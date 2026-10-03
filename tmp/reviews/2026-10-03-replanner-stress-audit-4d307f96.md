# Replanner Hardening V1 — read-only adversarial stress audit

Branch: `hardening/replanner-compaction-v1` @ `1c23f22`
Base: `origin/main` = `d4ad4c26b605cb4a6eccde521909408f6e898994`
Nature: **read-only**. No production change resulted from this audit.

## Host: macOS (darwin), x86_64, Apple Git 2.50.1

## Baseline (pre-hardening, commit `e8232e6`) versus current

Serialized Replanner request bytes, deterministic test-owned synthetic Goals built
through the real `materialize_initial_plan_with_contract` and
`apply_task_replacements` transactions.

| shape | active | history | before | after | reduction |
| --- | --- | --- | --- | --- | --- |
| small | 6 | 0 | 6,998 | 6,461 | 8% |
| 30-40 Task PokedCPU-like | 38 | 0 | 29,127 | 12,228 | 58% |
| PokedCPU-like with history | 38 | 24 | 63,157 | 22,634 | 64% |
| history-heavy | 8 | 64 | 148,977 | 35,200 | 76% |
| deep dependency chain | 26 | 0 | 21,827 | 11,060 | 49% |
| broad fan-out | 26 | 0 | 26,589 | 15,778 | 41% |
| many unrelated completed | 48 | 0 | 57,817 | 33,552 | 42% |
| blocker + criterion weight | 8 | 0 | 16,076 | 17,675 | grew 10% |

The one regression is deliberate and recorded: padded completion-criterion prose is
semantically load-bearing and is never truncated, so a Goal weighted toward criteria
grows slightly. That shape is also the one the ceiling test uses deliberately,
because 64 criteria at the `goal_start` limit of 8192 characters is a Goal that
genuinely cannot fit any transport and must fail closed rather than be silently cut.

## Non-amplification sweep

Active graph held constant at 8 Tasks while superseded history grows.

| history | request bytes | delta |
| --- | --- | --- |
| 0 | 6,874 | - |
| 8 | 10,333 | +3,459 |
| 32 | 20,655 | +10,322 |
| 64 | 34,415 | +13,760 |
| 128 | 34,495 | +80 |

Flat past the bounded history window. The only residual growth is the
`history_omitted_count` integer. Before the change the request grew proportionally
to every superseded Task body and reached 148,977 bytes at 64 history entries.

## Per-Task payload invariant

| history | entries | widest single entry |
| --- | --- | --- |
| 0 | 8 | 1,419 |
| 8 | 8 | 1,419 |
| 32 | 8 | 1,419 |
| 64 | 8 | 1,419 |
| 128 | 8 | 1,419 |

Constant. History contributes nothing to the active task array: entry count tracks
the active graph, never the history depth, and no single entry grows.

## Durable-history ceilings, each proven to fire

| dimension | fixture measured | ceiling | active graph inside every active ceiling |
| --- | --- | --- | --- |
| superseded scope paths | 4,056 | 4,096 | yes |
| superseded verification entries | 3,900 | 4,096 | yes |
| superseded Tasks | 512 | 1,024 | charged set empties the active graph |
| superseded dependency edges | 4,105 active | 4,096 | charged set empties the active graph |

## What this audit did NOT establish

- No live model backend was invoked. Every claim about what the model *can* justify
  from its compacted context is prompt-contract inference, not runtime evidence.
- No PokéCPU durable Goal was resumed, run, cancelled, or mutated. Every reproduction
  used a test-owned synthetic durable store under a temporary directory.
- No conclusion is claimed about `goal_status` pagination. Its size was not measured
  here; that remains a separate branch.
- The suite runtime grew from roughly 400s to roughly 720s locally, almost entirely
  from building thousand-entry durable histories. `Goal::validate` re-scans the Task
  map once per durable replacement record, so record count is quadratically
  expensive to build. That is a pre-existing production cost, not introduced here,
  and it is recorded as a follow-up rather than fixed in this branch.

## Raw measurement output

```
MEASURE label=small active=6 history=0 request_bytes=6461 durable_goal_bytes=7701 ratio=0.839
MEASURE label=pokecpu-36 active=38 history=0 request_bytes=12228 durable_goal_bytes=29405 ratio=0.416
MEASURE label=pokecpu-36-with-history active=38 history=24 request_bytes=22634 durable_goal_bytes=109189 ratio=0.207
AMPLIFY history=0 request_bytes=6874 active=8
AMPLIFY history=8 request_bytes=10333 active=8
MEASURE label=history-heavy active=8 history=64 request_bytes=35200 durable_goal_bytes=273009 ratio=0.129
MEASURE label=deep-chain active=26 history=0 request_bytes=11060 durable_goal_bytes=23029 ratio=0.480
AMPLIFY history=32 request_bytes=20655 active=8
MEASURE label=wide-fan-out active=26 history=0 request_bytes=15778 durable_goal_bytes=32149 ratio=0.491
MEASURE label=many-unrelated-completed active=48 history=0 request_bytes=33552 durable_goal_bytes=73984 ratio=0.454
MEASURE label=blocker-and-criterion-weight active=8 history=0 request_bytes=17675 durable_goal_bytes=20738 ratio=0.852
AMPLIFY history=64 request_bytes=34415 active=8
AMPLIFY history=128 request_bytes=34497 active=8
TASKPAYLOAD history=0 entries=8 widest=1419
TASKPAYLOAD history=8 entries=8 widest=1419
TASKPAYLOAD history=32 entries=8 widest=1419
HISTORYCEILING accumulated dimension=superseded scope paths transactions=4 measured=4056 ceiling=4096
TASKPAYLOAD history=64 entries=8 widest=1419
TASKPAYLOAD history=128 entries=8 widest=1419
HISTORYCEILING accumulated dimension=superseded verification entries transactions=4 measured=3900 ceiling=4096
HISTORYCEILING accumulated dimension=superseded Tasks transactions=1 measured=512 ceiling=1024
HISTORYCEILING accumulated dimension=superseded dependency edges active_edges=4105 ceiling=4096
```
