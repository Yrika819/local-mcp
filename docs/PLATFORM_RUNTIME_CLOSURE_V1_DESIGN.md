# Platform Runtime Closure V1 — Design

Status: frozen design, implemented on `hardening/platform-runtime-closure-v1`.
Baseline: `f4bfb9e0c339d0338825a5bdb0ed7a6d994322b1` (`main`, CI 37305138421, 11/11 green).

## Purpose

Resource Bounds V1 closed unbounded *growth*. It explicitly deferred process
*lifetime*: "Linux `PR_SET_PDEATHSIG`, watchdog processes, a persistent PID registry,
or a post-crash orphan sweeper" were named non-goals, and abnormal parent death was
recorded as a later concern.

This design closes the lifetime half. It answers one question per platform:

> When Local MCP stops owning an execution tree — by finishing it, timing it out,
> cancelling it, stopping it, shutting down, or dying outright — what actually
> happens to the descendants?

It answers that question **per platform, honestly**. It does not claim a universal
guarantee it cannot deliver.

## Non-goals (explicitly out of scope)

- Managed Worktrees Phase 5, worktree cleanup, parallel writers.
- `Goal::validate` optimization, Replanner request-window redesign,
  `goal_status`/`goal_result` pagination.
- A shipped supervisor/helper binary. See §9.
- Widening Session, Task, or Writer authority. This design only narrows.
- Any change to approval consent semantics. See §8.
- Retry, fallback, or approval authority of any kind.

## Authority invariants preserved

This is a *lifetime* policy, not an authority policy. Nothing here may:

- grant retry, fallback, or approval authority;
- convert a lifecycle failure into a permission, safety, or tool-missing verdict;
- treat "the host is gone" as proof that a command did not run.

The last point is load-bearing and is §7.

## 1. The inventory that motivates this design

Every production child spawn in the repository, after this design is applied:

| # | Site | API (before) | Process group | Timeout |
| --- | --- | --- | --- | --- |
| 1 | `sandbox::run_tracked_with_path` (sandboxed) | `ProcessGroup::spawn` | yes (Unix) | none by design |
| 2 | `sandbox::run_unrestricted_inner` (host-native) | `ProcessGroup::spawn` | yes (Unix) | none by design |
| 3 | `sandbox::run_unrestricted_clean_raw_with_limits` (trusted Git) | `ProcessGroup::spawn` | yes (Unix) | caller-supplied |
| 4 | `agent::run_bounded_process_async` (model/agent) | `ProcessGroup::spawn` | yes (Unix) | caller-supplied |
| 5 | `managed_worktree_observe::HostGit::run` | `std::process::Command::output()` | **no** | **none** |
| 6 | `managed_worktree_create::HostWorktreeCreator::create` | `std::process::Command::output()` | **no** | **none** |
| 7 | `bubblewrap_support::probe` (Linux) | `std::process::Command::output()` | no | **none** |

Sites 5, 6 and 7 are the defect this design closes. Sites 1–4 already had the Unix
ownership proof and are hardened, not restructured.

Sites 5 and 6 matter more than their size suggests. `HostWorktreeCreator::create`
performs `git worktree add`, a **mutating** operation, on the **Managed Worktrees**
authority path, and it blocks a runtime thread with no deadline: a hung Git holds
that thread forever and the durable creation sequence never reconciles. `HostGit`
fans out to roughly a dozen sequential blocking Git children per observation.

## 2. The Windows defect this design exists to fix

`ProcessGroup` is Unix-first. On Unix it places every child in a new process group
and signals the whole group. On Windows the `#[cfg(unix)]` guards leave only:

```rust
let _ = self.child.start_kill();
```

`start_kill` terminates **the direct child and nothing else**. So on Windows today:

- a command that spawns a descendant and then hangs leaves that descendant running
  after a timeout;
- a command that floods its output bound is terminated, and its descendants keep
  running;
- `stop_job` aborts the task, drops the lease, and signals one process;
- server shutdown (`release_all_jobs`) leaves every descendant running.

