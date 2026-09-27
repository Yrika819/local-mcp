# Local MCP Goal / Task Orchestrator V1 — Formal Design

Status: **DESIGN ONLY — NO PRODUCTION ORCHESTRATOR IMPLEMENTATION**

Repository inspected: `repository root`

Observed baseline at design time:

- branch: `main`
- local HEAD: `737b756` (`Harden Codex fallback policy`)
- `origin/main`: `21025d0`
- local branch ahead of `origin/main` by two commits
- existing production modules: `main.rs`, `approvals.rs`, `config.rs`, `fallback.rs`, `mcp.rs`, `sandbox.rs`
- existing MCP protocol response: `2025-06-18`
- bundled Codex CLI observed: `codex-cli 0.154.0-alpha.6.2`

This document defines the first production-ready design for a durable Goal / Task Orchestrator. It intentionally does not implement the Orchestrator and does not change the authority of the existing execution, fallback, sandbox, approval, job, or session layers.

---

## 1. Design goals and non-negotiable constraints

The Orchestrator is a higher-level control layer. It does not replace the current Local MCP execution path.

Target flow:

```text
ChatGPT
  ↓
Local MCP MCP adapter
  ↓
Goal Manager
  ↓
Planner
  ↓
Task DAG
  ↓
Worker / Local operation
  ↓
EXISTING Local MCP execution authority
  ↓
EXISTING fallback policy
  ↓
Verifier
  ↓
Replanner when required
  ↓
Goal completion gate
```

The following constraints are normative for V1:

1. `fallback.rs` remains the authority for safe recovery of an individual low-level operation. It is not repurposed as a Goal state machine.
2. A Goal does not grant filesystem, network, host-native, Git, publication, or fallback authority.
3. Every effectful low-level operation is routed through the same Local MCP authority used by existing tools.
4. Existing approval, sandbox, side-effect, fallback, and budget rules remain authoritative and may be stricter than the Orchestrator's desired action.
5. Worker output is advisory evidence, not proof of completion.
6. Task and Goal completion are mechanical host decisions.
7. Goal state is durable. Existing in-memory `Job` state is not Goal authority.
8. V1 permits multiple read-only investigators but exactly one production writer at a time.
9. V1 Codex processes are read-only. A `CODEX_WRITER` is a writer *role* that proposes exact changes; Local MCP applies the accepted changes through its existing authority.
10. A process crash never implies success.

---

## 2. Current architecture

### 2.1 Process modes

`src/main.rs` exposes two process modes:

```text
local-mcp start [session_id]
local-mcp mcp
```

`start` owns the per-session approval/activity UI. `mcp` owns the JSON-RPC MCP server.

### 2.2 Persistent session state

`src/config.rs` persists a `Session` containing:

```rust
Session {
    id,
    cwd,
    permitted_directories,
}
```

under `config::state_dir()/sessions/<session-id>.json`.

Current session persistence uses a same-directory temporary file followed by rename. V1 Goal persistence follows the same philosophy but strengthens durability with explicit synchronization before the rename.

### 2.3 Approval authority

`src/approvals.rs` owns:

- per-session local IPC;
- approval requests;
- activity messages;
- volatile session `yolo` mode;
- persistent sandbox-root allow/revoke changes.

`yolo` is deliberately process/session-lifetime state. Goal persistence MUST NOT persist or recreate `yolo` authority.

### 2.4 MCP adapter

`src/mcp.rs` currently:

- speaks JSON-RPC over stdin/stdout;
- returns MCP protocol version `2025-06-18` from `initialize`;
- exposes the existing tool catalog;
- resolves `session_id` and session-relative paths;
- owns the current in-memory `Job` map;
- starts sandboxed and host-native commands;
- invokes fallback classification and fallback execution;
- invokes explicit Codex fallback;
- emits activity records.

### 2.5 Current in-memory jobs

The current job registry is:

```rust
static JOBS: OnceLock<Mutex<HashMap<Uuid, Job>>>
```

It is suitable for legacy `execute` / `start_command` lifecycle behavior, but it is not durable and MUST NOT become Goal authority.

### 2.6 Sandbox authority

`src/sandbox.rs` owns command process creation and execution:

- Linux: Codex Landlock/Linux sandbox path;
- macOS: Seatbelt;
- Windows: host-native execution with documented limitations;
- restricted environment for sandboxed commands;
- network denial for ordinary Linux/macOS sandboxed execution;
- lifecycle evidence indicating whether process spawn/completion was observed.

### 2.7 Fallback authority

`src/fallback.rs` already contains a mature per-operation policy state machine around:

- `FailureClass`;
- `SideEffectClass`;
- `SideEffectState`;
- `FallbackAction`;
- `OperationContract`;
- `Budget`;
- `LifecycleEvidence`;
- `FallbackDecision`.

The Orchestrator consumes this result. It does not duplicate, reinterpret, or loosen it.

### 2.8 Current Codex integration

The existing fallback path can locate the bundled Codex executable and invokes `codex exec` in read-only mode. Current fallback deliberately uses `--ephemeral` and does not rely on persistent Codex session state.

The bundled CLI observed during this design exposes:

- `codex exec`;
- `codex exec resume`;
- `codex exec fork`;
- `codex exec review`;
- `--output-schema`;
- `--json`;
- `--output-last-message`;
- `--sandbox`;
- `--worktree`;
- `--model`.

Because the observed binary is an alpha build, V1 MUST NOT treat every currently visible flag as a permanent API.

---

## 3. Freeze existing behavior before Orchestrator implementation

The first implementation phase MUST add regression coverage before production Orchestrator behavior is introduced.

The required rule is:

> Adding Goal tools may be additive, but existing Local MCP tool semantics, authority, timeout behavior, sandboxing, approvals, fallback decisions, and session/job behavior must not change unless a later explicit migration phase says so.

### 3.1 Legacy MCP surface contract

The following tools must retain their existing schemas and semantics:

- `session_info`
- `read_file`
- `list_directory`
- `write_file`
- `execute`
- `start_command`
- `poll_job`
- `stop_job`
- `codex_fallback`
- `without_sandbox`

The test suite should retain a golden/snapshot representation of each legacy tool schema. The six Goal tools are allowed only as additive entries to `tools/list`.

`initialize` MUST continue returning `2025-06-18` during Orchestrator V1. Native Tasks protocol migration is not part of V1.

### 3.2 `session_info` regression contract

Verify that:

- a valid session loads by `session_id`;
- output retains `id`, `cwd`, and `permitted_directories`;
- an unknown session retains the current error behavior;
- Goal creation does not rewrite the session JSON merely to attach Goal state.

### 3.3 `read_file` regression contract

Verify that:

- relative paths resolve from the session cwd;
- absolute paths keep current behavior;
- UTF-8 content is returned unchanged;
- read errors keep current error propagation;
- no approval request is introduced.

### 3.4 `list_directory` regression contract

Verify that:

- relative paths resolve from the session cwd;
- entries remain lexicographically sorted;
- directories keep the `/` suffix;
- output remains newline-separated;
- no approval request is introduced.

### 3.5 `write_file` regression contract

Verify current platform behavior without broadening it:

- Unix/macOS/Linux writes continue through the current Local MCP sandbox path;
- Windows retains its documented host-native behavior;
- existing file content is replaced exactly;
- activity diff/count generation remains unchanged;
- the Orchestrator does not install a parallel file-writing implementation.

The Orchestrator may impose *stricter* path scope on planner-generated work, but it must not silently change the legacy `write_file` tool contract in this phase.

### 3.6 `execute` regression contract

Verify that:

- normal Linux/macOS execution remains sandboxed;
- Windows retains the current host-native + approval contract;
- default accepted exit code remains `0`;
- Policy V2 metadata remains additive;
- success preserves exit/stdout/stderr/failure-class payload semantics;
- semantic failure does not become an executable fallback opportunity;
- commands completing inside the foreground window return directly;
- commands exceeding the foreground window retain current background-job behavior;
- Orchestrator task retries do not reset a low-level operation's remaining fallback/side-effect budget.

### 3.7 `start_command`, `poll_job`, `stop_job` regression contract

Verify that:

- `start_command` immediately returns an in-memory legacy `job_id`;
- jobs remain bound to a session;
- a different session cannot poll/stop a job;
- `poll_job` retains `running` then terminal-result behavior;
- `stop_job` aborts the current legacy handle and removes it;
- MCP restart still loses legacy in-memory job authority exactly as today;
- Goal durability does not implicitly make legacy jobs durable.

### 3.8 `codex_fallback` regression contract

Preserve the current Policy V2 contract, including:

- `DIAGNOSE_ONLY` read-only Codex behavior;
- executable fallback only for `SANDBOX_PERMISSION` with required lifecycle proof;
- explicit operation authorization;
- exact operation scope;
- no fallback recursion;
- fallback budget enforcement;
- terminal platform/safety classification;
- no automatic retry of ambiguous remote mutations;
- independent postcondition verification.

Existing fallback unit cases must remain green. Orchestrator tests must call the fallback authority rather than reproduce its logic.

### 3.9 `without_sandbox` regression contract

Verify that:

- every call still passes through `approvals::request` unless existing session `yolo` applies;
- the Goal layer cannot set `yolo`;
- host-native/network-capable execution retains the existing foreground/background behavior;
- a persisted Goal cannot replay an old approval as new authority.

### 3.10 Approval regression contract

Verify:

- `ask` remains default on session start;
- `yolo` remains volatile and is not persisted in Goal state;
- pending approval ordering remains unchanged;
- deny remains terminal for that invocation;
- sandbox-root allow/revoke persistence remains in Session state;
- the session cwd cannot be revoked;
- closing the approval UI remains a failure to obtain approval, not implicit approval.

### 3.11 Sandbox regression contract

Retain platform tests proving at minimum:

- workspace writes allowed under the current sandbox rules;
- writes outside permitted sandbox scope denied on Unix platforms;
- network denied for ordinary Linux/macOS sandboxed commands;
- host-native execution is not mislabeled as a sandbox execution;
- Windows limitations remain documented and tested separately.

### 3.12 Fallback regression contract

The existing fallback classification/decision matrix is a hard compatibility gate. Tests must continue covering:

- expected nonzero state -> no fallback;
- exact authorized sandbox-permission case -> bounded fallback;
- host-native permission failure != sandbox permission;
- required lifecycle evidence;
- platform/safety terminal block;
- semantic test/compile failure -> no executable fallback;
- remote ambiguous/performed -> no retry;
- missing tool -> diagnose only;
- unknown -> diagnose only;
- max depth;
- scope drift;
- exhausted budget;
- unauthorized Git index drift;
- unsafe Git pathspec rejection.

### 3.13 Session persistence regression contract

Verify:

- existing session path remains `state_dir()/sessions/<id>.json`;
- stable custom session IDs retain current validation;
- `cwd` and `permitted_directories` survive process restart;
- Goal files are stored separately and cannot make a session JSON unreadable.

### 3.14 Recommended regression test layout

Production implementation should add/organize tests approximately as:

```text
src/mcp.rs               legacy protocol/tool schema unit tests
src/config.rs            session persistence tests
src/approvals.rs         approval state-machine tests
src/sandbox.rs           platform sandbox tests
src/fallback.rs          Policy V2 matrix (existing + frozen additions)
tests/legacy_contract.rs black-box MCP request/response contract where practical
tests/job_contract.rs    legacy job lifecycle
tests/orchestrator_*.rs  new V1-only behavior
```

A production Orchestrator change MUST NOT be merged unless both legacy-contract and Orchestrator suites pass.

---

## 4. Proposed architecture

### 4.1 High-level ownership

```text
mcp.rs
  │
  ├─ legacy tools ────────────────┐
  │                               │
  └─ goal_* tools                 │
          ↓                       │
     orchestrator.rs              │
          ↓                       │
   goal.rs / task.rs              │
      ↓          ↓                │
 planner.rs   verifier.rs         │
      ↓          ↑                │
    agent.rs     │                │
      ↓          │                │
      └──── execution authority ──┘
                    ↓
              fallback.rs
                    ↓
              sandbox.rs
                    ↓
              approvals.rs
```

### 4.2 Recommended module layout

```text
src/main.rs
src/mcp.rs
src/config.rs
src/approvals.rs
src/sandbox.rs
src/fallback.rs

# New V1 control-plane modules
src/orchestrator.rs
src/goal.rs
src/task.rs
src/task_store.rs
src/agent.rs
src/planner.rs
src/verifier.rs

# Recommended behavior-preserving extraction during implementation
src/execution.rs
```

`execution.rs` is recommended even though it was not in the initial expected list. The reason is authority sharing: the Orchestrator must not copy the private execution logic currently embedded in `mcp.rs`.

