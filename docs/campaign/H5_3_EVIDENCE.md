# H5.3 — Evidence: real model, real processes, crash recovery

Campaign: GoalLatch H5.3 — External Backend Final Validation
Branch: `feat/h5-3-external-backend-closure-v1`
Base: PR #56 HEAD `0dcab4a109b73d88b7f74871bde70d184e8f147d`
Worktree: `.local/h53`

Every row below is re-derivable from the committed driver scripts plus the run
artifacts under `%TEMP%\goallatch-h53\goallatch-h5d-*`. Evidence categories are
kept separate on purpose: a fixed fixture, a host fault injection, a real OS
process fault, and a real model inference are four different kinds of proof.

---

## 0. Model policy actually used

| Item | Value |
| --- | --- |
| Model | `gpt-6.1-terra` (explicit `-m`, no fallback) |
| Reasoning | `high` (Codex CLI config; every call record carries `reasoning`) |
| Transport | `codex exec`, prompt on stdin (a Replanner request exceeds the ~32 KiB Windows command line) |
| Recorded per call | `model`, `sandbox`, `exit_code`, `elapsed_seconds`, `prompt_bytes`, `conditions_forwarded`, `notes_forwarded` |

Model-call budgets were explicit: C1 8, C2 14 (actual 7 and 6).

## 1. B2 root cause: the read-only sandbox blinds the model on Windows

