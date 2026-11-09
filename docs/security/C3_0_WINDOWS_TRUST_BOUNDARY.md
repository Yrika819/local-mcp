# C3-0 — Windows Owner Trust Boundary: threat model, option comparison, PoC plan

Status: **DESIGN + READ-ONLY FEASIBILITY ONLY.** No OS user was created, no
service was registered, no permission or security policy was changed, and no
host security setting was touched. Those need explicit Owner approval.

Scope of the problem (R-1 / C3): the Owner Control Surface (OCS) prevents grant
forgery **through MCP**, but the OCS, the MCP server and any agent process all run
as the same Windows user. An agent that can run code as that user can therefore
reach the same objects the Owner reaches. This document states what that means,
compares the isolation options, and specifies the PoC that would prove one.

---

## 1. What is actually being protected

| Asset | Where it lives today | Why it matters |
| --- | --- | --- |
| Pending-request nonce | shown on the OCS console, stored only as `nonce_sha256` in the ledger | Approval proof. Whoever can read the live nonce can approve their own request. |
| Owner decision input | OCS console (stdin of `local-mcp start`) | The human "yes". |
| Authorization ledger | `<state>/authorization/<session>/<goal>.json` + `.lock` | The grant, its budget, its revocation state. |
| Goal store | `<state>/goals/<session>/<goal>.json` | The durable Goal, WorkItem binding, journal. |
| Publication helper | `atomic-publish[.exe]` next to the binary | The only thing that writes workspace files. |

Today's protection is **logical**: `nonce_sha256`, `AUTHORIZATION_FENCE_TIMEOUT`,
CAS revisions, and the fact that the MCP tool surface has no grant-issuing tool.
The product already reports this honestly in `AuthorizationView`:

* `owner_attestation` — what the OCS actually attested;
* `same_uid_process_resistance: false` — visible to the Owner, not hidden.

## 2. Adversary model

| Level | Capability | Must the design stop it? |
| --- | --- | --- |
| A1 | Code execution as `Yuta`, **unelevated** — the normal case for a local AI agent | **Yes.** This is the whole point of C3. |
| A2 | Code execution as another *restricted* local user | Yes for Broker assets; the Broker is not that user's object. |
| A3 | Elevated / `SYSTEM` | **No.** Do not claim OS user separation stops an administrator. The correct answer is "a host administrator is the Owner". |
| A4 | Offline disk access (steal `%LOCALAPPDATA%`) | Only with at-rest protection (DPAPI/TPM keyed to a different principal). Out of scope for V1 unless the Broker key is DPAPI-scoped to the Broker user. |

Host facts verified read-only on this machine (2026-11-09):

* `DESKTOP-L7H6108\Yuta`, Windows NT 10.0.26300.0, **not** elevated
  (`IsInRole(Administrator) = False`).
* `net user` lists `CodexSandboxOffline` and `CodexSandboxOnline` — the Codex CLI
  already creates dedicated local users for its own sandboxing. A per-product
  service/worker account is therefore *precedented* on this host, not exotic.

## 3. Trust boundary (target shape)

```
┌───────────────────────────── User session (Yuta) ─────────────────────────────┐
│  AI agent / MCP client            local-mcp MCP server         atomic-publish │
│  (untrusted)                      (untrusted w.r.t. Owner)     (helper)       │
│        │  MCP stdio                     │  named pipe (authenticated)          │
└────────┼───────────────────────────────┼──────────────────────────────────────┘
         │                               │
         │        ══════ TRUST BOUNDARY (OS principal) ══════
         │                               │
┌────────┴───────────────────────────────┴──────────────────────────────────────┐
│  Broker (separate local user, Windows Service or per-user service)            │
│   • holds the Owner signing key (DPAPI machine/user scoped to Broker)         │
│   • owns the authorization ledger directory (ACL: Broker only)                │
│   • shows/collects the Owner decision on a channel the user session cannot    │
│     write to (service UI + Windows Hello, or a console owned by Broker)       │
│   • mints GoalWriteGrantV1 after a verified user-presence event               │
│   • never executes model output                                             │
└──────────────────────────────────────────────────────────────────────────────┘
```

Key point: the MCP server stays in the user session (it must read the workspace).
Only the *authority* moves. The server asks the Broker to charge an operation and
receives a signed, budgeted answer; it never holds the grant-signing key.

## 4. Option comparison

Scored for this product: a Rust MCP server on Windows 11, installed per-user,
used alongside Git/Cargo/the user's IDE.

