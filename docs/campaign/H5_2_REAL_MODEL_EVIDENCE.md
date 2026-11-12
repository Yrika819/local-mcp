# H5.2 — Real-model validation evidence

Campaign: GoalLatch H5.2 — Real-Model Validation
Branch: `feat/h5-2-real-model-validation-v1` (from H5.1 final HEAD `31540b7`)
Worktree: `.local/h52`

## 0. Model policy

Per the Owner's instruction, every Codex invocation in this leg is explicitly
dispatched on **`gpt-6.1-terra`**, reasoning **`high`**:

* `codex exec --sandbox read-only --skip-git-repo-check -m gpt-6.1-terra -C <repo> -`
* the prompt is passed on stdin (a Replanner request can exceed the Windows
  ~32 KiB command-line limit);
* no silent fallback to another model. Every recorded model call below says
  `codex gpt-6.1-terra (codex exec, sandbox=read-only)`.

Binary used for the runs:

* `local-mcp` debug build with `--features test-grant-fixture`
* SHA-256 `859b1bacfa857d622302a5776eefdd4960db217f3202d0ef09a4099b08bf134f`
* `atomic-publish` built alongside it.

Each scenario uses a throwaway state root and workspace under
`%TEMP%\goallatch-h5d-scenario-*`. The operator's primary checkout and default
state root are never used.

## 1. H5.2-0 — harness integration

* The H5-C `test-grant` command is used only from the isolated driver. It is
  compiled only under the non-default `test-grant-fixture` feature.
* The driver writes the documented `test-grant-fixture.json` marker in the
  disposable state root. The production reader independently enforces:
  absolute/canonical state root, not the default root, a disposable parent
  without `.git`, and a workspace sibling under the same parent.
* The grant carries `owner_attestation: TEST_FIXTURE_V1` and a fixture id.
  Publication, the mutation journal, the Reviewer, the Task Verifier, the Goal
  Verifier and the Finalizer are the production paths.
* The fixture grant is issued **after** `goal_start` and **before** the first
  `goal_next`, because the Planner step is refused with
  `WRITE_AUTHORIZATION_UNAVAILABLE` until a usable grant exists. This is an
  honest property of the production gate, not a bypass.
* No MCP tool was added. No production Owner approval was automated.

## 2. H5.2-A — Scenario A regression (real model)

Command:

```
python scripts/h5-d/real_model_scenario_a.py \
  --binary ../../.local/h51-target/debug/local-mcp.exe \
  --work-dir tmp/h5d/scenario-a-h52
```

Result: **PASS**.

| Item | Observed |
| --- | --- |
| Goal | `a6f540db-a69d-472e-9bea-7a6e2fa4af01` |
| Final status | `COMPLETED` |
| Model | `codex gpt-6.1-terra` (2 calls) |
| Contracts | Planner `…/PLANNER/planner-proposal-v1`; Readonly `…/READONLY/readonly-investigation-v1` |
| Negative control | bogus `status=COMPLETED` + empty tasks → `REJECTED_SCHEMA` / `PROPOSAL_SCHEMA_VIOLATION` |
| Workspace | unchanged (digest equal) |
| `codex_spawned_for_goal` | 0 (the host did not spawn Codex itself) |

This is a new inference, not a replay of the H5-D answer; the prompts and
answers are captured under the run's `logs/` directory.

## 3. H5.2-B — Writer / Reviewer E2E (real model + test grant)

Command:

```
python scripts/h5-d/real_model_scenario_b.py \
  --binary ../../.local/h51-target/debug/local-mcp.exe \
  --work-dir %TEMP%/goallatch-h5d-scenario-b
```

Result: **PASS**.

| Item | Observed |
| --- | --- |
| Goal | `930bc4b4-6787-4ee6-b9f7-68770d351daf` |
| Final status | `COMPLETED` |
| Model | `codex gpt-6.1-terra` (3 calls: Planner, Writer, Reviewer) |
| Contracts | Planner / Writer / Reviewer v2 |
| Grant | `TEST_FIXTURE_V1` `3bc7c6e4-f78e-4727-accf-50af7dce3352`, epoch 1, fixture root `3d3fe0a2-…` |
| Changed paths | exactly `["src/main.rs"]` |
| Final content | `fn main() {\n    println!("goal-latch h5.2 writer");\n}\n` |
| Writer verification | `FILE_DIGEST` with the exact intended SHA-256 |
| Publication | host `atomic-publish` helper; `MutationIntent` applied |
| Workspace outside the grant scope | 0 writes (the changed-path set is exactly the grant path) |
| Primary operator workspace | untouched (the run is under `%TEMP%`) |

The first Scenario B attempt used `STRUCTURED_EVIDENCE`; the Task Verifier
blocked because a Writer task has no host structured-observation provider.
That is a harness/plan-shape issue, not a host defect. The accepted run uses a
`FILE_DIGEST` verification, which the host verifies deterministically.

## 4. H5.2-C — Replanner E2E (real model)