This was the open question from H5.2-C ("the model says `data/inventory.txt` is
missing"). The answer is **not** a model error.

A real C2 run recorded the model's own inspection command and its result:

```text
exec
"...\\pwsh.exe" -Command '$paths = @("data/inventory.txt", "data/ledger.csv"); ...'
 in C:\Users\Yuta\AppData\Local\Temp\goallatch-h53\goallatch-h5d-c2-h53b\repo
 succeeded in 999ms:
MISSING: data/inventory.txt
MISSING: data/ledger.csv
```

Both files existed on disk at that moment (the driver verifies the premise before
the run, and `data/inventory.txt` is byte-identical to the fixture). The model
inspected honestly and reported what its sandbox showed it.

A direct probe of the same workspace under two Codex sandbox modes:

| `--sandbox` | Result of `cmd /c dir /b data` |
| --- | --- |
| `read-only` | the workspace is not readable — `Test-Path` reports the existing files as missing |
| `workspace-write` | `inventory.txt`, `ledger.txt` listed correctly |

**Classification: deployment constraint of the Codex CLI on Windows, not a
GoalLatch defect and not a model-quality defect.** `codex exec --sandbox
read-only` is therefore unsuitable for any task whose answer depends on
observing the workspace on this platform.

Consequences recorded:

* the H5.2 read-only real-model evidence (Scenario A, Scenario C run 1) cannot be
  read as "the model inspected the workspace". It answered without being able to
  see it;
* the H5.3 harness gained an explicit `--readonly-sandbox` option whose default
  stays `read-only`. Selecting `workspace-write` is a **recorded deviation**, and
  every model call is followed by a workspace-digest comparison that must show
  the model changed nothing. All six C2 calls recorded
  `workspace_unchanged_by_this_call: true`.

The GoalLatch authority boundary is unchanged by this: the host still refuses a
READONLY answer that carries any write field, and the host still performs every
write through the publication helper.

## 2. H5.3-B / C1 — host recovery path (real model)

Command:

```
python scripts/h5-d/real_model_scenario_c.py \
  --binary .local/h52/target/debug/local-mcp.exe \
  --work-dir %TEMP%/goallatch-h53 --run-id c1-h53b
```

Goal `f76eee2a-ee4c-4e03-a2da-4b9ea2f613c2`. 7 model calls, all
`gpt-6.1-terra`.

| Step | Role | Receipt |
| --- | --- | --- |
| 0 | PLANNER | `COMMITTED` / `PLAN_APPLIED` |
| 1 | READONLY | `REJECTED_SCHEMA` / `RESPONSE_TOO_LARGE` (deliberate 2 MiB host bound) |
| 2 | REPLANNER | `REJECTED_SCHEMA` / `REPLAN_SCHEMA_VIOLATION` |
| 3 | REPLANNER | `REJECTED_SCHEMA` / `REPLAN_SCHEMA_VIOLATION` |
| 4 | REPLANNER | `REJECTED_SCHEMA` / `REPLAN_SCHEMA_VIOLATION` |
| 5 | REPLANNER | `COMMITTED` / **`REPLAN_APPLIED`** |
| 6 | READONLY | `COMMITTED` / `READONLY_CANDIDATE_COMPLETE` |
| 7 | — | `GOAL_TASK_TRANSITION_REFUSED` |

The model's first three answers reasoned about the criterion and returned
summary-only objects; after the host's refusals it produced exactly the published
pairing:

```json
{"add_tasks":[{"proposal_id":"compact-count-method",...}],
 "add_dependencies":[{"task":{"ref_kind":"EXISTING","task_id":"fe5feb3b-…"},
                      "dependency":{"ref_kind":"NEW","proposal_id":"compact-count-method"}}],
 "resolve_needs_replan":["fe5feb3b-…"]}
```

That is the H5.2-C failure shape, now satisfied. H5.2 never achieved it in six
attempts; with the published condition and the notes example it took three
refusals.

The final `GOAL_TASK_TRANSITION_REFUSED` is **correct and by design**: the trigger
failed with `HOST_OUTPUT_LIMIT`, a structured failure class whose shape can never
be replayed, so `PENDING -> READY` is permanently refused
(`replanner_replacement_tests::a_resolved_replay_forbidden_task_can_never_be_rearmed_as_ready`).
C1 therefore proves the host recovery path, not Goal completion; completion is
C2's job.

## 3. H5.3-B / C2 — real-task replan to COMPLETED (real model)

Command:

```
python scripts/h5-d/real_model_scenario_c2.py \
  --binary .local/h52/target/debug/local-mcp.exe \
  --work-dir %TEMP%/goallatch-h53 --run-id c2-h53d \
  --readonly-sandbox workspace-write --max-model-calls 14
```

Goal `f33d32cc-12d5-430e-b527-6f01e0440c6f`, **6 model calls**, final status
**`COMPLETED`**.

| Step | Role | Receipt |
| --- | --- | --- |
| 0 | PLANNER | `COMMITTED` / `PLAN_APPLIED` |
| 1 | READONLY | `COMMITTED` / **`READONLY_NEEDS_REPLAN`** (the model inspected, found `data/ledger.csv` genuinely absent, and asked for a replan) |
| 2 | REPLANNER | `COMMITTED` / **`REPLAN_APPLIED`** — one `CODEX_WRITER` repair task, a new hard dependency from the blocked Task to it, and `resolve_needs_replan` |
| 3 | WRITER | `ACCEPTED_FOR_HOST_PROCESSING` / `WRITE_INTENT_PREPARED` — publication through the real helper |
| 4 | REVIEWER | `COMMITTED` / `REVIEWER_RECORDED` |
| 5 | READONLY | `COMMITTED` / `READONLY_CANDIDATE_COMPLETE` — the resolved Task re-ran and completed |

| Assertion | Observed |
| --- | --- |
| Goal result | `TERMINAL` / `COMPLETED` |
| Mandatory tasks | both `COMPLETED` |
| Changed paths | exactly `["data/ledger.csv"]` |
| Published bytes | SHA-256 `730492c582f98558fcbfe41081aeaff054304a478bf8912209dc197dedfc76a8`, exactly the intended content |
| Model writes | 0 — `workspace_unchanged_by_this_call: true` for all 6 calls |
| Host fault injection | none |
| Answer rewriting | none — every submission is the model's own object |

This is the H5.3-B gate: a real-model Replanner E2E in which the whole Goal
completes.

### 3.1 A defect this run found and closed

The first C2 attempt failed nine times with `WRITE_AUTHORIZATION_SCOPE_MISMATCH`
even though the Task scope and the grant scope were byte-identical. Root cause:
`authorization::relative_workspace_path` canonicalized the *target* path, and
`fs::canonicalize` fails for a file that does not exist yet — so a `WRITE_UTF8`
operation with `expected_preimage: {kind: ABSENT}`, a form the published response
contract explicitly offers, was unreachable. The external Writer could therefore
never create a file.

Fix: resolve a missing target through its parent, which must exist and must
canonicalize inside the workspace, then re-attach the final component and let the
existing strict component validation run. A parent that escapes the workspace
still fails `strip_prefix`, so the grant is not widened. Tests:
`a_new_file_inside_the_workspace_resolves_through_its_parent`,
`a_new_file_outside_the_workspace_or_without_a_parent_is_refused`.

## 4. H5.3-C / D1 — real host kill during a real publication

`scripts/h5-d/crash_host_kill.py --mode d1` drives a deterministic
host-contract Planner and Writer answer (no model; this is a process-containment
test), waits until the real `atomic-publish` helper is **inside its critical
section** — it holds the OS publication lock and has written nothing — and then
kills the host with `TerminateProcess`.

| Observation | Value |
| --- | --- |
| Helper reached the critical section | yes (injected hold, `test-grant-fixture` feature only) |
| Host killed | `taskkill /F`, exit code 1 |
| Helper died with the host | **yes** (Windows Job Object `KILL_ON_JOB_CLOSE`) |
| Workspace at crash | preimage intact, postimage absent |
| Fresh host, `goal_status` | `RUNNING` |
| Fresh host, `goal_resume` | `BLOCKED` — `MUTATION_RECONCILIATION_REQUIRED`, "durable Writer mutation is Unknown; blind retry is forbidden" |
| Durable quarantine | `ACTIVE` -> `QUIESCENT_VERIFIED` via `RECOVERY_QUIESCENCE_PROBE` in a **new host generation** |
| Lock file after recovery | `ABORTED` |
| Task | `BLOCKED`, side effect `UNKNOWN`, intent operation `APPLYING` |
| Goal completed | no |
| Double publication | none |

## 5. H5.3-C / D2 — a writer-capable process survives the host

`--mode d2` performs the same crash, and then — after the Job Object has killed
the real helper — a test-owned process takes the same OS lock and stays alive.
That process is **not** a detached `atomic-publish` helper, and this mode does
not claim the shipped helper can escape its Job; it cannot.

| Observation | Value |
| --- | --- |
| Survivor alive after host death | yes (own pid, `msvcrt.locking` on the publication lock) |
| Lock readable by a second process | no — `Permission denied`, i.e. a real exclusive OS lock |
| Fresh host `goal_resume` | `BLOCKED`, same `MUTATION_RECONCILIATION_REQUIRED` |
| Durable quarantine while the survivor lives | **`ACTIVE` — not cleared** |
| Goal completed | no |
| Revocation falsely confirmed | no |
| After the survivor exits | quarantine `QUIESCENT_VERIFIED`, lock `ABORTED`, Goal still `BLOCKED` (no blind retry) |

What D2 proves: the host's recovery decision procedure treats a live lock-holder
as unresolved, regardless of what the files contain, and does not promote an
`UNKNOWN` side effect to a retry.
What D2 does not prove: anything about a detached *helper*; the stand-in is a
test process, and it is labelled as one in the evidence.

## 6. H5.3-A defect: a state refusal was reported as a storage failure

C1's last step originally reached the client as
`GOAL_PERSISTENCE_ERROR` — "durable goal persistence failed" — for a
`PENDING -> READY` refusal. `DriverError::Persistence` mapped every
`OrchestratorError` except three to the persistence code, so a correct durable
state refusal told the operator the disk had failed.

Fixed: `InvalidTransition` now reports `GOAL_TASK_TRANSITION_REFUSED` with
"the durable task DAG refused a transition; the goal needs reconciliation, not a
retry". Tests: `a_durable_transition_refusal_is_not_reported_as_a_persistence_failure`,
`a_genuine_store_failure_is_still_a_persistence_failure`.

The behaviour itself was **not** relaxed. An early attempt to also refuse such a
resolution inside the Replanner validator was reverted after
`replanner_replacement_tests` showed the resolution-then-never-rearm shape is the
designed contract, with `replace_tasks` as the repair primitive.

## 7. What is still not verified

| Item | Status |
| --- | --- |
| POSIX detached descendants during publication | `NOT_VERIFIED` (no Linux/macOS real-process run in this campaign) |
| Real-model runs on Linux/macOS | `NOT_VERIFIED` |
| A real *detached* `atomic-publish` helper surviving the host | `NOT_VERIFIED` and not attempted; the Job Object is designed to prevent it |
| `codex --sandbox read-only` being usable on Windows | `NOT_VERIFIED` as a capability; it is recorded as broken |
| C3 / R-1 OS-user separation | unresolved design constraint, unchanged |
