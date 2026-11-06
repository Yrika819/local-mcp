# Setup Preview V1 — Test Plan (Design-Level, Closed)

Status: DESIGN-LEVEL / CLOSED (2026-11-04). This plan describes the tests to
be implemented with the Setup Preview V1 slices; it is normative for the
implementation PR. See `docs/SETUP_PREVIEW_V1_DESIGN.md` §12 and
`docs/SETUP_PREVIEW_V1_THREAT_MODEL.md`.

Audience: implementers of Slices 1–5 (see the Design §12) and release CI
maintainers.

## 1. Test levels

| Level | Scope | Runner |
| --- | --- | --- |
| Unit (pure) | manifest/pin parsing, validation, artifact states, renderer logic | `cargo test` |
| Golden snapshot | human & JSON rendering of fixed fixtures | `cargo test` |
| Negative | every failure state in §3 | `cargo test` |
| Contract | Doctor payload embedding, parsed-value semantic equality | `cargo test` |
| Side-effect | filesystem unchanged outside test-owned areas | `cargo test` + CI |
| CI gates | schema/fixture validation, producer validation | GitHub Actions |
| Native smoke | each OS leg builds and renders | CI matrix |

Global rules:

- Tests never touch the operator's real install/state directories; all
  fixtures live in test-owned temp areas removed on completion.
- No test performs network I/O. Pinning is exercised with synthetic
  manifests/archives/checksums.
- No test asserts user-facing paths from the developer machine.
- "NOT RUN" is an explicit, recorded outcome — never silently skipped.
- Tests never weaken a verification condition to achieve green; any such
  change invalidates the slice's readiness claim.

Exit-code classes asserted by this plan (Design §7):

- user-input error → `64` (manifest invalid, pin mismatch, unsupported
  platform, archive mismatch/unreadable, metadata-changed, bad flags,
  non-UTF-8 args, bare `setup`);
- unverifiable → rendered with `0` (no archive supplied, required UNKNOWN,
  untrusted install root); the unestablished state is data, not an error;
- internal error → `70` (serialization failure, output bound exceeded,
  unparseable Doctor payload, renderer bug).

## 2. Happy-path cases (summary)

| ID | Scenario | Expected |
| --- | --- | --- |
| H-1 | valid pinned manifest/pin, healthy fixture layout | renders platform/release/contract/doctor; exit 0 |
| H-2 | `--json` output | valid envelope; parsed `doctor` value semantically equal to the fixture payload (§5.1); key order/whitespace may differ |
| H-3 | Doctor PASS / WARN / BLOCK / UNKNOWN fixtures | each rendered verbatim, incl. exit-code implication text |
| H-4 | unsupported but known platform in manifest | renders "unsupported platform" failure state; exit 64 |
| H-5 | `--archive` with bytes matching the pin | `artifact_verification.status == VERIFIED_ARCHIVE`; exit 0 |
| H-6 | no `--archive` (default) | `artifact_verification.status == PINNED_UNVERIFIED`; prominent unverified rendering; exit 0 |

## 3. Negative cases

Each case lists: input → expected result → evidence → NOT RUN condition.

### N-1 Invalid release manifest
**Input:** manifest with malformed JSON, missing keys, or duplicate keys.
**Expected:** `MANIFEST_INVALID` failure state; no Doctor section rendered as
actionable; exit `64`; error names the failing key.
**Evidence:** snapshot test of stderr/stdout; golden failure fixture.
**NOT RUN:** never; pure unit test.

### N-2 Archive checksum mismatch
**Input:** archive whose bytes don't match the pinned SHA-256 (flipped-byte
fixture) via `--archive`.
**Expected:** `ARCHIVE_MISMATCH` failure state; actual vs. expected digest
shown (truncated for display, full in error); archive is not opened,
extracted, or executed; exit `64`; nothing actionable rendered.
**Evidence:** test-owned temp dir; assert zero extracted files afterwards.
**NOT RUN:** never. The archive is hashed with a bounded streaming read; the
cap is exercised by a synthetic oversize-manifest fixture (not a 256 MiB file).

### N-3 Version mismatch
**Input:** manifest version `0.2.0` vs. embedded pin `0.1.2` (or asset name
carrying a different version).
**Expected:** `PIN_MISMATCH` failure state naming version; exit `64`; no
actionable output.
**Evidence:** snapshot test.
**NOT RUN:** never.

