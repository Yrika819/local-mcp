# H5-D — real-model end-to-end evidence

Executed: 2026-11-09 (UTC 16:5x), Windows 11 x64, rustc 1.96.0.

## 0. Honest summary first

| Scenario | Result |
| --- | --- |
| **A — Readonly DAG** | **EXECUTED, PASSED** with a real model, real server process, isolated state root. Goal reached `COMPLETED` on host-owned evidence |
| **B — Writer → Reviewer → Finalizer** | **NOT_VERIFIED** — not executed. H5-C delivers and tests the isolated grant machinery, but no automated live Writer loop was wired in this campaign |
| **C — Replanner** | **NOT_VERIFIED** — not executed |
| **D — Crash / recovery injection** | **NOT_VERIFIED** — not executed. The F1/F2 barrier is covered by a deterministic journal test (`at_e5_050`), not by process-kill injection |

Nothing below presents a fixture as a model answer, a replayed answer as fresh
inference, or a source reading as an execution.

## 1. What is real in the Scenario A run

* a real `local-mcp mcp` child process, driven over newline-delimited JSON-RPC
  (the transport a normal MCP client uses);
* a real durable Goal in a throwaway state root (`LOCAL_MCP_STATE_DIR`), with a
  throwaway workspace (3 seeded files);
* real host-issued `ModelWorkItem`s with real `request_digest`s and the new H5-B
  `response_contract`;
* **real model inference**: `codex exec --sandbox read-only` (Codex
  `gpt-6.1-sol`), one process per WorkItem, answering the host-authored request
  and reading the fixture files itself;
* real host validation, real Task Verifier, real Goal Finalizer;
* a real negative control (an out-of-contract answer) that the host refused.

Driver: `scripts/h5-d/real_model_scenario_a.py`. Evidence:
`tmp/h5-d/scenario-a-run2/evidence.json` (per-step receipts, prompts and model
stdout beside it under `logs/`).

## 2. Run record

| Field | Value |
| --- | --- |
| Binary | `target/debug/local-mcp.exe`, sha256 `96aec3f7778d7f19…` (built from the H5 HEAD under test) |
| State root | `tmp/h5-d/scenario-a-run2/state` (throwaway) |
| Workspace | `tmp/h5-d/scenario-a-run2/repo` (throwaway, 3 seeded files) |
| Session | `h5d-205762be-44d5-4488-8c7a-1ffb2534ca3c` |
| Goal | `e57ddcac-9965-4275-981d-955d57215d2c` |
| Model | Codex `gpt-6.1-sol` via `codex exec`, sandbox `read-only` |
| Model calls | **2** (PLANNER 45.0 s, READONLY 19.1 s), exit code 0 |
| Codex CLI spawned **for the Goal** | **0** (the server never spawns Codex; the model work is the client's) |
| Final status | **`COMPLETED`**, revision 10, plan revision 1 |
| Task | `COMPLETED`, worker `CODEX_READONLY`, verification `PASSED` |
| Workspace | unchanged (`workspace_unchanged: true`) |
| Write scope / grant | none (`write_scope: null`, anchor absent) — a read-only Goal created no authority |

### Steps

| Step | Role | Outcome | Code |
| --- | --- | --- | --- |
| 0 | — | `MODEL_REQUIRED` (PLANNER) | contract `…/PLANNER/planner-proposal-v1`, digest `sha256:18df3866…` |
| 0 | **negative control** | **`REJECTED_SCHEMA`** | `PROPOSAL_SCHEMA_VIOLATION` — the answer was `{"status": "COMPLETED"}` with empty tasks |
| 0-reissued | PLANNER | `COMMITTED` | `PLAN_APPLIED`, `request_digest 6d7d7491…` |
| 1 | READONLY | `COMMITTED` | `READONLY_CANDIDATE_COMPLETE`, contract `…/READONLY/readonly-investigation-v1`, digest `sha256:892b1348…` |
| 2 | host | `COMPLETED` | Task Verifier `PASSED` → Goal Finalizer |

Host-owned verification detail recorded on the Task:

```
structured requirement 61ccf360-1d18-4a91-96d9-d45b28db869a from EXTERNAL_READONLY_AGENT:
"Read src/helper.rs and src/main.rs; each contains a function declaration.
 Read data/inventory.txt: widgets=42, gadgets=7, spares=0; total=49."
```

Evidence items on the Task: `WORKER_REPORT`, `STRUCTURED_OBSERVATION`,
`VERIFICATION_OBSERVATION`, `VERIFICATION`.

## 3. The failure that produced a real finding (run 1)

The **first** Scenario A run (same driver, same binary minus the last contract
change) reached `BLOCKED`:

* PLANNER `COMMITTED`, READONLY `COMMITTED` (`READONLY_CANDIDATE_COMPLETE`),
* Task Verifier: `INDETERMINATE` — *"required host structured evidence
  <requirement_id> is missing"*,
* `goal_next` then returned `NO_ACTION_WITH_REASON` forever.

Cause: `apply_readonly_result_to_goal` keys each model evidence item by
`requirement_id = evidence.kind`, so a model must echo the Task's
`verification[].requirement_id` as an evidence `kind`. **The published H5-B
READONLY contract did not say that.** This is structurally the same defect class
as H4-D-F1 (a vocabulary the client cannot know), so it is recorded as
**H5-D-F2 (Medium, contract)** and was fixed in the contract:

* `evidence[].kind` now documents the mapping explicitly,
* a new condition `READONLY_EVIDENCE_KIND_IS_THE_REQUIREMENT_ID` is published,
* a note states it in the client-facing notes list.

After the fix, the re-run above reached `COMPLETED` on the model's **first**
READONLY answer, with the same model and the same objective. That is the
strongest evidence in this campaign that the machine-readable contract changes
model behaviour.

## 4. What was NOT executed (and is therefore not claimed)

* **Scenario B**: no real-model Writer publication, no Reviewer, no managed
  worktree evidence. The `test-grant` fixture command (H5-C) is the intended
  enabler and is not yet wired into an automated scenario.
* **Scenario C**: no live Replanner.
* **Scenario D**: no process-kill crash injection at the eight publication
  points. What exists instead is a deterministic journal-state test for the
  revocation barrier (`at_e5_050`) and the H2/H3 journal tests that already
  passed.
* No Linux/macOS execution of any H5 leg.
* No Owner Control Surface run; no production grant was issued.