Implementation should move the existing low-level execution primitives behind an internal `ExecutionAuthority` facade while preserving the existing MCP wrappers and response behavior. This is a refactor, not a semantic change.

### 4.3 Module responsibilities

#### `orchestrator.rs`

Owns:

- Goal lifecycle;
- scheduler ticks;
- transition validation;
- concurrency/resource gates;
- recovery orchestration;
- pause/cancel semantics;
- final completion gate;
- interaction among planner, worker, verifier, and store.

It does **not** own raw process execution or fallback classification.

#### `goal.rs`

Owns:

- `Goal`;
- `GoalStatus`;
- `GoalCheckpoint`;
- Goal-level invariants;
- final result shape;
- active-goal uniqueness rules.

#### `task.rs`

Owns:

- `Task`;
- `TaskId`;
- `TaskDependency`;
- `TaskStatus`;
- `WorkerKind`;
- `TaskAttempt`;
- `TaskEvidence`;
- task transition table;
- DAG validation primitives.

#### `task_store.rs`

Owns:

- path construction under `config::state_dir()`;
- schema/version loading;
- cross-process/session lock;
- revision checks;
- atomic write/replace;
- fsync policy;
- corrupt-file behavior;
- migration dispatch;
- stale temporary cleanup.

It does not schedule work.

#### `agent.rs`

Owns:

- Codex executable discovery for Orchestrator workers;
- feature/capability detection;
- construction of read-only Codex invocation;
- structured worker request/response schema;
- strict parsing/validation of worker output;
- optional thread/session metadata.

It does not apply code changes.

#### `planner.rs`

Owns:

- initial plan contract;
- replan contract;
- validation of suggested tasks into a candidate DAG mutation;
- conversion from planner output to host-owned Task IDs and contracts.

Planner output does not become state until the host validates it.

#### `verifier.rs`

Owns:

- mechanical `VerificationSpec` evaluation;
- file/Git/process evidence gathering;
- no-forbidden-change checks;
- task verification result;
- Goal final invariant verification;
- recovery reconciliation checks.

It does not trust worker-declared completion.

#### `execution.rs` (recommended extraction)

Owns the behavior-preserving internal service that both legacy MCP tool handlers and Orchestrator operations call.

It should encapsulate the current low-level authority around:

- sandboxed command execution;
- approved host-native command execution;
- file writing;
- execution/fallback payload;
- fallback policy invocation;
- activity emission hooks where appropriate.

Legacy `execute`, `start_command`, `without_sandbox`, and `codex_fallback` wrappers retain their current externally visible behavior.

---

## 5. V1 data model

The following Rust shapes are design-level contracts. Exact field names may change during implementation only if the same invariants remain explicit and testable.

### 5.1 IDs and timestamps

Use server-generated UUIDv4 IDs for Goal, authoritative completion criterion, Task, Attempt, Verification, and Checkpoint identities.

Use an RFC 3339 UTC representation for durable timestamps. Time is evidence/ordering metadata, not a security authority.

```rust
pub struct GoalId(pub Uuid);
pub struct CompletionCriterionId(pub Uuid);
pub struct TaskId(pub Uuid);
pub struct AttemptId(pub Uuid);
pub struct VerificationId(pub Uuid);
```

### 5.2 `Goal`

```rust
pub struct Goal {
    pub schema_version: u32,          // structured final-verification schema == 2
    pub revision: u64,                // increments on every committed mutation
    pub id: GoalId,
    pub session_id: String,
    pub cwd: PathBuf,                 // snapshot of session cwd at creation
    pub objective: String,
    pub title: Option<String>,
    pub constraints: Vec<String>,
    pub completion_criteria: Vec<GoalCompletionCriterion>,
    pub final_verification_spec: Option<GoalFinalVerificationSpec>,
    pub status: GoalStatus,
    pub plan_revision: u32,
    pub tasks: BTreeMap<TaskId, Task>,
    pub blockers: Vec<GoalBlocker>,
    pub final_verifications: Vec<GoalFinalVerificationRecord>,
    pub checkpoints: Vec<GoalCheckpoint>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}
```

`completion_criteria` no longer means authoritative natural-language predicates. Each entry has a host-assigned stable identity and human description; `final_verification_spec` contains the mechanical proof contract. `final_verifications` is append-only history.

V1 allows at most **one non-terminal Goal per Local MCP session**. Historical terminal Goals may coexist.

This rule intentionally makes “resume the current goal” mechanically unambiguous without adding a separate mutable current-goal pointer.

### 5.2.1 Goal completion criteria and final-verification contract

The structured Goal completion model is:

```rust
pub struct GoalCompletionCriterion {
    pub id: CompletionCriterionId,
    pub description: String,
    pub required: bool,
}

pub struct GoalFinalVerificationSpec {
    pub plan_revision: u32,
    pub criterion_bindings: Vec<GoalCriterionBinding>,
}

pub struct GoalCriterionBinding {
    pub criterion_id: CompletionCriterionId,
    pub requirements: Vec<GoalVerificationRequirement>,
}

pub enum GoalVerificationRequirement {
    TaskVerified { task_id: TaskId },
}
```

V1 rules are intentionally narrow:

- `CompletionCriterionId` is host-assigned UUIDv4 authority, unique within the Goal, durable, independent of array position, and never derived from mutable prose.
- `description` is explanatory only. The host MUST NOT parse it to decide success; in V1 it is immutable after `goal_start` so prose edits cannot masquerade as contract changes.
- every V1 authoritative criterion is `required == true`; optional completion criteria are not supported in V1. Informational text belongs outside the authoritative completion set.
- `GoalVerificationRequirement` has exactly one V1 authority-bearing family: `TaskVerified { task_id }`. Adding another family requires a bounded deterministic design amendment; arbitrary predicates are forbidden.
- one criterion may reference multiple Tasks and one Task may satisfy multiple criteria. Requirements within one criterion are **AND**. All required criteria at Goal scope are also **AND**. V1 has no implicit OR or arbitrary nested boolean tree.
- a bound Task MUST exist, be mandatory, have at least one host-valid `VerificationSpec`, and retain its stable `TaskId`. Replanner never replaces Task identity and pretends the old proof survived.
- `final_verification_spec == None` is permitted only before initial plan materialization or while an explicit legacy authority upgrade is incomplete. Every executable `RUNNING` or later schema-2 Goal must have a complete spec.
- `final_verification_spec.plan_revision` MUST equal current `Goal.plan_revision`. Any contract strengthening increments `plan_revision`.

The universal mandatory-plan gate is separate from these criterion bindings. `all mandatory Tasks COMPLETED` is a global finalization invariant, not a synthetic user criterion and not a substitute for criterion coverage.

### 5.3 `GoalStatus`

```rust
pub enum GoalStatus {
    Planning,
    Running,
    Replanning,
    Pausing,
    Paused,
    Blocked,
    Verifying,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}
```

Terminal states are `Completed`, `Failed`, and `Cancelled`.

### 5.4 `Task`

```rust
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub objective: String,
    pub mandatory: bool,
    pub status: TaskStatus,
    pub dependencies: Vec<TaskDependency>,
    pub worker: WorkerKind,
    pub scope: TaskScope,
    pub verification: Vec<VerificationSpec>,
    pub max_attempts: u32,
    pub attempts: Vec<TaskAttempt>,
    pub evidence: Vec<TaskEvidence>,
    pub blockers: Vec<TaskBlocker>,
    pub created_plan_revision: u32,
    pub updated_at: String,
}
```

### 5.5 `TaskDependency`

```rust
pub struct TaskDependency {
    pub task_id: TaskId,
    pub condition: TaskDependencyCondition,
}

pub enum TaskDependencyCondition {
    Completed,
}
```

V1 supports only a hard “dependency must be `COMPLETED`” condition. The enum shape reserves room for future conditions without adding them now.

### 5.6 `TaskStatus`

V1 task states are exactly the required core set:

```rust
pub enum TaskStatus {
    Pending,
    Ready,
    Running,
    Blocked,
    Retryable,
    NeedsReplan,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}
```

`Completed`, `Failed`, and `Cancelled` are terminal.

### 5.7 `WorkerKind`

```rust
pub enum WorkerKind {
    LocalOperation,
    CodexReadonly,
    CodexWriter,
    CodexReviewer,
    Verifier,
}
```

Normative V1 interpretation:

- `LOCAL_OPERATION`: exact host-owned operation executed through existing Local MCP authority.
- `CODEX_READONLY`: repository investigation/synthesis in read-only Codex mode.
- `CODEX_WRITER`: Codex proposes a machine-readable mutation plan, but the Codex process remains read-only.
- `CODEX_REVIEWER`: read-only independent review.
- `VERIFIER`: host-owned mechanical verification and, where needed, low-level commands routed through existing authority.

### 5.8 `TaskScope`

```rust
pub struct TaskScope {
    pub allowed_paths: Vec<PathBuf>,
    pub forbidden_paths: Vec<PathBuf>,
    pub operation_kind: TaskOperationKind,
    pub replay_safety: ReplaySafety,
}

pub enum TaskOperationKind {
    ReadOnly,
    LocalMutation,
    HostNativeApproved,
}

pub enum ReplaySafety {
    SafeReadOnly,
    VerifyBeforeRetry,
    NeverAutomatic,
}
```

Planner-generated paths must be canonicalizable within the session cwd or permitted directories. V1 Goal scope may be stricter than the legacy tool surface; it may never be broader.

### 5.9 `TaskAttempt`

```rust
pub struct TaskAttempt {
    pub id: AttemptId,
    pub number: u32,
    pub worker: WorkerKind,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub worker_process_ref: Option<String>, // advisory only, never completion authority
    pub codex_thread_id: Option<String>,    // optional enhancement, never required for recovery
    pub low_level_request_ids: Vec<String>,
    pub side_effect_class: Option<SideEffectClass>,
    pub side_effect_state: Option<SideEffectState>,
    pub remaining_attempt_budget: Option<u32>,
    pub remaining_side_effect_budget: Option<u32>,
    pub worker_report: Option<WorkerReport>,
    pub outcome: Option<AttemptOutcome>,
}
```

A new high-level Task attempt MUST NOT replenish a low-level operation's fallback/side-effect budget. For the same side-effectful intent, the Orchestrator persists and reuses the stable operation identity and remaining budget returned by existing execution authority.

### 5.10 `TaskEvidence`

```rust
pub enum TaskEvidence {
    WorkerReport {
        attempt_id: AttemptId,
        report_digest: String,
    },
    CommandResult {
        request_id: String,
        exit_code: Option<i32>,
        stdout_digest: Option<String>,
        stderr_digest: Option<String>,
    },
    FileSnapshot {
        path: PathBuf,
        exists: bool,
        size: Option<u64>,
        sha256: Option<String>,
    },
    GitSnapshot {
        head: Option<String>,
        changed_paths: Vec<PathBuf>,
        staged_paths: Vec<PathBuf>,
    },
    ReviewResult {
        summary: String,
        blocking_findings: u32,
    },
    RecoveryReconciliation {
        summary: String,
        side_effect_state: SideEffectState,
    },
    Verification {
        verification_id: VerificationId,
        passed: bool,
    },
}
```

Large command output should not be copied without bound into Goal JSON. Persist bounded summaries plus digests and the existing low-level request IDs needed to correlate activity.

### 5.11 `VerificationResult`

```rust
pub struct VerificationResult {
    pub id: VerificationId,
    pub outcome: VerificationOutcome,
    pub checks: Vec<VerificationCheckResult>,
    pub started_at: String,
    pub finished_at: String,
}

pub enum VerificationOutcome {
    Passed,
    Failed,
    Indeterminate,
}
```

`Indeterminate` never satisfies completion.

### 5.12 Goal final-verification record

Goal-level final verification reuses the existing `VerificationId`, `VerificationOutcome`, and timestamp semantics, but does **not** reuse Task-specific `VerificationCheckResult` indices. It has a Goal-specific durable record with provenance and criterion bindings:

```rust
pub enum VerificationOrigin {
    HostDeterministicGoalVerifier,
}

pub struct GoalFinalVerificationRecord {
    pub id: VerificationId,
    pub outcome: VerificationOutcome,
    pub source: VerificationOrigin,
    pub goal_id: GoalId,
    pub evaluated_goal_revision: u64,
    pub committed_goal_revision: u64,
    pub plan_revision: u32,
    pub contract_digest: String,
    pub criterion_results: Vec<GoalCriterionVerificationResult>,
    pub started_at: String,
    pub finished_at: String,
}

pub struct GoalCriterionVerificationResult {
    pub criterion_id: CompletionCriterionId,
    pub outcome: VerificationOutcome,
    pub observations: Vec<GoalRequirementObservation>,
}
```