### N-4 Unsupported platform
**Input:** platform triple with no release asset mapping (e.g.
`riscv64-unknown-linux-gnu`).
**Expected:** "unsupported platform" failure state; names the detected
platform and the supported list; exit `64`.
**Evidence:** table-driven unit test over all non-supported triples.
**NOT RUN:** never.

### N-5 Required helper missing (Cases A and B)

There are exactly two distinct cases. They are **not** interchangeable.

**Case A — Doctor detects a missing (or otherwise refused) helper in the
executable's own sibling layout.**
The preview embeds the Doctor report verbatim; it performs no filesystem
inspection for this condition of its own. A Doctor check such as
`installation.helper.atomic_publish` or `installation.helper.linux_sandbox`
with `status: BLOCK` (e.g. `INSTALL_REQUIRED_HELPER_MISSING`) renders with
first-class placement and its reason codes; nothing is rendered as actionable
or installable.
**Expected (normative):** exit `0` (the preview rendered); the BLOCK appears
verbatim in human output and in the embedded `doctor` payload; JSON
`overall_preview_result == "DOCTOR_BLOCK"`; the mandatory disclaimer lines are
present; no hint of installability anywhere.
**Evidence:** golden snapshot of a synthetic Doctor payload containing the
helper BLOCK; JSON assertion on the embedded report. An integration leg may
additionally assert this where the test binary's sibling layout can omit the
helper; legs that cannot emulate that layout record NOT RUN with reason and
rely on the synthetic fixture.

**Case B — Preview input validation, including paths and layout checks V1
actually performs.**
The complete V1 validation surface is: flag parsing (unknown/rejected flags;
including `--install`-class flags), manifest strict-parse (JSON syntax,
duplicate keys, trailing data, non-UTF-8), schema/version and six-target
completeness checks, asset name/SHA-256 rules, embedded-pin comparison,
double-read byte comparison of an operator-supplied `--manifest`, archive
hashing pre-conditions (regular file, ≤ 256 MiB) and SHA-256 comparison,
platform support detection, and output bounds. There is **no dedicated layout
validator, install-root probe, or extracted-install check** in V1, and none of
these returns `64` for a *missing helper*.
**Expected (normative):** each of the following, when it fails, exits `64`
with the failure state indicated: unknown/`--install`-class flag → clap usage
error (`64`); malformed/oversize/non-UTF-8 manifest → `MANIFEST_INVALID`
(`64`); manifest/pin disagreement → `PIN_MISMATCH` (`64`); manifest changed
between the two reads → `METADATA_CHANGED` (`64`); archive unreadable,
oversize, or non-regular → `ARCHIVE_UNREADABLE` (`64`); archive digest mismatch
→ `ARCHIVE_MISMATCH` (`64`); non-target triple → `UNSUPPORTED_PLATFORM`
(`64`).

**Contract clarification:** exit `64` applies **only** to the input errors
listed under Case B. A missing required helper is a Doctor outcome (data in the
report), never an exit-`64` preview input error; it is never reported as PASS
or rendered as advisable. The only exit-`64` helper-adjacent cases that exist
are Case B input failures (e.g. bytes that cannot be hashed).

**No dedicated layout validator exists; none is claimed.** If a future slice
adds an install-root/layout validator, this section, Design §6.1, and Threat
Model T-12 must be re-specified in the same PR. Until then, any test asserting
"missing helper → exit `64`" for the preview is invalid and must be rewritten
against the cases above or recorded NOT RUN with reason.

**Evidence:** Case A — synthetic helper-BLOCK fixture (golden snapshot + JSON
assertions, all legs). Case B — one failure-state test per listed input class
(`MANIFEST_INVALID`, `PIN_MISMATCH`, `METADATA_CHANGED`, `ARCHIVE_UNREADABLE`,
`ARCHIVE_MISMATCH`, `UNSUPPORTED_PLATFORM`, flag rejection).
**NOT RUN:** only the optional native-layout integration leg may be NOT RUN
(recorded with reason); Case A fixture assertions and all Case B tests run on
every CI leg.

### N-6 Doctor UNKNOWN (required)
**Input:** synthetic Doctor payload with required UNKNOWN (e.g.
`BWRAP_VERSION_UNKNOWN` with requirement=required) via `--json` embed path.
**Expected:** UNKNOWN rendered as first-class state with reason codes;
preview notes the Doctor exit-2 implication without calling it "broken";
preview exit still `0` (the preview rendered); UNKNOWN never upgraded to
PASS in human or JSON output; `overall_preview_result` does not read as
verified.
**Evidence:** golden snapshot; JSON key assertions.
**NOT RUN:** never.

