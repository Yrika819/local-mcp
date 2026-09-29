# Managed Worktrees V1 — Research and Contract Freeze

Status: **CONTRACT FROZEN — IMPLEMENTATION NOT STARTED**

Repository: `Yrika819/local-mcp`

Baseline main at freeze research start:

`ffb6375c217ca9ef2055d9e3f049a78129817382`

This document is additive to `docs/GOAL_TASK_ORCHESTRATOR_V1_DESIGN.md`.
It does not reopen Public v1 release hardening, the existing Goal V1 authority
model, Windows experimental status, or release infrastructure.

The objective is narrow:

> Allow Goal-driven implementation work to occur in one host-managed Git linked
> worktree so that normal writer mutations do not touch the primary workspace
> before host review.

Managed worktrees are isolation and lifecycle state. They are **not** a new
filesystem, Git, network, publication, approval, fallback, or model authority.

---

## 1. Research findings

The current production implementation is intentionally rooted in one physical
workspace:

- `Goal.cwd` is durably bound to the session cwd.
- Planner scope paths are normalized and canonicalized against `Goal.cwd`.
- `TaskScope.allowed_paths` / `forbidden_paths` are persisted as resolved
  filesystem paths.
- Writer mutation materialization resolves against `Goal.cwd`, rejects
  `.git` internals and path escapes, and enforces TaskScope.
- Verifier Git observation and verification paths also resolve against
  `Goal.cwd`.
- Scheduler and writer already enforce one workspace-mutation lease per Goal.
- Writer mutation state, preimages, side-effect state, Goal revision, and
  verification evidence are already durable.

Therefore a safe implementation cannot create a worktree after a plan has
already materialized absolute TaskScope paths and then merely switch cwd. The
worktree must become the execution root **before initial plan materialization**.

Git's documented worktree behavior also matters:

- linked worktrees share repository refs and common Git data while keeping
  per-worktree HEAD/index state;
- `git worktree add --lock` can create and lock a worktree without a race
  between add and lock;
- `git worktree list --porcelain -z` is the stable machine-readable discovery
  surface;
- normal `git worktree remove` refuses dirty worktrees;
- `--force` can bypass safeguards and is therefore outside this V1 contract;
- stale administrative metadata can be diagnosed without immediately pruning;
- a branch normally cannot be checked out in multiple worktrees without force.

Primary Git documentation:

https://git-scm.com/docs/git-worktree

The locally observed development host during research used:

`git version 2.50.1 (Apple Git-155)`

and already had two worktrees:

- primary: `/Users/yuta/local-mcp` at `ffb6375c...` on `main`
- existing Codex worktree: `/Users/yuta/.codex/worktrees/e7ba/local-mcp` at
  `eb7b602e...`, detached

No Managed Worktrees V1 design may assume it is the only linked worktree.

---

## 2. Non-negotiable authority invariants

All existing Goal V1 invariants remain authoritative.

Additional Managed Worktrees V1 invariants are:

1. A Goal/worktree never creates new authority by itself.
2. Worktree creation, recovery, use, and cleanup are host-owned decisions.
3. Model output never chooses the worktree path, branch name, base revision,
   repository common directory, cleanup action, merge action, or remote.
4. Session filesystem authority is never silently expanded to include a
   managed worktree.
5. The existing single writer/mutation lease remains the only writer model.
6. Managed Worktrees V1 does not enable parallel writers.
7. Writer TaskScope is narrowed to the managed worktree execution root; it
   never gains write access to the primary workspace merely because the primary
   repository owns the linked worktree.
8. `.git` / Git administrative internals remain forbidden writer targets.
9. Worktree lifecycle Git operations are not model-proposed writer operations.
10. No automatic stash, reset, clean, force checkout, force worktree add/remove,
    branch deletion, merge, rebase, push, force-push, PR creation, or release
    publication is authorized.
11. A crash or timeout never implies that worktree creation/removal succeeded.
12. Ambiguous Git mutation state is reconciled before any retry.
13. Primary-workspace dirtiness is never hidden or silently copied into the
    managed worktree.
14. Existing non-worktree Goal behavior remains available and regression-frozen.
15. Windows managed-worktree support does not change Windows' experimental
    sandbox/security classification.

