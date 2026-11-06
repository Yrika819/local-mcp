# Soak Harness V1 — implementation

Implementation of `docs/SOAK_HARNESS_V1_DESIGN.md` (PR #26). Python 3.11+
standard library only. It never imports product code and never touches the
operator's session or state: the child server is spawned with a fully
redirected test-owned environment and a test-owned session file.

## Layout

- `soak_v1/ledger.py` — ResponseLedger (Slice 2): every driver-issued request
  id ends in exactly one final state; unclassified ids fail the run.
- `soak_v1/driver.py` — MCP stdio protocol driver (Slice 1): newline-delimited
  JSON-RPC, request-id management across restarts, bounded in-flight queue,
  per-request timeout, shutdown protocol with grace window.
- `soak_v1/scenarios.py` — seeded weighted scenario engine (Slice 3), S1–S8 in
  the weighted mix; S9 (client EOF) runs as an always-on phase; Windows
  records S5 as `not-applicable (platform experimental)` — never as a sandbox
  result.
- `soak_v1/failure_injection.py` — FI-1..FI-5 (Slice 4). FI-5 (disk-full) is
  reported NOT RUN unless a real quota mount exists; simulation by
  byte-editing state files is forbidden by the design.
- `soak_v1/resources.py` — working-set/fd samplers, latency reservoir with the
  10k cap and the ≥100-sample percentile floor, bound checks.
- `soak_v1/harness.py` — orchestration, CLI, verdicts (PASS / FAIL / NOT RUN).
- `soak_v1/report.py` — JSON + Markdown report; session ids and run paths are
  redacted from the report tree.
- `stub_mcp_server.py` — test-owned stub server for the unit tests.
- `test_soak_v1.py` — 38 unit tests (ledger consistency, timeout, duplicate,
  unknown id, null-id, oversized-frame refusal, EOF/restart, bounds).

## Usage

```
python -m soak_v1 --server-cmd "/path/local-mcp mcp" \
    --duration 60 --seed 1 --out runs/smoke \
    [--failure-injection] [--commit-sha <sha>] [--pacing-s 0.05]
```

Exit codes: 0 = PASS, 1 = FAIL, 2 = NOT RUN.

All bounds are explicit CLI flags with the design defaults: in-flight ≤ 128,
wire line cap 16 MiB (design §2; the product's *request*-frame cap is 8 MiB —
oversized-frame probes exceed the request cap and are refused with an
`id: null` resource-limit error, which the ledger records as a refusal
against the probe id), response byte cap 1 GiB, working set 1.5 GiB absolute
cap with a growth allowance of +50 % of baseline **subject to a 256 MiB
absolute floor** (Slice-3 adjustment per design §2: the pure percentage bound
was hair-trigger against a ~13 MB Windows baseline; recorded here and flagged
for the PR #26 design review — never removed, only adjusted), fd ceiling
4 096, latency samples ≤ 10 000. Percentiles below 100 samples report
`NOT-MEASURED`.

## Verified behaviors from the first real runs (Windows x64, v0.1.2 binary)

- the product answers malformed JSON with `-32700` and `id: null`;
- oversized request frames are discarded without parsing and answered with an
  `id: null` resource-limit error — the connection stays on frame boundaries;
- notifications (no id) receive no response;
- the durable Goal model allows **one** non-terminal Goal per session — the
  engine rotates a single goal; `goal_start` refusals (e.g. the durable
  creation-attempt cap) are classified as expected refusals. This reconciles
  the design table's "≤ 20 concurrent goals" with the V1 product behavior
  (flagged for the PR #26 design review);
- Windows state-dir isolation: the product resolves its state from the user
  profile, so the harness redirects `USERPROFILE`/`HOME` (empirically
  verified) and writes the test-owned session file under
  `<state_root>/AppData/Local/local-mcp/sessions/`.

## Honest limitations

- Long-running soaks (the 8-hour acceptance runs in design §10) have **not**
  been executed in this campaign; short runs are not evidence of long-run
  stability.
- FI-5 is NOT RUN until a real quota mount is available.
- macOS resource sampling is best-effort via `ps` and is flagged approximate.
- A run without `--failure-injection` records FI-1..FI-4 as NOT RUN.
