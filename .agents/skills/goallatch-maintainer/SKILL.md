---
name: goallatch-maintainer
description: Maintain GoalLatch safely. Use for substantial implementation, refactoring, architecture, Goal/Task Orchestrator, sandbox/approval/fallback, Managed Worktrees, CI/release, security-boundary, persistence, or cross-platform changes in this repository. Reconcile repository state first, preserve host-owned authority, follow frozen design contracts, keep compatibility identifiers stable unless explicitly migrating them, and verify changes before commit or push.
---

# GoalLatch Maintainer

Use this skill for non-trivial GoalLatch maintenance. Treat the repository's checked-in
contracts as authoritative; this skill is a workflow guardrail, not a substitute for
reading the relevant design.

## 1. Establish authority and scope before editing

Before changing code or repository state:

1. Read the relevant current sources of truth:
   - `README.md` for public behavior and product identity.
   - `SECURITY.md` for security and platform boundaries.
   - `CONTRIBUTING.md` for contributor verification expectations.
   - `docs/GOAL_TASK_ORCHESTRATOR_V1_DESIGN.md` for Goal/Task semantics.
   - `docs/MANAGED_WORKTREES_V1_DESIGN.md` for any managed-worktree work.
   - `docs/release/RELEASE_CHECKLIST.md` for release/tag/publication work.
2. Inspect `git status --short --branch`, current HEAD, current branch, and worktree
   inventory when worktrees are relevant.
3. Identify whether the task is:
   - read-only investigation,
   - documentation/metadata only,
   - bounded implementation,
   - security/authority-sensitive implementation,
   - release/publication work.
4. Keep the requested phase boundary. Do not opportunistically implement later
   phases of a frozen design.
5. If a design document conflicts with this skill, the design document wins.

## 2. Non-negotiable authority model

Preserve these properties unless an explicit, reviewed migration changes them:

- Goal state does not grant filesystem, network, host-native, Git, publication,
  approval, or fallback authority.
- Effectful operations continue through existing host-owned authority and approval
  paths.
- Safety and platform refusals are terminal; do not reinterpret them as permission
  failures or fallback opportunities.
- Model output proposes work; it does not become authority by being persisted,
  planned, reviewed, or retried.
- V1 model processes used by the Goal Orchestrator remain read-only. Writer is a
  proposal role; host code applies accepted mutations.
- Existing side-effect budgets, attempt budgets, preimage/evidence rules, and
  reconciliation rules remain authoritative.
- Unknown or ambiguous side effects must be reconciled before retry.
- Existing single-writer/mutation-lease semantics remain in force unless a later
  approved design explicitly replaces them.
- Do not silently widen session filesystem roots or infer approval from durable
  Goal state.

For security-sensitive changes, actively look for ways the patch could accidentally
convert state, labels, paths, retries, or model output into new authority.

## 3. Managed Worktrees rules

For any Managed Worktrees task, read
`docs/MANAGED_WORKTREES_V1_DESIGN.md` before editing.

At minimum preserve:

- one non-terminal Goal -> at most one managed linked worktree in V1;
- managed worktree setup occurs before Planner path materialization;
- `Goal.cwd` remains the primary/session identity root;
- managed execution uses a separate host-derived execution root;
- the model never chooses worktree path, branch name, base revision, repository
  common directory, cleanup action, merge action, or remote;
- session authority is never silently expanded to include the managed root;
- no parallel writers are introduced in Managed Worktrees V1;
- no automatic stash, reset, clean, force worktree operation, branch deletion,
  merge, rebase, push, force-push, PR creation, or release publication;
- crash/timeout never proves a Git mutation succeeded;
- recovery uses exact durable ownership and machine-readable Git evidence;
- primary-workspace dirtiness is never hidden or silently copied;
- Windows remains experimental and managed worktrees do not claim Unix sandbox
  parity.

When the requested phase is **Schema + pure state model**, do not perform or add
Git worktree mutation. Keep it to schema, data structures, validation, migration/
compatibility semantics, and pure tests.

## 4. Compatibility and project identity

The public project brand is **GoalLatch**.

Until an explicit migration says otherwise, preserve these compatibility
identifiers:

- repository path: `Yrika819/local-mcp`
- Cargo package: `local-mcp`
- executable / CLI examples: `local-mcp`
- existing release asset naming
- durable store identifiers such as `local-mcp-goal`
- existing internal/ref prefixes that are part of frozen contracts

Do not perform a broad `Local MCP -> GoalLatch` replacement across code or stored
identifiers.

Historical release notes must continue to describe the historical release
truthfully.

## 5. Git and repository safety

Default to conservative Git behavior.

Do not, unless the user explicitly authorizes the exact action:

- force-push;
- rewrite public history;
- `git reset --hard`;
- `git clean`;
- stash user work;
- delete branches or tags;
- remove worktrees;
- run destructive `git worktree prune`;
- use forced worktree add/remove;
- overwrite unrelated uncommitted work;
- publish a release or create/update a release tag.

Never assume this is the repository's only linked worktree.

Before committing:

- review `git diff --check`;
- review the complete diff and changed-file list;
- verify no unrelated files were modified;
- ensure generated/cache/build outputs are not accidentally staged.

Before pushing:

- confirm the intended branch and remote;
- confirm the commit contains only the authorized scope;
- report the exact commit SHA after the push.

## 6. Implementation style

Prefer small, reviewable, contract-driven patches.

For a large task:

1. Reconcile current behavior against the frozen design.
2. State the bounded phase and explicit non-goals.
3. Implement the smallest coherent slice.
4. Add focused tests that prove the new invariant or behavior.
5. Run focused checks before broad checks.
6. Review the diff before starting the next slice.
7. Keep architecture changes separate from cleanup-only churn.

Do not mix unrelated formatting, refactors, dependency upgrades, branding changes,
or release work into an authority-sensitive patch.

When a failure appears, diagnose the first causal error rather than patching
downstream symptoms.

## 7. Verification

Choose verification proportional to the change.

Baseline expectations:

- `cargo fmt --check` for Rust changes;
- focused tests for the changed module/contract;
- `cargo test --locked --all-targets` when practical for implementation changes;
- `cargo clippy --locked --all-targets --all-features -- -D warnings` for code
  changes when the current repository baseline supports it;
- `git diff --check`;
- GitHub Actions for the authoritative cross-platform matrix.

For CI/workflow changes, inspect the affected workflow semantics and verify the new
run rather than treating YAML parsing alone as sufficient.

For security/authority changes, verification must include negative tests proving
that forbidden authority is still refused.

For user-visible behavior changes, load the project/global `regression-review`
skill when available. For a substantial implementation diff, load `code-review`
for one bounded post-implementation review before declaring the work ready.

## 8. CI and platform interpretation

Do not infer cross-platform security parity from successful compilation.

- macOS and Linux have their documented sandbox boundaries.
- Windows support remains experimental where `SECURITY.md` says so.
- A compatibility runner proves only what its job actually checks.
- Keep build caches isolated when ABI/toolchain/runner differences can make cached
  artifacts unsafe to reuse.
- Prefer dependency/download caching over sharing compiled `target/` outputs
  across materially different compatibility environments.

If one matrix leg fails while others pass, inspect that leg's first causal error
before changing production code.

## 9. Completion report

At the end of a task, report:

- what changed;
- what intentionally did not change;
- tests/checks run and their results;
- CI run/status if applicable;
- exact commit SHA and push status if committed;
- remaining risks, blocked items, or next bounded phase.

Never claim green CI before the run is actually complete.
