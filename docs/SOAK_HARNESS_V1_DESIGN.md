# Soak Harness V1 — Design (Request/Response Completeness for Long-Running MCP Sessions)

Status: **DESIGN / NOT IMPLEMENTED** (2026-11-04). This document proposes an
independent, new-main-based harness for long-running stability testing of the
GoalLatch MCP stdio server. It does not modify, extend, or integrate PR #15,
and it does not authorize merging it. No code is included here.

Base: `main` at
`e6950fdc3bab72982285888a81cce822743fbba5` (re-synced from the original
`ea50127e13dc0d24e3b7bc3057690c5f8cca12c1` v0.1.2 base; the design is
unchanged by the re-sync — `main` gains Setup Preview V1 and test
stabilization, none of which this harness depends on or exercises).
Companion: `docs/BETA_READINESS_V0.1.2.md` (functional beta), PR #15
(verification-only integration record — read-only reference, never input to
this design's correctness).

## 1. Why this exists

Release v0.1.2 notes state plainly that long-running mixed Goal/Job load and
tail latency under high load are not fully verified. PR #15 retained a
7,200-second run with 1,029,243 parsed responses and 21 unclassifiable
in-flight requests at shutdown — its old harness could not establish a
success-rate bound at all. An improved harness (§3) fixed accounting but was
run for only 60 seconds and has never been integrated. Any soak claim for
GoalLatch must come from this new harness, on the commit under test, or not
be made.

## 2. Goals and non-goals

Goals:

1. **Request/response completeness** — every attempted request is eventually
   classified: normal response, JSON-RPC error, invalid frame, duplicate,
   timeout, send failure, or shutdown-inflight (recovered or not). No request
   is silently dropped from the ledger.
2. **Long-running Goal/Job stability** — mixed read, Goal, Job, and refusal
   traffic sustained for hours, with the durable Goal/Job state read back and
   reconciled at the end.
3. **Resource bounds** — memory (working set), descriptor count, in-flight
   requests, and response size are bounded; exceeding a bound fails the run.
   V1 design defaults (all explicit CLI flags, never implicit): in-flight
   requests ≤ 128 (reused from the PR #15 reference shape); wire-line size cap
   16 MiB; per-run response byte cap 1 GiB (counters retained, payload not);
   working-set ceiling 1.5 GiB or +50 % baseline, whichever first; descriptor
   ceiling 4,096; latency-sample retention ≤ 10,000 per scenario (reservoir
   sampling beyond that, with the dropped-sample counter retained). The
   defaults are validated against a maintainer-observed baseline in Slice 3;
   if a bound is found wrong it is re-fixed there — never removed, only
   adjusted, and never after publishing numbers. A bound that cannot be
   evaluated at runtime makes the run NOT RUN, not PASS.
4. **Tail latency** — p50/p95/p99 recorded per scenario over sufficient
   samples; no claim without raw samples retained. Percentiles are reported
   only when ≥ 100 latency samples exist for the scenario; below that the
   percentile fields report `NOT-MEASURED`, never an extrapolated number.
5. **Cancellation** — a Goal/Job can be cancelled mid-flight and reaches a
   reconciled terminal state without hanging the server.
6. **Failure injection** — malformed frames, oversized frames, client EOF,
   and child termination at adversarial moments; recovery is measured, not
   assumed.

Non-goals:

- No load balancer, no cluster, no network transport (stdio only, matching
  the product's MCP boundary).
- No benchmark comparison against other servers.
- No performance marketing claims; the harness reports, it does not sell.
- No modification of the product runtime to accommodate the harness.
- No use of PR #15's 60-second result as evidence; it is design input only.

## 3. Reusable elements from PR #15 (design-level reuse)

The improved `scripts/soak/soak.py` in `integration/longrun-audit-v1` is the
reference design, not the code to be copied wholesale. Reusable ideas:

- **ResponseLedger pattern**: three bit-array ledgers (attempted/sent,
  responded, timed_out) + a pending map keyed by request id; counters for
  invalid/duplicate/unknown; explicit shutdown-inflight accounting with a
  `shutdown_recovered` tally. This is the correct shape; keep it.
- **Bounded memory**: never retain response payloads; retain at most
  latency samples (reservoir or full list with a cap + counter), slow-event
  ring buffer (`MAX_SLOW_EVENTS`), bounded in-flight (`MAX_INFLIGHT = 128`),
  bounded line size (`16 MiB`), bounded total requests.
- **Working-set sampling** of the child on both Linux (`/proc/pid/statm`)
  and Windows (`GetProcessMemoryInfo`), every N seconds.
- **CI regression tests for the harness itself** (`scripts/soak/test_soak.py`,
  17 tests) — a harness without tests produces unfalsifiable numbers.

Not reused:

- Any assumption about PR #15's merge status (it must not merge into main).
- Its statistics presentation; this design defines reporting below.
- Its build pins; the new harness runs against a freshly built binary of the
  commit under test.

## 4. Architecture

```
soak.py (driver, host)
  ├── spawns: local-mcp mcp   (child, stdio JSON-RPC)
  ├── spawns/uses: local-mcp start (session for Goal traffic; headless
  │     because approval UI is interactive — see §6)
  ├── scenario engine (seeded RNG, reproducible seeds)
  ├── ResponseLedger (§3)
  ├── resource sampler (memory, fd count, inflight, uptime)
  └── report writer (JSON + Markdown; raw samples retained)
```

- The driver is Python 3.11+, stdlib-only (subprocess, json, threading,
  statistics, argparse); no product dependencies.
- Seeds are explicit CLI arguments; every report records the seed, the
  commit under test (`git rev-parse HEAD` of a supplied checkout), OS, and
  runner.
- The child is spawned with a test-owned state directory (`XDG_*`/`TEMP`
  redirected), never the operator's state.

### Scenarios (weighted, seeded mix)

| # | Scenario | MCP surface | Bound |
| --- | --- | --- | --- |
| S1 | Read file / list directory (small fixture tree) | `read_file`, `list_directory` | ≤ 5k req/min |
| S2 | Session info poll | `session_info` | interleaved |
| S3 | Goal start → status → cancel | `goal_start`, `goal_status`, `goal_cancel` | ≤ 20 concurrent goals |
| S4 | Goal run (bounded `max_steps`, stubbed/absent Codex CLI) | `goal_run` | expect recorded failure without Codex CLI; must not hang |
| S5 | Foreground execute (sandboxed `echo`-class command) | `execute` | Linux/macOS only. Windows leg: S5 is **excluded per Experimental status** and recorded `not-applicable (platform experimental)` — Windows behaves as the documented host-native, non-sandboxed platform and is never admitted to the latency/coverage matrix; no Windows result is presented as a sandbox success |
| S6 | Refusal path (outside roots) | any write/execute outside `permitted_directories` | must refuse; zero silent truncation |
| S7 | Malformed frame (garbage line, wrong version, missing id) | raw stdio | must be answered or logged, never crash child |
| S8 | Oversized frame beyond cap | raw stdio | must be refused within the 16 MiB cap |
| S9 | Client EOF (driver half-closes stdin) | raw stdio | child exits or reports; driver classifies |
| S10 | Child kill at adversarial moments (SIGTERM/SIGKILL between request phases) | process control | recovery section asserts durable-state readability after restart |

## 5. Accounting contract (normative)

Every request id produced by the driver is in exactly one final state:

```
ATTEMPTED ──sent──► SENT ──ok──► RESPONDED (normal | jsonrpc-error)
    │                  │            └── invalid-frame (counted separately)
    │                  └── duplicate (counted separately)
    │                  └── unknown-id (counted separately)
    ├── send-failed
    └── timed-out (timeout budget: configurable, default 30 s)
SHUTDOWN-INFLIGHT ──recovered-at-EOF──► RESPONDED-POST-SHUTDOWN
                 └── unrecovered (reported; never erased)
```

Metrics reported per run:

- attempted, sent, send_failed, normal_responses, jsonrpc_errors,
  invalid_responses, duplicate_responses, unknown_ids, timeouts,
  shutdown_recovered, shutdown_unrecovered;
- coverage = (normal_responses + jsonrpc_errors) / max(sent, 1);
- error rate, invalid rate, timeout rate (all per attempt and per sent);
- p50/p95/p99 latency overall and per scenario;
- peak/mean working set of the child; fd/descriptor count;
- max inflight observed; bytes read/written.

**A run that cannot produce these numbers is NOT RUN and must be reported as
such.** No partial summary may be published as a "soak result".

## 6. Environment and permissions

- Linux leg: full matrix including S5 (Bubblewrap required; gate honored).
- macOS leg: S5 allowed with Seatbelt; Apple Silicon noted as CI target
  (not locally release-closure validated, same boundary as release notes).
- Windows leg: S5 excluded (Experimental; no Unix sandbox); S6 and S7–S10
  still exercised; results explicitly labeled Experimental.
- The interactive approval UI is out of the harness's loop: Goal/mutation
  scenarios run with a **test-owned non-interactive session**. Where a step
  hard-requires approval, the step is classified `not-applicable` with the
  reason recorded — never faked. (Goal model execution additionally needs
  Codex CLI; without it, S4 must produce a recorded, non-hanging failure.)
- No network access except localhost pipes created by the child's own stdio;
  the fixture tree is local and test-owned.

## 7. Failure injection and recovery acceptance

- FI-1 kill child mid-S3 (goal active): after restart, `goal_status` returns
  a coherent state (created/running/blocked/cancelled/failed/done — never
  corrupt, never fabricated).
- FI-2 kill child mid-write (S6): project fixture tree is either unchanged
  or matches the last acknowledged atomic publish; no torn files (hash
  fixture before/after).
- FI-3 malformed frame flood (S7): child remains responsive to S1 requests
  interleaved in the same stream within the latency bound; no deadlock.
- FI-4 EOF during sustained in-flight traffic: exercise ≥ 1,000 **attempted**
  requests with no more than the in-flight bound (128) outstanding at any
  moment (§3, §2 items). Driver classifies every in-flight request as
  shutdown-recovered or shutdown-unrecovered; the ledger never leaks a
  "lost" id.
- FI-5 disk-full state dir (if safely emulatable with a small quota mount):
  child fails closed; no partial Goal state is reported as committed. If the
  mount cannot be created in CI, report NOT RUN, do not simulate by
  byte-editing state files.

## 8. Outputs and reporting

Each run writes `soak-report-<seed>-<sha>-<platform>.json` and a short
Markdown companion containing: commit, OS/runner, seed, duration, the §5
metrics, NOT RUN ledger, and an explicit **Limitations** block restating the
platform boundaries (Windows Experimental; Apple Silicon CI target). Reports
must not include session IDs, user paths, or payload content.

## 9. CI integration

- Nightly or manual-dispatch workflow (not per-PR): `soak` job builds the
  release binary, runs 15-minute soak by default with a seed matrix
  (≥ 3 seeds), publishes the report as an artifact; failure = any metric
  outside the agreed bound or any FI-* failure.
- Per-PR: only the harness's own unit tests (the §3 `test_soak.py`-equivalent)
  run, to keep PR CI fast.

## 10. Acceptance criteria (before any public reliability claim)

1. Two consecutive 8-hour Linux runs on the same commit with ≥ 3 distinct
   seeds: zero lost ledger entries, zero child crashes unexplained in the
   report, coverage and latency reported with raw samples retained.
2. FI-1..FI-4 green on all three OS legs (FI-5 as applicable).
3. The report template reviewed to contain no secrets/paths.
4. Maintainer sign-off on the metrics definition before publishing numbers.
5. No claim text published until (1)-(4) are met; the v0.1.2 release notes'
   open question on mixed Goal/Job load is answered by reference to these
   reports, not before.

## 11. Sequencing

- Slice 1: driver + ledger + S1/S2/S7/S9 + harness unit tests (closes the
  accounting hole).
- Slice 2: S3/S4/S6 + FI-1/FI-2 (session/goal durability).
- Slice 3: resource sampler + bounds + report writer.
- Slice 4: CI nightly + seed matrix.
- Slice 5: 8-hour runs + maintainer claim review.

Each slice is independently reviewable and does not touch the product
runtime. Estimated total: 1–2 weeks of focused work, excluding soak wall
time.
