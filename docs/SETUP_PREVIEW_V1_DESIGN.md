# Setup Preview V1 — Design (Closed for Implementation)

Status: DESIGN / CLOSED (2026-11-04). This document specifies the read-only
Setup Preview V1 surface. Implementation is tracked on branch
`feat/setup-preview-v1` and is not authorized to merge to `main` without the
Owner approvals listed in §14. Nothing in this document authorizes any
install, update, or network behavior.

Base: `main` at `ea50127e13dc0d24e3b7bc3057690c5f8cca12c1` (v0.1.2 release).
The original design proposal (`5f89670`) and its closure (`e267368`) were
written on top of `1d702d5c58bbc1173ee3a666ef5826405e6264ba` and have been
ported onto the current base with the closure fixes below (Findings A–E).

## 1. Scope and Non-Goals

Setup Preview V1 is a **read-only, presenter-only** surface that shows a user
*which fixed, already-published release* a later Setup would install on their
platform, and *what the Doctor V1 contract would say about that combination*,
**without** performing, resuming, or enabling any installation.

Explicit non-goals (hard boundaries for this design):

1. No Setup/Update execution. No `setup` execution path is proposed, staged, or
   granted any capability. `local-mcp setup` (bare) prints help and exits `64`.
2. No install changes. Install Contract V1 (`docs/INSTALL_CONTRACT_V1.md`) is
   read and displayed; it is never mutated, re-signed, or re-issued by this
   surface.
3. No network changes. The default preview performs **no network I/O**.
4. No permission changes. No new capabilities are requested from the helper,
   the approval UI, or the OS.
5. **No writes of any kind in V1.** See §2.
6. No execution, extraction, or launch of any archive member — including with
   `--archive`. Byte verification hashes the file only.

### 1.1 Carried non-goals from the original proposal

- No PR, push, merge, tag, or release for the implementation without the
  Owner approvals in §14.

## 2. Read-only contract (V1)

"Read-only" is redefined strictly. The preview execution MUST NOT create,
modify, or delete any file, including:

- temporary caches or browser/HTTP caches;
- install-preparation or staging directories;
- token, credential, or claim files;
- queued jobs, goals, or tasks;
- approval records, consent markers, or dismissal files;
- logs written outside the inherited stdout/stderr streams.

Required outputs are assembled **in memory** and emitted on stdout (human) or
stdout (JSON with `--json`). No capability is persisted for a later session.
There is no "user saw preview" bit anywhere.

Any test fixture the implementation needs (synthetic releases, archives,
layouts) is created and destroyed inside an isolated, test-owned area and is
managed by the tests alone; tests never touch the operator's install or state
directories.

If a future implementation finds it cannot honor this contract on some
platform, it must refuse to run there rather than weaken the contract.

## 3. Relationship to Doctor V1 (approval boundary)

Doctor Core V1 (contract in `docs/DOCTOR_CORE_V1_DESIGN.md`, JSON schema V1 in
`docs/schemas/doctor-v1.schema.json`) is a **diagnostic provider**. It is one
input to a preview, not the safety guarantee of the install.

Doctor's role in Setup Preview:

- The Preview renders Doctor outcomes `PASS / WARN / BLOCK / UNKNOWN`
  **verbatim**, with required `UNKNOWN` reasons surfaced as first-class UI
  states, never as collapsed secondary text.
- Doctor `UNKNOWN` or `BLOCK` **must never be silently degraded**, restyled
  into a suggestion, or omitted from the preview. The severity legend is
  fixed and cannot be user-modified in the preview.