---

## 3. Public opt-in and compatibility

Managed mode is explicit and additive.

A future `goal_start` schema may add:

```text
workspace_mode:
  PRIMARY            # default; current behavior
  MANAGED_WORKTREE   # explicit opt-in
```

Rules:

- omitting `workspace_mode` is exactly current `PRIMARY` behavior;
- existing clients do not acquire worktree behavior;
- `MANAGED_WORKTREE` is isolation policy, not additional authority;
- public callers cannot supply a worktree path, branch name, base SHA, Git
  command, or permission root.

The MCP protocol generation does not need to change merely to add this optional
field.

---

## 4. One Goal owns one worktree

Managed Worktrees V1 freezes:

> **one non-terminal Goal -> at most one managed linked worktree**

Task-scoped worktrees are out of scope.

Reasons:

- V1 already serializes production writers;
- tasks in one Goal frequently depend on earlier mutations;
- one execution root preserves a coherent accumulated candidate;
- one worktree gives the Verifier one Git view;
- crash recovery and ownership are substantially simpler;
- task-scoped worktrees would immediately introduce merge/composition
  authority, which belongs with a future parallel-writer design.

A worktree may outlive the Goal for review/cleanup purposes, but its durable
ownership remains the originating Goal ID.

---

## 5. Creation timing

A managed worktree must be established **before Planner path materialization**.

The high-level managed flow is:

```text
goal_start
  -> Goal PLANNING with MANAGED_WORKTREE requested
  -> host PrepareWorkspace step
  -> durable creation intent
  -> Git linked worktree creation/reconciliation
  -> durable ACTIVE workspace binding
  -> Planner
  -> normal Task DAG / single writer / verification
```

Planner MUST NOT materialize a managed plan while the workspace is only
requested/preparing/ambiguous.

This prevents existing absolute TaskScope paths from accidentally pointing at
the primary workspace.

---

## 6. Repository eligibility and clean-primary rule

Managed mode V1 requires all of the following before creation:

- `Goal.cwd` still equals the bound session cwd under the existing canonical
  binding rules;
- `Goal.cwd` is the canonical Git top-level directory;
- repository common-dir identity can be observed mechanically;
- `HEAD` resolves to a commit;
- the primary worktree has no tracked, staged, or untracked changes;
- no merge/rebase/cherry-pick/revert/bisect operation is in progress;
- the selected managed path is unoccupied;
- the managed branch ref does not already exist unless durable ownership proves
  it belongs to this exact Goal.

The clean-primary requirement is deliberate. V1 does not guess whether dirty
or untracked user files are part of the intended base and never stashes or
copies them silently.

If any eligibility check fails, the Goal is blocked with evidence; the primary
workspace is unchanged.

---

## 7. Worktree root and filesystem authority

The host chooses a deterministic managed root. The model never supplies it.

Recommended host layout:

```text
<managed-root>/<session-id>/<goal-id>/
```

The exact `managed-root` is host configuration/state, not Goal authority.

Before any planner, worker, verifier, or command uses the linked worktree, its
exact root must already be covered by current Session path authority.

If it is not covered:

- Local MCP MUST NOT silently append it to `permitted_directories`;
- the Goal becomes blocked awaiting the existing explicit sandbox-root approval
  path (or equivalent host authorization);
- denial leaves the Goal blocked and creates no broader authority.

After authorization, managed execution receives the exact worktree root, not a
broader parent unless the user explicitly authorized that broader root.

The primary session cwd remains the durable identity root. Managed execution
uses a separate host-derived `execution_root`.

---

## 8. Durable worktree identity

Managed workspace state is authoritative host state, not a convention inferred
from directory names.

A future schema should persist, at minimum:

```text
ManagedWorktreeRecord {
    worktree_id
    lifecycle
    primary_root
    repository_common_dir
    worktree_root
    branch_ref
    base_commit
    source_ref?              # informational; may be absent for detached source
    created_goal_revision
    created_plan_revision    # expected to be 0 at creation
    lock_reason
    last_reconciled_head
    last_reconciled_at
}
```

