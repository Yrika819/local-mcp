# Writer Integrity V1

Status: frozen design for `hardening/writer-integrity-v1`.
Base: `1c23f2279a4183e33fc3de3d1a9b646adda2a749` (`origin/main`, CI run `37091259710`,
11/11 success).

This document is reviewable independently of the code. It **narrows** what the Writer may
do. It does not widen Session authority, TaskScope authority, Goal state, model output,
or any host-native approval path.

Non-goals for this branch, explicitly:

- Managed Worktrees Phase 5. No `workspace_snapshot_digest`, finalizer worktree binding,
  `BASE_ADVANCED`, cleanup eligibility, unlock/remove/prune, merge/rebase/cherry-pick,
  push/PR/release automation, or parallel writers.
- Generic Resource Bounds hardening. No stdout/stderr ceiling, job TTL, finished-job GC,
  MCP frame/input ceiling, `read_file`/image size, directory entry limit, or server
  orphan recovery.
- Multi-file transactions, rollback, two-phase commit, repository-wide locks, or any
  automatic Git reset/revert of user work.
- Durable-schema changes. The existing `MutationIntent`/`MutationOperationIntent` states
  are sufficient and are preserved byte-for-byte.

## 1. The confirmed defects

All four were confirmed in the current production path before this branch.

**A. Preimage TOCTOU.** `materialize_operations()` (`src/writer.rs`) resolves path
authority, observes the target, verifies `expected_preimage`, and stores that observation
in `MaterializedWrite`. Between materialization and `WriteBoundary::write`, the host
persists a durable `MutationIntent` and crosses `BeginOperation`. `execution::write_file_content`
then wrote the target with **no revalidation**. An external edit in that window was
silently overwritten.

**B. Non-atomic target writes.** `execution::write_file_content` used, on Unix,
`sh -c 'cat > "$1"'`, and on Windows `tokio::fs::write`. Both update the destination
in place. A crash, error, or concurrent reader could observe a truncated or partially
written destination.

**C. User-facing `write_file` read failure.** `src/mcp.rs` did

```rust
let previous = tokio::fs::read_to_string(&absolute).await.unwrap_or_default();
```

A read error or non-UTF-8 existing file was treated as empty previous content, after
which the target was still overwritten.

**D. Lossy path identity.** `scope_identity` incorporated
`operation.path.to_string_lossy()`, and `MutationIntent::validate` de-duplicated
operations via `operation.path.to_string_lossy()`. Two distinct `PathBuf` values that
differ only in bytes not representable as UTF-8 collapse to the same lossy text, so
authority/evidence identities could alias.

## 2. The authority invariant (non-negotiable)

The current Unix Writer mutation runs **inside** the platform sandbox (bubblewrap on
Linux, Seatbelt on macOS) through `sandbox::run`, with the target's parent directory as
the only writable root. That containment is the security boundary and it is preserved.

The authority polarity is unchanged by this branch:

```
model proposes path + content + expected_preimage
        ↓
host resolves against execution_root, TaskScope, .git rules   (unchanged)
        ↓
host durably PREPAREs the MutationIntent                        (unchanged)
        ↓
host re-validates path authority AND preimage immediately pre-commit   (NEW, narrower)
        ↓
bounded host-selected mutation mechanism: the SAME sandboxed helper   (unchanged boundary)
        ↓
only the already-authorized parent/target is mutated
```

This branch **does not** move the target write into unrestricted host Rust filesystem
calls. §3 explains why that is the design.

### 2.1 Empirically validated sandbox properties

The design was validated against the real sandbox before any production change
(a temporary probe was added, run, and reverted). Observed on macOS/Seatbelt with the
workspace-write profile:

| Property | Observed |
|---|---|
| Exec of a host binary at a **non-system** absolute path | **succeeds** (binary ran; its own exit status was returned) |
| `rename()` within the writable root | **succeeds**, destination atomically replaced |
| Write **outside** the writable root | **denied** (`Operation not permitted`), no file created |
| `link()` onto an existing destination | **fails with `EEXIST`**, existing content untouched |

The same profile on Linux is bubblewrap-backed (`--ro-bind / /` read baseline plus
`--bind` for each writable root), and the requested command is exec'd inside the
namespace, so a host helper at an arbitrary path is reachable the same way. This is why
the atomic commit can run *inside* the sandbox rather than beside it.

## 3. Why a host-owned helper, and not host-native `std::fs`

The requirement is that atomicity must not be bought by widening mutation authority.
Two options were considered:

1. Perform staging and publication with unrestricted in-process `std::fs` calls.
2. Perform staging and publication with the **same sandboxed mechanism** already used
   today, changing only *what* is executed.

