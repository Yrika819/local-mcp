# Code Review Report

## Report Contract

- Report type: `code-review`
- Report ID: `cr-20261007-6bdc41`
- Review chain ID: `rc-20261006-5a2e8b31`
- Review generation: `1`
- Review trigger: `post-implementation`
- Parent review report ID: `cr-20261006-5a2e8b31`
- Parent review report path: `tmp/reviews/2026-10-06-code-review-report-5a2e8b31.md`
- Parent resolution ID: `rr-20261007-29aaa1e8`
- Parent resolution path: `tmp/reviews/2026-10-07-receiving-code-review-resolution-29aaa1e8.md`
- Generated at: `2026-10-07T00:00:00Z`
- Report path: `tmp/reviews/2026-10-07-code-review-generation-1-platform-runtime-6bdc41.md`
- Source skill: `code-review`
- Status: `Review complete`
- Git mutation during review: `None`
- Scope fingerprint: `sha256:418667d6be2adaafa427b65377ddadc372f98d6ead31aeb2fb00fb0f0c6da2e6`

## Scope

- Review date: `2026-10-07`
- Scope kind: `working tree`
- Scope description: Terminal generation-1 review of the implementation delta and affected security authority paths: SIGCHLD normalization/process ownership; managed Git filter refusal, executable and environment; sandbox/tool/PATH; side-effect UNKNOWN and retry; Writer/execution roots. Parent report and complete receiving resolution were read before this review.
- Scope mode: `implementation delta plus affected execution chains`
- Baseline: `23fa4af0a3acb49c6878773bd4c84be2cb490569`
- Target: current working tree; normalized diff SHA256 matches receiving resolution target `418667d6be2adaafa427b65377ddadc372f98d6ead31aeb2fb00fb0f0c6da2e6`
- Changed paths: `11`
- Diff size: `548 additions / 116 deletions`
- Completion: `Complete within reviewed scope`
- Requirements consulted: User's explicit authority/security checks; parent generation-0 report; complete receiving resolution; `docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md` §§4-7; managed worktree prepare/observe and fallback side-effect contracts.
- Prior resolution consulted: `rr-20261007-29aaa1e8` at `tmp/reviews/2026-10-07-receiving-code-review-resolution-29aaa1e8.md`
- Assumptions: The running Local MCP executable's environment is host-owned. A concurrently writing same-user process can mutate repository Git config; the filter preflight is not an atomic config snapshot. Windows/Linux/macOS target-specific runtime claims require native CI.
- Excluded as unrelated: Untracked prior audit/report artifacts and unrelated implementation surfaces. No source changes or Git operations that mutate state were made.

## Review Orchestration

- Assessment subagent: `Coordinator assessment - bounded changed security boundaries with shared execution/authority call chains; one reviewer can trace coherently`
- Orchestration decision: `Single reviewer`
- Decision confidence: `high`
- Decision rationale: Requested scope is narrow and cohesive; the filter-config race, Git identity and process ownership all meet at the host mutation path. Platform runtime evidence is separately recorded as unavailable rather than split into duplicated static passes.
- Coordinator override: `None`
- Context or tool limits: No subagent execution used. Host is macOS; no Linux Bubblewrap or Windows runtime available. Focused tests covered filter refusal, SIGCHLD reset, and shutdown cancellation on this host only.

### Risk Dimensions

- Replacing SIGCHLD disposition/flags must preserve the unreaped-child ownership proof before any runtime or child starts.
- Host Git checkout must not run repository-selected filters outside the sandbox; check/use races could defeat a one-time config refusal.
- Executable identity and environment must not be selectable by repository or model input.
- Unknown mutation outcomes must remain uncertain and must not restore durable retry budget.
- Platform sandbox and Windows Job guarantees require native runtime evidence.

### Reviewer Assignments

| Reviewer | Angle | Owned surfaces | Mandatory cross-checks | Status |
| --- | --- | --- | --- | --- |
| `Coordinator` | Security, authority, process/state contracts | Current 11-path diff plus `sandbox`, host Git resolver, managed prepare/observe, fallback, Writer/execution callers | SIGCHLD ordering; filter check/use; PATH/env; sandbox/fallback; UNKNOWN/retry; root authority; inherited dispositions | Complete - static trace plus focused macOS tests |

### Synthesis Statement

