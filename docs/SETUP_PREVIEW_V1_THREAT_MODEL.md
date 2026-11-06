# Setup Preview V1 — Threat Model

Status: DESIGN-LEVEL / CLOSED (2026-11-04). This document enumerates threats
against the proposed read-only Setup Preview surface and the mitigations the
design requires. It is a companion to `SETUP_PREVIEW_V1_DESIGN.md` and
`SETUP_PREVIEW_V1_TEST_PLAN.md`.

## 1. Assets

- A-1: User trust in what the preview shows (no false "safe to install").
- A-2: Integrity of displayed release metadata (version/asset/SHA-256) —
  including the distinction between *metadata present*, *pin consistent*,
  and *archive bytes verified* (Design §6.1).
- A-3: Host state (the preview must not change it).
- A-4: Secrets in paths/commands (must not leak into output).
- A-5: Boundary between preview and any future install execution.
- A-6: Authenticity (not just shape) of the embedded Doctor payload.

## 2. Adversaries

- External attacker controlling a mirror or release artifact (supply chain).
- Local same-user malware (explicitly out of complete defense — see R-4).
- A malicious or buggy release manifest embedded in a compromised distribution
  or supplied by the operator.
- A tampered `local-mcp`/`doctor` binary producing schema-valid output
  (see T-4 / R-5) — indistinguishable from genuine output by inspection.
- The user is not the adversary, but user error (misreading preview output)
  is a threat to A-1 and is mitigated by design, not by warnings alone.

## 3. Threats and required mitigations

| ID | Threat | Mitigation (design) | Test |
| --- | --- | --- | --- |
| T-1 | Forged/tampered archive presented as the pinned release | pinned SHA-256 verified against actual bytes before any claim of `VERIFIED_ARCHIVE`; mismatch → fail closed | N-2, H-5 |
| T-2 | Manifest claims a different version than the pin | version fixed to pin; mismatch → fail closed | N-3 |
| T-3 | "latest" mutable pointer swapped after user reads preview | preview shows only pinned immutable identity; no actionable pointer | Design §6 |
| T-4 | Doctor output forged (PASS manufactured) by a tampered binary **with schema-valid payload** | Schema validation proves shape only, never origin. Mitigations: (a) payload is rendered only as *unverified diagnostic data*; (b) authenticity disclaimer is mandatory in human output and `residual_warnings`; (c) PASS≠authorization line always rendered; (d) no output artifact is consumable as approval; (e) CI schema validation is a shape gate, never authenticity evidence | N-8, N-14 |
| T-5 | Preview output misread as "install authorized" | every output (human & JSON) states Doctor PASS ≠ install authorization; no proceed affordance exists; key-scan test asserts absence of `installable`/`authorized`/`approved` keys | H-1, N-6, N-7, N-14 |
| T-6 | Preview mutates host (cache/staging/token) | strict no-write contract; side-effect test on all CI legs | N-11 |
| T-7 | Silent transition from preview to install | no transition exists; no flags; bare `setup` exits 64 | N-12 |
| T-8 | Secrets/paths leak via output or errors | sentinel-scan tests; no env dumping; fixtures-only paths | N-9 |
| T-9 | Release metadata changes between load and render (TOCTOU on operator-supplied manifest) | double-read byte comparison per Design §6.2; differing reads fail closed with "release metadata changed" | N-13 |
| T-10 | Untrusted install root presented as OK | trust reported UNKNOWN/UNTRUSTED, never PASS; exit 0; disclaimer line mandatory | N-10 |
| T-11 | Unsupported platform silently "best-effort" rendered | explicit unsupported-platform failure state | N-4 |
| T-12 | Missing required helper masked by generic error or silently PASS-rendered | V1 has no layout validator of its own; the helper condition is surfaced by Doctor (`installation.helper.*`) and rendered verbatim as BLOCK/UNKNOWN data (exit 0). Exit `64` is reserved for Case B input errors; any `--install`-class flag is rejected by clap | N-5 (Cases A and B) |
| T-13 | Schema/version drift between Doctor releases | typed contract consumption; schema used only in CI; unknown schema/status → reject (exit 70) | Design §5, N-8 |
| T-14 | Exit code misuse (scripts treat 0 as installable) | preview exit 0 documented as "rendered", not "safe"; distinct codes 64/70; 1/2 reserved for Doctor | Design §7 |
| T-15 | Metadata-only comparison displayed as "verified" | `VERIFIED_ARCHIVE` requires hashing actual bytes in this run; metadata agreement is reported separately as `pin_consistent`; the two are never conflated | H-5, H-6, N-2, artifact-state exclusivity test |
| T-16 | Output bound exceeded → truncation hiding BLOCK/UNKNOWN | hard bounds with fail-closed exit 70; no partial output | N-15 |

## 4. Residual (accepted in preview, must gate install)

- R-1: check-to-open window for helper images (inherited from Security V2) —
  preview inherits the limitation and reports it; it is an install blocker
  for a future Setup until Security V3 closes it.
- R-2: ancestor directory redirection of install roots — same status.
- R-3: trusted-updater replacement races — availability, not authenticity,
  when both images are trusted; still no authenticity guarantee if updater
  provenance is unestablished.
- R-4: same-token malware — out of scope for any sandbox-less helper story;
  previews do not weaken it, and install authorization must state it.
- R-5 (expanded, Finding B): **Doctor payload authenticity is not
  established.** Neither schema conformance, nor the Doctor binary's location,
  nor the preview's own parsing can prove the payload was produced by a
  genuine Doctor from the intended install. A schema-valid forged `PASS`
  (N-14) is indistinguishable from a genuine one. The preview's only honest
  posture: render the payload as unverified diagnostic data, disclose the
  limitation in every output, and make the preview unconsumable as an
  authorization. Any future Setup must establish authenticity independently
  (e.g. measured launch of the installed helper/Doctor, signed attestations,
  or an OS-bound broker channel) — this is Security V3 scope, not preview
  scope.

## 5. Out of scope for the preview

- Any guarantee about the future install transaction (see Design §13).
- Network lookup as a trust extension (not in V1; future: explicit, verified,
  UNKNOWN-on-failure as per Design §6).
- Detection of runtime tampering of any kind, including the preview's own
  binary.
