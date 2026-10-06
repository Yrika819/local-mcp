# Receiving Code Review Resolution

## Report Contract

- Report type: `receiving-code-review`
- Report ID: `rr-20261007-1d7f3637`
- Resolution ID: `rr-20261007-1d7f3637`
- Review chain ID: `rc-20261007-8f551df6`
- Review generation being received: `0`
- Source report ID: `cr-20261007-8f551df6`
- Source review report ID: `cr-20261007-8f551df6`
- Source review report path: `tmp/reviews/2026-10-07-code-review-report-platform-runtime-8f551df6.md`
- Generated at: `2026-10-07T00:00:00Z`
- Report path: `tmp/reviews/2026-10-07-receiving-code-review-resolution-platform-runtime-1d7f3637.md`
- Git mutation during receiving: `None; implementation remains uncommitted on the unpublished local branch`
- Status: `Resolution complete; generation 1 is terminal`

## Scope and Authorization

- Authorization basis: the user explicitly authorized implementation, adversarial review, validation, first normal branch push, and complete CI, with no merge to main.
- Baseline at freeze: `origin/main` `f4bfb9e0c339d0338825a5bdb0ed7a6d994322b1`
- Branch: `hardening/platform-runtime-closure-v1`
- No history was rewritten in this receiving phase. The only local unpublished-branch sanitation is the previously authorized and completed reconstruction; no public ref was altered.

## Dispositions

| ID | Severity | Disposition | Change made | Verification |
| --- | --- | --- | --- | --- |
| `T1` | Minor | Deferred | No new change. The dedicated managed-creation timeout/overflow/incomplete-capture through durable attempt consumption and reconciliation composition test remains absent. | Full parallel suites and 187 managed-worktree tests passed, but do not establish this missing composition directly. Retain as an open follow-up. |
| `T2` | Minor | Deferred | Shutdown requests abort for all drained jobs before awaiting any job. No further production change. The batch test uses pending tasks rather than a real descendant witness and cancellation of the actual shutdown future. | Batch cancellation test passed; separate descendant cancellation test and full parallel suites passed. Keep the direct composition test as an open gap. |
| `T3` | Minor | Deferred to Windows CI/test seam | No additional failure-injection hook was added. Source ordering remains suspended spawn -> assign -> resume, with failed setup terminal and no uncontained user code resumed. | macOS source review only. Native Windows CI and focused assignment/resume failure coverage remain required. |
| `A7` | Not covered | Open pending CI | No Windows runtime claim is made locally. | Must run the branch's actual Windows jobs, including abnormal owner death, Job assignment/outer Job behavior, and blocking-runner descendant tests. |
| `A8` | Not covered | Open pending CI | Linux Bubblewrap probe now uses bounded process-tree capture, but the Linux-only helper build/runtime has not run locally. | Must run Linux CI, including `codex-linux-sandbox` build and sandbox tests. |
| `A10` | Not covered | Open pending CI | macOS x64 local tests do not prove the full platform matrix. | Require all 11 named branch CI jobs to complete successfully. |

## Candidate Adjudications

| Candidate | Disposition | Evidence and rationale |
| --- | --- | --- |
| Same-user Git filter-config change between preflight and `git worktree add` | Accepted documented residual; not a code finding | `SECURITY.md` §73–75 states same-user processes can race final Git-config/filesystem/executable checks because Git provides no shared atomic snapshot. The managed-creation guard rejects stable configured filters and fails closed on incomplete queries, but does not claim serialization. The earlier separate generation-1 candidate report `tmp/reviews/2026-10-07-code-review-generation-1-platform-runtime-6bdc41.md` classified this as Major; coordinator re-adjudication uses the explicit documented threat boundary and current tests. |
| Linux `bubblewrap_support::probe` unbounded/uncontained launch | Fixed before this report freeze | It now uses `run_bounded_blocking_with_limits` with a five-second deadline and 4 KiB per output stream; the Linux helper target includes the shared process-group/blocking modules and normalizes SIGCHLD before its own spawn path. Linux runtime proof remains pending CI. |
| Parallel timeout-descendant test false-survivor from inherited witness writer | Fixed as a test-fixture race | The parent no longer owns a witness writer descriptor across concurrent fork/exec. The intended child opens the FIFO in `pre_exec`; the focused test passed 20/20 and four concurrent copies passed alongside Unix process-runtime, stress, and ownership suites. |
| Inherited SIGCHLD auto-reap invalidating Unix PGID proof | Fixed under standalone/no-competing-reaper invariant | Main and the Linux sandbox helper normalize SIGCHLD before runtime/child spawn; isolated SIG_IGN/SA_NOCLDWAIT regression passed. |
| Cancellation of drained shutdown jobs could detach later tasks | Fixed | All drained jobs receive abort before the first join await; regression and full parallel suites passed. |

## Verification After Receiving

- `cargo fmt --all -- --check` -> passed.
- `git diff --check` -> passed.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> passed.
- `cargo test --locked --all-targets --quiet` -> two consecutive default-parallel runs, each 1125 main tests passed, 0 failed.
- `cargo test --locked --all-targets managed_worktree -- --quiet` -> 187 passed, 0 failed.
- Focused platform-runtime 11; process-group ownership 8; process-group stress 9; job registry 18; sandbox 37; blocking-reader panic 1 -> all passed.
- Updated timeout-descendant witness: 20/20 consecutive focused passes; under contention, 4 copies plus Unix runtime, group stress, and ownership test processes all passed.
- Windows cross-target check attempted: `cargo check --locked --target x86_64-pc-windows-msvc --all-targets` stopped in third-party `ring`/`aws-lc-sys` C compilation because the macOS host has no Windows SDK headers (`assert.h`, `windows.h`); it did not reach project code and is not counted as a Windows compile pass.
- Protected Resource Bounds audit SHA256 remained `fcbdd96c7ef94ed425b7ed7e54d900563a9b71af53481e46af37eeedcd61acc0`.

## Remaining Items Returned to Generation 1

- `T1`, `T2`, and `T3` remain deferred test gaps.
- `A7`, `A8`, and `A10` remain open until actual platform CI completes.
- Do not reinterpret the accepted same-user Git-config race as serialized; continue to reject filters observed by the preflight.
- Generation 1 is terminal; no automatic receiving cycle follows this resolution.