Option 1 would replace a kernel-enforced containment boundary with in-process
correctness. Any defect in the host's own validation would then write wherever the
process can write. That is strictly more authority than today. **Rejected.**

Option 2 keeps every existing containment property: the same sandbox profile, the same
writable root (the target's parent), the same refusal to escape. It replaces a shell
pipeline with a host-owned helper that performs the same mutation with atomic
publication and preimage revalidation. This is the chosen design.

The helper is:

- **host-owned**: a Cargo binary target in this repository, compiled and shipped by the
  release pipeline next to `local-mcp` — exactly like the existing
  `src/bin/codex-linux-sandbox.rs`. It is not a repository script, not model-selected,
  and not arbitrary shell code.
- **constrained**: it is exec'd *through* `sandbox::run` with the validated parent as
  the writable root, so it can only ever mutate the directory the host already
  authorized.
- **not the model**: the model supplies content and a relative path string. It never
  selects the helper, never supplies its arguments, and never supplies authority-bearing
  fields.

The helper is invoked with structured argv (`--parent`, `--target`, `--preimage-kind`,
`--preimage-sha256`, `--content-sha256`) and receives the content on stdin, so shell
quoting and path-encoding ambiguity are removed (this also closes defect C's sibling
concern on the Writer path).

## 4. The commit contract

For each Writer operation, in this exact order:

1. Model proposal validated (existing bounds and path rules).
2. Target resolved against `execution_root` (existing).
3. TaskScope `allowed_paths` / `forbidden_paths` / `.git` rules enforced (existing).
4. Initial preimage observed (existing).
5. Durable `MutationIntent` **PREPARED** persisted (existing).
6. **NEW** — commit-time revalidation immediately before publication:
   a. re-resolve the target and parent;
   b. re-check `execution_root` containment;
   c. re-check `.git` exclusion;
   d. re-check TaskScope `allowed_paths` and `forbidden_paths`;
   e. re-observe the target's digest;
   f. compare against the durable expected preimage.
7. Durable operation crosses **APPLYING** via `BeginOperation` (existing ordering; see §7).
8. Replacement content is staged in the target's own directory (§5).
9. **NEW** — publication is atomic at the filename/content level (§6).
10. **NEW** — parent-directory durability sync (§8).
11. Postimage observed and SHA-256 verified (existing).
12. Durable operation becomes **APPLIED** only after postimage verification (existing).

Steps 6 and 7 are ordered deliberately: the preimage gate runs *before* the durable
APPLYING boundary is crossed, so a stale preimage never produces a durable intent that
claims a mutation was in flight. The helper re-checks the preimage again as its very
last action before publication (§6), so the gate is enforced twice with only the
unavoidable publication window between.

If the commit-time revalidation fails, the Writer returns `PreimageMismatch`, which the
existing caller maps to `finish_valid_non_mutating_result` with `NeedsReplan`. No
durable intent exists, no target is touched, and the durable model stays consistent.

## 5. Same-directory staging and temporary-file ownership

Staging happens in the **same directory** as the destination, so publication is a
same-filesystem namespace operation. `/tmp` staging followed by a cross-filesystem copy
is not used and would not be atomic.

Requirements met by the staging step:

- **create-new semantics**: the temporary path is created with `O_CREAT|O_EXCL`
  (Rust `create_new(true)`) plus `O_NOFOLLOW` on Unix, so an existing or symlinked
  attacker-planted path is never opened or followed.
- **host-owned, durable identity**: the temporary name is
  `.{stem}.{hash16}.{request_id}.tmp` where `hash16` is derived from the destination
  file name and `request_id` is the **already-durable** `MutationOperationIntent.request_id`
  (a host-generated UUID persisted in the Goal store before any staging). A crashed
  process therefore leaves at most one debris file per durable operation, and the host
  can identify precisely which request owned it.
- **no repository-selected temp path**: the model never influences the temporary name.
- **regular-file validation**: the staged handle is `fstat`ed and must be `S_IFREG`.
- **full content write, flush, then `fsync` of the staged file** before publication.
- **cleanup on normal failure**: on any pre-publication failure the host removes only the
  exact path it created, and only after confirming it is a regular file.
- **no broad cleanup**: nothing in this branch deletes "files matching `*.tmp`".
  `secure_fs::remove_private_temp` is private-state code and is deliberately **not**
  reused for workspace files (§8).

**Crash between staging and publication** leaves one debris file named for a durable
request id. It is inert: it is never a target (the target name is unchanged), it is
never read back as target content, and recovery reconciles from the *target's* observed
digest, not from debris. A subsequent attempt for the same request id re-creates the
same name with `O_EXCL`; the host treats `EEXIST` on staging as a recoverable failure and
reports it rather than silently reusing unknown bytes.

## 6. Publication mechanism

The helper performs the commit with the platform's strongest appropriate primitive.
No shell is used for the commit on any platform.

### 6.1 Expected `ABSENT` — no-clobber

`ABSENT` requires a mechanism that does **not** replace a target that appeared
concurrently. A plain `rename()` would clobber it, so it is **not** used here.

- **Unix**: `link(staged, target)` followed by `unlink(staged)`. `link()` fails with
  `EEXIST` and leaves the external target byte-for-byte intact; it is atomic and
  same-filesystem. (Empirically confirmed above.) Hard-link support is not assumed
  beyond what the platform provides; a filesystem that refuses `link()` fails the
  operation rather than silently degrading to a clobbering rename.
- **Windows**: `CreateFileW`/`MoveFileExW` **without** `MOVEFILE_REPLACE_EXISTING`,
  which fails when the destination exists.

This satisfies the required invariant:

```
target absent at validation
external actor creates target
Writer publishes
=> Writer MUST NOT clobber the external target
```

### 6.2 Existing target

- **Unix**: `rename(staged, target)`, which atomically replaces the destination
  directory entry. The helper re-reads and compares the target's digest as its final
  action immediately before the `rename()` syscall.
- **Windows**: `MoveFileExW` **with** `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`
  (the same primitive already used by `secure_fs::atomic_replace`, so the semantics are
  not newly invented here).

### 6.3 The residual race, stated honestly

There is **no** portable, practical compare-and-swap-by-content primitive on any of the
supported platforms. Linux has no `renameat2` content-CAS, and macOS has none either. A
hash check immediately followed by `rename()` is therefore **not** a cross-process
compare-and-swap, and this document does not claim it is.

What this branch actually closes is the **large** materialization→write race: today that
window spans materialization, durable intent persistence, and lease acquisition. After
this branch the window is bounded by the helper's own last-instruction sequence —
revalidate, then publish — with no durable-store round trip and no second process spawn
in between.

That residual window is a real, documented limitation, not a solved problem.

## 7. Durable mutation state ordering

The existing `MutationIntent` states (`PREPARED`, `APPLYING`, `APPLIED`) and
`MutationOperationIntent` states (`PREPARED`, `APPLYING`, `APPLIED`) are preserved
exactly. No new durable state is added, and no serialized field changes shape.

Ordering guarantee, matching §4:

- `PREPARED` is persisted **before** any target mutation.
- The commit-time preimage gate runs **before** `BeginOperation`, so a stale preimage
  cannot be recorded as an in-flight mutation.
- `BeginOperation` (which crosses the intent to `APPLYING`) is persisted **before**
  publication.
- `CompleteOperation` (`APPLIED`) is persisted **only after** postimage verification
  succeeds.

Because the sandboxed helper is spawned between `BeginOperation` and publication, a
crash in that span leaves `APPLYING` with an unproven target — exactly the state the
existing reconciliation model already classifies from evidence.

## 8. Durability and file metadata

`secure_fs::sync_parent_directory` and `secure_fs::create_unique_temp` are **not**
reused for workspace files. Both encode private-GoalLatch-state policy:

- `sync_parent_directory` opens through `open_private_directory_file`, which **`fchmod`s
  the directory to `0700`** and requires euid ownership. Applying that to a user's
  project directory would rewrite its permissions merely to fsync it. Workspace
  directories get a new, separate primitive that opens the parent `O_RDONLY|O_DIRECTORY`
  and `fsync`s it with **no** `chmod` and no ownership mutation.
- `create_unique_temp` forces `0600` and calls `validate_private_regular`, which
  `fchmod`s to `0600`. That is correct for private state and wrong for workspace files.

Staged-file permissions:

- **Existing target**: the destination's mode is read before staging and applied to the
  staged file via `fchmod` before publication, so **an existing executable file does not
  silently lose its executable bit**, and an ordinary non-executable file does not
  silently become executable.
- **New file**: created with ordinary workspace semantics (`0o666` masked by umask),
  not private-state `0600`.

**Stated metadata limitation.** Only Unix permission bits are preserved. Ownership,
ACLs, xattrs, and macOS resource forks are **not** preserved across the replacement,
because the staged inode is a new object. On Windows the read-only attribute is not
carried over, and `MOVEFILE_REPLACE_EXISTING` inherits the destination's security
descriptor in the normal way. This is reported rather than hidden; widening the metadata
contract would need a broader platform design that is out of scope here.

## 9. Path-identity encoding

Two distinct changes, both narrowing:

1. **`scope_identity` (authority/evidence identity).** Rebuilt with a version tag and
   length-prefixed framing over a lossless path byte representation:
   - Unix: `OsStrExt::as_bytes()`.
   - Windows: `OsStrExt::encode_wide()` code units, little-endian.

   The preimage is framed as `b"local-mcp/scope-identity/v1"` followed by, for each
   operation in order, `u32le(len) || path_bytes || u32le(len) || content_digest_hex`.
   Length-prefixing makes the framing unambiguous, so `(path A, content B)` cannot
   collide with `(path C, content D)` through ambiguous concatenation. The
   version tag means an old value can never be confused with a new one.

   *Compatibility:* `scope_identity` is an opaque stored string that is never compared
   for equality across goals and never re-derived from older records; it is only checked
   for length bounds. Existing durable records therefore remain valid and readable. The
   value changes only for newly created mutation intents.

2. **`MutationIntent::validate` de-duplication.** Deduplicates on the lossless path byte
   representation instead of `to_string_lossy()`, so two distinct non-UTF-8 paths are no
   longer rejected as duplicates and, more importantly, are never conflated.

User-visible display formatting is unchanged. Nothing about how paths are *shown* to the
model changes; only the internal identity encoding becomes lossless.

## 10. Multi-file semantics are not a transaction

Writer operations remain sequential and independently atomic. A process may crash
after operation N and before operation N+1. This branch **does not** add rollback,
two-phase commit, repository-wide locking, or any automatic Git reset. The existing
`Partial` / `Unknown` reconciliation states already represent this and are used as-is.

## 11. Crash-boundary reconciliation table

Reconciliation continues to be driven by evidence observed at the *target* paths. No
case is inferred from the presence or absence of a staged debris file.

| Boundary | Durable state | Target observation | Decision |
|---|---|---|---|
| A. after durable `PREPARED`, before staging | `PREPARED` | all match preimage | `ReconciledNotPerformed` |
| B. during staging | `PREPARED` | all match preimage | `ReconciledNotPerformed` |
| C. after staged `fsync`, before gate | `PREPARED` | all match preimage | `ReconciledNotPerformed` |
| D. after commit-time gate, before `BeginOperation` | `PREPARED` | all match preimage | `ReconciledNotPerformed` |
| E. after `BeginOperation`/`APPLYING`, before publication | `APPLYING` | all match preimage | `Unknown` (conservative: publication unproven) |
| F. immediately before publication | `APPLYING` | all match preimage | `Unknown` |
| G. immediately after publication | `APPLYING` | all match intended | `ReconciledPerformed` |
| H. after postimage verification | `APPLYING` | all match intended | `ReconciledPerformed` |
| I. before `CompleteOperation`/`APPLIED` | `APPLYING` | all match intended | `ReconciledPerformed` |
| J. between two operations, N after / N+1 before | `APPLYING` | mixed | `Partial` |
| any | any | third-party content | `Unknown` |

`ReconciledNotPerformed` and `ReconciledPerformed` are the existing reconciliation
outcomes of the existing `classify_observations`/`reconcile_goal_mutations` model; no
reconciliation logic is invented here. Blind retry remains forbidden for `Partial` and
`Unknown`.

## 12. User-facing MCP `write_file`

The confirmed read-failure defect is closed. `write_file` now:

- **existing target**: a read error **fails before any mutation**, and existing
  non-UTF-8 content **fails before any mutation** for this UTF-8 text tool. Neither is
  silently treated as empty.
- **absent target**: absence is a valid empty previous state for diff purposes, and the
  `ABSENT` no-clobber guarantee applies.
- captures the real preimage digest (or `ABSENT`) and rejects a concurrent modification
  detected between capture and publication.
- uses the same host-owned publication primitive, so it no longer truncates in place.
- keeps the existing Windows `write_file_host_native` approval gate unchanged, and does
  not report `Edited` when publication failed.

## 13. Failure semantics

The public schema is not expanded. Internally the Writer distinguishes, and reports
with distinct messages, the cases that matter to recovery:

preimage mismatch, path-authority changed, staging failure, staged-write failure,
staged-sync failure, publication conflict (expected-`ABSENT` target appeared),
publication I/O failure, and postimage mismatch. Temp cleanup never masks the primary
causal error, never deletes an unproven path, and never converts an uncertain target
state into "not performed".

## 14. Preserved Phase 4 / security-closure behavior

Unchanged and covered by tests: `Goal.cwd` remains primary identity; a managed `ACTIVE`
execution root is used for mutations; `PRIMARY` behavior is unchanged; exact Session
authority is still required; TaskScope allowed and forbidden paths are enforced; `.git`
internals remain forbidden; Unix symlink escape and Windows junction/reparse escape
remain refused; managed Git-compatible path spelling is preserved; the single Writer
mutation lease is preserved; no permitted directory is widened.

## 15. Explicit non-goals restated

No Resource Bounds work. No Managed Worktrees Phase 5. No PokéCPU. No durable-schema
migration. No multi-file transaction. No automatic rollback of user work.