`GoalRequirementObservation` is bounded structured evidence. For V1 `TaskVerified`, it records at minimum requirement kind, `TaskId`, observed `TaskStatus`, authoritative `VerificationId` when present, observed `VerificationOutcome` when present, and the resulting requirement outcome. It does not copy whole files, command streams, or Task histories.

The canonical `contract_digest` is computed from a deterministic serialization of the current structured contract, ordered by `CompletionCriterionId` and then requirement kind/`TaskId`. Evaluation and evidence emission use the same deterministic ordering and never depend on `HashMap` iteration order.

A record is authoritative only when `source == HOST_DETERMINISTIC_GOAL_VERIFIER`. Planner, Replanner, worker, reviewer, scheduler, runner, and MCP client output cannot author this marker.

`final_verifications` is append-only. Re-evaluation appends a new record. The old `Option<VerificationResult>` latest-only model is not sufficient because it cannot preserve stale-but-auditable results across plan revisions.

### 5.13 `GoalCheckpoint`


```rust
pub struct GoalCheckpoint {
    pub id: Uuid,
    pub goal_revision: u64,
    pub plan_revision: u32,
    pub at: String,
    pub reason: CheckpointReason,
    pub goal_status: GoalStatus,
    pub active_task_ids: Vec<TaskId>,
}
```

A checkpoint is compact audit/recovery metadata. The complete current Goal snapshot remains the authority.

---

## 6. Goal state machine

All transitions are performed by a single host transition function. Direct assignment to `Goal.status` outside that function is prohibited by design.

### 6.1 Allowed transitions

| From | Allowed next states |
|---|---|
| `PLANNING` | `RUNNING`, `BLOCKED`, `CANCELLING`, `FAILED` |
| `RUNNING` | `REPLANNING`, `PAUSING`, `BLOCKED`, `VERIFYING`, `CANCELLING`, `FAILED` |
| `REPLANNING` | `RUNNING`, `BLOCKED`, `PAUSING`, `CANCELLING`, `FAILED` |
| `PAUSING` | `PAUSED`, `BLOCKED`, `CANCELLING` |
| `PAUSED` | `RUNNING`, `REPLANNING`, `BLOCKED`, `CANCELLING` |
| `BLOCKED` | `RUNNING`, `REPLANNING`, `PAUSED`, `CANCELLING`, `FAILED` |
| `VERIFYING` | `COMPLETED`, `REPLANNING`, `BLOCKED`, `PAUSING`, `CANCELLING`, `FAILED` |
| `CANCELLING` | `CANCELLED`, `BLOCKED` |
| `COMPLETED` | none |
| `FAILED` | none |
| `CANCELLED` | none |

### 6.2 Goal transition preconditions

`PLANNING -> RUNNING` requires:

- a valid persisted DAG;
- no duplicate Task IDs;
- no cycle;
- every dependency target exists;
- at least one mandatory Task, unless the Goal is explicitly modeled as verification-only;
- no authority expansion in any Task scope;
- every required `CompletionCriterionId` has exactly one durable binding entry with at least one host-valid structured requirement;
- every bound `TaskId` resolves to an existing mandatory Task with host-evaluable Task verification;
- `final_verification_spec.plan_revision == Goal.plan_revision`.

An unmapped required criterion makes plan materialization invalid and the Goal MUST remain `PLANNING` (or become `BLOCKED` on an explicit planning failure). It can never be ignored and later treated as satisfied.

`RUNNING -> VERIFYING` is not a generic transition anymore. It requires the sealed Goal Verifier entry capability and all of:

- every mandatory Task is already `COMPLETED`;
- the latest authoritative verification for every mandatory Task is `PASSED`;
- no Task is `RUNNING` or `VERIFYING`;
- no unresolved mandatory blocker;
- no `NEEDS_REPLAN` Task;
- no unknown side-effect state requiring reconciliation;
- the DAG and single-writer invariants validate;
- the structured final-verification contract is complete and bound to the current `plan_revision`;
- all criterion Task references still resolve to the same durable Task identities.

Only the host-owned Goal Verifier may exercise this capability. Scheduler selection, Runner control flow, worker output, or Task count cannot transition the Goal directly.

After Goal Verifier evaluation, the authoritative state mapping is:

| Final verification outcome | State after Goal Verifier commit | Recovery/next authority |
|---|---|---|
| `PASSED` | `VERIFYING` | Phase 9 Goal Finalizer may recheck all gates and, separately, perform `VERIFYING -> COMPLETED`. |
| `FAILED` | `FAILED` | Terminal in V1. A false required monotonic criterion cannot be removed or weakened by Replanner. |
| `INDETERMINATE` | `BLOCKED` | Host reconciliation via `goal_resume`; after the cause is repaired, transition to `RUNNING` for reevaluation or `REPLANNING` if monotonic plan/contract strengthening is required. |

`VERIFYING -> COMPLETED` remains exclusively Phase 9 Goal Finalizer authority and requires all Goal completion gates in section 13.

`CANCELLING -> CANCELLED` requires:

- no worker is still known active;
- no mutating attempt remains in an unresolved `UNKNOWN` side-effect state;
- any required recovery reconciliation is complete.

A cancel request is not permission to lie about ambiguous side effects. An ambiguous Goal may remain `BLOCKED` instead of becoming `CANCELLED` until state is reconciled.

---

## 7. Task state machine

All Task transitions are likewise centralized and mechanically validated.

### 7.1 Allowed transitions

| From | Allowed next states |
|---|---|
| `PENDING` | `READY`, `CANCELLED` |
| `READY` | `RUNNING`, `CANCELLED` |
| `RUNNING` | `VERIFYING`, `RETRYABLE`, `BLOCKED`, `NEEDS_REPLAN`, `FAILED`, `CANCELLED`* |
| `VERIFYING` | `COMPLETED`, `RETRYABLE`, `BLOCKED`, `NEEDS_REPLAN`, `FAILED`, `CANCELLED`* |
| `RETRYABLE` | `READY`, `FAILED`, `CANCELLED` |
| `BLOCKED` | `READY`, `NEEDS_REPLAN`, `FAILED`, `CANCELLED` |
| `NEEDS_REPLAN` | `PENDING`, `BLOCKED`, `FAILED`, `CANCELLED` |
| `COMPLETED` | none |
| `FAILED` | none |
| `CANCELLED` | none |

`* RUNNING/VERIFYING -> CANCELLED` is allowed only after the active worker is stopped/finished and the host proves that cancellation does not hide an unresolved side effect. Otherwise the Task transitions to `BLOCKED` for reconciliation.

### 7.2 `PENDING -> READY`

Allowed only when every hard dependency is `COMPLETED`.

The scheduler recomputes readiness from durable DAG state. A persisted `READY` flag is not sufficient if dependencies no longer satisfy invariants during a migration/recovery check.

### 7.3 `READY -> RUNNING`

Before worker launch:

1. acquire the per-session state lock;
2. re-read current revision;
3. confirm dependencies are still `COMPLETED`;
4. confirm Goal is runnable;
5. confirm concurrency/resource rules;
6. append a new `TaskAttempt`;
7. transition to `RUNNING`;
8. persist + fsync + atomic replace;
9. release the state lock;
10. only then launch the worker.

This ordering ensures a crash cannot create an unrecorded worker that the Orchestrator intentionally launched.

A crash after durable `RUNNING` but before actual spawn is conservatively handled by recovery rules; mutating work is never assumed not to have started unless low-level evidence proves it.

### 7.4 `RUNNING -> VERIFYING`

A worker returning `status: candidate_complete` only authorizes this transition. It never authorizes `COMPLETED`.

### 7.5 `VERIFYING -> COMPLETED`

Allowed only if:

- every `VerificationSpec` has a host-owned result;
- all mandatory checks are `PASSED`;
- dependencies are still `COMPLETED`;
- no forbidden path/state drift exists;
- no unresolved blocker exists;
- side-effect state is compatible with the Task contract.

### 7.6 `RETRYABLE`

A Task is retryable only when all of the following hold:

- Task attempt count remains below `max_attempts`;
- replay safety permits another attempt;
- any side-effectful low-level operation is either `CONFIRMED_NOT_PERFORMED` or has a remaining budget that existing policy permits;
- no low-level budget is replenished;
- retry does not bypass an approval or safety denial.

### 7.7 `NEEDS_REPLAN`

Replanning may strengthen a Task by adding new prerequisite Tasks and new hard dependencies. V1 does not rewrite a previously attempted Task's objective, worker authority, allowed paths, or verification criteria to make it easier to pass.

After a valid replan:

```text
NEEDS_REPLAN
  ↓ add validated new tasks/dependencies
PENDING
  ↓ dependencies complete
READY
```

This preserves the original Task identity and completion contract while permitting discovery of missing prerequisites.

---

## 8. Task DAG invariants

The DAG is validated before every plan/replan commit.

### 8.1 Required invariants

1. Every Task ID is unique within a Goal.
2. Every dependency references an existing Task in the candidate post-mutation DAG.
3. A Task cannot depend on itself.
4. The graph must be acyclic. Use a host implementation of Kahn's algorithm or DFS cycle detection; planner claims are ignored.
5. A Task cannot become `READY` until all hard dependencies are `COMPLETED`.
6. A Task cannot become `COMPLETED` while a required dependency is not `COMPLETED`.
7. A Goal cannot become `COMPLETED` while any mandatory Task is not `COMPLETED`.
8. At most one production writer/mutation lease may be active per session.
9. A `COMPLETED`, `FAILED`, or `CANCELLED` Task is immutable.
10. A `RUNNING` or `VERIFYING` Task cannot have its dependencies changed.
11. A `NEEDS_REPLAN` Task may only gain hard dependencies; dependencies cannot be removed.
12. Verification requirements may be added by replan but never removed after the first attempt.
13. Replanning cannot convert a mandatory Task to optional.
14. Replanning cannot turn a mutating Task into a read-only Task merely to make recovery/retry rules easier.

### 8.2 Dynamic Task insertion in V1

Dynamic insertion is supported because a usable Replanner requires it.

It is intentionally constrained:

- planner-provided IDs are not trusted; the host allocates new Task UUIDs;
- inserted Tasks begin `PENDING`;
- insertion increments `plan_revision`;
- the entire DAG mutation is validated before persistence;
- no partial task insertion is committed;
- suggested tasks from a worker are advisory until a planner/replanner pass accepts them;
- a replan can only strengthen existing prerequisites/verification after execution has begun.

### 8.3 Writer exclusion

V1 has a single logical `WORKSPACE_MUTATION` resource with capacity 1.

A mutating Task must reserve that resource from the pre-mutation baseline capture through the final post-mutation verification/reconciliation checkpoint.

Read-only investigators may run concurrently with one another before the writer phase. The recommended generated DAG is:

```text
read-only investigator A ─┐
read-only investigator B ─┼─> synthesis/planner -> ONE writer -> reviewer -> verifier
read-only investigator C ─┘
```

V1 SHOULD NOT schedule new read-only investigators concurrently with an active mutation lease unless their Task contract explicitly proves they observe a stable immutable input. The safe default is not to overlap them.

---

## 9. Durable persistence

### 9.1 Storage location

Use the existing Local MCP state root, never a repository-local hidden database:

```text
config::state_dir()/
  sessions/
    <session-id>.json
  goals/
    <session-id>/
      .lock
      <goal-uuid>.json
```

On the current macOS installation this resolves conceptually under the observed Local MCP application-support area, but production code MUST call `config::state_dir()` rather than hard-code a platform path.

### 9.2 Why one JSON document per Goal is sufficient for V1

SQLite is not justified for V1 because:

- only one non-terminal Goal is allowed per session;
- Task counts are expected to be modest;
- state mutations benefit from whole-Goal invariant validation;
- there is no need for ad-hoc relational queries;
- high write concurrency is deliberately not supported;
- a same-directory atomic snapshot is simpler to inspect and recover.

SQLite should be reconsidered only if future requirements add multiple active Goals, high-frequency event histories, multi-process distributed workers, very large DAGs, or partial-record write throughput that makes whole-snapshot replacement impractical.

### 9.3 Durable schema header

Every Goal file starts with an explicit format and version:

```json
{
  "store_format": "local-mcp-goal",
  "schema_version": 2,
  "revision": 42,
  "goal_id": "...",
  "session_id": "my-session",
  "...": "..."
}
```

Unknown future `schema_version` values are rejected, not guessed.

### 9.4 Atomic commit algorithm

Every state mutation uses this transaction sequence while holding the session Goal lock:

1. read authoritative Goal file;
2. validate schema and all invariants;
3. verify expected `revision`;
4. apply one host transition/mutation in memory;
5. increment `revision` exactly once;
6. serialize complete JSON;
7. create a unique temporary file in the **same directory**;
8. write complete bytes;
9. flush the file and call file `sync_all`;
10. atomically replace/rename the authoritative file;
11. sync the parent directory where supported/required for durable rename semantics;
12. release the lock.

Implementation MUST use a platform-correct atomic replace operation. If Rust's basic rename behavior is insufficient on a target platform, use a small well-audited helper crate or an OS-specific helper. Do not silently fall back to delete-then-rename.

### 9.5 Temporary files after crash

Temporary files are never authority.

Recovery rules:

- if the final Goal JSON exists and validates, load it and ignore/remove stale temp files best-effort;
- if the final file is missing, an abandoned temp file is not automatically promoted;
- no worker may start before the newly created Goal's authoritative final file exists durably.

This means a crash before the first rename may lose an unstarted Goal request, but it cannot create unrecorded side effects. That is preferable to guessing that a temp file committed.

### 9.6 Corrupt-file behavior

If the authoritative Goal file:

- does not parse;
- has the wrong `store_format`;
- has an unsupported schema version;
- violates a durable invariant;
- contains duplicate IDs or a cycle;

then Local MCP returns a typed `GOAL_STATE_CORRUPT` / unsupported-schema error and performs **no automatic overwrite or repair**.

The exact corrupt file is preserved for diagnosis. A production recovery tool may be added later, but V1 tool calls do not guess at repair.

### 9.7 Concurrent access

An in-process `Mutex` alone is insufficient because two `local-mcp mcp` processes could be started against the same state directory.

V1 requires an OS-backed advisory **per-session Goal lock** at:

```text
goals/<session-id>/.lock
```

All Goal state mutations and active-Goal uniqueness checks acquire this lock exclusively.

Do not hold the filesystem lock while:

- waiting for user approval;
- running Codex;
- running a command;
- waiting for tests;
- polling a worker.

Instead use short state transactions before and after the external work and compare `revision` on re-entry.

### 9.8 One active Goal per session

While holding the session Goal lock, `goal_start` scans valid Goal files and rejects creation if another non-terminal Goal exists, unless the request is an idempotent replay of the same `idempotency_key`.

This invariant provides a safe implementation of omitted `goal_id` for `goal_status`, `goal_pause`, `goal_resume`, and `goal_cancel`.

### 9.9 Migration strategy

The frozen Phase 0-9 implementation stores schema `1`. The structured Goal final-verification contract requires **schema `2`** because authoritative criterion IDs, the materialized contract, provenance/revision binding, and append-only final-verification history cannot be represented safely by optional schema-1 fields. This is a semantic authority change, not a cosmetic additive field.

General schema migrations MUST remain explicit, validated, atomic, non-weakening, and fixture-tested. However, schema `1 -> 2` cannot be an automatic pure migration for non-terminal Goals because schema 1 contains only free-form criterion prose and therefore lacks the semantic criterion-to-proof mapping that schema 2 requires. The host MUST NOT invent that mapping.

Compatibility rule:

- new Goals created by the Phase 9B-capable implementation use schema `2`;
- terminal schema-1 Goals may remain readable as immutable historical results through a compatibility reader;
- a non-terminal schema-1 Goal is not eligible for authoritative Goal final `PASSED`;
- continuing such a Goal requires an explicit **contract rematerialization/authority upgrade**, which preserves Goal/Task history, assigns fresh host `CompletionCriterionId` values, obtains Planner-proposed structured bindings, host-validates them, and only then commits a schema-2 snapshot;
- any legacy `final_verification` value is preserved as legacy audit evidence but is not authoritative for schema-2 completion because it lacks plan/Goal revision binding and deterministic provenance;
- no natural-language criterion is silently marked satisfied during upgrade.

This rematerialization is not a background or guessed migration. It is an explicit authority-establishing operation and must be separately tested. A binary encountering a higher unsupported schema version still refuses mutation.

Migration impact is explicit: existing schema-1 durable Goal JSON is not mutation-compatible with schema 2; non-terminal old Goals need the rematerialization path above; schema-2 test fixtures that represent executable/finalizable Goals must include host criterion IDs and complete structured mappings; legacy schema-1 fixtures remain only for compatibility/migration tests; and the public `goal_start` request schema can remain unchanged because the host creates criterion IDs internally.

---

## 10. Planner and replanner model

### 10.1 Planner is control-plane only

The Planner converts a user objective into a candidate DAG. It does not execute mutations.

The initial planner receives:

- Goal objective;
- explicit constraints/completion criteria;
- session cwd and permitted roots;
- bounded repository context gathered read-only;
- V1 WorkerKind/scope rules;
- prohibited V1 operations.

Its structured output is parsed into a candidate plan and then host-validated.

For schema 2, `goal_start` keeps the existing public MCP input shape. Each user-supplied completion-criterion string is converted immediately into a durable `GoalCompletionCriterion` with a host-assigned `CompletionCriterionId`; the string becomes `description` only. If the public request supplies no completion criteria, the host creates one required criterion whose human description is the Goal objective so an executable Goal never has an empty user-specific final contract. This does not make the prose machine-evaluable; the Planner must still bind the criterion to structured proof.

The Planner request contains the authoritative criterion IDs and descriptions. Planner output must include bounded `criterion_bindings` that map each existing criterion ID to one or more proposal-local Task references. The Planner may echo a host criterion ID and may use its own proposal-local Task IDs, but it may not mint authoritative criterion or Task IDs, record verification truth, or choose revisions.

Host initial-plan materialization resolves proposal-local Task references to newly host-assigned `TaskId` values, validates the entire DAG and Task verification specs, validates every required criterion is covered, and atomically materializes the Task DAG plus `GoalFinalVerificationSpec`. No plan may leave `PLANNING` with an unmapped criterion.

Criterion bindings are postcondition-proof references, not execution dependencies and not mutation authority. Host validation rejects unknown/nonexistent proposal Task references, mappings without mechanically evaluable Task verification, any authority-bearing payload embedded in a binding, and any representation that creates a semantic self-authorization loop. The only execution ordering remains the validated Task DAG.

### 10.2 Replanner triggers

Replanning is permitted when:

- a Task enters `NEEDS_REPLAN`;
- mechanical verification shows the existing plan is insufficient but not terminally invalid;
- a required prerequisite was discovered;
- a worker provides advisory `suggested_tasks` that the planner accepts.

Replanning is NOT a retry mechanism for:

- platform/safety refusals;
- denied approvals;
- ambiguous remote side effects;
- exhausted fallback budgets;
- attempts to broaden an operation that was denied by existing authority.

### 10.3 Replan output restrictions

A V1 replan may:

- add Tasks;
- add hard dependencies to `PENDING`, `READY`, or the triggering `NEEDS_REPLAN` Task;
- add Task verification requirements;
- add additional `TaskVerified` bindings to an existing completion criterion;
- add additional Task coverage for a criterion;
- move a valid `NEEDS_REPLAN` Task back to `PENDING` after prerequisites are committed.

It may not:

- delete completed history;
- alter terminal Tasks;
- remove dependencies;
- reduce Task verification;
- remove a completion criterion;
- change a required criterion to optional;
- rewrite criterion description/identity as a substitute for proof;
- remove or replace an existing criterion requirement;
- declare a criterion satisfied;
- rewrite old final-verification history;
- increase execution authority;
- introduce automatic commit/push/merge/publication;
- reset consumed budgets.

Criterion mapping is monotonic. Replanner proposals may reference existing host `CompletionCriterionId` and existing/new Task references, but host materialization owns all authoritative IDs and rejects any weakening. Every accepted replan increments `plan_revision`; any contract addition is bound to that new revision and makes every previous Goal final-verification record inapplicable to the new plan. Existing Task IDs remain stable forever, including completed historical Tasks.

---

## 11. Worker authority model

### 11.1 General rule

A worker can propose facts/actions. Only Local MCP host code can transition Task/Goal state or invoke effectful authority.

### 11.2 Read-only investigators

`CODEX_READONLY` workers may run concurrently up to a conservative configurable cap (recommended V1 default: 4).

They are launched with Codex's read-only sandbox. Their output may contain evidence and suggested tasks but no effectful action is automatically trusted.

### 11.3 Writer

`CODEX_WRITER` is intentionally named for its role in the workflow, not for process permissions.

In V1:

```text
Codex writer role
   ↓ read-only analysis
machine-readable mutation proposal
   ↓ host validation
exact Local MCP low-level operation(s)
   ↓ existing sandbox/approval/fallback authority
mechanical verification
```

The Codex process MUST NOT be launched with `workspace-write` or `danger-full-access` in V1.

This is required to satisfy the rule that the Orchestrator cannot bypass existing Local MCP mutation authority.

### 11.4 Reviewer

`CODEX_REVIEWER` is read-only and receives:

- Task contract;
- actual host-observed changed paths/digests;
- relevant diff/context;
- verification criteria.

Reviewer prose/findings are advisory. A blocking review criterion is enforced only because the host Task contract says the review check is mandatory.

### 11.5 Verifier

`VERIFIER` is primarily deterministic host logic.

When a verification requires a command (for example a focused test), it is executed through existing Local MCP command authority rather than by arbitrary direct process creation inside verifier code.

### 11.6 Local operation worker

`LOCAL_OPERATION` represents an exact operation already materialized by host-owned planning logic.

Examples:

- current `write_file` semantics;
- current sandboxed `execute` semantics;
- an approved host-native command where the existing approval path grants it.

The Goal does not create a new bypass path for these operations.

---

## 12. Codex integration contract

### 12.1 Required V1 dependency

The smallest required Codex dependency for a Goal that uses Codex workers is:

- a discoverable Codex executable;
- non-interactive `codex exec`;
- ability to set the working root (`-C` / equivalent detected capability);
- a read-only sandbox mode;
- prompt via stdin/argument;
- final textual response that Local MCP can parse and validate.

If these are unavailable, Codex-dependent Tasks become `BLOCKED` / `TOOL_MISSING`; already completed Tasks remain completed.

The Orchestrator itself remains able to load/status/cancel persisted Goals without Codex.

### 12.2 Optional current CLI enhancements

The following observed features are useful but MUST be capability-detected and nonessential to the correctness of V1 persistence:

- `--output-schema` — preferred when available for machine-readable final response validation;
- `--output-last-message` — convenient final-output extraction;
- `--json` — richer event telemetry;
- `codex exec review` — optional reviewer specialization;
- `codex exec resume` — optional worker-context continuation;
- `codex exec fork` — optional branch of an existing agent context.

The V1 state machine must remain correct if these optional capabilities disappear in a later CLI build.

### 12.3 Future-only Codex capabilities

The following are explicitly not required by V1:

- `--worktree`;
- Codex workspace-write worker execution;
- parallel write agents;
- automatic merge of agent worktrees.

### 12.4 Structured output

Preferred worker result shape:

```json
{
  "task_id": "uuid",
  "status": "candidate_complete",
  "summary": "what the worker concluded",
  "changed_files": [],
  "evidence": [
    {
      "kind": "file_or_command_or_reasoning_reference",
      "value": "bounded machine-readable evidence"
    }
  ],
  "blockers": [],
  "suggested_tasks": []
}
```

Allowed worker `status` values should be host-defined, for example:

```text
candidate_complete
blocked
needs_replan
failed
```

There is deliberately no authoritative worker status named `completed`. `candidate_complete` means “begin host verification.”

### 12.5 Writer proposal extension

A `CODEX_WRITER` result may additionally contain a mutation proposal. The proposal is not executed directly.

V1 should prefer host-reconstructable operations such as:

```json
{
  "proposed_operations": [
    {
      "kind": "write_utf8",
      "path": "src/example.rs",
      "expected_preimage_sha256": "...",
      "content": "..."
    }
  ]
}
```

Before applying a proposal, the host:

1. canonicalizes path scope;
2. confirms the preimage still matches;
3. confirms no writer lease conflict;
4. maps the proposal to an existing Local MCP low-level authority call;
5. persists the attempt checkpoint;
6. applies through existing execution authority;
7. independently verifies actual changes.

If the preimage changed, the Task does not blindly apply stale output; it enters `NEEDS_REPLAN` or `RETRYABLE` depending on the contract.

### 12.6 Codex resume/fork semantics

Codex thread/session continuity is optional optimization only.

The durable Goal JSON is the source of truth. A missing, corrupt, expired, or incompatible Codex thread MUST NOT make completed Goal work disappear.

For V1 recovery:

