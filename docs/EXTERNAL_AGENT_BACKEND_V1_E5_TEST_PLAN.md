# GoalLatch External Agent Backend V1 — E5 Acceptance & Test Plan

> Status: DRAFT / Owner approval required. Design only.
> Baseline: PR #41 HEAD 4f2264249d1b40e7b43314c3392da58007b24333 (2026-11-08 review).
> Dependencies: E5 design document and PR #30–#41. No claims of E5 implementation.

## 0. Common execution rules

- Use deterministic Rust fixtures, injected faults, host-only transports and isolated Git repositories for the required automated tests.
- Separately perform human/agent-led real external-model dogfooding; scripted Python proposal builders count as transport/E2E verification but not independent general-model reasoning.
- No test may turn off host authority, bypass user approval, bypass preimage checks or treat UNKNOWN as not performed.
- Each test records pre/post Goal revision, plan revision, Task/Attempt states, WorkItem/Receipt IDs, operation/request IDs, workspace digest and verification evidence when relevant.
- Test both CODEX and EXTERNAL. EXTERNAL no Codex CLI spawn by instrumented counter + forbidden transport/spawn interception. CODEX remains behavior-compatible.
- Supported target CI: quality, Windows x64/arm64, Linux x64/arm64 and published Ubuntu compatibility targets, macOS Intel/Apple Silicon. Do not claim local test coverage on OSes not locally tested.
- All protocol/error codes should be frozen as closed machine-readable types, with redacted and bounded detail.
- Critical or High authorization vulnerabilities, false completion and duplicate write are hard NO-GO.

## 1. E5-0 DAG and Scheduling (AT-E5-001..012)

| Test | Scenario | Passing condition |
|---|---|---|
| 001 | Simple predecessor A → B | B stays PENDING until A has host verification COMPLETED |
| 002 | Diamond A → B,C → D | D enters READY only when both B and C are verified complete |
| 003 | Two independent PENDING tasks | Deterministic READY order with no duplicate transition |
| 004 | Mixed mandatory/optional | Stable task selection per existing mandatory-first ordering |
| 005 | BLOCKED predecessor | Descendant never becomes READY |
| 006 | FAILED/NEEDS_REPLAN predecessor | No premature READY; typed blocked/replan reason |
| 007 | Superseded predecessor after replan | Active-graph closure used; old Task completion not proof |
| 008 | Bounded host-step budget 1 / 8 | At most allowed commits; no hidden spin |
| 009 | Two concurrent goal_next calls | CAS prevents double readiness advancement |
| 010 | Crash before/after READY commit | Restart preserves exactly one committed transition |
| 011 | All READY tasks complete | Goal verification runs only after required proof records exist |
| 012 | CODEX scheduler regression | Existing selection/transition tests unchanged |

## 2. E5-A Writer issue and preflight (AT-E5-013..027)

| Test | Scenario | Passing condition |
|---|---|---|
| 013 | One allowed Writer Task | Writer Attempt + WorkItem durably issued together; no model call |
| 014 | Writer workspace lease held | Another Writer cannot issue or publish |
| 015 | READ_ONLY scope with WRITE_UTF8 | Rejected before staging or mutation |
| 016 | Path traversal, .git, symlink parent, forbidden path | Refused by existing host scope validator |
| 017 | Existing preimage mismatch | Non-mutating NEEDS_REPLAN or typed refusal; no intent publish |
| 018 | ABSENT preimage target appears | Cannot clobber target |
| 019 | Duplicate canonical destination | Rejected |
| 020 | Oversized content, too many operations, deep JSON | Bounded rejection |
| 021 | Non-candidate status carrying operations | Rejected |
| 022 | Missing/foreign Attempt/Goal/session | Rejected without existence oracle |
| 023 | Stale Goal/PlanRevision | Rejected; no staged mutation |
| 024 | Same submission_id + matching payload/work | Stable receipt, no duplicate stage |
| 025 | Same ID + different work/payload | Refusal/CONFLICT, no mutation |
| 026 | Blob/object staging interrupted | Orphan safely isolated; no workspace side effect |
| 027 | Private staged typed payload | Integrity hash, bounded bytes, POSIX permissions/Windows ACL and no raw model JSON in logs |

## 3. E5-A Writer publish and recovery (AT-E5-028..045)