This is not an abnormal-death edge case. It is the ordinary path on a supported
platform, and it is the largest single defect found.

## 3. What "process ownership" means here

Local MCP owns an execution **tree**, not a process. Ownership is a property of the
*lease*, not of a remembered integer:

- The lease is acquired at spawn.
- The lease is released when the `ProcessGroup` value is dropped.
- **A group may only be signalled while the lease is alive.**

On Unix this is enforced by the unreaped-leader proof in `process_group.rs`: the
group leader is this process's own direct child and is never reaped while the lease
lives, so the kernel cannot recycle the group identifier. That proof is **unchanged**
by this design. See §5.

## 4. Lifecycle guarantees

Guarantees are stated per platform. "Tree" means the leader and every descendant
that has not deliberately escaped containment.

| Event | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Ordinary completion | leader reaped; group terminated | same | job terminated, tree reaped |
| Timeout (trusted Git / model) | group SIGKILL | group SIGKILL | job terminated |
| Output overflow | group SIGKILL | group SIGKILL | job terminated |
| `stop_job` | group SIGKILL | group SIGKILL | job terminated |
| Cancellation (dropped future) | group SIGKILL | group SIGKILL | job terminated |
| stdin EOF / server shutdown | `release_all_jobs`, every job terminated | same | same, jobs terminated |
| **Host process graceful exit** | tree terminated | tree terminated | tree terminated |
| **Host process abnormal death** | **residual gap, §6** | **residual gap, §6** | **tree terminated, §6** |

"Leader reaped; group terminated" on Unix is deliberate and pre-existing: after the
leader finishes, the group is still signalled so no descendant survives the command.

## 5. Unix: the ownership proof is preserved, not weakened

The invariant below is unchanged and remains the load-bearing security property:

> A negative process-group signal may be issued **only** while Local MCP still holds
> a host-owned proof that the group identifier belongs to the process tree it
> launched.

Specifically this design does **not**:

- issue `kill(-pgid, …)` because an integer PGID was remembered;
- reap the group leader while the lease needs its identifier;
- reintroduce a PID/PGID reuse race;
- use a global process scanner as cleanup authority.

`ProcessGroup::terminate()` remains callable repeatedly, and every call still lands
inside the proof's lifetime.

## 6. Abnormal host death, per platform

This is where the platforms genuinely differ, and where the honest answer is not
uniform.

### 6.1 Windows — strong guarantee

Windows **Job Objects** provide kernel-enforced tree containment. Each execution
tree is assigned to its own Job, and the Job carries
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.

That limit is what makes the guarantee strong: the Job is killed when the last
handle to it closes, and **Windows closes every handle a process owns when the
process terminates**, including on abnormal termination. A Local MCP process that
is killed, crashes, or is terminated by any means therefore has its Jobs closed by
the kernel, and every process in every Job is killed.

Descendants are included automatically: a process in a Job puts its children in the
same Job unless it requests `CREATE_BREAKAWAY_FROM_JOB`. The Job does not permit
breakaway.

### 6.2 Linux — residual gap, stated plainly

Linux has no kernel primitive that kills a *tree* when a parent dies.

`PR_SET_PDEATHSIG` is **direct-child only**. If the leader has already forked a
descendant, killing the leader leaves the descendant running. Adding
`PR_SET_PDEATHSIG` and declaring the orphan problem solved would be exactly the fake
fix this design refuses (§9). It is therefore **not** added.

What Linux does get: nothing new for abnormal death, and everything else in the
table above.

### 6.3 macOS — residual gap, stated plainly

macOS has no parent-death signal at all, and no Job-Object equivalent. Process
groups remain, and are signalled on every orderly path. On abrupt host death,
descendants may survive.

### 6.4 What is not claimed

No mechanism in this design, on any platform, cleans up after:

- power loss,
- kernel panic / OOM kill,
- machine crash,
- `SIGKILL` of the *whole* process tree at once on Linux/macOS.

GoalLatch does not claim those, and must not grow a mechanism that implies it.

