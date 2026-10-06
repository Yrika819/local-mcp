# Receiving Code Review Resolution

## Report Contract

- Report type: `receiving-code-review`
- Report ID: `rr-20261007-29aaa1e8`
- Resolution ID: `rr-20261007-29aaa1e8`
- Review chain ID: `rc-20261006-5a2e8b31`
- Review generation being received: `0`
- Source report ID: `cr-20261006-5a2e8b31`
- Source review report ID: `cr-20261006-5a2e8b31`
- Source review report path: `tmp/reviews/2026-10-06-code-review-report-5a2e8b31.md`
- Generated at: `2026-10-07T00:00:00Z`
- Report path: `tmp/reviews/2026-10-07-receiving-code-review-resolution-29aaa1e8.md`
- Git mutation during receiving: `None; all implementation changes remain uncommitted on the authorized unpublished local branch`
- Status: `Resolution complete; bounded generation-1 review follows`

## Scope and Authorization

- Authorization basis: the user explicitly authorized implementation, adversarial review, regression review, validation, and first normal push of the unpublished `hardening/platform-runtime-closure-v1` branch, without merging it.
- Baseline at review freeze: `23fa4af0a3acb49c6878773bd4c84be2cb490569`
- Current target: working tree relative to that baseline; exact changed-code fingerprint at resolution time: `sha256:418667d6be2adaafa427b65377ddadc372f98d6ead31aeb2fb00fb0f0c6da2e6`
- Branch: `hardening/platform-runtime-closure-v1`
- Protected untracked Resource Bounds audit remains outside the implementation diff and retains SHA256 `fcbdd96c7ef94ed425b7ed7e54d900563a9b71af53481e46af37eeedcd61acc0`.

## Parent Test Gap Disposition

| ID | Severity | Disposition | Change made | Verification |
| --- | --- | --- | --- | --- |
| `T1` | Minor | Deferred | The branch still lacks one dedicated end-to-end test that drives managed `git worktree add` timeout/overflow/incomplete capture through durable attempt consumption and reconciliation. Generic bounded-runner tests and managed retry/reconciliation tests remain separate. No production retry classification was changed. | `cargo test --locked --all-targets managed_worktree -- --quiet` passed 187 tests; full parallel suite passed twice. The exact timeout-to-durable-reconciliation composition is still not directly asserted, so the gap remains open for generation 1. |
| `A9` | Not covered | Open pending platform CI | Local host is macOS; Linux Bubblewrap and Windows Job Object runtime guarantees cannot be proven locally. | Must be resolved by the branch's complete 11-job GitHub Actions matrix; no local compile result is substituted for Windows runtime evidence. |

## Findings / Candidate Dispositions

| Candidate | Origin | Disposition | Evidence and rationale |
| --- | --- | --- | --- |
| Stale Unix PGID signal if inherited SIGCHLD auto-reaps a leader | Unix process-ownership specialist; coordinator verified | Fixed | Added `normalize_child_signal_policy()` to reset SIGCHLD to default and clear SA_NOCLDWAIT before the main Tokio runtime and before process spawn. Added isolated regression that sets SIG_IGN/SA_NOCLDWAIT and verifies normalization. This relies on the documented standalone-host invariant that the application does not later install a competing child reaper. |
| Cancellation of `release_all_jobs()` can detach drained jobs not yet awaited | Async lifecycle specialist; coordinator verified | Fixed | `terminate_jobs()` synchronously requests abort for every drained job before the first await, then joins. Regression test proves all tasks receive cancellation before their JoinHandles are dropped. |
| Configured local Git filter driver can run during managed checkout | Blocking Git/security specialist; coordinator reproduced by static trace and test | Fixed for stable pre-existing configuration | Host Git creation now queries local and worktree filter configuration through the validated executable and clean environment; incomplete/overflowed/timed-out queries fail closed, and configured filters block checkout. A real fixture configures a smudge filter and verifies no marker/worktree is created. Concurrent same-user edits to repository config between query and mutation are not serialized; this remains a bounded review caveat, not represented as an atomic configuration lock. |
| Late reader observes output overflow after leader completion | Earlier full-suite reproduction and process-runtime review | Fixed | Runner re-reads the shared overflow flag after both readers join; overflow/incomplete output remains rejected. |
| Reader thread creation failure or panic could yield accepted incomplete capture | Process-runtime review | Fixed | Reader spawn is fallible, failure terminates the owned tree, and reader panic returns incomplete capture. |
| Direct child PID signal after Unix waitid loses ownership proof | Process-runtime review | Fixed | Blocking runner does not signal remembered PID/PGID after observation loss; result remains unknown. |
| Foreground future drop detached owned task | Async lifecycle review | Fixed | `AbortOnDropJoinHandle` aborts unless the handle is explicitly transferred to the background registry; a descendant witness covers cancellation. |
| Windows assignment-failure cleanup / nested-job behavior | Windows specialist | Deferred to Windows CI and generation-1 report | Source ordering is suspended spawn -> Job assignment -> resume; failure is fail-closed. Runtime assignment, close, nesting, and injected failure behavior require Windows execution and are not claimed as locally proven. |
| Deliberately escaped Unix descendant survives process-group termination | Unix specialist | Disproved as a defect within the approved guarantee | The design defines the contained tree as descendants that have not deliberately escaped. Tests demonstrate bounded caller cleanup while explicitly not claiming containment after a descendant changes process group/session. This limitation remains documented. |
| 15-second integration-test deadline intermittently expired after approval server completed | Coordinator investigation | Fixed as test-only timing assertion | 20 consecutive focused runs passed at roughly 7 seconds, then four copies plus three process-runtime sibling suites passed concurrently at roughly 10 seconds. The affected test now has a bounded 60-second test-only deadline; production timeouts and the semantic side-effect assertions are unchanged. |

## Verification After Receiving

- `cargo fmt --all -- --check` -> passed.
- `git diff --check` -> passed.
- `cargo test --locked --all-targets started_trusted_git_failure_payload_is_unknown_locked_and_not_successful` -> 20/20 focused repetitions passed before increasing the test-only contention deadline; the targeted test passed again after the change.
- Four concurrent copies of the failing test plus platform-runtime, process-blocking, and job-registry sibling tests -> all seven passed.
- `cargo test --locked --all-targets --quiet` -> two consecutive default-parallel runs passed; each reported 1125 tests in the main test target, with no failures.
- `cargo test --locked --all-targets managed_worktree -- --quiet` -> 187 passed, 0 failed.
- Focused runtime 11; process-group ownership 8; process-group stress 9; job registry 18; sandbox 37; blocking-reader panic 1 -> all passed.
- `cargo clippy --locked --all-targets --all-features -- -D warnings` -> passed.
- Protected audit SHA256 -> unchanged.

## Remaining Items Returned to Generation 1

- `T1`: managed creation timeout/incomplete-capture through durable reconciliation lacks a single dedicated composition test.
- `A9`: Linux/Windows/macOS cross-platform runtime evidence remains pending actual branch CI.
- Review the check-then-use race for local Git filter configuration under concurrent same-user repository-config mutation; do not claim it is serialized.
- Review Windows direct termination failure paths and nested/already-in-job behavior; do not infer runtime safety from macOS tests.
- Generation 1 is terminal and must state the above residuals without reopening settled parent issues absent a concrete code, contract, or evidence change.