All paths are canonical absolute host paths.

The branch ref is deterministic and host-owned:

```text
refs/heads/local-mcp/goal/<full-goal-uuid>
```

No shortened UUID is authoritative.

A branch with the expected name but without matching durable ownership is a
conflict, not an object to adopt or overwrite.

---

## 9. Durable schema boundary

Managed workspace identity changes Goal authority/evidence semantics enough that
it must not be smuggled into the frozen schema as an unversioned assumption.

The implementation plan should advance the Goal durable schema for newly
managed-aware Goals and keep explicit compatibility for existing stored Goals.

Requirements:

- existing terminal Goal history remains readable;
- existing non-managed Goals retain their current semantics;
- no active legacy Goal is silently converted to managed mode;
- migration cannot invent worktree ownership from a directory/branch name.

The exact schema migration code is implementation work, but the semantic rule
is frozen here.

---

## 10. Creation command contract

The host may use the semantic equivalent of:

```text
git worktree add
  --lock
  --reason "local-mcp goal <goal-id>"
  -b local-mcp/goal/<goal-id>
  <worktree-root>
  <base-commit>
```

The implementation MUST NOT use:

- `-B`
- `--force`
- implicit branch-name guessing
- an unvalidated caller/model path
- an unvalidated caller/model commit-ish

The base commit is the mechanically observed primary `HEAD` captured in the
durable creation intent.

The branch is local-only. Creation grants no push/publication authority.

---

## 11. Crash-safe creation and reconciliation

Worktree creation is a Git mutation affecting both worktree metadata and a
branch ref. It needs explicit side-effect state analogous to writer mutation
recovery.

Before invoking Git, persist a creation intent containing:

- Goal/worktree identity;
- expected repository common dir;
- exact target path;
- exact branch ref;
- exact base commit;
- operation ID;
- state `PREPARED`.

After Git returns, reconcile mechanically before committing `ACTIVE`.

Recovery after a crash/timeout MUST inspect at least:

- `git worktree list --porcelain -z`;
- exact worktree path;
- exact branch ref;
- observed worktree HEAD;
- repository common-dir identity;
- lock state/reason where available.

Recovery outcomes:

- exact owned worktree exists at expected branch/base -> adopt and mark ACTIVE;
- neither owned path nor branch/ref side effect exists -> bounded retry may be
  allowed;
- branch exists but path/HEAD/ownership differs -> BLOCKED;
- path is registered to another worktree -> BLOCKED;
- missing path with stale/prunable metadata -> BLOCKED for explicit recovery;
- any ambiguous observation -> no automatic retry.

No recovery path may use force to make observations match expectations.

---

## 12. Git worktree locking

Managed worktrees are created locked.

The lock:

- reduces accidental prune/move/remove risk;
- carries a host-owned reason identifying the Goal;
- is lifecycle evidence, not filesystem security.

The implementation must not mistake a Git worktree lock for a process or
mutation lock. The existing Local MCP writer lease remains authoritative.

---

## 13. Execution-root binding

`Goal.cwd` remains the primary/session identity root.

Managed mode adds a host-derived effective root:

```text
Goal.execution_root()
  PRIMARY          -> Goal.cwd
  MANAGED_ACTIVE   -> managed worktree_root
```

For managed Goals after activation:

- Planner normalizes TaskScope and VerificationSpec paths against
  `execution_root`;
- writer request cwd and writer path resolution use `execution_root`;
- Verifier file checks, command cwd, and Git observation use
  `execution_root`;
- read-only Goal workers that need repository state use `execution_root`;
- TaskScope effectful paths must be descendants of `execution_root`;
- primary `Goal.cwd` is not an allowed writer target through managed TaskScope.

Session/Goal identity validation still checks the primary `Goal.cwd` against
the bound session. A separate managed-root validation checks current Session
authority and durable worktree ownership.

This split is required; replacing `Goal.cwd` itself would erase the original
session/repository binding.

---

## 14. Worktree-aware writer lease

Managed Worktrees V1 retains exactly one production writer/mutation lease per
Goal.

No per-worktree parallel lease is introduced because one Goal owns one
worktree.

The existing lease rule remains effective:

- mutating RUNNING/VERIFYING tasks hold the lease;
- performed/unknown side effects continue holding it until reconciled;
- another writer is not dispatched concurrently.

Parallel writers remain a separate future design.

---

## 15. Worktree evidence and verifier binding

Verification must prove which managed workspace it observed.

Managed-mode task verification evidence should bind to:

- worktree ID;
- repository common-dir identity;
- worktree root;
- managed branch ref;
- base commit;
- observed current HEAD;
- Goal revision;
- plan revision;
- authoritative task/attempt/verification IDs.

Git status observation continues to include untracked files.

For final Goal verification, the host must also compute a bounded
`workspace_snapshot_digest` that binds the review candidate at that moment.

The digest input must include at least:

- worktree ID;
- base commit;
- observed HEAD;
- normalized set of Git-visible changed/staged/untracked paths;
- content identity for every writer-mutated path still present in the candidate;
- absence markers for writer-mutated paths that are now absent.

The exact serialization must be canonical and versioned.

The finalizer must re-observe the workspace snapshot and require the same digest
before `VERIFYING -> COMPLETED`. A changed candidate after final verification
invalidates the previous final-verification applicability rather than being
silently accepted.

This is a workspace-integrity gate, not a model-declared completion criterion.

---

## 16. Primary branch advancement

The creation record freezes `base_commit`.

At review/finalization time the host observes the source branch/current primary
HEAD again.

If it still equals `base_commit`:

- the candidate is based on the current primary revision.

If it advanced:

- the worktree candidate is **not** automatically rebased or merged;
- verification evidence remains valid for the recorded base;
- the result reports `BASE_ADVANCED` with old/new SHAs;
- promotion requires a separate explicit host/user action.

A concurrent primary advance is not authority to rewrite the candidate history.

---

## 17. Merge, rebase, commit, and primary mutation boundary

Managed Worktrees V1 initial scope intentionally stops at a verified review
candidate.

Inside Goal execution it does **not** automatically:

- create Git commits;
- stage files;
- merge;
- rebase;
- cherry-pick;
- update the primary branch;
- switch the primary worktree;
- push a branch.

Therefore the primary workspace may be mutated only after Goal review through a
separate explicit user/host-authorized integration operation outside this
initial Managed Worktrees V1 execution contract.

That integration operation must independently check:

- candidate identity/evidence;
- current primary HEAD;
- primary cleanliness;
- branch/ref expectations;
- conflict state;
- requested exact Git operation.

A conflict is reported and left for explicit resolution. It is never resolved
by silently choosing one side or by force.

Automatic promotion can be designed later without changing the isolation
contract frozen here.

---

## 18. Diff/review evidence

A completed managed Goal must make review possible without relying on model
summary text.

`goal_result` or its internal result source should expose bounded structured
workspace evidence including:

- worktree ID/path;
- branch ref;
- base commit;
- current HEAD;
- base-advanced state;
- changed path list;
- staged path list;
- untracked path list where distinguishable;
- workspace snapshot digest;
- task verification identities;
- whether cleanup is currently safe.

Large textual/binary diffs must remain bounded. The host may provide hashes,
sizes, path summaries, or a separately bounded diff excerpt rather than
unbounded file content.

---

## 19. Success, failure, cancellation, and cleanup

Cleanup is conservative.

### Success

A successful managed Goal is retained locked for host review. It is **not**
silently removed, because the uncommitted candidate may be the primary review
artifact.

### Failure / cancellation

A failed or cancelled managed Goal is also retained if it contains candidate
changes or ambiguous side effects.

If it is mechanically proven pristine, it may be marked
`CLEANUP_ELIGIBLE`, but V1 still does not need to delete it automatically.

### Explicit cleanup

An explicit cleanup operation may:

1. reconcile exact durable ownership;
2. require no active writer/mutation intent;
3. require an exact clean worktree unless the user separately authorizes
   destructive handling;
4. unlock the exact owned worktree;
5. call normal `git worktree remove` without force;
6. verify that the path/registration is gone;
7. persist `REMOVED`.

The local branch is retained by default.

Branch deletion is a separate explicit action and is never implied by worktree
cleanup.