- safe read-only Tasks may simply start a fresh Codex invocation from durable Task context;
- `resume` may be used when feature-detected and a valid thread ID exists;
- `fork` may be used for future investigation branches;
- neither can bypass Task attempt limits or low-level budgets.

### 12.7 Approval requirement for Codex network execution

Codex inference requires host/network-capable execution. The current fallback path already obtains approval before its host-native Codex invocation.

V1 Orchestrator Codex workers MUST likewise use the existing approval authority for host-native/network-capable worker launch. A persisted Goal cannot treat a previous worker approval as permanent network authority.

Session `yolo` may satisfy the existing approval mechanism while that session UI is alive, exactly as it does today. The Goal file does not persist `yolo`.

---

## 13. Verification model and completion gates

### 13.1 `VerificationSpec`

V1 should support a compact deterministic set:

```rust
pub enum VerificationSpec {
    CommandExit {
        command: Vec<String>,
        cwd: Option<PathBuf>,
        accepted_exit_codes: Vec<i32>,
    },
    FileExists {
        path: PathBuf,
        must_be_file: bool,
    },
    FileDigest {
        path: PathBuf,
        expected_sha256: String,
    },
    GitScope {
        allowed_changed_paths: Vec<PathBuf>,
        require_no_other_changes: bool,
    },
    NoForbiddenChanges {
        forbidden_paths: Vec<PathBuf>,
    },
    StructuredEvidence {
        requirement_id: String,
    },
    ReviewGate {
        max_blocking_findings: u32,
    },
}
```

The exact set may be implemented incrementally, but Task completion always flows through a host-owned `VerificationResult`.

### 13.2 Baseline and post-state evidence

For writer Tasks in a Git repository, capture a host baseline before mutation, including at minimum:

- HEAD/ref where available;
- staged paths;
- unstaged paths;
- relevant file digests;
- pre-existing untracked paths relevant to scope.

Post-verification compares against this baseline so unrelated pre-existing dirty state is not falsely attributed to the Task, while new forbidden drift is rejected.

### 13.3 Worker evidence is not completion proof

The following statements are never sufficient by themselves:

- “tests passed” in Codex prose;
- a worker-declared `changed_files` list;
- a worker-declared “done” status;
- a suggested next state.

If a test is required, the host observes the command result. If a path restriction matters, the host inspects the filesystem/Git state. If a file digest matters, the host computes it.

### 13.4 Task completion gate

A Task can become `COMPLETED` only when all are true:

1. current state is `VERIFYING`;
2. every hard dependency is still `COMPLETED`;
3. every mandatory verification check is `PASSED`;
4. no verification result is `INDETERMINATE`;
5. no unresolved Task blocker remains;
6. actual changed paths are within scope;
7. no forbidden change is observed;
8. side-effect state is reconciled;
9. the attempt/budget history is internally consistent.

### 13.5 Goal completion gate

A Goal can become `COMPLETED` only when all are true:

1. **every mandatory Task is `COMPLETED`;**
2. the latest authoritative Task verification for every mandatory Task is `PASSED`;
3. no required Task or Goal verification is pending;
4. no Task is `RUNNING`, `VERIFYING`, `RETRYABLE`, `BLOCKED`, or `NEEDS_REPLAN` in a way relevant to the mandatory plan;
5. no unresolved mandatory Goal or Task blocker remains;
6. no unknown side-effect state remains;
7. the Task DAG still validates;
8. the single-writer invariant still validates;
9. every required Goal completion criterion is structurally covered by the current `GoalFinalVerificationSpec`;
10. the latest applicable Goal final-verification record is `PASSED`, has deterministic host origin, and is bound to the current Goal/plan/contract snapshot;
11. a final durable checkpoint is committed before returning success.

All mandatory Tasks being `COMPLETED`, even with passing Task verification, is necessary but **not sufficient**. Structured criterion coverage and a current host Goal final-verification record are separate gates. Premature completion is a state-machine error and MUST be rejected mechanically.

### 13.6 Structured Goal final-verification contract

#### 13.6.1 Human intent versus authority

`GoalCompletionCriterion.description`, `objective`, and other prose remain human-facing context for display, Planner, and Replanner. None is parsed by the host at finalization time. Authority-bearing facts are `CompletionCriterionId`, the current materialized `GoalFinalVerificationSpec`, stable Task identities, authoritative Task verification results, and global finalization invariants.

Every required criterion must have at least one valid structured requirement before `PLANNING -> RUNNING`. An unmapped criterion makes initial plan materialization invalid. It is never ignored and can never become `PASSED` by default.

#### 13.6.2 V1 requirement semantics

V1 uses only `TaskVerified { task_id }`. The Goal Verifier does **not** execute commands, hash files, inspect Git, run regexes, evaluate arbitrary code, or ask an LLM whether prose is true. If a Goal-specific check is inherently needed, materialize a dedicated mandatory `VERIFIER`/`LOCAL_OPERATION` Task whose existing `VerificationSpec` mechanically proves that fact. The Goal criterion then references that Task.

Direct Goal-scope `VerificationSpec` execution is deliberately not supported in V1. This avoids a second verification engine and preserves Phase 6 Task Verifier authority.

For one `TaskVerified` requirement:

- `PASSED`: the referenced Task identity resolves, the Task is mandatory and `COMPLETED`, its current authoritative Task verification result exists and is `PASSED`, and the result validates against the Task's materialized verification specifications.
- `FAILED`: the host successfully resolves authoritative identities and observes an explicit false state, such as a referenced required Task being terminal non-completed or its authoritative verification outcome being `FAILED`.
- `INDETERMINATE`: the snapshot is well-formed but the host cannot safely establish truth, for example because required authoritative evidence is absent, a referenced verification identity is stale/inapplicable, or a supported structured binding cannot be unambiguously resolved. Schema corruption is not downgraded to `INDETERMINATE`; corrupt state is rejected before evaluation.

Criterion outcome precedence is deterministic: any `FAILED` requirement makes the criterion `FAILED`; otherwise any `INDETERMINATE` makes it `INDETERMINATE`; otherwise all requirements passed and the criterion is `PASSED`. Goal-specific criterion aggregation uses the same precedence across all required criteria.

#### 13.6.3 Global finalization invariants

Universal host gates are separate from user/Goal-specific criteria: valid DAG/dependency identities; all mandatory Tasks `COMPLETED`; latest mandatory Task verification `PASSED`; no active Task work; no unresolved mandatory blocker; no `NEEDS_REPLAN` in the mandatory plan; no unresolved `UNKNOWN` side effect; single-writer validity; eligible Goal state/revision; current plan revision matching the structured contract; and complete required-criterion mapping.

Final Goal `PASSED` requires **both** all global invariants and all required structured criteria.

#### 13.6.4 Deterministic Goal Verifier algorithm

The host-owned Goal Verifier:

1. confirms the Goal is `RUNNING` and eligible;
2. binds evaluation to `goal_id`, current Goal `revision`, current `plan_revision`, and canonical `contract_digest`;
3. validates DAG/global invariants and the complete structured contract;
4. evaluates every required criterion in canonical criterion-ID order;
5. evaluates each criterion's requirements in canonical requirement-kind/Task-ID order with AND semantics;
6. collects bounded observations containing criterion ID, requirement kind, Task/result identity, and observed outcome;
7. derives criterion outcomes and aggregate `VerificationOutcome`;
8. under Goal-store compare-and-swap, rejects commit if Goal revision, plan revision, contract digest, referenced Task state, or referenced authoritative verification identity changed;
9. exercises sealed `RUNNING -> VERIFYING` authority, appends exactly one `GoalFinalVerificationRecord`, and applies the outcome state mapping in section 6.2.

If eligibility/global invariants are false before step 9, no passing record is produced. If the snapshot becomes stale before commit, no record is produced from that stale evaluation.

#### 13.6.5 Revision binding and invalidation

Every record binds to Goal ID, evaluated and committed Goal revisions, plan revision, canonical contract digest, and exact referenced Task/Verification identities. A previous record cannot authorize completion after a plan revision change, contract change, referenced Task/result change, or intervening relevant Goal mutation. For V1 safety, Phase 9 Finalizer requires current Goal revision to equal the record's committed revision before beginning its own atomic finalization mutation.

Records are retained as history rather than cleared. Staleness means “not applicable,” not deletion. Re-evaluation appends a new record.

#### 13.6.6 Final verification outcome recovery

`PASSED` leaves the Goal in `VERIFYING`; it does not complete the Goal. Only Phase 9 Finalizer can subsequently complete it.

`FAILED` transitions `VERIFYING -> FAILED` and is terminal in V1. This is deliberate: the final contract is monotonic and AND-composed, so Replanner cannot legitimately repair a false required requirement by deleting, replacing, or weakening it. A future versioned higher-authority Goal-contract replacement requires a separate design.

`INDETERMINATE` transitions `VERIFYING -> BLOCKED` with a durable mandatory blocker identifying the indeterminate criterion/requirement identities. `goal_resume` owns recovery entry: host reconciliation may clear the blocker only after ambiguity is resolved. If no contract change is needed, return to `RUNNING` for fresh Goal verification; if monotonic additional Task coverage/verification is required, return through existing `REPLANNING`, increment `plan_revision`, and verify again.

#### 13.6.7 Capability separation

Future implementation must use separate sealed capabilities for:

1. Goal Verifier entry: `RUNNING -> VERIFYING`;
2. authoritative Goal final-verification record append.

These are owned only by the host Goal Verifier and are distinct from Phase 6 Task Verifier completion authority and Phase 9 `GoalFinalizationAuthority` for `VERIFYING -> COMPLETED`. No caller can construct them from planner/worker JSON or public MCP input.

---

## 14. Security and trust boundaries

### 14.1 Trust boundary summary

```text
User/ChatGPT objective
   │ untrusted as execution authority
   ▼
Planner / Replanner output
   │ untrusted proposal
   ▼
Host DAG/scope validator
   │
Worker output
   │ untrusted evidence/proposal
   ▼
Host operation materializer
   │
EXISTING approval/sandbox/fallback authority
   │ authoritative low-level decision
   ▼
Host verifier
   │ authoritative completion evidence
   ▼
Host Goal state machine
```

### 14.2 Goal creation is not authorization

`goal_start` MUST NOT expose fields such as:

- `yolo`;
- `skip_approval`;
- `danger_full_access`;
- `authorized: true`;
- `force_push`;
- `bypass_sandbox`.

A natural-language objective saying “do anything necessary” also does not grant additional authority.

### 14.3 Low-level operation authorization

A planner may propose an operation, but it cannot manufacture `OperationContract.authorized = true` as host authority.

If an exact operation requires explicit fallback authorization, the Orchestrator must obtain that authorization through the existing approval mechanism at dispatch time and only for that exact scope. Existing fallback may still require its own approval according to current policy.

A denied approval does not become a replan invitation to find another route.

### 14.4 Host-native / `without_sandbox`

Any V1 operation that requires host-native unrestricted execution uses the existing approval path.

The Orchestrator never calls `sandbox::run_unrestricted` as a secret alternate mutation path. Internal production implementation should go through the shared `ExecutionAuthority` abstraction that preserves existing approval semantics.

### 14.5 Remote mutations

Automatic Git commit, push, workflow dispatch, API write, release, publication, and remote merge are V1 non-goals.

If a future version adds them, existing `SideEffectClass::RemoteMutation` and `UNKNOWN` side-effect blocking semantics remain authoritative.

### 14.6 Budget continuity across Task retries

High-level retry is never a mechanism to obtain a fresh low-level side-effect budget.

For the same effectful operation identity:

- persist the low-level `operation_id`;
- persist remaining attempt/side-effect budgets returned by execution authority;
- persist side-effect state;
- reuse remaining values on retry;
- if state is `UNKNOWN`, block until reconciliation;
- if state is `CONFIRMED_PERFORMED`, do not replay the same mutation merely because Task verification failed.

### 14.7 Planner-generated path scope

V1 Orchestrator imposes a stricter automation scope than the general legacy tools:

- canonicalize target paths;
- require them to fall inside session cwd or explicitly permitted directories;
- reject parent traversal, pathspec magic, and `.git` internals for writer proposals unless a future explicit operation type defines them safely;
- no automatic mutation outside the Goal's declared Task scope.

---

## 15. Pause, cancel, resume, and crash recovery

### 15.1 Pause is cooperative and durable

`goal_pause` records intent durably.

If no Task is active, transition directly through `PAUSING -> PAUSED`.

If a safe read-only worker is active, it may be aborted; its Task becomes `RETRYABLE` with a pause reason so resume can continue it.

An in-flight mutating operation is not blindly killed and declared paused. The Goal stays `PAUSING` until the operation reaches a safe checkpoint, or becomes `BLOCKED` if side-effect state is ambiguous.

