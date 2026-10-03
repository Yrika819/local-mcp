# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261001-poll91a7`
- Review chain ID: `rc-20261001-poll91a7`
- Review generation: `0`
- Review trigger: `initial`
- Parent review report ID: `None`
- Parent review report path: `None`
- Parent resolution ID: `None`
- Parent resolution path: `None`
- Generated at: `2026-10-01T00:00:00Z`
- Report path: `tmp/reviews/2026-10-01-code-review-report-pollready-91a7.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:894cba100676b6ca287f986ad1fd09001ec1ca43a517cf9eee380e57903152a8`

## Scope

- Review date: `2026-10-01`
- Scope kind: `file set`
- Scope description: `Narrow follow-up review of the current uncommitted poll-based readiness correction in src/approvals.rs against HEAD 91a7f3f6de7695b3de117b2597b7c8e4cf3d7060. The Windows responder now wraps pinned listener.accept() with std::future::poll_fn and sends readiness only after Poll::Pending; std::future::Future is imported under cfg(windows). Context: prior CI showed the pipe bound but approvals::request got ERROR_FILE_NOT_FOUND. Review checks whether readiness follows first connect-future polling, startup/accept errors propagate, and test-only authority remains unchanged. User reports Mac focused integration passes (non-Windows no-op selected); Windows CI is pending.`
- Scope mode: `full frozen scope`
- Baseline: `commit 91a7f3f6de7695b3de117b2597b7c8e4cf3d7060 (HEAD)`
- Target: `working tree change in src/approvals.rs`
- Changed paths: `1`
- Diff size: `16 insertions, 5 deletions`
- Completion: `Complete within reviewed scope`
- Requirements consulted: `Current user request; prior narrow responder review cr-20261001-cfg701fa; existing SessionListener::accept and approvals::request protocol.`
- Prior resolution consulted: `None`
- Assumptions: `Mac focused integration result is user-reported and not independently rerun. Windows Tokio named-pipe runtime behavior is assessed statically; Windows CI is pending.`
- Excluded as unrelated: `All other paths and prior changes already present in HEAD; no broader approval or managed-worktree review.`

## Review Orchestration

- Assessment subagent: `Coordinator assessment - one Windows test helper's async startup handshake is a cohesive poll/error-flow review; separate partitions would share the same state machine and authority path.`
- Orchestration decision: `Single reviewer`
- Decision confidence: `high`
- Decision rationale: `The diff changes only readiness timing and one cfg-gated import. One trace can verify future polling, channel state, worker result ownership, and unchanged authorization assertions.`
- Coordinator override: `None`
- Context or tool limits: `No tests/build/CI run per read-only request; Windows CI is pending.`

### Risk Dimensions

- `Readiness must not be published until the Windows named-pipe connect operation has been polled and armed.`
- `A pre-readiness accept error must not be mistaken for successful readiness or silently lose actionable failure detail.`
- `The responder must stay confined to test builds and retain exact approval request/cwd validation before allow.`

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `R1 (Coordinator)` | Async startup lifecycle, error propagation, authority | `src/approvals.rs::spawn_test_approval_responder`; `SessionListener::accept`; `approvals::request` | `poll state`, `send-once behavior`, `pre/post-readiness errors`, `request transport`, `cfg(test, windows)`, exact operation/cwd checks | `Complete` |

### Synthesis Statement

The readiness move is directionally correct: `poll_fn` polls the pinned `listener.accept()` future first and only sends `Ok(())` after that future returns `Pending`. For Tokio's named-pipe connect path, this enters `NamedPipeServer::connect` and gives it an opportunity to initiate/register the OS connect before `approvals::request` is allowed to run. An error after readiness is retained in the returned worker `JoinHandle<Result<()>>`. One minor diagnostics issue remains: if the accept future returns `Ready(Err(_))` before ever returning `Pending`, the channel sender is dropped without sending the original error; the synchronous caller receives only the generic `test approval listener stopped before startup` channel-disconnect error, and the worker handle containing the original error is dropped. The operation/cwd authorization checks and `allow\n` are unchanged under `cfg(all(test, windows))`; no product authority path is affected.

## Review Snapshot

- Recommendation: `Pass with caveat`
- Completion: `Complete within reviewed scope`
- Why now: `The poll-based handshake addresses the bind-before-connect race, but an accept failure before the first Pending loses its original diagnostic at the synchronous startup boundary.`
- Must-review now:
  1. `F1` `Preserve pre-readiness accept error details`
- Findings count: `Blocker 0 | Major 0 | Minor 1 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 0`
- Coverage confidence: `medium`
- Biggest blind spot: `Actual Windows CI validation of the named-pipe startup timing is pending.`

