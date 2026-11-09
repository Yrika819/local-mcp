# H4-D — real-model dogfooding evidence (Scenario A executed, B/C/D not)

Executed: 2026-11-09, Windows 11 x64 (build 26300), rustc 1.96.0.
Binary under test: `local-mcp.exe` built from this branch head
(`feat/h4-a-mcp-contract-v1`, PR #51; parent commit `072fa15` = PR #50 head).
`cargo build --locked --bin local-mcp` → `target/debug/local-mcp.exe`,
sha256 `c7147f9ca956ebba93c7133154a5c731f24aa31ebbd8170d74de69333d9409fc`
(local build of this branch head `37820a4`; CI builds its own).

The run was executed twice: once to capture a genuine model schema failure
(§ "the failure is the evidence") and again, on the final head, to confirm the
end-to-end result. The model answers were authored by Codex `gpt-6.1-sol`; the
second run replayed those same model-authored answers against the final binary
with the newly issued host identity fields, which is why it has no rejection
row.

External model that authored both model responses: **Codex `gpt-6.1-sol`**
(reasoning effort medium), a *different model* from the campaign agent, invoked as
an ordinary model call. It was not a fixture, and the driver never synthesised a
model answer.

## What is real here, and what is not

Real:

* a real `local-mcp mcp` child process, driven over the same newline-delimited
  JSON-RPC transport a normal MCP client uses;
* a real durable Goal in an isolated state root
  (`LOCAL_MCP_STATE_DIR=<tmp>/fixture/state`), never the operator's state root;
* real `ModelWorkItem`s issued by the host, with real `request_digest`s;
* real model output (plan and investigation answer), produced by a model that
  actually read the fixture files;
* real host validation of that output, including a genuine schema rejection;
* real Host verification and Goal finalization on host-owned evidence.

Not real / not done: the Writer leg (Scenario B) needs an Owner grant, which
needs the interactive Owner Control Surface; the Replanner (C) and crash/recovery
(D) scenarios were not driven against a live server. Those are **NOT_VERIFIED**.

## Isolation, proven

* State root: `tmp/h4-campaign/fixture/state` (throwaway), Session
  `h4d-scenario-a` (throwaway), workspace `tmp/h4-campaign/fixture/repo`
  (throwaway, two seeded files).
* The operator's real state root was checked afterwards: no `h4d-scenario-a`
  directory under `%LOCALAPPDATA%\local-mcp\goals` — the run is not visible there
  at all.
* Each driver step spawned a **fresh** server process, so every step after the
  first also exercises durable-state resumption from disk.

## Scenario A — Readonly DAG, end to end

| Field | Value |
| --- | --- |
| Session | `h4d-scenario-a` |
| Goal | `f0c44a92-659f-4871-8077-10a851eadcce` |
| Objective | read-only statistics: files under `src/` declaring `fn`, and the total of `data/inventory.txt` |
| Completion criterion | `84e8aaff-5796-4373-8734-b5735f9a0e43` |
| Durable Goal file | `<state>/goals/h4d-scenario-a/f0c44a92-….json`, sha256 `d97f210067bbf3da3fe9c4d06acaaa5c65ec8a45c79b682fd18a898bba0c8454` |
| Final status | `COMPLETED` (revision 11, plan revision 1) |
| Final verification | `PASSED`, `final_verification_id 6c617e37-9fa0-4b14-9b86-7bd6c7636969` |
| Finalizer | progress code `GOAL_FINALIZED` — "the host Goal Finalizer completed the Goal on host-owned evidence" |
| Codex spawned for this Goal | **0** (`model_backend: EXTERNAL`; the server only ever issued WorkItems) |

Final-head rerun (same model answers, new host identity fields):

| Field | Value |
| --- | --- |
| Goal | `905e0699-42fe-4838-9e2d-1f23a29a6211` |
| Durable Goal file sha256 | `a2f29f00088b7bbc2aeb68f08afd194d81292f9357499ee49ce437aea9f42294` |
| Planner receipt | `COMMITTED` / `PLAN_APPLIED` (rev 3) |
| Read-only receipt | `COMMITTED` / `READONLY_CANDIDATE_COMPLETE` (rev 5) |
| Final | `HOST_PROGRESS` → `COMPLETED` (rev 8), `GOAL_FINALIZED`, verification `PASSED`, 5 evidence items, task attempts 1 |
| Identity replay | `REPLAY_OF_ACCEPTED_SUBMISSION`, stored receipt `received_at 13:18:24Z`, `committed_revision 3`, Goal unchanged at rev 8 |
| Different payload, same ids | `CONFLICTING_DUPLICATE` — refused, no side effect |
| Workspace after the run | exactly the 2 seeded files, unchanged (`src/main.rs` sha256 `77c072ca…`, `data/inventory.txt` sha256 `ed192a34…`) |

### Model work items and receipts

| Step | Role | work_id | request_digest | submission_id | decision / code |
| --- | --- | --- | --- | --- | --- |
| 1 | PLANNER | `4803e9b4-09aa-4787-973b-7f8b9b0b3639` | `4ed59e9c…e7bd54` | `094b076f-9b0c-4d60-8161-da50b5edce25` | `COMMITTED` / `PLAN_APPLIED` (rev 3) |
| 2 | READONLY (attempt 1) | `e250242a-7448-4817-a3cf-6a7cff854c78` | `a65549ce…c5633` | `33cdae90-83fc-4196-bbaa-d6c1a7153d6d` | **`REJECTED_SCHEMA` / `READONLY_OUTPUT_INVALID`** |
| 3 | READONLY (attempt 2) | `6d435a12-131f-4a87-88a1-1650b9490914` | — | `6c5bfd38-7743-479f-8809-7796e686f7a4` | `COMMITTED` / `READONLY_CANDIDATE_COMPLETE` (rev 8) |
| 4 | replay of step 1 | `4803e9b4-…` | `4ed59e9c…e7bd54` | `094b076f-…` (same) | `REPLAY_OF_ACCEPTED_SUBMISSION`, stored receipt echoed, revision stayed 11 |

Step 2 is the most valuable row: the model produced a valid-looking answer with
`"status": "COMPLETED"`, the host refused it because the read-only status
vocabulary is `candidate_complete | blocked | needs_replan | failed`, re-issued a
new WorkItem at a new revision, and the corrected answer was then accepted. The
host validated untrusted model output and did not accept a model-declared
success. See finding **H4-D-F1** below.

### Host-observed evidence on the Task

Five evidence items, including:

```
WORKER_REPORT          attempt a95683d9…  report_digest c0bab122…
STRUCTURED_OBSERVATION requirement_id readonly-survey
                       source EXTERNAL_READONLY_AGENT  passed true
                       detail "…only src/main.rs … 42 + 7 + 0 = 49. No files were changed."
```

`source` is host-assigned (`EXTERNAL_READONLY_AGENT`), not model-authored, and the
`STRUCTURED_EVIDENCE { requirement_id: "readonly-survey" }` verification spec was
satisfied by that observation — which is why the Task verifier and the Goal
finalizer both passed.

### Immutability

After the run the fixture workspace still contained exactly the two seeded files
(`src/main.rs`, `data/inventory.txt`), and the durable authorization anchor stayed
zero (`highest_epoch 0`, `usage_operations 0`) with no `write_scope`: a read-only
Goal created no grant and wrote no file.

## Findings from the real run

**H4-D-F1 (MEDIUM, contract/documentation) — the published response contract
does not tell a client the enum vocabulary.**
`ModelWorkView.response_required_fields` lists field *names* only
(`status`, `evidence`, …), and `output_contract: "readonly-investigation-v1"`
names no schema. A real external model therefore cannot know that `status` must
be `candidate_complete` rather than `COMPLETED`; the first real attempt failed on
exactly that. The host behaved correctly (fail closed, typed rejection, new
attempt). The *contract* is under-specified. Candidate fix (additive, not done
here): publish the per-role response schema — including enum values and the
structured-evidence requirement ids — in the WorkItem, or add a
`docs/schemas/external/*.json` set that the WorkItem references by name.

**H4-D-F2 (INFO) — a rejected submission consumes a role attempt.**
The rejected read-only answer used one of the 8 role attempts and one Task attempt
budget. That is the correct fail-closed accounting, but it means a client that
guesses the vocabulary spends budget. Consequence for Serea: the adapter must be
built against the published schema, not against a guess.

## Scenario B / C / D — NOT_VERIFIED

| Scenario | Status | Concrete blocker |
| --- | --- | --- |
| B — Writer + Reviewer | **NOT_VERIFIED** | Publication requires `GoalWriteGrantV1`, issued only by the Owner Control Surface, which is an interactive console. A `goal_start` with `write_scope` correctly leaves `authorization.state = REQUIRED`, and the Writer leg refuses without a grant (`GrantRefusal::Required`). No non-interactive grant path exists in the shipped binary, and inventing one (or writing a grant record by hand into the ledger) would bypass exactly the boundary H4-B just reviewed. The test-only `install_grant` harness exists in `authorization_store::test_support` but is not reachable from an out-of-process run. |
| C — Replanner | **NOT_VERIFIED** | Same reason plus one more: a replan that needs a wider grant must not continue without Owner approval, so a meaningful Replanner scenario needs the same grant harness. |
| D — Crash / recovery | **PARTIAL** | The identity/idempotency half was verified live: every driver step ran in a **new** server process (durable reload), and the step-4 replay returned `REPLAY_OF_ACCEPTED_SUBMISSION` with the original `received_at`, `committed_revision 3`, and the Goal unchanged at revision 11 — no duplicate side effect. The *crash-injection* half (killing a server mid-publication, UNKNOWN reconciliation, no double publication) was **not** executed; it is covered only by the existing in-process fault tests (`external_writer_failure_tests`, `external_h3_reliability_tests`). |

Nothing above is presented as a PASS. The honest summary of H4-D is: the
Readonly path works end to end with a real model, a real host validation failure
was observed and correctly refused, replay is idempotent across processes, and
the Writer/Replanner/crash legs remain unverified because the grant boundary has
no non-interactive entry point.

## Reproduction

The driver is `tmp/h4-campaign/scenario_a.py` (not committed; it is campaign
tooling, not product code). It writes only under `tmp/h4-campaign/fixture` and
requires `H4_LOCAL_MCP` to point at a built binary. The model answers it fed in
are `model-planner.json` and `model-readonly2.json`, both authored by Codex
`gpt-6.1-sol`.