- Permitted statuses: unknown statuses from future Doctor versions render as
  UNKNOWN-equivalent (matching the schema's forward-compatibility note) and
  are never treated as PASS.
- The Preview embeds the Doctor report under a `doctor` key (§8). It does not
  re-run probes, reinterpret reason codes, or re-count summaries.

Doctor is **not** the sole install-safety guarantor. A future Setup execution
requires its own, independent verification, none of which Doctor provides:

- artifact integrity (archive checksum vs. pinned SHA-256, member layout);
- install-target authority (who may write the install root);
- filesystem trust (ancestor protection for the install destination);
- owner approval (a fresh, explicit Owner action at execution time);
- final pre-execution re-check (re-validate immediately before any mutation);
- failure recovery (atomic staging, rollback, and cleanup on partial failure).

**A Doctor `PASS` is never an Install Authorization.** The preview must say
so explicitly in both human and JSON output, on every render path including
failure screens.

**Doctor payload authenticity is not established by this design.** Schema
conformance proves shape, not origin. See Threat Model T-4 and residual
risk R-5: a tampered but schema-valid `PASS` is indistinguishable from a
genuine one, and the preview must render it only as *unverified diagnostic
data*. Nothing in the preview — no status, exit code, field, or artifact —
is readable by any future component as an approval.

## 4. UNKNOWN / BLOCK handling

- UNKNOWN and BLOCK are never hidden, elided, truncated away, or counted as
  "ok" in summaries.
- On Linux and macOS, a *required* UNKNOWN (e.g. an unrun Bubblewrap or
  Codex probe) makes `local-mcp doctor` exit `2`. The Preview must display
  the Doctor payload and its exit-code implication as diagnostic content.
  Exit `2` means "not established", not "Doctor is broken"; the Preview must
  not collapse it into a generic error screen, and must not convert UNKNOWN
  into PASS.
- The Preview's own exit codes are its own contract (see §7). It does not
  reuse `1`/`2` to encode Doctor verdicts; Doctor verdicts are data in the
  output. Conflating the two would make scripting against the preview
  indistinguishable from scripting against the installer.
- If Doctor itself fails to run or emits an unparseable payload, the Preview
  shows the validator-failure screen and exits `70` (internal). It never
  fabricates a Doctor result.
- Required UNKNOWN and BLOCK render with first-class placement and their
  reason codes; they are never relegated below verified items.

## 5. Doctor schema, JSON contract, and distribution form

The JSON Schema for Doctor (`docs/schemas/doctor-v1.schema.json`) exists in
this repository. **A schema file present in the repository is not necessarily
present in a release archive**, and a runtime that reads a repository-path
schema at install or preview time would be broken-by-packaging. Therefore:

- The Preview consumes the **typed Doctor report** (the Rust types behind
  `local-mcp doctor --json`, i.e. the same contract the schema describes),
  not the schema document at runtime.
- Schema-conformance of the emitted JSON is a **CI gate**
  (`scripts/doctor/schema-validator`), not a production dependency. The
  CI-only validator is not added as an unconditional production dependency.
- If a future Setup needs runtime contract checking, it must use the typed
  contract or a vendored, versioned schema bundled with the binary, each by
  explicit design; it must not read `docs/` from a source checkout.

### 5.1 JSON compatibility contract (Finding C)

The previous wording — "byte-equal modulo whitespace" — is retired. The
contract is now:

1. **Parsed-value semantic equality.** The `doctor` object embedded in the
   preview JSON must be *semantically equal* to the parsed
   `local-mcp doctor --json` output: the same object graph — same keys,
   same values, same array order, same scalar types and values. Key order,
   insignificant whitespace, and number formatting (e.g. `1` vs `1.0`) may
   differ; everything else may not. Implementations compare parsed values
   (`serde_json::Value`), never raw bytes.
2. **No byte-for-byte guarantee anywhere in V1.** No field of the preview
   envelope is required to be byte-identical to any other output. If a
   future field requires byte stability, it must be named here and pinned
   in the test plan; none exists today.
3. **Schema vs. producer vs. consumer.**
   - The *JSON Schema* is the shape contract. It is enforced in CI against
     fixtures and against producer output. It is never read at runtime.
   - The *producer* (Doctor Core V1) is responsible for emitting a
     schema-valid, bounded, non-secret payload with truthful statuses.
   - The *consumer* (this Preview) is responsible for: parsing the payload;
     checking `schema_version`, `diagnostic_contract_version`, and `doctor`
     identity fields are present and supported; checking every status is
     one of the known values (unknown → UNKNOWN-equivalent rendering, never
     PASS); and refusing (validator-failure screen, exit `70`) anything it
     cannot parse or place. The consumer never re-derives Doctor outcomes.
4. **Number and string fidelity.** Semantic equality is defined over JSON
   data model values, not text. Consumers must not compare payloads after
   lossy re-encoding (e.g. float round-trip through a narrow type).

### 5.2 Output bounds (V1)

- Human output: at most **64 KiB** on stdout.
- JSON output: at most **1 MiB** on stdout.
- If assembly would exceed a bound, the preview fails closed with exit `70`,
  writes no partial output, and prints a bounded internal-error message on
  stderr. Limits are asserted in tests (Slice 5), not just documented.

## 6. Release pinning and artifact verification

The preview subject is fixed by all three of:

- **version** (e.g. `0.1.2`),
- **asset names** for the detected platform (per the Install Contract V1
  naming, `local-mcp-v<version>-<target>.<ext>`),
- **SHA-256** of each asset.

Rules:

- The default preview performs **no network I/O**. It shows the pinned
  release from a manifest compiled into the binary (the v0.1.2 pin, carried
  as a constant with the `SHA256SUMS` values published with the release),
  or from an operator-supplied `--manifest <PATH>`. It does not discover
  "latest".
- A mutable reference such as "latest published release" is never, by itself,
  the final install target. If it is ever shown, it is labeled as an
  unverified, moving pointer and is not actionable.
- If a future preview variant performs an online lookup, it must be an
  explicit operator action, must verify the fetched metadata (version,
  asset names, SHA-256) against pinned expectations, and any failure must
  yield UNKNOWN — never a silent fallback to unverified content.

### 6.1 Artifact verification states (Finding A)

The preview MUST distinguish the following, in human and JSON output:

| State | Meaning | Fail-closed? |
| --- | --- | --- |
| `MANIFEST_INVALID` | Manifest present but unparsable/missing/duplicate keys | yes — dedicated failure state, exit `64` |
| `PIN_MISMATCH` | Manifest parses, but version/asset/SHA-256 disagree with the pin | yes — dedicated failure state, exit `64` |
| `PINNED_UNVERIFIED` | Pin is consistent, but archive bytes were **not** hashed in this run | no — first-class "unverified" state, exit `0` |
| `VERIFIED_ARCHIVE` | The local archive at `--archive <PATH>` was hashed in this run and its SHA-256 matches the pin | no — first-class "verified bytes" state, exit `0` |
| `ARCHIVE_MISMATCH` | The archive was hashed and its SHA-256 differs from the pin | yes — dedicated failure state, exit `64` |
| `ARCHIVE_UNREADABLE` | `--archive` path unreadable/oversize/non-regular file | yes — dedicated failure state, exit `64` |

**Input-validation scope (normative).** The full V1 input surface is exactly:
(1) flag parsing (unknown or rejected flags, e.g. `--install`-class; clap
rejects → `64`); (2) manifest load — strict JSON parse (duplicate keys,
trailing data, encoding), schema/version checks, exactly the six pinned
targets, asset name/SHA-256 rules; (3) embedded-pin consistency check;
(4) archive hashing pre-conditions (regular file, size limit) and digest
comparison; (5) the double-read byte comparison of an operator-supplied
manifest; (6) platform support detection; (7) output bounds.

V1 implements **no dedicated layout validator**: the preview performs no
inspection of any install target, and no `--install-root`-style argument
exists. In particular, "required helper missing" is **not** a preview failure
state and never yields exit `64` from the preview. A missing sibling helper is
detected, if at all, by **Doctor**, reported verbatim in the embedded report,
and the preview renders it as data (exit `0`; see Test Plan N-5, Case A).
Do not document a layout check, install-root probe, or extracted-install
validation as implemented in V1. If a future slice adds one, this section,
Test Plan N-5, and Threat Model T-12 must be re-specified in the same PR.

Rules:

- **`VERIFIED_ARCHIVE` may be displayed only when the archive file's bytes
  were actually read and hashed in this run and the digest matched the pin.**
  Metadata comparison alone never produces `VERIFIED_ARCHIVE`.
- `PINNED_UNVERIFIED` is the *normal* V1 result when no `--archive` is given
  (which is always, unless the operator supplies one). It is rendered as a
  prominent "archive bytes not verified" state, never as a green check and
  never as "safe to install".
- With `--archive`, the file is read in a single bounded streaming pass
  (≤ 256 MiB, matching Install Contract V1 caps), hashed incrementally, and
  **never extracted, opened, or executed**. A mismatch yields
  `ARCHIVE_MISMATCH`; nothing is rendered as actionable.
- "Manifest present" and "pin consistent" are reported as *separate facts*
  (`artifact_verification.manifest_present`, `.pin_consistent`), so a reader
  can tell metadata existence from metadata agreement from byte verification.

### 6.2 Double-read consistency rule (Finding E)

Any operator-supplied metadata file that is both parsed and re-checked must
be read twice — once at load time and once immediately before rendering —
with byte-wise comparison of the two reads:

- `--manifest <PATH>`: read at load; read again before output assembly.
  If the byte strings differ, the preview fails closed with the
  "release metadata changed" failure state (exit `64`), reports both
  observations (short digests), renders nothing actionable, and never
  displays any verification state. This is the design-level rule behind
  test N-13.
- `--archive <PATH>`: hashed in a single streaming pass; it is **not** re-read
  for a second comparison (a 256 MiB re-read is pointless and racy by a
  different route). The reported digest is exactly "SHA-256 of the bytes
  read in this run" and is labeled as such. V1 makes no claim about the
  file after the pass ends.
- Embedded pin (default): immutable compile-time constant; double-read is
  not applicable and is not simulated.
- Any future code path that re-reads user-supplied metadata for display,
  cross-checking, or "helpful" re-verification must apply this same rule:
  differing reads fail closed; they are never reconciled by preferring
  one value.

## 7. CLI specification (full contract)

Selected: **`local-mcp setup preview`**.

Contract:

- `local-mcp setup --help` (and `-h`) prints the `setup` help text and exits
  `0`.
- `local-mcp setup` (bare) prints the `setup` help text to stderr and exits
  `64` (usage error). It never installs, never prompts toward install.
- `local-mcp setup preview` renders the preview.
- `local-mcp setup preview --help` prints the preview help and exits `0`.
- Flags (V1 only): `--json`, `--manifest <PATH>`, `--archive <PATH>`.
  No `--yes`, `--confirm`, `--apply`, `--install`, `--force`, or any flag that
  advances toward execution. Unknown flags are rejected by clap with exit
  `64`.
- Exit codes:
  - `0` — the preview **rendered** (says nothing about install safety).
  - `64` — usage/input error, limited to the V1 validation scope defined in
    §6.1 ("Input-validation scope"): unsupported platform, manifest invalid,
    pin mismatch, archive mismatch/unreadable, release-metadata-changed,
    bare `setup`, unknown flag, non-UTF-8 arguments. A missing required
    helper is **not** a `64` condition — it is a Doctor outcome rendered as
    data (see Test Plan N-5, Case A).
  - `70` — internal error: serialization failure, output bound exceeded,
    Doctor payload unparseable, renderer bug.
  - Exit `1` and `2` are not used by the preview; they remain exclusively
    Doctor's contract.
- **"Unverifiable" is not an error.** A render in which checks cannot be
  established (no archive supplied, required UNKNOWN present, untrusted
  install root) exits `0` and encodes the unestablished state as data
  (`UNKNOWN` statuses, `PINNED_UNVERIFIED`, warnings). Only usage/input
  errors (`64`) and internal errors (`70`) are non-zero exits. User-input
  error (`64`), unverifiable (rendered, `0`), and internal error (`70`) are
  three disjoint classes with disjoint handling; tests assert the mapping
  (§Test Plan).
- Error messages: every failure state names the failing check, the expected
  vs. actual value (version/asset/SHA-256/path digests), and the
  non-mutating nature of the result. No error message may say or imply
  "install anyway".
- Output bounds per §5.2. Display is not consent: no output, exit code, or
  artifact of the preview is readable by any future component as approval.
  There is no token, file, or record to forge.
- Cross-platform: no OS-specific code paths in the renderer; platform
  detection reuses the existing read-only platform probes used by Doctor.
- Backward compatibility: `start`, `mcp`, `doctor`, `--version`, `--help`
  behave exactly as in v0.1.2. The `setup` noun is additive;
  `local-mcp setup preview` does not appear in v0.1.2.

### 7.1 Human output layout (fixed order)

1. Header: `local-mcp setup preview` banner and the literal line
   `DRY RUN — no installation will be performed`.
2. Platform: OS, architecture, supported/experimental classification.
3. Pinned release: version, target, asset name, SHA-256 (or `not supplied`),
   and the artifact verification state label from §6.1.
4. Install Contract summary: required helpers per platform, experimental
   notes, symlink/reparse refusal note (Security V2 residual), no-install
   statement.
5. Doctor section: outcome counts and exit-code implication, verbatim
   required UNKNOWN/BLOCK with reason codes, the fixed severity legend, and
   the authenticity disclaimer from §3.
6. Confirmed items / Unconfirmed items: two explicit lists.
7. Hypothetical install requirements: what *would* be required (helpers,
   install root trust, fresh Owner approval, final re-check), labeled as
   hypothetical; no step is executed or staged.
8. Residual warnings and the fixed line:
   `A Doctor PASS is not an installation authorization.`

### 7.2 JSON output layout

One JSON object on stdout; schema version of the envelope is separate and
versioned. Keys (V1):

```json
{
  "schema_version": "1.0.0",
  "preview_contract_version": 1,
  "goallatch_version": "0.1.2",
  "platform": { "os": "windows", "arch": "x86_64", "target": "windows-x86_64",
                "supported": true, "experimental": true },
  "release": { "version": "0.1.2", "asset": "local-mcp-v0.1.2-windows-x86_64.zip",
               "sha256": "f5d2…", "experimental": true },
  "artifact_verification": {
    "status": "PINNED_UNVERIFIED",
    "manifest_present": true, "pin_consistent": true,
    "archive_present": false, "archive_sha256": null,
    "expected_sha256": "f5d2…", "detail": "archive bytes not verified in this run"
  },
  "install_contract": { "required_helpers": ["local-mcp.exe", "atomic-publish.exe"],
                        "summary": "…" },
  "doctor": { "…": "verbatim Doctor report, §5.1" },
  "overall_preview_result": "RENDERED_UNVERIFIED",
  "evidence": { "doctor_embedded": true, "manifest_source": "embedded",
                "double_read_consistent": true },
  "residual_warnings": ["Doctor payload authenticity is not established", "…"]
}
```

`overall_preview_result` is one of `RENDERED_VERIFIED`,
`RENDERED_UNVERIFIED`, `DOCTOR_BLOCK`, `DOCTOR_REQUIRED_UNKNOWN`, `FAILED`.
It is a convenience summary for humans and scripts; the authoritative facts
live in `artifact_verification.status` and the embedded `doctor` report.
User names, home directories, environment variable values, and credential
material never appear; paths supplied by the operator appear only as given,
and the operator's own paths are never echoed beyond the flag value itself.

## 8. Doctor → Preview type and contract mapping

| Doctor artifact (main) | Preview usage | Notes |
| --- | --- | --- |
| `src/doctor.rs` `DiagnosticStatus` (PASS/WARN/BLOCK/UNKNOWN) | rendered verbatim; embedded in JSON | unknown future statuses → UNKNOWN-equivalent |
| `src/doctor.rs` reason codes | shown as first-class reasons | never restyled or dropped |
| `docs/schemas/doctor-v1.schema.json` (v1.0.0) | CI conformance of emitted JSON only | not read at runtime; see §5 |
| `docs/examples/doctor-v1-{pass,warn,block,unknown}.json` | golden fixtures for renderer tests | synthetic fixtures, not captured output |
| `tests/doctor_cli.rs` exit codes 0/1/2/64/70 | referenced; preview does not reuse 1/2 | see §7 |
| `local-mcp doctor --json` parsed value | embedded under `doctor` key | semantic equality per §5.1; unparseable → §4 fallback, exit 70 |

## 9. No auto-elevation

- The preview contains no internal transition into the actual installer.
  There is no "proceed and install" path inside it, no booking of follow-up
  work, and no pre-approved capability bundle a later session could inherit.
- The preview must not persist state such as "user saw preview and consented"
  that future Setup code could read as consent.
- Cross-surface promotions are structurally impossible: the preview produces
  no side effects, no tokens, no staged trees, no queued goals.

## 10. Owner Approval Boundary

- Setup Preview itself requires **Owner approval** before any implementation
  PR that adds it to a release surface is merged (this document alone is
  design text and does not require one).
- The approval must name: intended release surface, platforms, the exact
  Doctor version(s) the preview may consume, the pinned-release source for
  V1 (the embedded v0.1.2 pin), and explicit confirmation that no
  execution, install, or update capability is implied.

## 11. Verification plan (design-level)

See `docs/SETUP_PREVIEW_V1_TEST_PLAN.md` for the full negative-test matrix
and `docs/SETUP_PREVIEW_V1_THREAT_MODEL.md` for the threat model.
Design-level expectations:

1. Golden-preview snapshot tests asserting Doctor UNKNOWN/BLOCK renders
   verbatim (mirroring the schema fixtures).
2. Negative test: a mutated/corrupt Doctor payload does not render; only the
   validator-failure screen is shown, exit 70.
3. Side-effect test: after a full preview session (including `--json`), no
   filesystem change is observable outside test-owned areas (snapshot the
   relevant roots before/after).
4. Release-pinning test: manifest with wrong version / asset / SHA-256 fails
   closed with a dedicated failure state; nothing is rendered as actionable.
5. Artifact-state test: `PINNED_UNVERIFIED`, `VERIFIED_ARCHIVE`,
   `ARCHIVE_MISMATCH` are mutually exclusive and each asserted by fixture.
6. No-transition test: the preview module links no installer path; import
   graph and integration test prove no code path from preview reaches a
   mutation.
7. Authenticity-label test: a schema-valid forged PASS renders with the
   authenticity disclaimer; the disclaimer text is asserted verbatim.

## 12. Implementation PR slices (proposed)

Implementation is **not** approved by this document. When it is, split into:

1. **Slice 1 — Preview contract and pure data model**: typed manifest,
   pinned release model, artifact verification states, validation errors;
   pure functions; no I/O. (`setup` module, `SetupPreviewReport` types.)
2. **Slice 2 — Read-only renderer and JSON output**: human renderer + JSON
   envelope embedding Doctor verbatim; golden snapshot tests.
3. **Slice 3 — CLI integration**: `setup` noun, `preview` subcommand,
   `--json`, `--manifest`, `--archive`, exit codes, help; no execution path
   exists.
4. **Slice 4 — OS/platform tests**: path/layout/platform cases for
   Linux/macOS/Windows; fixtures in test-owned areas only.
5. **Slice 5 — Failure injection and CI**: N-1…N-14 green, CI legs confirmed,
   schema-CI hookup for preview JSON, user docs cross-links.

Each slice is independently reviewable and ≤ ~500 net lines where practical.

## 13. Setup V1 hand-off — unimplemented requirements

The following are required before any real Setup/Update exists, and are
deliberately **not** designed here beyond this list:

- artifact integrity verification bound to the opened archive (not just the
  downloaded file);
- install-target authority model (what may be written, by whom, with what
  ancestry protection);
- failure recovery: atomic staging, rollback, partial-install cleanup;
- fresh Owner approval at execution time, distinct from preview display;
- final pre-execution re-check of every precondition;
- update/rollback semantics and schema compatibility checks;
- signed/verified release metadata if online lookup is ever added;
- **Doctor payload authenticity**: binding between the Doctor binary image
  and its output (see Threat Model R-5). Schema validity cannot provide this;
  it needs an independent channel (e.g. measured launch, signed diagnostic
  attestations) and is a Security V3-scope question;
- reconciliation with helper-execution provenance (Security V2) limits:
  ancestor replacement and check-to-open windows remain open and must be
  addressed or explicitly accepted by the Owner before install authorization
  can exist.

## 14. Owner approvals required before implementation merges

- [ ] Owner approval of this design as the implementation basis
- [ ] Owner approval of the pinned-release source for V1 (embedded v0.1.2 pin)
- [ ] Explicit non-goal sign-off repeated at implementation-PR review time
- [ ] Security V3 gate review (see `design/security-v3`) for install-blocking
      items before any Setup executes
