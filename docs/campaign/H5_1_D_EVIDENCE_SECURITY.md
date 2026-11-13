# H5.1-D — Repository hygiene and evidence security

Status: **DONE for the new branch tip. The raw files remain in public Git history
and are not claimed to be erased.**

## 1. What was tracked

PR #52 tracked two real-model Scenario A runs under `tmp/h5-d/`:

* `tmp/h5-d/scenario-a/` and `tmp/h5-d/scenario-a-run2/`
* `evidence.json` (the campaign evidence record)
* `logs/step-*-prompt.txt` and `logs/step-*-model-stdout.txt`
* a disposable repository (`repo/src/*.rs`, `repo/data/inventory.txt`)
* the disposable state root (`state/sessions/*.json`, `state/goals/*/*.json`, `.lock`)

## 2. Content and secret scan (observed, not assumed)

A scan of every tracked file and of the two commits that introduced them
(`90b28e0`, `0d9ea59`) found:

| Check | Result |
| --- | --- |
| API keys / `sk-…` / `ghp_…` / `github_pat_…` / `AKIA…` | **none found** |
| Bearer / authorization headers | **none found** |
| Passwords, client secrets, private keys | **none found** |
| Access tokens | **none found** |
| Prompt / model output | **present** (the two captured Scenario A answers) |
| Absolute host paths | **present** (`C:\Users\Yuta\…` inside `evidence.json` and the session record) |
| Session / Goal / WorkItem identities | **present** (disposable, generated for the run) |
| Test grants or production grants | **none** (Scenario A is READONLY) |
| Internal durable state | **present** (the disposable state root) |

**Conclusion:** no credential or secret material was found. The files are still
unnecessary public artifacts: they contain host paths, disposable identities,
prompts, and model output. They are removed from the branch tip for that reason,
not because a secret was found.

Because the files were already published in PR #52's branch, untracking them in a
new stacked branch **does not remove them from the public Git history of that
branch**. No history rewrite was performed. If the Owner concludes that any
published value is sensitive, invalidation is an Owner decision and must not be
inferred from this document.

## 3. Digests of the removed artifacts

These digests let a reviewer confirm that a copy is the artifact from
`90b28e0`/`0d9ea59` without re-publishing it in this branch.

| File | SHA-256 |
| --- | --- |
| `tmp/h5-d/scenario-a/evidence.json` | `b98e65e78e9fd33d93229ab2ab0a25160141d0918261bcb797e17272bb2e1777` |
| `tmp/h5-d/scenario-a-run2/evidence.json` | `595f8305dcd0caf1edb7aa858c2caf6471f52cbfebad0a7f6c704681ba26bcd8` |
| `tmp/h5-d/scenario-a/logs/step-0-model-stdout.txt` | `872ba6ac131cad88cf6290d0c977444eba8b41fa4307d7d0333387edc0f41b9e` |
| `tmp/h5-d/scenario-a/logs/step-1-model-stdout.txt` | `f307cbbbf831ddf65d4963d10e31b40f368ae2ce9a1c2efb23322ec4e5bd1db7` |
| `tmp/h5-d/scenario-a-run2/logs/step-0-model-stdout.txt` | `d58cf7ec5a86da88f3ce8724a7d694b058749e280777f1a2ab24aaa872760e91` |
| `tmp/h5-d/scenario-a-run2/logs/step-1-model-stdout.txt` | `1efbf449f9d5a51a3a6e182ca4b36bd711888196e1150975571f1567ec181097` |
| `tmp/h5-d/scenario-a/state/goals/…/86e8b679-….json` | `e9fc94125de85af6ee2a405efeb86518c76cf493fbce5f707135034c7a93ff5c` |
| `tmp/h5-d/scenario-a-run2/state/goals/…/e57ddcac-….json` | `62f07e7aa7853b16ca7ebee28e1e38999d77d27619ee9a68aa0ed398d50dc717` |

## 4. What is retained instead

The human-readable evidence summary is in `docs/campaign/H5_D_REAL_MODEL_EVIDENCE.md`
and `docs/campaign/H5_CAMPAIGN_CHECKPOINT.md`. Those files record the outcome,
the binary digest, the model calls, the contract digests, and the negative
control without copying the raw prompts, model output, absolute host paths, or
disposable state.

The reproduction procedure is `scripts/h5-d/real_model_scenario_a.py`. It writes
under a run directory chosen by the operator, never under a tracked path, and it
does not require committing its output.

## 5. Recurrence prevention

* `.gitignore` now ignores `tmp/` entirely, so a future run cannot stage raw
  state or logs by accident.
* The campaign convention is: commit the **summary + digests + reproduction
  command**, never the raw run directory.
* CI artifacts remain the release digest and the schema evidence already defined
  in `.github/workflows/ci.yml`; no new artifact includes a prompt or model
  output.
* A `tmp/` path is never used as an input to a test that asserts byte equality;
  the real-model driver is outside the Rust test suite and is executed
  explicitly.

## 6. Gate

| Requirement | Result |
| --- | --- |
| Content and history investigated | **PASS** |
| No secret material found | **PASS** (table above) |
| Owner decision identified if a published value is sensitive | **PASS** — none found, decision recorded as not required |
| Raw artifacts untracked from the new branch tip | **PASS** |
| Evidence summary, digests, and reproduction retained | **PASS** |
| History erasure claimed | **NOT CLAIMED** — the files remain in PR #52's public history |