## 7. Side-effect semantics under abnormal termination

Abnormal lifecycle cleanup must never fabricate `ConfirmedNotPerformed`.

- Termination of a spawned tree says nothing about whether the command ran. It is
  recorded as unknown/fail-closed under the existing authority model.
- The retry budget is **not** restored by this design.
- Resource Bounds overflow semantics are unchanged. An overflow still reports the
  command as started, because the host launched and observed it.
- A containment failure is reported as a lifecycle failure. It is never re-derived
  from discarded output text, so a command cannot forge a verdict by flooding bytes.

## 8. Windows approval wait — audited, classified, unchanged

The `approvals::request` reply read is bounded for *connect* (`ERROR_PIPE_BUSY`,
5 s) but **unbounded for the reply** (`BufReader::read_line`, no timeout). The
classification is:

> **bounded-availability-defect (P2), not an intended-interactive-wait, and not
> unreachable.**

A missing session fails fast on both platforms (`ENOENT` on Unix,
`ERROR_FILE_NOT_FOUND` on Windows — neither is `ERROR_PIPE_BUSY`). But when the
session exists and no human answers, the future never resolves. It holds an
`EXECUTION_PERMITS` slot, so enough unanswered approvals can exhaust host-native
execution.

It is **not fixed here**, because bounding it changes user-consent semantics: a
timeout would turn "waiting for a human" into "denied by default", which is a
product decision. Recorded as a separate item (§10).

`release_session` likewise keeps its honest design. There is no product event
meaning "this Session is permanently ending", so no caller is invented.

## 9. Why there is no supervisor helper in V1

A host-owned supervisor process would give Linux and macOS the tree guarantee
Windows gets from the kernel. It is deliberately **not** introduced here:

- it must ship in every release archive, and the Writer Integrity review already
  demonstrated how a helper that exists in source but not in packaging fails;
- it adds a second lifetime to reason about, bounded IPC, and a new failure mode
  on the path that is currently simplest;
- its benefit is bounded to two platforms' abnormal-death path, which is the
  residual gap already stated honestly in §6.

§10 records it as the next bounded phase, with the constraints it must satisfy.

## 10. Residual risks and next phase

**Deliberately not fixed, with reasons:**

- **Linux/macOS abnormal host death** (§6.2, §6.3). Needs a supervisor helper.
  Next phase, subject to: ships in release archives, structured host-owned
  arguments only, no repository-selected code, no TaskScope widening, bounded IPC
  with bounded frames and bounded waits, deterministic shutdown.
- **Windows approval reply is unbounded** (§8). Product decision on consent.
- **`bubblewrap_support::probe` has no timeout.** It runs before anything is
  spawned, argv is fixed to `--version`, and it feeds a fail-closed gate, so it is
  low severity. Left for a later phase.

**Explicitly out of scope for this branch and its follow-ups:** Managed Worktrees
Phase 5, `Goal::validate` complexity, `goal_status`/`goal_result` pagination,
Replanner window redesign, Writer metadata redesign, full filesystem CAS, parallel
Writers.

## 11. Compatibility

No protocol change. No schema migration. No new binary. No new dependency. Behavior
changes are intentional:

- on Windows, descendants of a timed-out, stopped, cancelled, or shut-down
  execution are now terminated (previously they survived);
- on every platform, the Managed Worktrees Git seams are now bounded and contained;
- the Unix process-group ownership proof is unchanged.

The public project brand, compatibility identifiers, and historical release notes
are untouched.

## 12. Verification obligations

Runtime behaviour must be proven with **real processes and a descendant witness**,
never by a direct-child PID and never by `kill(pid, 0)` alone (a zombie satisfies
it). Termination is observed through a witness whose lifetime the kernel decides —
a pipe EOF or a closed handle — not through a sleep.

Windows Job Object behaviour is only provable on Windows. A `cfg(windows)` compile is
not evidence; the branch must be green on the full 11-job matrix, including
`test (windows-x64)` and `compat (windows-2022)`.