## Complete Findings Index

| ID | Severity | Surface | Review risk | Confidence | Origin | Verification | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `F1` | `Minor` | Windows test responder startup | An accept error before readiness is reduced to a generic channel disconnect; the original cause is lost to the caller. | `high` | `Coordinator` | `Static control-flow trace` | `behavior; entry=Windows test approval responder startup; contract=accept startup failures preserve their underlying cause; effect=listener accept error is reported only as a generic readiness-channel disconnect` | `ifp-sha256:67e2957e32e88f918a637bb5a6dbff6f09cd20ff63ee24cff838bd2161a11260` | `kind:owner-decision; strength:authoritative; evidence:current user request to review whether startup errors propagate` |

## Blocker

None.

## Major

None.

## Minor

### F1 Minor - Preserve pre-readiness accept error details

Impact: `A Windows test responder startup failure still fails the test, but its underlying accept error is not reported at the synchronous helper boundary, complicating diagnosis of named-pipe CI failures.`
Review reason: `The changed readiness boundary introduces an accept-future failure path before readiness. The channel currently signals only Pending; on Ready(Err), the caller sees only disconnection.`
Surface: `Windows-only test responder startup handshake`
Issue key: `behavior; entry=Windows test approval responder startup; contract=accept startup failures preserve their underlying cause; effect=listener accept error is reported only as a generic readiness-channel disconnect`
Issue fingerprint: `ifp-sha256:67e2957e32e88f918a637bb5a6dbff6f09cd20ff63ee24cff838bd2161a11260`
Expected basis: `kind:owner-decision; strength:authoritative; evidence:current user request to review whether startup errors propagate`
Confidence: `high`
Origin: `Coordinator`
Coordinator verification: `Traced poll_fn Ready(Err) through the async block and ready_receiver.recv(); because no send occurs before Pending, the sender drops, receiver returns RecvError, and the JoinHandle retaining the worker's Err is dropped on the error return.`

Look here first:
- `src/approvals.rs#L247-L280`

Failure mode:
- Expected: `A failure from the first listener.accept() poll should be returned to the caller with the underlying accept cause.`
- Current: `Only Poll::Pending sends readiness. A first-poll Poll::Ready(Err(error)) exits through .await?, drops ready_sender, and the caller bails with the generic channel-disconnected message; the JoinHandle carrying error is dropped.`

Evidence:
- `In the poll_fn closure at src/approvals.rs#L250-L259, readiness is sent only in the Pending branch. The following .await? propagates an accept error only within the responder thread. In the synchronous match at src/approvals.rs#L277-L280, RecvError is converted to a generic startup message; the thread handle is not joined on that branch.`

Assumptions and limits:
- `This is a diagnostic/error-context defect, not evidence that the failure is allowed or that the request can bypass approval. The Windows-only path was not run locally.`

Reviewer action:
`Preserve the accept error in the readiness channel (or otherwise join/report the worker result) when it resolves with an error before the first Pending.`

## Questions

None.

## Test Gaps

None. The user reports the Mac focused integration passes and correctly exercises the non-Windows no-op, but it cannot verify Windows poll/readiness behavior. Windows CI is pending and listed as the principal blind spot.

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | Poll-based readiness and NamedPipeServer connect future | `src/approvals.rs::spawn_test_approval_responder`; `SessionListener::accept` | `R1` | `dependency trace` | `Reviewed - no issue found` | Pinned accept future is polled before readiness; Pending triggers one send, allowing the caller to initiate its request only after connect has been driven. Actual Windows CI remains pending. | Static control-flow/API trace; confirm with Windows CI. |
| `A2` | Startup/accept error propagation | `src/approvals.rs::spawn_test_approval_responder` | `R1` | `dependency trace` | `Finding F1` | Bind failures retain existing explicit error signal. Accept error before any Pending is reduced to generic channel-disconnect error at caller. Errors after Pending remain in the returned join handle. | F1; focused Windows fault-injection or equivalent CI diagnostic test would verify. |
| `A3` | Test-only authority and response contract | `src/approvals.rs::spawn_test_approval_responder`; `approvals::request` | `R1` | `contract trace` | `Reviewed - no issue found` | Function remains `cfg(all(test, windows))`; only `approval`/`start_command` with canonical expected cwd gets `allow\n`. Production request and policy paths are unchanged. | Diff and source trace. |

## Subagent Candidate Adjudication