The parent dispositions for SIGCHLD normalization, stable filter refusal, process cleanup, and task cancellation remain supported; no repository-selected Git path, sandbox fallback, UNKNOWN upgrade, retry-budget restoration, or Writer/execution-root widening was found. One security candidate remains: the filter-driver query and host checkout are separate invocations without serialization, leaving a concurrent same-user Git-config replacement window. The inherited timeout-composition test gap and native platform evidence gap remain open. Windows direct termination/nested Job behavior and the stated abnormal-owner-death guarantees remain unproven locally.

## Review Snapshot

- Recommendation: `Changes requested`
- Completion: `Complete within reviewed scope`
- Why now: A concurrent config change between the filter guard and host checkout can make Git execute a repository-configured filter in the host process; inherited test and platform evidence gaps also remain.
- Must-review now: `F1` filter configuration check/use race; `T1` ambiguous managed mutation lifecycle test; `A7` native platform evidence
  1. `F1` Non-atomic Git filter refusal
  2. `T1` Managed timeout/capture error through durable reconciliation
  3. `A7` Linux/Windows platform runtime evidence
- Findings count: `Blocker 0 | Major 1 | Minor 0 | Question 0`
- Standalone test gaps: `Blocker 0 | Major 0 | Minor 1`
- Coverage confidence: `medium` static and focused macOS tests; `low` cross-platform runtime
- Biggest blind spot: Windows Job assignment/termination/nesting and Linux Bubblewrap runtime, plus the check/use race's practical concurrent exploitability under the supported threat model.

## Complete Findings Index

| ID | Severity | Surface | Review risk | Confidence | Origin | Verification | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `F1` | `Major` | Managed host Git checkout filter guard | A repository config update after preflight can cause host Git to execute a filter outside the sandbox | `high` | `Coordinator` | Static control-flow trace; filter regression test confirms a pre-existing configured filter is blocked | `behavior; entry=managed worktree creation; contract=repository-controlled filter configuration cannot execute outside the sandbox during host checkout; effect=concurrent config replacement executes an attacker-selected filter in the host process` | `ifp-sha256:9fa365b85e06491f1ee0d31bd4e1603fd4be7c59acfe50b4cff670a7e852e650` | `kind:hard-invariant; strength:authoritative; evidence:user-requested no authority widening and receiving resolution filter-driver security boundary` |

## Blocker

None.

## Major

### F1 Major - Git filter guard has a check/use race

Impact: A concurrent same-user writer can change local or worktree Git config after the guard query and before `git worktree add`; Git may then execute the configured smudge/process filter outside the sandbox with host process authority.

Review reason: The new check establishes a point-in-time absence of `filter.*` config, but the security-sensitive consumer is a separate later process. There is no config lock, immutable snapshot, or other serialization across the two processes. The parent resolution explicitly fixed refusal only for stable pre-existing configuration and left this race for generation-1 review; this review does not treat it as settled safe behavior.

Surface: Managed worktree host mutation (`git worktree add`).

Issue key: `behavior; entry=managed worktree creation; contract=repository-controlled filter configuration cannot execute outside the sandbox during host checkout; effect=concurrent config replacement executes an attacker-selected filter in the host process`

Issue fingerprint: `ifp-sha256:9fa365b85e06491f1ee0d31bd4e1603fd4be7c59acfe50b4cff670a7e852e650`

Expected basis: `kind:hard-invariant; strength:authoritative; evidence:user-requested no authority widening and receiving resolution filter-driver security boundary`

Confidence: `high` that the race exists; exploitability depends on same-user concurrent repository-config mutation being within the threat model.

Origin: `Coordinator`

Coordinator verification: Re-read the guard and call sequence; it runs `git config --local` and `git config --worktree` queries in a loop, returns, then separately spawns `git worktree add`. The tests prove only the stable preconfigured-filter case.