### Forbidden cleanup shortcuts

Managed Worktrees V1 must not automatically run:

- `git worktree remove --force`;
- `git worktree prune` as a destructive cleanup sweep;
- `git clean`;
- `git reset --hard`;
- `git stash`;
- branch deletion.

`git worktree prune --dry-run` may be used for diagnosis only.

---

## 20. Stale / abandoned worktree recovery

On Goal resume and before any managed operation, reconcile durable state with
Git's machine-readable worktree inventory.

Classify at least:

- ACTIVE_EXACT
- MISSING_PATH
- PRUNABLE_METADATA
- BRANCH_MISSING
- HEAD_MISMATCH
- BRANCH_MISMATCH
- COMMON_DIR_MISMATCH
- PATH_OWNED_BY_OTHER_WORKTREE
- LOCK_MISMATCH
- REMOVED_EXACT

Only exact, mechanically safe states may proceed automatically.

Abandoned worktrees are reported; they are not mass-pruned merely because they
are old.

The existing manually observed Codex worktree proves why ownership must be
exact rather than based on a directory naming pattern.

---

## 21. Goal revision and plan revision semantics

Workspace lifecycle changes are durable Goal mutations and increment
`Goal.revision`.

They do **not** increment `plan_revision` unless the Task DAG/completion
contract itself changes.

Expected sequence:

- Goal created: plan revision 0;
- creation intent/reconciliation: Goal revisions advance, plan revision remains
  0;
- workspace ACTIVE;
- Planner materializes initial plan: plan revision becomes 1 under existing
  rules;
- later worktree reconciliation may advance Goal revision without changing the
  plan revision.

Planner/worker/reviewer/verifier requests remain bound to the current Goal and
plan revisions.

---

## 22. Remote/publication authority

Managed worktree state grants zero remote authority.

In particular it does not authorize:

- fetch/pull that changes the chosen base;
- push;
- force-push;
- PR creation;
- Actions dispatch;
- tag creation/update;
- release publication.

Any future remote operation continues through its existing explicit authority
and side-effect rules.

---

## 23. Platform behavior

### Linux / macOS

The linked worktree execution root must be present in the sandbox/path
authority supplied to existing execution primitives.

The managed mode must not weaken:

- Landlock/bubblewrap behavior on Linux;
- Seatbelt behavior on macOS;
- ordinary network denial;
- path canonicalization/symlink-escape checks.

### Windows

Git worktree lifecycle may be implemented and tested on Windows, but:

- Windows remains experimental;
- managed worktrees do not create a Unix-equivalent process/filesystem/network
  sandbox;
- reparse/junction/path authority checks remain mandatory;
- host-native mutation remains approval-gated under current Windows policy.

No documentation may present managed worktrees as closing the current Windows
sandbox boundary.

---

## 24. Interaction with existing Codex model invocation

Local MCP owns worktree lifecycle.

Do not delegate lifecycle to the observed Codex CLI `--worktree` flag.

Reasons:

- the current production model seam is intentionally read-only;
- Goal JSON must remain authoritative;
- Local MCP needs durable ownership, crash recovery, path authority, revision
  binding, and exact cleanup semantics independent of a model CLI flag;
- the previously observed Codex CLI was explicitly not treated as a stable API
  for every visible flag.

The model may reason about the candidate; it never owns the linked worktree.

---

## 25. Test plan freeze

Implementation is not acceptable until tests cover the following groups.

### A. Opt-in / regression

- PRIMARY remains byte/behavior compatible where practical.
- Existing 18-tool behavior remains compatible except the explicitly additive
  `goal_start` field.
- Existing non-managed Goal fixtures remain valid.
- Existing single-writer tests remain green.

### B. Eligibility

- non-Git cwd blocked;
- subdirectory cwd blocked in managed V1;
- dirty tracked file blocked;
- staged change blocked;
- untracked file blocked;
- in-progress merge/rebase state blocked;
- detached source HEAD handled according to contract;
- existing managed branch collision blocked;
- target path collision blocked.

### C. Authority