| # | Option | Stops A1 | Stops A2 | Install cost for a normal user | Win11 fit | Rust effort | Git/Cargo/tool compat | Process-spawn/sandbox fit | Impact on existing GoalLatch | GUI automation (future) | Non-Windows portability |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| A | **Separate OS user Broker** | Yes | Yes | Medium: needs one account creation (admin) at install, then never again | Good; `CodexSandbox*` shows the pattern | Medium (service/IPC/ACL work in Rust; `windows-sys` already a dep) | None: Broker touches no workspace | Neutral: Broker spawns nothing | Additive: ledger path + a signed charge call; goal/workitem model unchanged | Neutral | Conceptually portable (Unix user + unix socket); Windows is the hard part |
| B | **Windows Service + restricted Worker** | Yes (service SID differs from user) | Yes | Medium-high: service registration (admin) | Native | Medium-high (SCM, recovery, upgrade) | None for the Broker; the Worker still needs workspace access | Good: service can launch the worker under a restricted token | Additive, same as A | Good: a service can host a UI-less Broker and a per-session helper | Low (launchd/systemd differ) |
| C | **Restricted token / AppContainer for the worker** | Partly: blocks writes to Broker objects only if ACLs are set | Partly | Low: no admin needed (`CreateAppContainerProfile` is per-user) | Native | Medium | **Poor**: AppContainer cannot read arbitrary workspace paths without capabilities; Git/Cargo need broad FS access | Good for spawning | Would change how the MCP server runs today → high regression risk | Poor: GUI automation from an AppContainer is heavily restricted | Low |
| D | **Windows Hello / TPM user presence** | Yes, *if* the approval is bound to a Hello-protected key | Yes | Low for the user (a PIN/biometric they already have) | Native (`UserConsentVerifier`, `KeyCredentialManager`) | Medium | None | Neutral | Additive: the OCS approval becomes a Hello-signed assertion | Good | **None** (Windows-only API) |
| E | **Serea Android challenge signature** | Yes for remote approvals; no for a local process that can also reach the phone | Same | High (needs a paired device) | N/A (phone) | Medium | None | Neutral | Additive: a second approval channel | Good (it is the future remote console) | Good (device-agnostic) |

Reading of the table:

* **C alone is not enough.** An AppContainer restricts the *worker*, but the
  agent that must be stopped (A1) is not in an AppContainer — it is the user's
  own shell/agent. Adding C without A/B just constrains GoalLatch more.
* **D alone is not enough.** Hello proves a human was present, but if the
  approval prompt is drawn by the untrusted process, the untrusted process can
  relay a Hello signature for its own request. D is only sound *behind* A or B
  (the prompt must be owned by the trusted principal).
* **A is the minimum viable boundary.** It is the only option that both stops A1
  and fits a normal user install without constraining Git/Cargo.
* **B is A plus lifecycle.** It adds crash recovery and a stable principal, at
  the cost of service registration.
* **E is the right future answer for "approve from my phone"**, and it composes
  with A/B: the Broker verifies either a local Hello assertion or a
  device-signed challenge.

**Recommendation: A now, D as the local consent factor, E as the remote consent
factor, B when the Broker needs to survive logoff.** C is a *worker* control, not
an Owner-authority control, and should be evaluated separately as a sandbox
feature.

## 5. PoC specification (not executed — needs Owner approval for the account)

The PoC must show that a restricted worker **cannot** impersonate the Owner. It
is deliberately adversarial.

Environment: a throwaway local account `gl-broker-poc` (created by the Owner, or
by an elevated shell the Owner authorises), plus the existing unelevated `Yuta`
session. No change to production install paths.

| # | Attack (run as `Yuta`, unelevated) | Expected result |
| --- | --- | --- |
| P1 | Read the Broker's ledger file directly | `Access is denied` (ACL grants only `gl-broker-poc`) |
| P2 | Write a forged ledger file (replace a grant) | Denied |
| P3 | Open the Broker's named pipe and send a forged "approve" frame | Rejected: the pipe ACL admits only the Broker SID; a caller-SID check plus a per-connection nonce |
| P4 | Replay a captured approve frame | Rejected: single-use nonce, monotonic decision counter, request digest bound |
| P5 | Start a process as `gl-broker-poc` without credentials | Requires the account password; a standard user cannot `CreateProcessWithLogonW` without it |
| P6 | Read the Broker's DPAPI-scoped signing key | Denied (key is scoped to the Broker principal) |
| P7 | Replay a *valid* signed charge outside its operation id / budget | Rejected by the existing idempotent charge + budget floor |
| P8 | Approve while the Owner console shows a *different* request | Rejected: the decision is bound to `request_id` + nonce + `goal_identity_digest` |
| P9 | Same attacks from an elevated shell | **Succeeds.** Recorded explicitly as out of scope — an administrator is the Owner. |

Also recorded, because they decide feasibility:

* whether the Broker can be started per-user at logon without admin (Task
  Scheduler with "run as" + stored credential, or a service);
* what the additional UAC prompts are for a normal user install;
* whether the Broker can present a Windows Hello prompt in its own session and
  return the assertion over the pipe;
* the failure mode when the Broker is not running (must be: **no grant**, i.e.
  fail closed, never "auto-approve because the Broker is unavailable").

## 6. Residual risk after C3 is implemented

1. **A3 (administrator) is unmitigated by design.** Any statement that C3 makes
   EXTERNAL Writer "safe on a normal Windows machine" must be qualified with
   "against a non-elevated adversary".
2. **The workspace is still user-writable.** The Broker protects *authority*, not
   files. A same-user agent can still edit the workspace directly, outside
   GoalLatch. That is a different threat (it is the user's own shell) and
   GoalLatch must not be described as protecting a repository from its owner.
3. **Availability**: a Broker that is down blocks all writes. That is the correct
   fail-closed direction but is a product decision the Owner must accept.
4. **The GUI/HID channel** (future Serea PC automation) will need its own
   authority design; the C3 boundary is about grants, not about input injection.

## 7. Relation to Serea (E)

The Serea-side answer is a device-bound challenge signature: the Broker issues a
challenge bound to `request_id` + nonce + goal digest; the phone signs it with a
key that never leaves the device; the Broker verifies against a pinned public
key. This is the same object as D, with a different consent channel, and it does
not change the wire contract in
`Serea/docs/protocols/08-goallatch-adapter-protocol.md` — it stays inside
`host.goal.*` approval, i.e. inside Serea's own approval protocol. See the S0-0
assessment for the boundary rules that must hold either way.