Result: **PARTIAL**.

### Run 1 (observed, evidence file overwritten by run 2)

* Goal `a0a97edf-8d6e-4538-99e3-9f864c8b5f5a`
* Planner `COMMITTED`; Readonly `REJECTED_SCHEMA` / `RESPONSE_TOO_LARGE`
  (host 2 MiB bound, built from the real model answer)
* the Task moved to `NEEDS_REPLAN` (H5.1-C F3); the next step issued a
  Replanner WorkItem
* Replanner `COMMITTED` / `REPLAN_APPLIED`; plan revision 1 → 2
* the replacement DAG had two tasks: one `COMPLETED` through the host Task
  Verifier; one `BLOCKED` with `READONLY_BLOCKED`
  ("Could not calculate the total because data/inventory.txt is missing")
* final status `RUNNING`; workspace unchanged (read-only)
* 5 model calls, all `gpt-6.1-terra`

### Run 2 (src-only objective)

* Goal `7f1e556e-6b9b-41d0-a59e-d92ad7fb3df6`
* NeedsReplan and Replanner issuance worked
* the model's Replanner answers repeatedly omitted the required prerequisite
  `add_dependencies` for `resolve_needs_replan`, so the host refused them with
  `REPLAN_SCHEMA_VIOLATION` (6 times) until the driver's 8-call budget was
  exhausted
* no repository file was written

### Honest classification

The **host** path is proven: NeedsReplan → Replanner issuance → additive
validation/application (`REPLAN_APPLIED`) → host Task Verifier on a new task.
The **real model** did not produce a plan that completed the Goal: in run 1 one
replacement task's answer was factually wrong, and in run 2 the Replanner
violated a published contract condition. That is a model-quality/instruction-
following outcome, not a host safety defect. Scenario C is recorded as
**PARTIAL**, not PASS.

## 5. H5.2-D — Crash / recovery

### 5.1 Real subprocess lock-holder (new)

`publication_lock_subprocess_tests::a_real_subprocess_lock_holder_blocks_quiescence_until_it_dies`

* the test re-executes the real test binary as a child process;
* the child arms/claims the real OS file lock and signals readiness;
* the parent observes `QuiescenceProbe::Active`;
* the parent kills the real child, waits, and observes a later
  `probe_and_abort` become `Quiescent`.

This is a real surviving-process witness, not an in-process flag. Result:
**PASS**.

### 5.2 Publication fault-injection matrix (local Rust tests)

| Crash point | Test | Result |
| --- | --- | --- |
| Boundary failure before write | `at_e5_037` | PASS |
| Write then unknown | `at_e5_038` | PASS |
| Postimage mismatch | `at_e5_039` | PASS |
| CompleteOperation commit failure | `at_e5_039b` | PASS |
| Crash after last publication commit | `at_e5_040b` | PASS |
| Cancel during first write | `at_e5_042` | PASS |
| Applying + helper alive + goal_resume | `r2_1_recovery_never_clears_a_quarantine_while_the_helper_holds_the_lock` | PASS |
| Partial file + helper alive | `r2_1_a_partial_file_change_with_a_live_helper_keeps_the_quarantine` | PASS |
| Concurrent revoke/resume + held lock | `r2_1_concurrent_revoke_and_resume_cannot_clear_a_held_quarantine` | PASS (100/100) |
| Missing/unarmed/ABORTED lock | `publication_lock` unit tests | PASS |
| Failed abort sync then retry | `a_retry_cannot_accept_an_abort_whose_sync_failed` | PASS |

### 5.3 Not verified

* **Full host-kill during a real Writer publication** (kill `local-mcp` while
  the `atomic-publish` helper is inside its critical section, then restart the
  host and prove the quarantine survives until helper death): **NOT_VERIFIED**.
  The host-owned helper is currently contained by the Windows Job Object, so a
  host kill also tears the helper down; a surviving-helper variant needs a
  deliberately detached helper or a test-only supervisor and was not executed.
* POSIX detached descendants (D1) remain a design constraint.
* Cross-platform real-process behavior (Linux/macOS) is **NOT_VERIFIED**; CI
  covers the Rust suite and the subprocess lock-holder test on its own host only.

## 6. H5.2 gate

| Gate | Result |
| --- | --- |
| Scenario A real model | **PASS** |
| Scenario B real-model Writer/Reviewer | **PASS** |
| Scenario C real-model Replanner | **PARTIAL** (host path PASS; model did not complete) |
| Scenario D real-process crash/recovery | **PARTIAL** (real subprocess lock-holder PASS; full host-kill publication NOT_VERIFIED) |
| Critical / High unresolved | **0 known** |
| Required CI | checked on the H5.2 PR (not assumed here) |
| Independent review of the final H5.2 implementation | not yet performed |

**H5.2 is PARTIAL**, not PASS. The H5.1 safety fixes are not weakened by this
result. C3 / R-1 remains unresolved and is not claimed safe for unrestricted
EXTERNAL Writer use on a normal Windows account.
