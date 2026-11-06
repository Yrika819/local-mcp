# Beta Operations Templates — GoalLatch v0.1.2

Status: **PREPARATION / NO BETA IS RUNNING** (2026-11-06). Companion to
`docs/BETA_READINESS_V0.1.2.md` (which defines the participant checklist,
platform matrix, metric definitions, and feedback template). This file adds
the maintainer-facing operational templates that the readiness pack refers to
but does not yet contain. It claims no participants, no results, and no
public announcement. Recruiting, sending invitations, and declaring the beta
open are **Owner actions**, not part of this document.

## 1. Security warning template (invitation + repo-facing)

Use verbatim (adapt bracketed values only):

> **Security model — read before installing.**
> GoalLatch v0.1.2 executes tool calls on your machine under a
> session-scoped permission model. The security guarantees are documented in
> `SECURITY.md` (v0.1.2) and include known residuals:
>
> - **Windows is Experimental.** Windows runs tools host-native: there is
>   **no Unix-equivalent sandbox** on Windows, and no sandbox equivalence is
>   claimed anywhere. Only use it in disposable environments if you rely on
>   sandboxing.
> - **Apple Silicon (macOS aarch64) is a CI-validated target**, not a
>   locally release-closure-validated platform; report anomalies rather than
>   assuming correctness.
> - **Long-running mixed Goal/Job load is not fully verified** in v0.1.2
>   (see the v0.1.2 release notes' open questions).
> - Anything with the same user token as GoalLatch is outside complete
>   defense (residual R-4 in `SECURITY.md`).
>
> Report security issues **privately** through the channel in `SECURITY.md`.
> Never post exploit detail in public issues. The Setup Preview feature
> exists only on the development `main` branch — it is **not** part of the
> v0.1.2 release; do not validate or report it under this beta.

## 2. Installation verification checklist (maintainer-side, per platform cell)

For each of the six platform cells before inviting participants:

- [ ] Release archive builds and publishes for the cell (release workflow).
- [ ] `SHA256SUMS` matches every published artifact byte-for-byte.
- [ ] Archive contents match `docs/release/INSTALL.md`'s documented layout
      (helper set complete; no missing sibling helpers).
- [ ] Install contract validation (`scripts/release/verify_package.py`,
      install_contract validator) passes against the published archive.
- [ ] Doctor runs against the installed layout and reports honest
      `UNKNOWN`/`not established` states for unrun probes (exit `2` on a
      healthy host with the sandbox required but unprobed).
- [ ] `local-mcp --version` reports `0.1.2`.
- [ ] MCP stdio handshake works with at least one client config the
      maintainer can reproduce on that cell.
- [ ] Windows cells additionally: the Experimental wording above appears in
      whatever invitation text reaches Windows participants.

Any unchecked box in a cell: that cell is not offered to participants, and
the platform matrix records why.

## 3. Failure classification (for triaging beta submissions)

| Class | Definition | Immediate action |
| --- | --- | --- |
| FC-1 Install failure | archive/layout/hash/install contract fails | reproduce; block the cell if contract-level |
| FC-2 Session failure | `local-mcp start` or `mcp` handshake fails | collect doctor output (redacted); triage |
| FC-3 Permission failure | refusal paths do not refuse, or refuse wrongly | security-relevant → private channel first |
| FC-4 Goal failure | goal lifecycle states incoherent or hanging | capture goal_status/goal_result; check durable state |
| FC-5 Client interop | client-side connect/config/restart behavior | record client+version verbatim, "works for me" scope |
| FC-6 Stability | crash, hang, leak under sustained use | request the soak-style repro; treat as stability, not beta noise |
| FC-7 Security concern | anything that looks like a sandbox/permission bypass | **private reporting only**; stop public triage |
| FC-8 Documentation | doc/checklist mismatch | file docs issue; no code claim |

Rules: a failure is recorded in the class the participant observed, never
re-characterized. FC-3/FC-7 override everything else and go to the private
channel. Unanswered checklist items stay in the *unknown* bucket (§8 of the
readiness pack); they are never converted into failures or successes.

## 4. GO / NO-GO criteria for opening the external beta

The Owner answers GO only when every applicable line is true, with evidence
linked:

1. [ ] v0.1.2 release published and `SHA256SUMS` verified (evidence: release
       assets + hash run).
2. [ ] Main CI 11/11 SUCCESS on the release commit (evidence: run URL).
3. [ ] All six platform cells pass the maintainer-side checklist in §2, or
       the cell is explicitly withheld with a recorded reason.
4. [ ] Windows invitation text carries the Experimental/no-sandbox wording
       verbatim (§1).
5. [ ] Beta Readiness pack (PR #26) merged: checklist, metric definitions,
       feedback template with redaction rules.
6. [ ] Private reporting channel confirmed working (SECURITY.md contact).
7. [ ] Soak Harness V1 exists (PR #29) for stability triage; its long-run
       acceptance runs are NOT a GO prerequisite, but their absence must be
       stated in the invitation's risk description.
8. [ ] Setup Preview V1 / development-branch distinction documented wherever
       participants are told what to install (release only).

NO-GO is the default while any box is unchecked. Partial GO (per-cell) is
allowed only if the withheld cells are named and their reason recorded in the
platform matrix.

## 5. What this file is not

Not an announcement, not a recruiting message, not evidence that any beta
participant exists, and not a claim that any checklist has been executed by
anyone. As of this document there are **no beta participants**; every metric
in the readiness pack therefore reports "no data" per its own definition.
