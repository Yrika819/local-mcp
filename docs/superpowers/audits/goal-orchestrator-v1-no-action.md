# Goal Orchestrator V1 NO_ACTION Audit

`NO_ACTION` is a valid scheduler result only when the durable state cannot safely advance in the current call and the returned reason identifies the control state, typed blocker, dependency wait, bounded retry policy, or finalization handoff.

| Branch | Predicate | Valid fixture | Executable work allowed? | Recovery |
|---|---|---|---:|---|
| `Pausing` | Goal is `PAUSING` | pause during active work | no | `goal_resume` after pause completes |
| `Paused` | Goal is `PAUSED` | paused Goal | no | `goal_resume` |
| `Cancelling` | Goal is `CANCELLING` | cancellation awaiting reconciliation | no | cancellation/reconciliation |
| terminal | Goal is terminal | completed/failed/cancelled Goal | no | immutable |
| Goal blocked | Goal is `BLOCKED` | unresolved external or unknown side effect | no | typed blocker resolution, resume, or replan |
| finalization required | Goal is `VERIFYING` | Goal Verifier passed and Finalizer is next authority | no scheduler work | runner invokes Finalizer |
| replanning without trigger | Goal is `REPLANNING` with no `NEEDS_REPLAN` | reload between replan commit and recovery | no uncommitted work | recover to `RUNNING` or typed blocker |
| active work | task is `RUNNING` | external worker still active | no duplicate dispatch | worker/restart reconciliation |
| retry policy deferred | task is `RETRYABLE` but not yet made `READY` | verification failure before resume | no duplicate dispatch | resume/readiness policy |
| blocked tasks | task blockers or exhausted safe budget | blocked readonly/writer | no unsafe replay | typed recovery/replan |
| writer lease held | another mutating task owns lease | two READY writers | other writer only | current writer completes/reconciles |
| waiting for dependencies | no READY task and hard dependency incomplete | pending dependent DAG | no | dependency completion |
| readiness propagation deferred | pending task becomes eligible only after propagation | replan/reload | no silent spin | readiness propagation on next durable step |
| failed tasks | a task is failed and terminal policy applies | unrecoverable semantic failure | no | Goal failure/final result |
| no eligible action | no route exists | empty/non-runnable DAG | no | typed invariant failure or external resolution |

The liveness regression suite must additionally assert that any `RUNNING` Goal containing an executable `READY` task selects `RUN_READONLY`, `RUN_WRITER`, `VERIFY_TASK`, or `UNSUPPORTED_WORKER`, never a generic `NO_ACTION`.