### 15.2 Cancel is not “mark cancelled now”

`goal_cancel` transitions the Goal to `CANCELLING`, stops scheduling new work, and safely terminates/readies existing workers where possible.

A mutating operation with unknown side effects forces reconciliation. Only after no unresolved effect remains can the Goal become `CANCELLED`.

### 15.3 `goal_resume`

`goal_resume` is both:

- the user-facing resume command after conversation interruption; and
- the entry point for process-crash recovery.

It:

1. resolves the Goal;
2. acquires the session lock;
3. loads and validates durable state;
4. executes stale-state recovery rules;
5. persists any recovery transitions;
6. releases the lock;
7. resumes scheduler execution from incomplete work only.

Completed Tasks are never rerun merely because the conversation or process restarted.

### 15.4 Recovery by persisted Task state

#### `PENDING`

Keep `PENDING`, then recompute readiness.

#### `READY`

Revalidate dependencies and concurrency. Keep `READY` if still valid; otherwise return to `PENDING` or `BLOCKED` as dictated by invariants.

#### stale `RUNNING`

Never mark completed.

Apply this matrix:

| Worker/effect class | Recovery action |
|---|---|
| `CODEX_READONLY` | `RETRYABLE` if attempt budget remains |
| `CODEX_WRITER` proposal-only | `RETRYABLE`; V1 Codex process had no mutation authority |
| `CODEX_REVIEWER` | `RETRYABLE` |
| deterministic read-only local operation | `RETRYABLE` if replay-safe |
| possible local mutation, proven `CONFIRMED_NOT_PERFORMED` | `RETRYABLE` subject to remaining budget |
| possible local mutation, intended postcondition independently proven and no forbidden drift | move to `VERIFYING`, not directly `COMPLETED` |
| local/remote mutation state `UNKNOWN` | `BLOCKED` pending reconciliation |
| mutation proven already performed | `VERIFYING` if intended state is present; never replay simply to get a clean result |

If the process may still exist but the host cannot prove its lifecycle, V1 remains conservative. It does not launch a duplicate mutating worker.

#### stale `VERIFYING`

Never mark completed from the old state.

Re-run only verification checks explicitly classified replay-safe. If a verification itself may have effectful/ambiguous state, transition to `BLOCKED` for reconciliation.

#### `RETRYABLE`

Transition to `READY` only if retry and low-level budgets remain valid.

#### `BLOCKED`

Preserve the blocker. `goal_resume` may run authorized read-only reconciliation. If the blocker remains, Goal stays `BLOCKED`.

#### `NEEDS_REPLAN`

Goal enters `REPLANNING`; host validates any new DAG mutation before returning the Task to `PENDING`.

#### terminal Task

`COMPLETED`, `FAILED`, and `CANCELLED` remain unchanged.

### 15.5 Recovery by persisted Goal state

On process restart:

- `COMPLETED`, `FAILED`, `CANCELLED`: stable terminal state;
- `PAUSED`: remain paused until explicit `goal_resume`;
- `RUNNING`, `REPLANNING`, `VERIFYING`, `PAUSING`, `CANCELLING`: run Task-level recovery before scheduling anything new;
- `BLOCKED`: remain blocked unless resume can mechanically clear the blocker;
- `PLANNING`: restart planning only if no durable valid DAG was committed.

### 15.6 Legacy job IDs are not recovery authority

A Goal Task may have been implemented by an ephemeral worker handle, but the durable Task state is authoritative.

After Local MCP restart:

- do not require an old `job_id` to resume a Goal;
- do not infer Task success because the in-memory job map is empty;
- do not recreate completed Tasks;
- use evidence/reconciliation rules above.

---

## 16. Minimal MCP Goal tool surface

V1 exposes exactly six public Goal tools. Internal planner/task primitives remain private.

All Goal tools require `session_id`, preserving the existing session-scoped Local MCP model.

### 16.1 `goal_start`

Purpose: create one durable Goal, generate/validate its first plan, and begin scheduling when possible.

Input schema concept:

```json
{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "session_id": { "type": "string" },
    "objective": { "type": "string", "minLength": 1, "maxLength": 131072 },
    "title": { "type": "string", "maxLength": 256 },
    "constraints": {
      "type": "array",
      "items": { "type": "string", "maxLength": 8192 },
      "maxItems": 64
    },
    "completion_criteria": {
      "type": "array",
      "items": { "type": "string", "maxLength": 8192 },
      "maxItems": 64
    },
    "idempotency_key": { "type": "string", "maxLength": 128 }
  },
  "required": ["session_id", "objective"]
}
```

No authority/approval-bypass fields are accepted.

Response includes at minimum:

```json
{
  "goal_id": "...",
  "status": "PLANNING|RUNNING|BLOCKED",
  "plan_revision": 0,
  "revision": 1,
  "created_at": "..."
}
```

The tool MUST NOT return a Goal ID until the Goal is durably readable from the Goal store.

### 16.2 `goal_status`

Input:

```json
{
  "session_id": "my-session",
  "goal_id": "optional-uuid"
}
```

If `goal_id` is omitted, resolve the unique non-terminal Goal for the session. Because V1 permits only one, ambiguity is an invariant failure rather than a heuristic selection.

Response contains:

- Goal status/revision/plan revision;
- progress counts by TaskStatus;
- currently active Task summaries;
- blockers;
- next runnable Tasks;
- latest checkpoint;
- read-only completion criterion summaries containing criterion ID, human description, mapped/unmapped state, and latest applicable verification outcome;
- whether a current plan-bound Goal final-verification record exists;
- whether final result is available.

These fields expose state only; they provide no mutation or verification-truth authority.

It does not mutate state except an optional read-time detection of corruption; recovery transitions belong to `goal_resume`.

### 16.3 `goal_pause`

Input:

```json
{
  "session_id": "my-session",
  "goal_id": "optional-uuid",
  "reason": "optional human reason"
}
```

Sets durable pause intent and returns `PAUSING` or `PAUSED`. It does not lie about an in-flight mutating operation.

### 16.4 `goal_resume`

Input:

```json
{
  "session_id": "my-session",
  "goal_id": "optional-uuid"
}
```

This is the main natural-language path for:

```text
@Local MCP Resume the current goal.
```

It executes recovery, replan if required, and resumes from durable incomplete work.

### 16.5 `goal_cancel`

Input:

```json
{
  "session_id": "my-session",
  "goal_id": "optional-uuid",
  "reason": "optional human reason"
}
```

Returns `CANCELLING`, `CANCELLED`, or `BLOCKED` if safe cancellation requires reconciliation.

### 16.6 `goal_result`

Input:

```json
{
  "session_id": "my-session",
  "goal_id": "uuid"
}
```

`goal_id` is required because completed Goals are historical and there may be many.

For terminal Goals it returns:

- terminal status;
- objective;
- final summary;
- mandatory Task outcomes;
- structured completion criterion IDs/descriptions and their final outcomes;
- the applicable Goal final-verification record plus historical record references where useful;
- unresolved/terminal blockers if failed/cancelled;
- evidence references;
- created/completed timestamps.

For a non-terminal Goal it returns a typed “result not yet terminal” response rather than manufacturing a partial success result.

---

## 17. Scheduler behavior

### 17.1 Scheduler is derived from durable state

The scheduler never persists a separate queue as authority. Runnable work is derived from:

- Goal status;
- Task status;
- hard dependencies;
- worker/resource capacity;
- pause/cancel flags encoded by Goal status;
- retry budgets;
- blocker state.

### 17.2 Scheduling order

V1 does not need sophisticated optimization. A deterministic ordering is preferable:

1. oldest `READY` mandatory Task by stable durable Task order;
2. Task verification/replan authority as required by durable Task state;
3. once all mandatory Task/global Goal-verification entry gates are satisfied, select **Goal verification before optional work**;
4. honor WorkerKind/resource gates;
5. allow parallel `CODEX_READONLY` Tasks up to the configured cap;
6. only one mutation lease;
7. reviewer and verifier Tasks follow explicit dependencies.

The frozen lifecycle after the final required Task is:

```text
scheduler step N:   Task Verifier completes the final required Task; Goal remains RUNNING
scheduler step N+1: Scheduler selects VerifyGoal; Goal Verifier evaluates/records the structured contract
                     PASSED -> Goal remains VERIFYING
later distinct action: Phase 9 Goal Finalizer rechecks and may perform VERIFYING -> COMPLETED
```

Scheduler owns **selection and dispatch only**. It never evaluates criterion descriptions/requirements, never writes `final_verifications`, and never directly performs `RUNNING -> VERIFYING`. When a current host-authored `PASSED` record leaves the Goal `VERIFYING`, scheduler reports finalization required rather than manufacturing completion.

Determinism improves reproducibility and crash debugging.

### 17.3 No busy polling

The current Local MCP design avoids idle polling. V1 should retain that principle.

Use:

- worker completion notifications/channels in-process;
- explicit `goal_status`/`goal_resume` calls;
- durable state transitions;
- no timer loop that wakes continuously when no work can progress.

---

## 18. MCP Tasks extension compatibility path

### 18.1 Current Local MCP does not support the new native Tasks extension

Current `src/mcp.rs` returns protocol version `2025-06-18` and implements the older `initialize`-based protocol shape.

The current code contains no:

- `io.modelcontextprotocol/tasks` capability declaration;
- `tasks/get`;
- `tasks/update`;
- `tasks/cancel`;
- task result discriminator.

Therefore native MCP Tasks are **not a V1 dependency** and no protocol version is upgraded in this phase.

### 18.2 Current official Tasks extension status

The official MCP Tasks extension is identified as:

```text
io.modelcontextprotocol/tasks
```

The current published extension is associated with the `2026-07-28` MCP generation and defines durable task handles for `tools/call`, with:

- `tasks/get`;
- `tasks/update`;
- `tasks/cancel`;
- status values `working`, `input_required`, `completed`, `failed`, `cancelled`.

Official references:

- https://tasks.extensions.modelcontextprotocol.io/
- https://tasks.extensions.modelcontextprotocol.io/specification/draft/tasks
- https://tasks.extensions.modelcontextprotocol.io/seps/2663-tasks-extension
- https://blog.modelcontextprotocol.io/posts/2026-07-28/

### 18.3 Future adapter, not a redesign

The internal Goal model is intentionally richer than one MCP native Task. A future adapter can expose one Goal as one native MCP Task while keeping the internal Task DAG private.

Proposed mapping:

| Internal Goal | Native MCP Tasks extension |
|---|---|
| `PLANNING`, `RUNNING`, `REPLANNING`, `VERIFYING`, `PAUSING`, `CANCELLING` | `working` + statusMessage |
| `PAUSED` | `working` unless actual client input is required |
| `BLOCKED` on required client input | `input_required` |
| `BLOCKED` on non-client condition | `working` + statusMessage |
| `COMPLETED` | `completed` with `goal_result` payload |
| `CANCELLED` | `cancelled` |
| domain-level Goal failure | normally a completed tool result with error semantics, not automatically protocol `failed` |
| JSON-RPC/server execution fault | `failed` |

The last distinction matters because the Tasks specification reserves `failed` for JSON-RPC execution errors; a normal tool-level failure may still be a `completed` Task carrying an error result.

The future adapter can use a separate opaque native task ID that references the durable Goal ID. The Goal file remains the source of truth.

### 18.4 Compatibility requirement for future migration

A future protocol migration MUST preserve the six V1 Goal tools for clients that do not advertise the extension. Native Tasks must be progressive enhancement, not a replacement that makes existing ChatGPT/legacy MCP clients unable to control Goals.

---

## 19. Regression and Orchestrator test plan

### 19.1 Test category A — legacy freeze

Must run before and after every Orchestrator implementation phase:

- current `cargo test --locked` suite;
- legacy tool-schema golden test;
- initialize protocol version test;
- session load/save tests;
- sandbox write/network tests;
- approval state tests;
- fallback decision matrix;
- legacy command output/failure classification tests;
- legacy job ownership/lifecycle tests.

### 19.2 Test category B — pure state machines

Table-driven tests for every valid and invalid Goal/Task transition.

Required negative tests include:

- `PENDING -> COMPLETED` rejected;
- `READY -> COMPLETED` rejected;
- `RUNNING -> COMPLETED` rejected without `VERIFYING`;
- terminal Task transitions rejected;
- `RUNNING -> CANCELLED` rejected when side effect is unresolved;
- `VERIFYING -> COMPLETED` rejected on one failed/indeterminate check;
- Goal `RUNNING -> COMPLETED` rejected directly;
- Goal completion rejected with one mandatory non-completed Task.

### 19.3 Test category C — DAG invariants

Tests for:

- cycle rejection;
- self-dependency rejection;
- duplicate ID rejection;
- missing dependency rejection;
- readiness only after all hard dependencies complete;
- dependent completion rejection if a dependency is incomplete;
- dynamic task insertion;
- replan dependency strengthening;
- dependency removal rejection after attempt;
- mandatory->optional weakening rejection;
- concurrent writer rejection.

### 19.4 Test category D — persistence

Use a temporary state root/test harness.

Tests for:

- create/load round trip;
- schema version rejection;
- revision increment exactly once per transaction;
- compare-and-swap stale revision rejection;
- atomic temp -> final replacement;
- stale temp ignored;
- authoritative corrupt final file preserved and rejected;
- lock exclusion across two processes where supported;
- one active Goal per session;
- completed historical Goals coexist;
- future schema refuses mutation.

### 19.5 Test category E — crash injection

Introduce test-only crash/fault injection points around:

1. before Goal temp write;
2. after temp write but before sync;
3. after temp sync but before rename;
4. after rename but before directory sync;
5. after `RUNNING` checkpoint but before worker launch;
6. after low-level operation started but before result persist;
7. after worker result persist but before verification;
8. during final Goal verification.

After each simulated crash, reload and assert no false `COMPLETED` state and no duplicate automatic mutation.

### 19.6 Test category F — worker trust

Tests that malicious/incorrect structured worker output cannot:

- mark itself completed;
- add an out-of-scope path;
- request workspace-write Codex mode;
- set `authorized=true` as host authority;
- reset budgets;
- add automatic push/commit;
- remove verification criteria;
- create a cycle through suggested tasks.

### 19.7 Test category G — verification

Tests for:

- command exit gate;
- file existence/digest gate;
- Git allowed-path gate;
- pre-existing dirty state not falsely attributed;
- new forbidden drift rejected;
- reviewer blocking-finding gate;
- worker prose “passed” ignored if command evidence fails.

### 19.8 Test category H — recovery

Persist fixtures for every Task state and assert exact recovery behavior.

Especially:

- stale read-only `RUNNING` -> `RETRYABLE`;
- stale writer-proposal `RUNNING` -> `RETRYABLE`;
- stale mutation `RUNNING` + unknown effect -> `BLOCKED`;
- mutation postcondition proven -> `VERIFYING`, not `COMPLETED`;
- completed Task unchanged;
- stale `VERIFYING` re-verifies;
- `goal_resume` never redoes completed mandatory Tasks.

### 19.9 Test category I — authority boundary

Use test doubles/recording adapters around `ExecutionAuthority` to prove:

- Orchestrator file writes invoke existing write authority;
- Orchestrator commands invoke existing execute authority;
- host-native operations invoke existing approval authority;
- fallback decision is consumed, not reproduced;
- denial is not rerouted through another path;
- no Goal field creates `yolo` or bypasses approval.

### 19.10 Test category J — Codex capability degradation

Test agent behavior when:

- Codex executable missing;
- `--output-schema` unavailable;
- `resume` unavailable;
- `review` unavailable;
- `worktree` unavailable.

Only the required `codex exec` + read-only capability should be a hard dependency for Codex-backed Tasks.

---

## 20. Implementation phases

No production implementation is performed in this design phase. When implementation begins, use the following gates.

### Phase 0 — legacy freeze tests

- add missing regression tests only;
- establish clean baseline on supported host(s);
- no Orchestrator production behavior.

Exit gate: legacy contract fully green.

### Phase 1 — shared execution-authority seam

- extract/refactor current low-level execution logic from `mcp.rs` into `execution.rs` or equivalent;
- legacy MCP handlers delegate to it;
- no externally visible behavior change;
- fallback remains in `fallback.rs`.

Exit gate: legacy golden/tool/fallback/job/sandbox tests unchanged and green.

### Phase 2 — data model + store + pure state machines

- `goal.rs`;
- `task.rs`;
- `task_store.rs`;
- locking;
- atomic persistence;
- transition/DAG tests;
- no Codex workers yet.

Exit gate: persistence/crash-injection pure tests green.

### Phase 3 — six Goal MCP tools with inert/local test plans

- expose `goal_start/status/pause/resume/cancel/result`;
- create durable Goals;
- exercise scheduler with deterministic test workers only.

Exit gate: restart/resume works without Codex.

### Phase 4 — read-only Planner and investigator workers

- `agent.rs` capability detection;
- planner structured output;
- read-only Codex investigators;
- no code mutation by Codex.

Exit gate: malformed/malicious worker output cannot alter authority/state illegally.

### Phase 5 — writer proposal + existing-authority mutation

- `CODEX_WRITER` proposal schema;
- preimage validation;
- single writer lease;
- exact host operation materialization;
- existing Local MCP sandbox/approval/fallback path;
- reviewer.

Exit gate: no direct Codex workspace mutation path exists in V1 and authority-boundary tests pass.

### Phase 6 — verifier + replan + recovery

- full verification specs;
- recovery reconciliation;
- stale `RUNNING` handling;
- constrained dynamic insertion/replan;
- Goal final gate.

Exit gate: crash-injection and no-false-completion tests pass.

### Phase 7 — production hardening

- macOS/Linux platform validation;
- Windows-specific authority behavior validation;
- corrupted state fixtures;
- multiple MCP process lock tests;
- large-but-bounded Goal fixture;
- privacy/logging review;
- final regression audit.

### Phase 9B — structured Goal final-verification authority

This design amendment authorizes the later implementation of **only** the missing Goal-level structured final-verification contract and deterministic Goal Verifier path. The authorized implementation impact is:

- `src/goal.rs`: schema-2 criterion IDs/types, structured contract, append-only final-verification records, validation/invalidation, and sealed Goal-Verifier transition/write entry points;
- `src/goal_verifier.rs`: new deterministic aggregate evaluator and private capabilities;
- `src/goal_verifier_tests.rs`: coverage, outcome, stale-snapshot, ordering, provenance, and authority-boundary tests;
- `src/planner.rs`: criterion IDs in planner context, proposal-local criterion-to-Task bindings, complete-coverage validation, and atomic materialization;
- `src/replanner.rs`: monotonic binding additions/strengthening and plan-revision invalidation rules;
- `src/scheduler.rs`: deterministic `VerifyGoal` selection/dispatch after mandatory Task completion and finalization-ready signaling, with no criterion evaluation;
- `src/goal_finalizer.rs`: narrow compatibility adaptation so Phase 9 consumes only the latest applicable schema-2 host `PASSED` record bound to the current plan/revision/contract; Phase 9 remains the sole completion authority;
- `src/task_store.rs`: schema-2 persistence/loading and explicit legacy compatibility/rematerialization boundary;
- `src/goal_api.rs`: read-only criterion/mapping/outcome visibility for status/result;
- `src/main.rs`: module/wiring for Goal Verifier;
- `src/mcp.rs`: only if current response serialization/routing requires the read-only status/result additions; the `goal_start` public input schema does not need to change;
- `src/task.rs`: no semantic change is preferred; change only if shared provenance/observation primitives cannot cleanly remain Goal-specific.

No Cargo dependency change is required by this design. Phase 9B does **not** authorize Phase 10 Runner implementation.

### Phase 10 Runner boundary (future, not authorized by this pass)

A future bounded foreground Runner may repeatedly invoke Scheduler. It MUST NOT evaluate Goal criteria, mint Goal Verifier capabilities, write final-verification records, or directly move `RUNNING -> VERIFYING`. It may invoke Phase 9 Goal Finalizer only after Scheduler/durable state shows the Goal Verifier has produced a current finalization-ready `PASSED` record.

Native MCP Tasks migration is not part of these V1 phases.

---

## 21. Explicit V1 non-goals

Unless a later design explicitly supersedes this document, V1 does not include:

- parallel production writers;
- Codex `workspace-write` as the mutation authority;
- automatic Git commits;
- automatic Git pushes;
- force push;
- automatic PR/release/workflow publication;
- autonomous merging;
- automatic worktree merging;
- mandatory SQLite;
- external LLM provider APIs;
- MCP Sampling dependency;
- native MCP Tasks protocol migration;
- distributed orchestration across machines;
- durable legacy `poll_job` IDs;
- treating Codex thread/session persistence as Goal authority;
- arbitrary worker-defined tools or authority;
- silent repair of corrupt Goal files.

---

## 22. Future evolution path

### 22.1 Managed worktrees

After V1 is stable, `--worktree` can support isolated implementation branches. It must first gain explicit lifecycle/evidence rules for:

- worktree creation;
- ownership;
- cleanup;
- diff extraction;
- merge conflict handling;
- no mutation of the primary workspace before review.

### 22.2 Parallel writers

Parallel writers require a new design, not a config toggle.

At minimum they need:

- disjoint mutation scopes or isolated worktrees;
- per-worktree writer leases;
- deterministic merge/rebase authority;
- conflict verification;
- no silent combined changes.

### 22.3 Persistent Codex resume/fork

Future agent optimization can persist Codex thread IDs and use `resume`/`fork`, while keeping Goal JSON authoritative.

### 22.4 Native MCP Tasks adapter

Once the Local MCP transport/protocol layer deliberately migrates to a compatible MCP generation and client support is verified, add an adapter:

```text
native MCP task handle
       ↓
Goal ID
       ↓
existing GoalStore / Orchestrator
```

No DAG/persistence redesign should be required.

### 22.5 SQLite trigger criteria

Re-evaluate JSON only if measured V2+ requirements include one or more of:

- multiple active Goals per session;
- high-frequency event append/load contention;
- very large Goal snapshots;
- cross-process worker pools;
- relational query requirements;
- snapshot rewrite cost shown by profiling to be material.

---

## 23. Production invariants summary

The implementation is acceptable only if these statements remain true:

1. The Orchestrator is above, not beside, existing execution authority.
2. `fallback.rs` remains per-operation recovery authority.
3. A Goal cannot grant new authority.
4. Planner/worker output cannot directly mutate production state.
5. Codex writer role is read-only in V1.
6. Exactly one writer/mutation lease exists per session.
7. Task completion requires independent verification.
8. Goal completion requires every mandatory Task completed, complete structured criterion coverage, and a current deterministic host Goal final-verification `PASSED` record; Task count alone is never sufficient.
9. Crash recovery never converts `RUNNING` or `VERIFYING` directly to `COMPLETED` without fresh mechanical evidence.
10. Completed work is durable and not repeated merely because ChatGPT/MCP/Codex restarted.
11. Low-level retry/side-effect budgets cannot be reset by a high-level Task retry.
12. Denial/safety/fallback blocks cannot be bypassed by replanning an alternate execution route.
13. Legacy tool behavior remains frozen by regression tests.
14. Native MCP Tasks support remains a future adapter in V1.

---

## 24. Phase 9B structured final-verification freeze answers

The authoritative answers for the Phase 9B precondition are:

1. A Goal completion criterion is a host-identified required human-visible success condition with a separate structured proof binding.
2. Its stable identity is host-assigned UUIDv4 `CompletionCriterionId`, unique within the Goal, durable, immutable, independent of position and prose.
3. `description` is human-readable only.
4. `GoalFinalVerificationSpec` plus authoritative Task identities/results is mechanically authoritative.
5. Planner echoes host criterion IDs and binds them to proposal-local Task references in `criterion_bindings`.
6. Host validates complete coverage, Task existence/mandatory status, verification evaluability, scope/DAG rules, then resolves proposal references to host Task IDs atomically.
7. Replanner may only add Task coverage/requirements or otherwise strengthen; it cannot remove, replace, weaken, optionalize, or declare satisfaction.
8. A criterion is `PASSED` only when every bound requirement mechanically passes.
9. It is `FAILED` when at least one mechanically evaluated requirement is authoritatively false.
10. It is `INDETERMINATE` when no requirement is failed but at least one cannot be safely established from authoritative structured evidence.
11. Requirements are AND; required criteria are AND. V1 has no implicit OR.
12. All V1 authoritative completion criteria are required.
13. Goal verification reuses latest authoritative Phase 6 Task `VerificationResult` identities/outcomes; it does not rerun them.
14. Direct Goal-level checks are not permitted in V1; model them as dedicated mandatory verified Tasks.
15. An unmapped required criterion invalidates plan materialization; the Goal cannot leave `PLANNING`.