| Test | Scenario | Passing condition |
|---|---|---|
| 028 | Submit valid Writer proposal | PREPARED host intent + reference + Receipt durable before host publication |
| 029 | Approval denied or channel missing | Zero unapproved writes; fail-closed typed stop |
| 030 | Approval delayed | TaskStore session lock not held during approval wait |
| 031 | Preimage changes after validation | Host revalidation refuses publication |
| 032 | Path authority changes before commit | Host revalidation refuses publication |
| 033 | One WRITE_UTF8 publish | Stable request ID; postimage verified; journal COMPLETE |
| 034 | Multi-file partial publication | Per-operation status accurately recorded; no false all-or-none claim |
| 035 | Crash before intent commit | No mutation; no accepted receipt |
| 036 | Crash after intent PREPARED before BeginOperation | Recovery uses durable typed bytes and rechecks all authority, never trusts stale stage |
| 037 | Crash after BeginOperation before publish | Unknown effects are reconciled, never blindly reissued |
| 038 | Crash during publish | UNKNOWN never treated as NOT_PERFORMED |
| 039 | Crash after publish before postimage commit | Host reconciliation or explicit BLOCKED; no double publication |
| 040 | Crash after CompleteOperation before Reviewer | Reviewer resumes; Writer not re-run |
| 041 | Staged content tampered or deleted | Hash mismatch → BLOCKED, never reconstruct from model guess |
| 042 | Cancel after intent, before write | No unauthorized continuation; safe cleanup of never-begun staged objects |
| 043 | Cancel during unknown side effect | Reconciliation required; no blind cleanup |
| 044 | Same Goal under two MCP clients | Exactly one valid mutation execution despite concurrent goal_next |
| 045 | File permissions / executable bit | Existing workspace_publish semantics preserved cross-platform |

## 4. E5-B Reviewer (AT-E5-046..056)

| Test | Scenario | Passing condition |
|---|---|---|
| 046 | Review requested before Writer confirmed | No Reviewer WorkItem |
| 047 | Writer performed, Reviewer pending | Reviewer WorkItem has same Task/Attempt and host-owned hashes |
| 048 | Reviewer models passing result | ReviewGate host check required; no direct Task COMPLETED |
| 049 | Too many blocking findings | Task BLOCKED with review evidence |
| 050 | Forged ReviewFileEvidence | Host ignores model-supplied authority; rejects mismatches |
| 051 | File modified after Writer | Typed stale-postimage refusal |
| 052 | Reviewer crash after work issue | Same pending work redelivered |
| 053 | Reviewer duplicate submission | One receipt and one ReviewResult |
| 054 | Reviewer rejected output | No Writer re-execution; explicit retry/stop |
| 055 | Reviewer metadata/summary secret injection | Redacted bounded storage |
| 056 | Passing Reviewer then Task Verify | Only Host Task Verifier may make Task COMPLETED |

## 5. E5-C Replanner (AT-E5-057..071)

| Test | Scenario | Passing condition |
|---|---|---|
| 057 | Readonly returns NEEDS_REPLAN | Host Scheduler selects eligible Replanner trigger |
| 058 | Replanner issued without durable trigger | Refused |
| 059 | Replanner request > budget | Deterministic stop, no infinite replan |
| 060 | Replacement graph has cycle | Rejected with no partial mutation |
| 061 | Unauthorized expanded scope | Rejected |
| 062 | Required criterion no longer covered | Rejected |
| 063 | Supersession on non-pristine Task | Rejected |
| 064 | Replay same replan identity/digest | Idempotent, no new plan revision |
| 065 | Replay same identity different digest | Conflict |
| 066 | Valid replacement plan | Task DAG + criterion binding + Receipt COMMITTED in one snapshot |
| 067 | Crash before/after replan commit | All or none; no partial graph |
| 068 | Rerun replan from stale Goal revision | Stale refusal |
| 069 | Replan followed by READY propagation | New tasks runnable under active graph only |
| 070 | Replan then Readonly then Host Goal Verify | Full deterministic E2E |
| 071 | External model-driven NeedsReplan | Separate real-client trace, non-hardcoded response |

## 6. E5-D Managed Worktree (AT-E5-072..082)

