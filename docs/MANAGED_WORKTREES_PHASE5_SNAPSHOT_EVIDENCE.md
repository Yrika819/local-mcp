# Managed Worktrees V1 — Phase 5 Snapshot Evidence (contract amendment)

Status: **NARROW ADDITIVE AMENDMENT — Snapshots 2A/2B/2C contract-frozen; 2D
coherence recheck defined**

This document is additive to `docs/MANAGED_WORKTREES_V1_DESIGN.md` (especially
section 15) and to the frozen Slice 1 pure contract in
`src/workspace_snapshot.rs`. It does **not** reopen the Goal V1 authority model,
the Managed Worktrees V1 isolation contract, Windows experimental status, or the
Slice 1 canonical encoding.

It records one narrow, durable decision (the 2D content-budget contract) and
defines the bounded **snapshot coherence recheck** (Slice 2D) that assembles the
already-frozen observers (2A metadata, 2B worktree objects, 2C index blobs) into
one sampled-stable `WorkspaceSnapshotManifestV1` candidate without producing the
manifest itself.

---

## 1. The content budget decision (resolves the 2D design blocker)

```
MAX_SNAPSHOT_CONTENT_BYTES = 256 MiB
```

is the maximum **semantic content evidence** represented by one successful
`WorkspaceSnapshotManifestV1`. It is **not** a total lifetime physical read
count of every validation pass.

Slice 2D is authorized one **additional independent validation-only content
pass**. Therefore the two physical passes are each independently bounded:

- **PASS A** physical hash/read budget: `<= 256 MiB`
- **PASS B** coherence-recheck physical hash/read budget: `<= 256 MiB`
- Maximum successful 2D content hashing work: `<= 512 MiB`

Explicitly **excluded** from these content budgets and separately bounded:

- Git metadata output (Slice 2A process capture bound);
- MCP/pipe protocol headers and batch responses (`HEADER_LIMIT`,
  `STDERR_LIMIT`);
- stderr evidence;
- filesystem metadata (`fstat`/`metadata` witnesses, not file bodies).

This budget decision **does not increase the final snapshot limit**. Final
semantic evidence remains `<= 256 MiB`, exactly as Slice 1 requires.

## 2. Semantic vs physical accounting

These are distinct concepts and must remain distinct.

### A. Physical pass budget

Counts bytes actually read and hashed in **one** content pass. It is enforced by
`SnapshotHashBudget`, which is checked-add capped at
`MAX_SNAPSHOT_CONTENT_BYTES` (256 MiB). Pass A owns one fresh budget; Pass B owns
a second fresh budget. Each is independently capped at 256 MiB.

Within a pass a pass-local **content cache** avoids duplicate physical reads:
one requested index OID whose raw bytes are byte-identical is read and hashed at
most once per pass, and one unique physical worktree path is opened and hashed at
most once per pass.

### B. Semantic evidence budget

Counts identity sizes exactly as the final Slice 1 manifest does, including
**logical duplication**. This budget is computed over the stable identities, not
over physical reads:

- for each Git-visible record: `index_object.size_bytes() +
  worktree_object.size_bytes()`;
- for each writer-mutated `Present`: `object.size_bytes()`;
- for each writer-mutated `Absent`: `0`.

Logical duplication is counted, for example:

- a path that is both a Git-visible worktree object and a writer-mutated object
  hashes the physical worktree file once per pass, but its semantic bytes appear
  twice in final evidence (once as the Git-visible `worktree_object`, once as the
  writer-mutated `object`);
- two logical index paths pointing at the same blob OID read the physical blob
  bytes once per pass via the pass-local cache, but the final evidence counts the
  blob once per represented path.

Before 2D returns success, **semantic evidence bytes must be `<= 256 MiB`**. 2D
fails closed with `LimitExceeded` rather than deferring discovery of obvious
over-budget evidence to a later slice.

The pass-local cache reduces physical reads only; it never reduces semantic
evidence accounting.

## 3. 2D authority, sampling, and residuals

Slice 2D samples the workspace with the fixed sequence
`AUTHORITY A → META A → CONTENT A → META B → CONTENT B → META C → AUTHORITY C`,
requires `META A == META B == META C` under a canonical equality projection,
requires the two content passes to be identity-equal at every path, re-proves
authority at C, and checks the semantic evidence bound before returning a
`StableWorkspaceObservation`. There is no retry.

`HOST_FILESYSTEM_STALL_RESIDUAL` remains accepted: regular filesystem syscalls do
not have a proven hard wall-clock deadline, so 2D adds no leaking timeout threads
and never reinterprets a stall as success.

`NON_TRANSACTIONAL_ABA_RESIDUAL` is accepted: 2D proves matching stable sampled
states. It does **not** prove that no transient mutation occurred at any instant
between samples; a `A → B → A` sequence falling entirely between samples may be
invisible. 2D does not claim temporal-history integrity.

## 4. What 2D deliberately does not do

- 2D does not assemble `WorkspaceSnapshotManifestV1` (a later slice does).
- 2D does not read or mutate the primary branch/base (`BASE_ADVANCED` belongs to
  later finalizer/review logic per design section 16).
- 2D does not query `TaskStore`; writer-mutated paths are supplied as
  already-validated input and the managed authority is re-proven through the
  existing `managed_worktree_prepare::validated_execution_root`.
