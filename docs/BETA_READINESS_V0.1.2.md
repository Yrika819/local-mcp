# External Beta Readiness — GoalLatch v0.1.2

Status: **PREPARATION / NO BETA IS RUNNING** (2026-11-04). This document
defines how a small, opt-in external beta would be run and measured. It
claims no beta cohort, no onboarding rate, and no user feedback. It must not
be published as evidence that any external testing has happened.

Audience: the maintainer inviting beta participants, and participants
completing the checklist. Companion: `docs/SOAK_HARNESS_V1_DESIGN.md`
(long-running stability), `docs/ROADMAP.md` ("Next: external beta readiness").

## 0. Ground rules

- **No data collection machinery exists.** Nothing in this document or in
  GoalLatch v0.1.2 phones home. Participants share checklists and feedback
  voluntarily, by posting them where the maintainer can read them (issue or
  discussion), by their own action.
- **No credentials or private content.** Participants must never include
  tokens, session IDs, absolute home paths, or proprietary project content in
  feedback. A feedback template with a redaction checklist is provided in §7.
- **No fabricated evidence.** Success rates are defined in §8 with
  denominators, numerators, and the "unanswered" bucket. If nobody answers,
  the result is "no data", not 0 % and not 100 %.
- **Success means "worked for this participant on this platform", nothing
  more.** No beta result can upgrade Windows from Experimental, certify
  Apple Silicon local validation, or clear Security V2 residual risks.

## 1. Invitation text (ready to adapt)

> GoalLatch v0.1.2 is released. Its security model is documented in
> SECURITY.md. I am looking for a small number of opt-in testers on
> Linux x86_64, macOS (Intel/Apple Silicon), and optionally Windows x86_64
> (Experimental — no Unix-equivalent sandbox). You would install the release,
> run the checklist below, and paste the results (redacted) into a GitHub
> discussion. Estimated 30–45 minutes. You will get no support SLA and no
> payment. If anything looks unsafe, stop and report privately.

## 2. First-run success checklist

Each item is PASS / FAIL / NOT-RUN. A participant must not mark an item PASS
without performing it.

### A. Download and verify