| Test | Scenario | Passing condition |
|---|---|---|
| 072 | PRIMARY mode | Workspace root correctly bound |
| 073 | MANAGED_WORKTREE with clean primary | Existing eligibility and approval reused |
| 074 | Dirty primary | Host refuses creation or advancement under existing policy |
| 075 | WorkItem forged worktree root | Rejected |
| 076 | Stale worktree identity | Stop before publication |
| 077 | Writer in managed workspace | Primary worktree remains unchanged |
| 078 | Snapshot evidence after Goal Verify | Durable identity/commit digest aligned |
| 079 | GOAL_FINALIZATION_BLOCKED case | Never falsely COMPLETED |
| 080 | Concurrent primary advancement | Existing reconciliation/error policy maintained |
| 081 | Crash during managed evidence persistence | No forged result/duplicate write |
| 082 | No auto-merge or push | Git refs remain unchanged unless separately approved |

## 7. E5-E Dogfooding, compatibility and security (AT-E5-083..098)

| Test | Scenario | Passing condition |
|---|---|---|
| 083 | Real external model, multi-task read-only DAG | Host-verified completion; actual model-generated response evidence |
| 084 | Real external model, scoped code edit | GoalLatch-owned approved publication, Reviewer, Verifier, complete evidence |
| 085 | Real external model, NeedsReplan | Replanner request and replacement witnessed |
| 086 | Scripted MCP wire regression | Actual JSON-RPC transport; separately labeled scripted |
| 087 | Client disconnect/reconnect with Writer PENDING | Same IDs and no false accepted writes |
| 088 | Host crash around mutation | Journal-derived recovery; no blind retry |
| 089 | Baseline CODEX role suite | Unchanged legacy behavior |
| 090 | Schema-5 migration and unsafe downgrade | Correct mode, no external state loss |
| 091 | Cross-session/Goal replay and information leak | Opaque denial, no data exposed |
| 092 | Approval absent/denied/yolo operator-set | Host policy retained; MCP cannot switch yolo |
| 093 | EXTERNAL no Codex binary on PATH | All E5 roles remain usable without spawning Codex |
| 094 | Forbidden spawn interception | Any attempted transport/spawn causes deterministic test failure |
| 095 | Secret sentinel in model content | None in logs, receipts, PR attachments, dogfood transcripts |
| 096 | Windows, Linux, macOS required CI matrix | All actual required checks success |
| 097 | Source audit of host mutation reachability | No new bypass of Writer boundary, WorkspacePublish, ReviewGate or Goal Verifier |
| 098 | Live Goal mismatch/permission refusal | Accurate PARTIAL/BLOCKED, no forged READY or COMPLETED |

## 8. Owner review decisions

OD-01: Approve host-only READY propagation semantics and which state transitions are bundled in one snapshot.
OD-02: Approve private typed content staging representation, ownership/ACL, retention and orphan cleanup; Stage bytes must be durable before acceptance.
OD-03: Define precise boundary for external Writer receipt COMMITTED vs Task verification.
OD-04: Confirm machine-readable approval/side effect/ReviewGate outcome vocabulary without changing legacy API fields.
OD-05: Review whether current receipt bound (64) and work issuance bound (64) permit realistic DAG+replan workloads; do not silently raise limits.
OD-06: Decide policy for legacy E3 SUBMITTED artifacts; never guess content, preserve audit data.
OD-07: Approve managed evidence shared seam and which public Goal API exposes worktree identifiers.
OD-08: Decide if independent security review is mandatory before V1 Beta launch.
OD-09: Confirm external process/agent dogfood protocol, redaction and reproducer evidence.
OD-10: Review dependency/license report before introducing any copied code/dependencies.

## 9. Final acceptance

E5 can be READY_FOR_OWNER_REVIEW only if:
- No unapproved writes, no duplicate mutation, no false verified completion, no Codex spawn, no untrusted Reviewer/Replanner authority escalation.
- All relevant tests above are implemented and passing; if a test is not applicable its omission is justified with a specific reason.
- The real external-model flow is recorded distinctly from scripted examples.
- CI and a repeatable integration merge-tree review are GREEN, with existing CODEX behavior unchanged.
- The PR stack and exact HEAD/BASE refs are reported; existing main/Draft PRs/Doctor WIP/Release remain unchanged.
