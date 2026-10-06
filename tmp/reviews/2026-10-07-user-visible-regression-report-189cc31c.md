# User-Visible Regression Review

## Scope

- Scope mode: working tree against `HEAD` `23fa4af0a3acb49c6878773bd4c84be2cb490569`, including affected user journeys introduced by the current branch.
- Branch: `hardening/platform-runtime-closure-v1`; comparison baseline: `origin/main` `f4bfb9e0c339d0338825a5bdb0ed7a6d994322b1` for full feature context.
- Changed paths: 15 source/docs files in current working tree; review includes callers and unchanged downstream surfaces listed in the ledger.
- Requirements: Platform Runtime Closure V1 task, `docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md`, Resource Bounds and Managed Worktrees designs, `SECURITY.md`.
- Excluded: protected untracked Resource Bounds audit; no content was read into the review or modified.

## Gate Snapshot

- Recommendation: **Discuss**
- Completion: Complete within the scoped code paths; platform runtime evidence incomplete.
- Why now: local macOS and parallel-suite coverage is strong, but native Linux Bubblewrap and Windows Job behavior must be established by the required branch CI before approval.
- Must-review now: Linux sandbox/probe runtime; Windows Job Object/abnormal owner death; full 11-job matrix.
- Findings count: 0 regressions; 0 open behavior findings.
- Intentional visible changes: configured Git filter drivers prevent managed-worktree creation; timed-out Bubblewrap version probes now fail closed as unavailable; dropped foreground execution futures cancel their task.
- Coverage confidence: high for macOS runtime and static path tracing; low for unrun Linux/Windows runtime behavior.
- Biggest blind spot: Windows x64/arm64 Job Object behavior and Linux Bubblewrap helper integration.

## Complete Findings Index

No unintended user-visible regressions were identified in the reviewed scope. Three deliberate behavior changes are recorded under `Intentional Changes`.

## Block

None.

## Discuss

No confirmed user-visible regression. Native Linux and Windows behavior remains a release/branch-approval question until CI completes; see the `Not covered` ledger rows.

## Watch

- **Windows approval reply can still wait indefinitely.** This behavior is unchanged and remains a separate product-consent/availability decision; no arbitrary timeout was introduced.
- **Same-user Git-config TOCTOU remains possible.** Existing security documentation acknowledges no shared atomic snapshot across same-user Git-config/filesystem/executable checks. The filter guard refuses stable configured filters but does not claim serialization.
- **Linux/macOS Unix process groups do not contain descendants that deliberately escape to a new session/group.** The design states this limitation; the caller remains bounded, but the escapee may continue.
- **Power loss, machine crash, and whole-tree SIGKILL on Unix are not cleanable guarantees.** Design explicitly disclaims them.

## Intentional Changes

- **I1 — Managed Git filters:** Managed-worktree creation now refuses repositories with configured local/worktree filter drivers, rather than invoking host checkout with potentially configured smudge/process filters. Evidence: a real fixture configures a smudge filter; the managed worktree is not created and the marker is not written. This is an intentional fail-closed error path.
- **I2 — Foreground Future drop:** Foreground sandboxed and host-native execution now aborts the owned task unless its handle is explicitly transferred to background-job ownership. Users cancelling/dropping a request no longer leave an unregistered execution running; normal completion/background behavior is unchanged.
- **I3 — Bubblewrap probe:** A stalled or over-limit `bwrap --version` probe now ends within five seconds with a bounded 4 KiB capture and reports the runtime unavailable. Sandbox setup fails closed instead of hanging the caller.

## Coverage Ledger