- [ ] A1 Download the archive matching your platform from the
      [v0.1.2 release](https://github.com/Yrika819/local-mcp/releases/tag/v0.1.2).
- [ ] A2 Download `SHA256SUMS` and verify the archive (`sha256sum -c`,
      `shasum -a 256 -c`, or `Get-FileHash` on Windows and compare manually).
- [ ] A3 Confirm the archive contains the documented files:
      Linux: `local-mcp`, `codex-linux-sandbox`, `atomic-publish`;
      macOS/Windows: `local-mcp`/`local-mcp.exe`, `atomic-publish[.exe]`;
      plus LICENSE, README.md, SECURITY.md, THIRD_PARTY_NOTICES.md,
      CODEX-LICENSE.txt, CODEX-NOTICE.txt.

### B. Install

- [ ] B1 Install all required helpers into **one** directory (see
      `docs/release/INSTALL.md` for the exact layout; never copy only
      `local-mcp`).
- [ ] B2 Linux: `bwrap --version` reports 0.12.0 or newer and the kernel
      permits unprivileged user/PID/network namespaces.
- [ ] B3 Place the directory on `PATH` or symlink `local-mcp` from it.

### C. CLI smoke checks

- [ ] C1 `local-mcp --version` reports `0.1.2`.
- [ ] C2 `local-mcp --help` prints usage and exits 0.
- [ ] C3 (Optional) `local-mcp doctor` renders a report. Record the outcome
      and exit code — on a healthy Linux/macOS host with unrun probes the
      documented exit code is `2` ("not established"), not an error.

### D. Session and MCP client

- [ ] D1 In a disposable project directory, run `local-mcp start`; record
      the session ID (privately; never paste it publicly).
- [ ] D2 Connect an MCP-capable client to `local-mcp mcp` over stdio (see
      `docs/MCP_CLIENTS.md`). Record the client name/version for the
      platform table — no client is claimed as "verified" overall.
- [ ] D3 Call `session_info`; confirm `cwd` and `permitted_directories`
      match the start directory.
- [ ] D4 Confirm the client reports the GoalLatch tool list (file/image
      read, list, write, execute, goals).

### E. Read-only Goal end-to-end

- [ ] E1 Ask the agent to call `goal_start` with a read-only inspection
      objective, then `goal_status`.
- [ ] E2 Call `goal_run` with a bounded `max_steps`; observe progress via
      `goal_status`.
- [ ] E3 Read `goal_result`; confirm the reported artifacts/state match the
      actual project (no invented files).

### F. Permission denial

- [ ] F1 Request an operation outside `permitted_directories`; confirm it is
      refused (not silently truncated).
- [ ] F2 Deny a permission prompt in the UI; confirm the operation does not
      run and the session remains usable.

### G. Recovery

- [ ] G1 Trigger (or observe) a failed Goal step; inspect `goal_status` /
      `goal_result` and confirm a human-legible failure record exists.
- [ ] G2 Confirm a denied/failed mutation leaves no half-applied file
      changes (check the project diff).
- [ ] G3 If recovery was needed, record what the docs did or did not explain
      clearly.

## 3. Platform verification table (fill in during beta)

| Platform | Archive name | SHA-256 verified | Install | `--version` | Doctor outcome | Client connected | Read-only Goal | Permission denied | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Linux x86_64 | `local-mcp-v0.1.2-linux-x86_64.tar.gz` | | | | | | | | |
| Linux aarch64 (Experimental) | `local-mcp-v0.1.2-linux-aarch64.tar.gz` | | | | | | | | |
| macOS x86_64 | `local-mcp-v0.1.2-macos-x86_64.tar.gz` | | | | | | | | |
| macOS aarch64 (CI target) | `local-mcp-v0.1.2-macos-aarch64.tar.gz` | | | | | | | | |
| Windows x86_64 (Experimental) | `local-mcp-v0.1.2-windows-x86_64.zip` | | | | | | | | |
| Windows aarch64 (Experimental) | `local-mcp-v0.1.2-windows-aarch64.zip` | | | | | | | | |

Record `outcome` values verbatim (e.g., Doctor: `UNKNOWN`, exit `2`), never
re-characterized.

## 4. MCP client connection checks

For each client the participant tries:

1. The client spawns `local-mcp mcp` without wrapper magic (stdio, not SSE).
2. `tools/list` returns a non-empty list; note the count and whether goal
   tools are present.
3. `session_info` succeeds with the session ID from `local-mcp start`.
4. Client config format is recorded **only as "works for me on <client
   version>"**, never as a general claim; no config is published as verified.
5. If the client times out or restarts the server, note the observed
   behavior; GoalLatch sessions persist separately from client lifecycle.

## 5. Doctor diagnostic checks (optional but recommended)

1. Run `local-mcp doctor` and `local-mcp doctor --json`; record both exit
   codes.
2. Confirm `UNKNOWN` is rendered for unrun probes (they are deliberately
   unrun); confirm the report names "not established", not "broken".
3. Confirm `dependencies.bubblewrap.minimum_version` is `UNKNOWN` on Linux
   and the doctor exit code is `2` when the sandbox is required.
4. Confirm the JSON validates against the v1.0.0 schema
   (`docs/schemas/doctor-v1.schema.json`) if you have a validator, or that
   `local-mcp doctor --json` emits a syntactically valid JSON document.
5. Confirm Doctor never prompts, never writes, never installs.

## 6. Goal creation and execution checks

1. `goal_start` with a read-only objective returns a Goal ID; record only
   that a UUID was returned (never paste it).
2. `goal_status` shows the Goal is created (not running).
3. `goal_run` with `max_steps: 2..6` advances the plan; partial progress is
   legal and must be reported as such.
4. `goal_result` shows either verified completion or an honest nonterminal/
   failed state; a nonterminal state is not a success.
5. Permission UI appears for model invocations (requires Codex CLI) — if
   Codex CLI is absent, `goal_run` fails with a recorded reason; that is the
   expected result, not a bug.

## 7. Feedback template

```markdown
### Beta feedback — v0.1.2
- Platform / arch:
- Archive verified: yes/no (method):
- OS version:
- Client and version (if any):
- Checklist items: A1 A2 A3 B1 B2 B3 C1 C2 C3 D1 D2 D3 D4 E1 E2 E3 F1 F2 G1 G2 G3
  (mark each PASS/FAIL/NOT-RUN)
- What worked:
- What broke / confused you:
- Docs that helped / docs that were wrong:
- Security concerns (report these **privately**, not here):
- Consent: may this be quoted (anonymously) in a public report? yes/no
```

Redaction rules: remove session IDs, absolute home paths, repository names,
tokens, and any proprietary content. Attach `local-mcp doctor` output only
after checking it contains no paths you consider private.

## 8. Success-rate metric definitions

Define the first-run success metric precisely before collecting anything:

- **Denominator (N)**: number of checklist submissions received that can be
  tied to a unique participant platform cell (duplicates by the same
  participant collapse to one; the count is recorded).
- **Numerator (x)**: submissions in which every checklist item A1–G3 that was
  attempted is marked PASS, and none is marked FAIL. NOT-RUN items are
  permitted only where the participant records why (e.g., no Codex CLI for §6).
- **Unanswered/unknown**: submissions missing required fields are reported
  separately as *unknown* and excluded from both x and N; they are never
  counted as failures or successes.
- **Reporting**: report per platform cell: `x / N`, the explicit
  NOT-RUN list, and the unknown count. Do not aggregate into a single
  headline number; platform cells differ in support level by design.
- **Interpretation limits**: even a 100 % cell means
  "every sampled participant completed their checklist", nothing about
  sandbox closure, security posture, or population prevalence.

## 9. What this document is not

It is not a claim that a beta has run, not a support channel, not a
security review, and not an endorsement of any client or platform. Security
findings go to the private reporting channel in SECURITY.md, never to a
public issue with exploit details.