### N-7 Doctor BLOCK
**Input:** synthetic Doctor payload with required BLOCK.
**Expected:** BLOCK rendered verbatim with blocking reasons; no "install
anyway" affordance anywhere; JSON `doctor.summary.status == "BLOCK"`;
`overall_preview_result == "DOCTOR_BLOCK"`.
**Evidence:** golden snapshot; JSON assertions.
**NOT RUN:** never.

### N-8 Unknown schema version / unparseable Doctor payload
**Input:** Doctor JSON claiming schema `99.0.0`, an unknown
`diagnostic_contract_version`, an unknown status value, or unparseable bytes.
**Expected:** payload rejected as UNKNOWN-equivalent/unsupported; preview
shows the validator-failure screen; never rendered as PASS; exit `70`.
**Evidence:** contract test over mutated fixtures.
**NOT RUN:** never.

### N-9 Secret/path leakage
**Input:** Doctor payload / manifest containing sentinel secrets (e.g.
`AKIA…`-style sentinel, absolute user paths with username).
**Expected:** renderer never echoes host-derived absolute paths with
username sourced from the *operator's* machine; fixtures' paths rendered only
as given; no environment variables printed; stderr contains no secret
material; stdout never contains the sentinel.
**Evidence:** sentinel-scan test over captured stdout/stderr.
**NOT RUN:** never.

### N-10 Untrusted install directory (specified — Finding D)
**Input:** fixture indicating a world-writable (mode 0777) install root, or
replaced-after-check layout metadata, supplied to the preview's install-root
trust evaluation.
**Expected (unique, normative):**
- the preview renders install-root trust as `UNKNOWN`/`UNTRUSTED` (never
  PASS);
- the output states plainly that Doctor PASS ≠ install authorization (the
  fixed disclaimer line is present);
- the preview exits **`0`** (this is an unverifiable *rendered* outcome, not
  a usage error and not an internal error — the three classes are disjoint,
  see §1 of this plan and Design §7);
- JSON: `install_root_trust.status ∈ {"UNKNOWN", "UNTRUSTED"}` (field
  present, never `PASS`), and `overall_preview_result` is not
  `RENDERED_VERIFIED`;
- no output or exit code claims installability.
**Evidence:** unit test with synthetic trust metadata; snapshot of rendered
output; JSON assertion.
**NOT RUN:** only the trust-metadata probe itself may be NOT RUN where the
platform/filesystem cannot report it (recorded with reason); the exit-`0`
behavior and the disclaimer line must still be asserted on every CI leg.

### N-11 Previewのfilesystem変更なし (no filesystem change)
**Input:** run full preview (human and `--json`) against a watched
test-owned root populated with fixture files.
**Expected:** zero file creations/modifications/deletions and zero mtime
changes attributable to the preview; stdout/stderr are the only outputs.
**Evidence:** before/after directory manifest comparison (paths + hashes +
mtimes).
**NOT RUN:** never; must run on every CI OS leg.

### N-12 PreviewからSetupへの無断遷移なし (no unauthorized transition)
**Input:** attempted `local-mcp setup` (bare); `local-mcp setup preview
--install`; `local-mcp setup preview --yes`; `local-mcp setup preview
--apply`; `local-mcp setup preview --force`.
**Expected:** bare `setup` prints help to stderr, exit `64`; unknown flags
rejected by clap, exit `64`; `setup preview` with no flags exits `0` with a
rendered preview; none of these invocations reaches any mutation path
(proven by the N-11 harness on each invocation).
**Evidence:** CLI integration tests; N-11 harness reused.
**NOT RUN:** never.

### N-13 Release metadata change race (specified — Finding E)
**Input:** operator-supplied `--manifest` fixture swapped between the two
reads the preview performs (load-time read and pre-render read; Design §6.2):
first read sees SHA-256 A, second read sees SHA-256 B. Also the unparseable
mid-run case.
**Expected:** the byte comparison of the two reads fails; the preview fails
closed with the "release metadata changed" failure state; exit `64`; both
observed short digests are reported; nothing actionable is rendered; no
artifact-verification state is displayed. The embedded default pin is exempt
(immutable constant) and is not artificially double-checked.
**Evidence:** injected double-read fixtures (test-owned path; a test hook or
sequential writes reproduce the swap); snapshot of the error; exit-code
assertion.
**NOT RUN:** never. Note the `--archive` single-pass rule (Design §6.2) is
covered by N-2 and is not re-tested here.