| Surface | Status | Evidence / behavior trace |
|---|---|---|
| Ordinary foreground execute | Reviewed - no user-visible regression found | `execute` still returns completion or explicit background result; the new abort guard only acts when the caller drops before ownership transfer. Full parallel tests passed twice; Unix descendant-cancellation test passed. |
| Ordinary background execute | Reviewed - no user-visible regression found | Admission still inserts into the registry before later awaits; rejected admission terminates the task. Job-registry tests passed (18). |
| `poll_job` | Reviewed - no user-visible regression found | Poll semantics/results unchanged; registry suite and full test suites passed. |
| `stop_job` | Reviewed - no user-visible regression found | Job termination remains host-owned, scoped to the retained job; process-group/Job lease cleanup path unchanged. |
| Resource Bounds output overflow | Reviewed - no user-visible regression found | Late reader overflow is rechecked after joins; incomplete/overflow output stays terminal. Sandbox tests passed (37), runtime/blocking tests passed. |
| Stdin timeout | Reviewed - no user-visible regression found | Existing bounded stdin timeout and UNKNOWN/error behavior are unchanged; covered by full test suites. |
| MCP stdin EOF cleanup | Reviewed - no user-visible regression found | Every drained job is synchronously sent abort before any join await; results are removed as before. Batch cancellation regression passed; direct real-descendant composition remains a test gap. |
| Agent/model execution | Reviewed - no user-visible regression found | Host process lifecycle uses the same ProcessGroup ownership; bounded process and cancellation behavior unchanged except task drop now aborts. Full suite passed. |
| Verifier host Git | Reviewed - no user-visible regression found | Host Git path and clean environment unchanged; blocking observation remains bounded and incomplete output rejected. Managed-worktree and full suite tests passed. |
| Managed Worktree creation Git | Intentional I1 | Stable local/worktree filter configuration now blocks creation. Timeout/overflow stays unknown and reconciles without restoring attempts. Focused filter test and 187 managed-worktree tests passed. |
| Managed Worktree observation Git | Reviewed - no user-visible regression found | Read-only command allowlist and host Git identity unchanged; bounded capture behavior remains fail-closed. Full suite passed. |
| Writer atomic helper | Reviewed - no user-visible regression found | Helper invocation and writable-root authority are unchanged; no release packaging/helper target was added. Full suite passed. |
| PRIMARY execution | Reviewed - no user-visible regression found | `Goal.cwd`/primary Session identity remains unchanged; no managed root expansion found. Full suite passed. |
| Managed `execution_root` | Reviewed - no user-visible regression found | Host-derived execution root and planner handoff remain unchanged. Managed-worktree suite passed. |
| Linux sandbox and Bubblewrap | Not covered | Source now uses the shared bounded process runner and remains fail-closed, but Linux-only helper build/runtime was not executable on this macOS host. Require Linux CI, including helper build and sandbox tests. |
| macOS Seatbelt | Reviewed - no user-visible regression found | Sandbox policy construction is unchanged; macOS sandbox tests passed as part of the focused sandbox suite. |
| Windows approval path | Reviewed statically; runtime Not covered | Approval consent/wait behavior was not changed. Windows Job/sandbox runtime and native approval tests require Windows CI. |
| Resource Bounds ceilings | Reviewed - no user-visible regression found | Trusted Git limits were moved to the central resource policy without changing their 64 MiB stdout / 1 MiB stderr values. Full tests/clippy passed. |
| Windows process-tree containment | Not covered | Static ordering is Job create -> suspended spawn -> assign -> resume; actual Job assignment, nested/outer Job, abnormal owner-death and timeout behavior require Windows CI. |

## Evidence Appendix

- `cargo test --locked --all-targets --quiet` -> two consecutive default-parallel passes; each main test target 1125 passed, 0 failed.
- `cargo test --locked --all-targets managed_worktree -- --quiet` -> 187 passed, 0 failed.
- Focused macOS tests: platform runtime 11; ownership 8; process-group stress 9; job registry 18; sandbox 37; blocking-reader panic 1 -> all passed.
- Failing timeout-descendant witness after fixture correction -> 20/20 focused passes; four concurrent copies plus platform runtime, ownership, and stress siblings all passed.
- `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets --all-features -- -D warnings`, and `git diff --check` -> passed.
- `cargo check --locked --target x86_64-pc-windows-msvc --all-targets` -> did not reach project code; third-party C compilation failed because this macOS host lacks Windows SDK headers. It is not counted as Windows compile/runtime proof.
- Behavior graph coverage: direct path traces used for foreground/background execution, registry admission/stop/shutdown, managed creation/reconciliation, and sandbox gate. No graph was needed for unchanged linear verifier/Writer/approval paths; those were checked against their caller contracts.