No subagents were used. Coordinator candidate adjudication:

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `C1` | `Coordinator` | `accepted` | `F1` | First poll Ready(Err) does not execute the Pending branch; sender drops, receiver sees disconnect, and the join result is not observed. | Startup failure is not swallowed entirely, but useful root-cause detail is lost at the API boundary requested for review. Minor diagnostic impact. |
| `C2` | `Coordinator` | `dismissed` | `None` | On the normal sequence, the verifier cannot call approvals::request until the helper returns after Pending; the named pipe is created before accept is polled. | No evidence the poll-based handshake retains the original bind-before-client race; Pending is reached only after the connect future has been polled. Windows CI remains pending. |
| `C3` | `Coordinator` | `dismissed` | `None` | The operation and cwd checks, test-only cfg, and allow response are unchanged in this delta. | No product authority bypass or security policy change. |

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `src/approvals.rs` | `test-only` | Windows-only readiness handshake, Future import, startup failure propagation, unchanged request validation and authority boundary |
| `SessionListener::accept` and `approvals::request` | `dependency` | Named-pipe connect future and client open/write/read sequence |

### Verification Commands

- `git --no-pager diff --stat` -> `src/approvals.rs, 16 insertions and 5 deletions`.
- `git --no-pager diff --check` -> `passed; no whitespace errors`.
- `git rev-parse HEAD` -> `91a7f3f6de7695b3de117b2597b7c8e4cf3d7060`.
- Mac focused integration -> `user-reported passing; not independently rerun`.
- Windows CI -> `pending per user`.

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `F1` | `primary` | `src/approvals.rs#L247-L280` | Poll branch, await propagation, and synchronous readiness receive/error conversion. |
| `A1` | `listener contract` | `src/approvals.rs#L52-L65` | Windows listener.accept awaits NamedPipeServer::connect before returning the connected stream. |
| `A3` | `request contract` | `src/approvals.rs#L192-L220` | Client connects, writes newline-delimited approval JSON, shuts down, and awaits a line response. |

### Dismissed Coordinator Candidates

| Candidate | Decision | Evidence |
| --- | --- | --- |
| Poll-based readiness still signals immediately after bind | `dismissed` | The sync send is now conditional on `accept.poll()` returning Pending, not just bind success. |
| Readiness can be sent repeatedly | `dismissed` | `ready_sender.take()` removes the sender on the first Pending; later polls cannot send again. |
| New code adds an authority bypass | `dismissed` | Same test-only cfg and request type/operation/canonical cwd checks remain before `allow\n`; production request policy is unchanged. |
| `Future` import affects non-Windows | `dismissed` | The import is `#[cfg(windows)]`, matching the only code that calls `.poll()` on this future. |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A1` | No direct Windows run of Tokio named-pipe connect readiness after polling. | The source ordering is sound, but the original ERROR_FILE_NOT_FOUND symptom is platform-specific. | Windows CI should confirm the responder emits readiness after first Pending and approvals::request connects successfully. |

## Prior Resolution Reconciliation

None - fresh generation 0 narrow review; prior responder review `cr-20261001-cfg701fa` had no findings and no linked resolution report.

## Receiving Handoff

- Handoff status: `Ready for receiving-code-review`
- Automatic receiving permitted: `No`
- Source report ID: `cr-20261001-poll91a7`
- Scope fingerprint to recheck: `sha256:894cba100676b6ca287f986ad1fd09001ec1ca43a517cf9eee380e57903152a8`
- Actionable finding IDs: `F1`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `None`
- Deferred test-gap IDs: `None`
- Open question IDs: `None`
- Open coverage area IDs: `None`
- Highest-risk verification to repeat: `Windows CI test of first-poll Pending readiness followed by successful approvals::request connect and exact request validation.`
- Suggested implementation boundaries: `Keep change limited to readiness-channel result handling for accept completion before first Pending.`
- Re-review note: `Treat every finding as a claim to verify. Challenges require a counterclaim, argument, evidence, limits, and settlement criterion.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or product owner.`

## Report Self-Check

- `yes` Actual assessment mode and rationale are recorded.
- `yes` Every changed review-relevant or unknown-impact area appears once in the coverage ledger.
- `yes` F1 appears once in the index and once as a matching card; area A2 references F1.
- `yes` No standalone test gaps or questions identified.
- `yes` F1 has a canonical key, unique fingerprint, and current-user-request expected basis.
- `yes` Generation, trigger, scope, parent-resolution state, and handoff are consistent for a fresh generation 0 review.
- `yes` All meaningful coordinator candidates are adjudicated.
- `yes` No Not-covered areas exist; the Windows CI blind spot has a concrete verification step.
- `yes` Recommendation follows the skill mapping for one Minor finding.
- `yes` The report validator passed with `1 finding, 0 test gaps, 3 coverage areas, recommendation=Pass with caveat`.
- `yes` Git metadata was not mutated; report artifact is untracked.