16. `RUNNING -> VERIFYING` requires all global entry gates plus complete current structured coverage and stale-snapshot checks.
17. Only the sealed host Goal Verifier owns `RUNNING -> VERIFYING`.
18. Only the same host Goal Verifier may append authoritative Goal final-verification records.
19. Each record binds to Goal ID, evaluated/committed Goal revision, plan revision, contract digest, and referenced Task/result identities.
20. Plan revision change, contract change, referenced Task/result change, or intervening relevant Goal revision makes an old record inapplicable.
21. `PASSED` leaves the Goal `VERIFYING`.
22. `FAILED` transitions to terminal `FAILED` in V1.
23. `INDETERMINATE` transitions to `BLOCKED`.
24. `FAILED` is not Replanner-repairable in V1 because monotonic AND authority cannot discard a false required requirement; `INDETERMINATE` recovers only through host reconciliation, then `RUNNING` reevaluation or monotonic `REPLANNING`.
25. Phase 9 Finalizer consumes only a current applicable host `PASSED` record, rechecks all gates, and alone performs `VERIFYING -> COMPLETED`.
26. Scheduler selects/dispatches `VerifyGoal` after final mandatory Task completion; it never evaluates criteria.
27. Future Runner only loops Scheduler and may call Phase 9 Finalizer after finalization-ready state; it owns no verification authority.
28. Legacy schema-1 Goals without structured mappings cannot receive authoritative automatic final `PASSED`; non-terminal ones require explicit contract rematerialization/authority upgrade.
29. `schema_version` must advance to 2. This is not safely representable as backward-compatible optional schema-1 fields.
30. The only newly authorized implementation is the Phase 9B scope enumerated in section 20: schema-2 structured criterion materialization, deterministic Goal Verifier, Planner/Replanner/Scheduler/Finalizer integration required for that contract, tests, and the legacy boundary. Phase 10 remains unauthorized.

**GOAL_ORCHESTRATOR_V1_GOAL_FINAL_VERIFICATION_CONTRACT_FROZEN**

---

## 25. Production model invocation contract freeze

This closure consumes the already-frozen Phase 10 foreground Runner without changing its scheduling, verification, finalization, retry, or revision semantics. The production Goal model seam and Phase 11 adapter are additive authority below and above that Runner respectively.

### 25.1 Narrow model-invocation authority

The production seam is internal to Goal orchestration and owns only one bounded read-only inference operation. Its typed boundary is:

```text
ModelInvocation { session_id, cwd, role, prompt }
    -> ModelTransport::invoke(...)
    -> ModelInvocationOutput { stdout, safe_stderr, exit_status }
       or AgentError
```

`cwd` is supplied only by existing host Goal/session request construction; model output can never select it. `ModelTransport` is injectable so authoritative tests use deterministic bytes and never require live inference or network access. The production transport is stateless and composition is inert until a backend is actually selected by Phase 10.

### 25.2 Model configuration and role policy

`LOCAL_MCP_GOAL_MODEL` is the one Goal-specific host configuration override. If it is absent, the seam reuses the existing Local MCP Codex fallback model configuration/default; an empty, control-containing, overlong, or non-Unicode override is invalid. This is host configuration only: `goal_run` has no provider/model/backend/prompt parameter. Planner, Writer, Reviewer, and Replanner use the same configured model in V1; role is conveyed only by fixed host-owned instructions and strict downstream schemas.

The Codex command reuses the existing command builder and is always equivalent to:

```text
codex exec -m <host-model> ... --ephemeral --ignore-user-config -s read-only -C <authoritative-cwd> -
```

The model receives no Local MCP tool catalog. In particular it receives no `write_file`, `execute`, `without_sandbox`, `goal_*`, or job authority. Writer is a proposal role, not a process permission.

### 25.3 Approval, process bounds, and output handling

Every production inference invocation passes through existing `approvals::request` for the resolved session before any host-native/network-capable Codex process is launched. A denial is terminal for that invocation and no persisted Goal can manufacture or replay approval. Public `without_sandbox` authority is neither accepted by `goal_run` nor inherited by the model seam.

The absolute host-owned invocation deadline is 120 seconds and includes approval plus process lifetime. The caller cannot extend it. Prompt input is capped at 256 KiB, stdout at 2 MiB, and stderr at 64 KiB. Stderr is bounded and control-sanitized before entering the typed internal output; it is never included in public `goal_run` output. Empty stdout is rejected.

The existing generic unrestricted runner waits for completion before returning fully buffered output and therefore cannot enforce these streaming caps and one absolute approval-plus-process deadline. For that reason only, `agent.rs` contains a narrow direct process launcher after approval. It accepts only the host-built Codex read-only command and is not a reusable arbitrary host command runtime.

Typed failures are exactly:

```text
ApprovalDenied
ExecutableUnavailable
SpawnFailed
Timeout
NonZeroExit
EmptyResponse
ResponseTooLarge
TransportFailure
Cancelled
InvalidConfiguration
```

No raw prompt, credential, environment dump, or raw stderr is placed in an MCP result.

### 25.4 Prompt and backend trust boundary

Each role prompt has a fixed host instruction/policy prefix, declares serialized Goal/Task request content untrusted data, states that no tools are available, and requests only the strict JSON proposal expected by the existing host parser. Repository/user text inside the serialized request cannot grant authority. Transport success returns stdout bytes unchanged; malformed JSON is not repaired or retried by another model call.

The production Planner, Writer, Reviewer, and Replanner adapters invoke only this seam and return proposal/advisory bytes. They do not transition Tasks or Goals, call Scheduler/Runner/Finalizer, or write the workspace. Existing Phase 4/5/7 parsers and host validators retain authority; existing Phase 5 host Writer remains the only path that materializes an accepted writer proposal.

### 25.5 Phase 11 public foreground adapter

The sole additive public tool is `goal_run`. Its strict session-scoped MCP input is `{ session_id, goal_id, max_steps }`, with `max_steps` required and limited to `1..=256`, and with unknown fields rejected. It resolves existing durable authority, constructs one inert production backend composition, calls `run_goal_foreground` exactly once, and serializes the bounded typed Runner result. It creates no legacy Job, detached task, daemon, timer, polling surface, outer retry loop, native MCP Task, or model/provider selection surface. The MCP protocol remains `2025-06-18`; the catalog becomes exactly 18 tools.

Phase 12 is not authorized by this freeze.

**GOAL_ORCHESTRATOR_V1_PRODUCTION_MODEL_INVOCATION_CONTRACT_FROZEN**

---

## 26. Formal design verdict

No architectural blocker was identified that prevents a production implementation of this V1 design.

The repository already contains the difficult low-level primitives the Orchestrator needs: persistent session identity, sandboxing, approval, job execution, lifecycle evidence, operation contracts, side-effect classification, retry budgets, Codex invocation, and fallback verification.

The required new work is a durable higher-level control plane with strict authority reuse and recovery semantics.

**GOAL_ORCHESTRATOR_V1_DESIGN_READY**

---

## 27. Failed-task replacement and budget-aware decomposition (maintenance)

This section is additive. It does not reopen any frozen contract above: the MCP
catalog remains exactly 18 tools, `pre_execution_plan_rejection` and
`pristine_plan_supersession` keep their existing semantics, and
`pristine_plan_supersession` remains the only whole-plan-revision rejection
path.

### 27.1 Structured failure classification

Every durable `TaskAttempt` may carry a host-owned `FailureClass`:
`TRANSIENT_MODEL_FAILURE`, `HOST_OUTPUT_LIMIT`, `CONTEXT_LIMIT`,
`TASK_SCOPE_TOO_BROAD`, `SEMANTIC_FAILURE`, `AUTHORITY_FAILURE`,
`PLATFORM_SAFETY`, `UNKNOWN`.

The class is produced at the narrowest trustworthy boundary: `FailureClass::
from_agent_error` runs in `readonly_worker` while the typed `AgentError` is
still in hand, never parsed back out of prose. Recovery policy reads the
class. The only string comparison in the taxonomy is an exact-equality match
against two host-authored legacy report constants, used solely so Goals
durably written before the field existed remain classifiable; anything
unrecognised classifies as `UNKNOWN` and therefore fails closed.

Policy is not collapsed:

| Failure class | Unchanged retry | Failed-task replacement |
| --- | --- | --- |
| `TRANSIENT_MODEL_FAILURE` | permitted (bounded) | refused |
| `SEMANTIC_FAILURE` | permitted (bounded) | refused |
| `HOST_OUTPUT_LIMIT` | refused | required |
| `CONTEXT_LIMIT` | refused | required |
| `TASK_SCOPE_TOO_BROAD` | refused | required |
| `AUTHORITY_FAILURE` | refused | refused |
| `PLATFORM_SAFETY` | refused | refused |
| `UNKNOWN` | refused | refused |

A Task whose class forbids replay can never become `READY` again through any
edge, not just an ordinary retry.

### 27.2 Generic failed-task replacement

`ReplanProposal` gains `replace_tasks`, a general replacement primitive
distinct from `pristine_plan_supersession`:

```json
"replace_tasks": [{
  "replan_request_id": "<durable request id>",
  "old_task_id": "<existing Task UUID>",
  "completion_closure_task_refs": [{ "ref_kind": "NEW", "proposal_id": "join" }],
  "criterion_rebindings": [
    { "criterion_id": "...", "replacement_task_refs": [{ "ref_kind": "NEW", "proposal_id": "join" }] }
  ]
}]
```

`replace_tasks` is exclusive with `add_dependencies`,
`strengthen_verification`, `strengthen_mandatory`,
`strengthen_criterion_bindings`, `resolve_needs_replan`, and
`pristine_plan_supersession`; the closure lives in `add_tasks`.

One transaction atomically: marks each old Task `SUPERSEDED` (preserving its
attempts, evidence, `max_attempts`, and consumed-attempt count byte for byte),
creates the replacement closure, rewires every active dependent off the
superseded Task and onto the declared completion closure, rebinds every
completion criterion that required a replaced Task, increments
`plan_revision` exactly once, and appends a `ReplanCommitted` checkpoint. It is
built on a private candidate and published only after `Goal::validate()`
passes, so no committed snapshot can show a superseded Task that is still
runnable, a dependency on superseded work, or a criterion bound to
permanently unsatisfiable proof. Any validation failure commits nothing.

Authority the host re-derives independently of the model: durable replan
request authority, trigger eligibility, side-effect safety, closure
maximality, scope non-widening (allowed paths may only narrow, forbidden paths
may only grow, operation kind and replay safety are fixed), acyclicity,
dependency/completion-closure semantics, writer serialization, host task, edge,
path and verification limits, and criterion coverage.

Replay is safe: the same `replan_request_id` with the same canonical proposal
digest returns the already-materialized result without a write or a revision
bump; the same identity with a different effective proposal is rejected.

### 27.3 Budget-aware task sizing

`PlannerRequest` and `ReplannerRequest` expose a deterministic,
host-derived `sizing` profile (`independent_entity_count`,
`evidence_dimension_count`, `evidence_shape_estimate`,
`max_single_readonly_task_records`, `over_budget`, `guidance`) and the
`readonly_output_contract` (32 evidence items, 8 KiB per field, 64 KiB total,
16 KiB summary). These are conservative structural bounds, never token
predictions. When a size or budget failure triggers a replacement, the host
requires a materially decomposed closure of at least three Tasks with a bounded
join Task, and bounds each replacement objective to 4 KiB, so a size failure
cannot be repaired by prepending another broad reassessment Task.

### 27.4 Transition matrix

`goal_resume` gains an optional `failed_task_replan_requests` array (bounded
to 8). It is a sibling of `pre_execution_plan_rejection`, not an expansion of
it, and the exposed MCP tool count is unchanged.

| Trigger Task state | Side effects | Admitted path |
| --- | --- | --- |
| `READY`, pristine, no attempts | none | `pre_execution_plan_rejection` remains valid; a `PRE_EXECUTION_REJECTION` replacement request may then name that rejection as its authority |
| `RETRYABLE`/`PENDING`/`NEEDS_REPLAN`, structured size or budget class | `CONFIRMED_NOT_PERFORMED` | replacement request admitted; the Replanner may supersede without replaying, and the remaining retry stays unconsumed |
| any state, `TRANSIENT_MODEL_FAILURE` or `SEMANTIC_FAILURE` | `CONFIRMED_NOT_PERFORMED` | replacement refused; the bounded unchanged retry is preserved |
| `AUTHORITY_FAILURE` or `PLATFORM_SAFETY` | any | replacement refused; authority/escalation handling only |
| any state with `UNKNOWN` side effects | unresolved | replacement and replay prohibited until reconciliation proves safety |
| `RUNNING` / `VERIFYING` | any | refused |

Both request kinds enter `NEEDS_REPLAN` and the Goal enters `REPLANNING` in
one durable mutation. The request itself never consumes retry budget, and
`pre_execution_plan_rejection` retains its existing one-per-plan-revision
limitation and pristine-trigger rules.

**GOAL_ORCHESTRATOR_V1_FAILED_TASK_REPLACEMENT_FROZEN**