- worktree root outside Session authority blocked;
- model cannot choose worktree path/ref/base;
- TaskScope cannot point back to primary root;
- writer cannot mutate primary root;
- writer cannot mutate linked-worktree `.git` file/admin data;
- symlink/junction escape remains blocked;
- Goal persistence cannot recreate an old approval.

### D. Creation / recovery

- PREPARED -> ACTIVE happy path;
- command failure with no side effect permits bounded retry;
- exact side effect after lost response is adopted;
- branch-only partial side effect blocks;
- registered-path mismatch blocks;
- HEAD mismatch blocks;
- common-dir mismatch blocks;
- stale/prunable metadata blocks without destructive prune;
- no `--force` path exists.

### E. Planner / writer / verifier routing

- Planner receives execution root only after ACTIVE;
- TaskScope absolute paths bind to worktree, not primary;
- writer request cwd is worktree root;
- verifier command/file/Git checks run in worktree;
- primary remains clean while writer candidate changes worktree;
- subsequent writer sees prior writer mutations in same worktree.

### F. Lease and recovery

- one managed Goal still permits only one mutating writer lease;
- performed/unknown mutation state holds lease;
- resume reconciles worktree before worker scheduling;
- worktree lifecycle recovery does not reset low-level side-effect budgets.

### G. Evidence / finalization

- task verification includes worktree identity;
- final workspace snapshot digest is deterministic;
- untracked writer-created files affect the digest;
- deleted writer-mutated files affect the digest;
- out-of-band candidate edit invalidates finalization;
- finalizer recheck must match prior snapshot digest;
- base advancement is reported without automatic rebase.

### H. Cleanup

- success retains locked candidate;
- dirty failed/cancelled candidate retained;
- exact clean explicit cleanup unlocks/removes without force;
- branch retained after worktree removal;
- ownership mismatch refuses cleanup;
- active writer/mutation intent refuses cleanup;
- no automatic stash/reset/clean/prune/branch-delete behavior.

### I. Cross-platform CI

- Linux managed lifecycle tests;
- macOS lifecycle/path tests where available;
- Windows Git worktree/path/reparse tests without claiming sandbox parity;
- existing legacy/fallback/security suites remain green.

---

## 26. Suggested implementation phases

Do not implement all of this in one patch.

Recommended bounded sequence:

1. **Schema + pure state model**
   - workspace mode
   - durable record/intent
   - validation only
   - no Git mutation

2. **Read-only discovery/reconciliation**
   - repository identity
   - worktree porcelain parser
   - eligibility
   - stale-state classifier

3. **Creation authority**
   - exact host Git command
   - PREPARED/ACTIVE recovery
   - explicit Session authority gate
   - no Planner yet

4. **Execution-root plumbing**
   - Planner
   - Writer
   - Verifier
   - readonly workers
   - regression tests proving PRIMARY unchanged

5. **Evidence/finalization**
   - worktree-bound verification
   - workspace snapshot digest
   - base-advanced reporting

6. **Conservative explicit cleanup**
   - exact ownership only
   - no force
   - branch retained

7. **CI + real-project acceptance**
   - exercise one real Goal in a managed worktree
   - prove primary worktree stayed unchanged
   - inspect restart/recovery behavior

Only after this closes should parallel writers be designed.

---

## 27. Explicit non-goals

Managed Worktrees V1 does not include:

- parallel writers;
- one-worktree-per-task;
- automatic commit;
- automatic merge;
- automatic rebase;
- automatic cherry-pick;
- automatic primary-branch update;
- automatic push/PR/release;
- automatic branch deletion;
- force worktree operations;
- automatic stash/reset/clean;
- replacing Session path authority;
- SQLite migration;
- native MCP Tasks migration;
- changing Windows support classification.

---

## 28. Contract verdict

The existing V1 architecture can support managed worktrees without replacing
its authority model, but only if the worktree becomes a first-class durable
host binding **before** Planner path materialization.

The safest first implementation is one explicitly requested worktree per Goal,
using the current single-writer lease and current execution/fallback/approval
authorities. The primary workspace remains identity/review authority; the
managed worktree becomes the effective execution root. Promotion remains a
separate explicit action.

**MANAGED_WORKTREES_V1_CONTRACT_FROZEN**