### N-14 Forged-but-schema-valid Doctor PASS (new — Finding B)
**Input:** a synthetic Doctor payload that is **schema-valid** but forged —
e.g. every check `PASS`, `goallatch_version` claimed, exit-code implication
`0` — produced to mimic a tampered binary's output. An additional fixture
mutates only the Doctor-reported signature/evidence fields (all still
schema-valid).
**Expected:**
- the payload is accepted for rendering (the preview cannot and must not
  detect forgery);
- the render discloses that Doctor payload *authenticity is not established*
  — the authenticity disclaimer line is present verbatim in human output and
  `residual_warnings` contains the equivalent entry in JSON;
- no field, status, exit code, or hint may be read as install authorization;
  the PASS≠authorization line is also present;
- `overall_preview_result` is not treated as authoritative for install by
  any consumer (documented; no consumer exists in V1);
- exit code remains `0` (rendered).
**Evidence:** golden snapshot asserting both disclaimer lines; JSON
assertions on `residual_warnings`; absence of any `installable`,
`authorized`, `approved`, or `proceed` key anywhere in the envelope
(explicit key-scan assertion).
**NOT RUN:** never.

### N-15 Output size bounds
**Input:** fixtures engineered to push human and JSON rendering over the
§5.2 bounds (64 KiB human / 1 MiB JSON), e.g. a Doctor report with the
maximum number of checks and maximum-length messages, or a manifest with
maximum-length strings that still parse.
**Expected:** exit `70`; no partial output on stdout; bounded internal-error
message on stderr; no panic.
**Evidence:** unit tests around the renderer bounds; CLI integration test
capturing stdout size.
**NOT RUN:** never.

### N-16 Non-UTF-8 input
**Input:** manifest/archive path arguments containing invalid UTF-8 (via
`OsStr` on Unix; synthetic on Windows), and a manifest file containing
invalid UTF-8 bytes.
**Expected:** manifest-invalid failure state (exit `64`) naming the encoding
problem; no panic; no mojibake rendered as a value.
**Evidence:** unit test with raw bytes; CLI integration test (Unix-only for
argument bytes, clearly marked; the file-content case runs on all legs).
**NOT RUN:** only the argument-bytes sub-case may be NOT RUN on Windows
(recorded with reason).

### N-17 Existing CLI compatibility
**Input:** `local-mcp start --help`, `local-mcp mcp --help`, `local-mcp
doctor`, `local-mcp doctor --json`, `local-mcp doctor --help`, `local-mcp
--version`, `local-mcp --help` after the change.
**Expected:** byte-observable behavior identical to v0.1.2 for these paths
(modulo the additive `setup` noun in global help); exit codes unchanged;
all existing tests pass unmodified.
**Evidence:** existing test suite green; new global-help snapshot updated to
include `setup` only.
**NOT RUN:** never.

## 4. Cross-cutting test requirements

- **Golden snapshots** for every human-rendered failure state (N-1…N-16) and
  every Doctor status outcome.
- **Parsed-value semantic equality contract test** (Finding C, replaces the
  old "byte-equal modulo whitespace" wording): the `doctor` key of the
  preview JSON, parsed, must deep-equal the fixture payload parsed. A test
  that canonicalizes both with a stable key order and compares is used only
  as a debugging aid; equality is defined on the parsed values.
- **No-network audit:** tests run with network disabled / unroutable proxy
  and must pass identically.
- **NOT RUN ledger:** any leg that cannot run is listed in CI output as NOT
  RUN with reason; CI may not convert it to PASS.
- **Artifact-state exclusivity:** `MANIFEST_INVALID`, `PIN_MISMATCH`,
  `PINNED_UNVERIFIED`, `VERIFIED_ARCHIVE`, `ARCHIVE_MISMATCH`,
  `ARCHIVE_UNREADABLE` are asserted mutually exclusive in one table-driven
  test.

## 5. Release-blocking criteria for the implementation slices

- N-1, N-2, N-3, N-6, N-7, N-8, N-11, N-12, N-14 must be green on all three OS
  CI legs before the Slice-5 PR merges.
- N-4, N-5, N-9, N-10, N-13, N-15, N-16 must be green or explicitly recorded
  NOT RUN with reasons.
- No test may be weakened, skipped, or marked ignore to achieve green; any
  such change invalidates the slice's readiness claim.