Look here first:
- [`managed_worktree_create.rs`](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_create.rs#L252)
- [`managed_worktree_create.rs`](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_create.rs#L340)

Failure mode:
- Expected: No repository-controlled filter command executes in the host context during managed checkout.
- Current: A check/use interleaving can replace config after the query; `git worktree add` subsequently reads the new filter configuration and may run its smudge/process command.

Evidence:
- `reject_repository_filter_drivers` runs config queries for `--local` and `--worktree` and returns success at lines 252-300. `HostWorktreeCreator::create` invokes it at line 340, then constructs and starts the mutating Git command at lines 342-356. No lock or shared snapshot spans those invocations.
- The added regression `creation_refuses_repository_filter_drivers_before_checkout` configures `filter.hostile.smudge` before calling prepare and proves the stable case is rejected; it does not exercise concurrent config replacement.
- Git filter commands are repository-selected executable behavior; running one in this host-side invocation would cross the sandbox boundary.

Assumptions and limits:
- The race requires a process able to mutate `.git/config` concurrently. The generation-0 resolution specifically identifies concurrent same-user config mutation as un-serialized; whether that actor is in scope should not be silently assumed away.
- This finding does not claim the filter guard is ineffective against stable configuration. It blocks the tested stable case.

Reviewer action:
`request proof or a design that closes/acceptably constrains the check/use window before approval`

## Minor

None.

## Questions

None.

## Test Gaps

| ID | Severity | Surface | Missing coverage | Risk | Origin | Evidence | Issue key | Issue fingerprint | Expected basis |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `T1` | `Minor` | Managed `git worktree add` timeout/overflow and durable retry/reconciliation | Still no single test drives `HostWorktreeCreator` timeout, output overflow, or incomplete capture through durable attempt consumption, post-attempt observation, and retry/block classification. Current diff adds stable filter-refusal coverage, not this composition. | A regression could make an ambiguous mutation retryable while helper-level timeout tests and lifecycle-level retry tests pass separately. | `Coordinator` | Inherited from parent report and left Deferred by resolution; current additions in `src/managed_worktree_creation_tests.rs` cover preconfigured filter refusal only. | `test-gap; entry=managed worktree creation attempt; contract=ambiguous mutation attempts remain consumed and require reconciliation before retry; gap=managed creator timeout overflow and incomplete capture lack end-to-end durable lifecycle assertions` | `ifp-sha256:6547df29a5a3982782b9d23a38bf2a0df9777ea9c8ce28eaf7e14ba1707025ed` | `kind:approved-design; strength:authoritative; evidence:docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md §7 and §12; src/managed_worktree_prepare.rs:745-797` |

## Review Coverage Ledger

| Area ID | Area / path | Touched files or entry points | Owner | Depth | Status | Result | Evidence / next step |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `A1` | SIGCHLD normalization and unreaped-owner proof | `src/main.rs:128-137`; `src/process_group.rs:98-120`; ownership test | `Coordinator` | changed-code and startup-order trace | `Reviewed - no issue found` | Main resets disposition and clears `SA_NOCLDWAIT` before runtime construction; no competing child-reaper code path was found in the reviewed execution chain. | `inherited_sigchld_auto_reap_is_disabled_before_owned_children` passed; design §5 documents standalone signal-policy ownership. |
| `A2` | Managed Git filter refusal | `src/managed_worktree_create.rs:252-300,321-367`; filter test | `Coordinator` | authority/data-flow trace | `Finding F1` | Stable local/worktree filter configuration is refused, but query and mutating checkout are not serialized; see F1. | Focused stable-filter regression passed; concurrent config mutation is not tested and no atomic config snapshot is present. |
| `A3` | Host Git tool identity, PATH and environment | `src/execution.rs:621-670`; `src/sandbox.rs:1435-1479`; creator/observer | `Coordinator` | authority trace | `Reviewed - no issue found` | Git executable is resolved once from the host process PATH to a canonical absolute path. Creator and observer clear inherited env and use the shared narrow environment with Git system/global config disabled. No repository-supplied PATH argument or executable selection is present. | Assumption: launcher PATH is host-owned; resolver validates PATH entries but does not prove their directories are outside a repository. No repo-driven environment loader was found in this call path. |
| `A4` | Sandbox construction and fallback classification | `src/sandbox.rs:244-269,366-478,495-583`; fallback callers | `Coordinator` | affected-chain trace | `Reviewed - no issue found` | No sandbox construction or fallback authority change in the delta. Linux typed setup refusal remains terminal; macOS wrapper setup ambiguity remains unproven, not proof for unsandboxed fallback. Windows remains explicitly not an application filesystem/network sandbox. | Bubblewrap/Seatbelt runtime behavior remains platform evidence gap A7. |
| `A5` | UNKNOWN side-effect and retry semantics | `src/managed_worktree_create.rs:348-367`; `src/managed_worktree_prepare.rs:745-797`; `src/fallback.rs:853-865,1336-1372` | `Coordinator` | caller/state trace | `Reviewed - no issue found` | Incomplete query/capture and mutation timeout become errors; attempt persists before invocation; observation follows; only proven no-side-effect reconciliation loops; UNKNOWN locks fallback budget. | T1 records missing end-to-end timeout-to-durable-reconciliation coverage. |
| `A6` | Writer atomic publication and managed execution root | `src/execution.rs:53-75,87-121`; `src/writer.rs`; validated managed root caller | `Coordinator` | authority boundary trace | `Reviewed - no issue found` | No change to Writer helper resolution, validated parent writable root, or execution-root derivation in this delta. | Parent report's A8 disposition remains applicable; no new changed caller widened authority. |
| `A7` | Native Linux/macOS/Windows platform evidence | Bubblewrap/Seatbelt branches, Windows Job paths, platform runtime tests | `Coordinator` | static trace; partial macOS runtime | `Not covered` | Current-host focused tests cannot establish Linux Bubblewrap or Windows Job Object behavior; the focused macOS tests do not establish the full Seatbelt matrix. | Run complete branch CI on Linux, macOS, and Windows, including Windows nested/already-in-Job and direct termination failure cases. |

## Subagent Candidate Adjudication

No subagents were used. Coordinator candidates and inherited claims were adjudicated below.

| Candidate ID | Proposed by | Decision | Final ID | Coordinator evidence | Reason |
| --- | --- | --- | --- | --- | --- |
| `C1` | `Coordinator` | `accepted` | `F1` | `src/managed_worktree_create.rs:252-300,340-356`; parent resolution §Findings/Candidate Dispositions | Stable filter refusal works, but independent Git invocations leave the explicitly noted concurrent-config TOCTOU window. |
| `C2` | `Coordinator` | `dismissed` | `None` | `src/main.rs:128-137`; `src/process_group.rs:98-120`; SIGCHLD regression | Reset executes before Tokio construction; default disposition and cleared SA_NOCLDWAIT restore waitability. Parent's Fixed disposition remains closed. |
| `C3` | `Coordinator` | `dismissed` | `None` | `src/execution.rs:621-670`; `src/sandbox.rs:1435-1469`; creator invocation | Executable is selected from host startup PATH and canonicalized, then invoked by absolute path. Child env is cleared/rebuilt. No repository-controlled PATH input is passed through the reviewed application path; ambient launcher PATH trust remains an explicit assumption. |
| `C4` | `Coordinator` | `dismissed` | `None` | `src/managed_worktree_prepare.rs:745-797`; `src/fallback.rs:853-865,1336-1372` | No evidence UNKNOWN becomes ConfirmedNotPerformed or that retry budget is restored. Query errors are reconciled like all creator outcomes. |
| `C5` | `Coordinator` | `dismissed` | `None` | `src/sandbox.rs:366-478,495-583`; parent report A1/A8 | No sandbox construction/fallback or writer/execution-root changes in this delta. Windows no-sandbox limitation remains explicit. |
| `C6` | `Coordinator` | `accepted` | `T1` | Parent report T1; receiving resolution §Parent Test Gap Disposition; current diff test inventory | The requested timeout/overflow-to-durable-reconciliation composition test remains absent and was explicitly deferred. |
| `C7` | `Coordinator` | `deferred` | `A7` | Current host and targeted test outputs; parent resolution A9 and Windows deferral | Native Linux/Windows evidence cannot be produced on the macOS host; request platform CI rather than inferring success. |

## Evidence Appendix

### Diff Inventory

| File or area | Classification | Semantic review area considered |
| --- | --- | --- |
| `src/main.rs`, `src/process_group.rs` | surface | Early process signal normalization and process identity proof |
| `src/process_group_ownership_tests.rs` | test-only | Inherited SIG_IGN/SA_NOCLDWAIT normalization regression |
| `src/managed_worktree_create.rs` | surface | Filter guard, absolute host Git identity, clean environment, timeout result |
| `src/managed_worktree_creation_tests.rs` | test-only | Stable preconfigured filter refusal; no race or timeout composition test |
| `src/execution.rs`, `src/sandbox.rs` | dependency | Host Git resolver/environment, sandbox root and Writer authority |
| `src/managed_worktree_prepare.rs`, `src/managed_worktree_observe.rs`, `src/fallback.rs` | dependency | Reconciliation, UNKNOWN state, retry/budget contracts |
| `src/process_blocking.rs`, `src/platform_runtime_tests.rs` | surface/test-only | Bounded capture, cleanup, Windows and Unix evidence |
| `src/job_registry.rs`, `src/mcp.rs` | surface | Batch cancellation before awaiting drained jobs |
| `docs/PLATFORM_RUNTIME_CLOSURE_V1_DESIGN.md` | docs-only | Signal policy invariant and residual platform contracts |
| `src/writer.rs` | dependency | Unchanged Writer publication authority |

### Verification Commands

- `git --no-pager diff --stat` -> 11 paths, 548 additions / 116 deletions; diff SHA256 matches the receiving resolution target.
- `git --no-pager diff --check` -> clean.
- `cargo test --locked --all-targets inherited_sigchld_auto_reap_is_disabled_before_owned_children -- --nocapture` -> 1 matching test passed on macOS.
- `cargo test --locked --all-targets creation_refuses_repository_filter_drivers_before_checkout -- --nocapture` -> 1 matching test passed on macOS.
- `cargo test --locked --all-targets shutdown_requests_cancellation_for_every_drained_job_before_joining -- --nocapture` -> 5 matching tests passed (shared contract harnesses plus implementation) on macOS.
- Parent resolution reports full suite, managed-worktree suite, Clippy, and focused runtime test results after implementation; those are inherited evidence, not native Linux/Windows evidence.

### Supporting Code Links

| ID | Role | Link | Why it matters |
| --- | --- | --- | --- |
| `F1` | filter query | [`managed_worktree_create.rs`](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_create.rs#L252) | Query only observes config at one point in time. |
| `F1` | query-to-mutation gap | [`managed_worktree_create.rs`](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_create.rs#L340) | Separate checkout follows without a shared config lock/snapshot. |
| `A1` | startup policy | [`main.rs`](/Users/yuta/local-mcp-connector-parity/src/main.rs#L128) | Normalization is before Tokio runtime creation. |
| `A3` | host tool resolver | [`execution.rs`](/Users/yuta/local-mcp-connector-parity/src/execution.rs#L621) | Canonical absolute Git executable identity and host PATH source. |
| `A5` | durable mutation/reconcile | [`managed_worktree_prepare.rs`](/Users/yuta/local-mcp-connector-parity/src/managed_worktree_prepare.rs#L745) | Attempt consumption precedes mutation; observation always follows. |
| `A6` | Writer boundary | [`execution.rs`](/Users/yuta/local-mcp-connector-parity/src/execution.rs#L53) | Existing host helper remains sandboxed to validated parent. |

### Dismissed and Open Claims

| Claim | Disposition | Evidence |
| --- | --- | --- |
| Inherited SIGCHLD auto-reap still permits PGID reuse signaling | Dismissed; inherited parent `Fixed` | `main` calls normalizer before runtime; function sets SIG_DFL and flags 0; isolated test sets SIG_IGN/SA_NOCLDWAIT then verifies reset. |
| Stable configured filter drivers still execute | Dismissed for stable preflight state; inherited parent `Fixed` | New regression configures a smudge filter, verifies creation blocks, marker absent, and target absent. |
| Filter check is serialized with checkout | Not supported; candidate F1 | The query helper returns before the independent mutation command is constructed/spawned; receiving resolution expressly says no serialization. |
| Tool is chosen from repository or model PATH input | Dismissed with host-environment assumption | Resolver reads process PATH, validates entries and canonicalizes the selected executable; mutator passes that absolute path. App call path does not load repository env files or take PATH as a request parameter. |
| Git env forwards arbitrary inherited variables | Dismissed | `env_clear` then `clean_git_environment`; safe env includes host PATH/locale/temp/SystemRoot and explicit Git system/global/attribute isolation, not arbitrary `GIT_*` or loader variables. |
| UNKNOWN upgrades or retry budget restores after uncertain cleanup | Dismissed; inherited parent behavior retained | Creator timeout/capture failures are errors; prepare consumes durable attempt, observes after attempt, and only loops for explicitly retryable reconciliation. Fallback `Unknown` locks budget. |
| Writer/execution root authority widened | Dismissed | No changes to Writer or execution-root validation; `publish_workspace_write` still passes validated parent as sole writable root through sandbox path. |
| Windows assignment/termination/nested Job behavior is proven | Not supported; inherited parent deferred | Source ordering is statically suspended spawn -> assign -> resume; native runtime, nested/already-in-Job and direct termination failure behavior remain for Windows CI. |

### Blind Spots

| Area ID | Blind spot | Decision risk | What would resolve it |
| --- | --- | --- | --- |
| `A7` | Linux Bubblewrap, Windows Job Object assignment/termination/nesting, and full macOS Seatbelt runtime are not established by these macOS focused tests. | Static review cannot prove OS APIs, cfg-specific compile behavior, or kernel containment witnesses. | Run the complete branch CI matrix, including Windows nested/already-in-Job and injected direct termination failures. |
| `A3` | Resolver trusts the launched host process PATH; it validates absolute entries but does not prove a PATH directory is outside a repository. No repo-controlled environment source was found in the traced app path. | If an external launcher intentionally derives PATH from untrusted repository state, Git identity could originate from that environment. | Confirm launcher/environment threat model or test resolver against repository-local PATH entries if those are considered untrusted. |
| `F1` | Practical exploitation requires concurrent same-user Git config mutation; the source does not serialize it. | Host-side filter command could cross the sandbox if concurrent config mutation is in the threat model. | Close or explicitly accept the TOCTOU under an authoritative threat-model decision; prove any proposed containment in tests. |

## Prior Resolution Reconciliation

| Issue key | Issue fingerprint | Parent item/verdict | Relevant change or new evidence | Decision |
| --- | --- | --- | --- | --- |
| `test-gap; entry=managed worktree creation attempt; contract=ambiguous mutation attempts remain consumed and require reconciliation before retry; gap=managed creator timeout overflow and incomplete capture lack end-to-end durable lifecycle assertions` | `ifp-sha256:6547df29a5a3982782b9d23a38bf2a0df9777ea9c8ce28eaf7e14ba1707025ed` | `T1 Deferred` | `kind:code; ref:src/managed_worktree_creation_tests.rs current diff; change:added stable filter rejection but still no injected timeout/overflow through durable prepare/reconciliation` | `carried forward as T1` |
| `Cross-platform runtime evidence` | `None` | `A9 Not covered; Windows behavior deferred` | `kind:evidence; ref:local focused tests and current host; change:SIGCHLD/filter/shutdown tests pass only on macOS, no Linux or Windows runtime` | `kept open as A7` |
| `behavior; entry=managed worktree creation; contract=repository-controlled filter configuration cannot execute outside the sandbox during host checkout; effect=concurrent config replacement executes an attacker-selected filter in the host process` | `ifp-sha256:9fa365b85e06491f1ee0d31bd4e1603fd4be7c59acfe50b4cff670a7e852e650` | `Receiving resolution candidate: fixed for stable pre-existing config; concurrent mutation caveat explicitly left for generation 1` | `kind:code; ref:src/managed_worktree_create.rs:252-300,340-356; change:review confirms query and checkout are distinct un-serialized Git processes` | `reopened as F1` |

No other parent finding fingerprints exist; generation-0 had no code findings. Parent SIGCHLD/overflow/reader/task-cancellation dispositions remain closed; no relevant contrary code or evidence was found.

## Receiving Handoff

- Handoff status: `Terminal post-review - return to user/owner`
- Automatic receiving permitted: `No`
- Source report ID: `cr-20261007-6bdc41`
- Scope fingerprint to recheck: `sha256:418667d6be2adaafa427b65377ddadc372f98d6ead31aeb2fb00fb0f0c6da2e6`
- Actionable finding IDs: `F1`
- Deferred finding IDs: `None`
- Actionable test-gap IDs: `None`
- Deferred test-gap IDs: `T1`
- Open question IDs: `None`
- Open coverage area IDs: `A7`
- Highest-risk verification to repeat: Resolve F1 against the same-user concurrent-config threat model and run complete Linux/Windows/macOS platform CI.
- Suggested implementation boundaries: Keep resolution limited to host Git filter safety and its test seam; do not broaden Writer roots, sandbox permissions, Git argv, or retry semantics.
- Re-review note: `Treat every finding as a claim to verify. Challenges require a counterclaim, argument, evidence, limits, and settlement criterion.`
- Chain rule: `Generation 1 is terminal. Do not automatically invoke receiving-code-review; return remaining findings to the user or product owner.`

## Report Self-Check

- `yes` Complete parent resolution and parent report were read before substantive review.
- `yes` Scope is implementation delta plus affected execution/authority chains; generation 1 is terminal.
- `yes` Every changed review-relevant area is in the coverage ledger.
- `yes` F1 has authoritative hard-invariant basis, semantic issue key, matching fingerprint, and exactly two primary code links.
- `yes` Inherited T1 issue identity/fingerprint is preserved; deferred test gap appears once in handoff.
- `yes` Parent `Fixed`, `Disproved`, and `Deferred` dispositions were reconciled; SIGCHLD was not reopened, stable-filter claim remains closed, and only the documented race caveat is reopened.
- `yes` Windows/Linux platform evidence and Windows Job caveats remain explicit.
- `yes` Recommendation is `Changes requested` because F1 is Major.
- `pending` Generation-1 report validator to be run with parent report and resolution.
- `yes` No source edits or Git mutations were made; this report is the review artifact.
