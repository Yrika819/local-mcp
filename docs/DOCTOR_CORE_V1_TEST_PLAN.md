# GoalLatch Doctor Core V1 — Contract Test Plan

Status: **test specification only**. Doctor is not implemented in this design
campaign. No command-level E2E, cross-platform runtime test, or production
behavior is claimed PASS here. The JSON files in `examples/` are synthetic
serialization fixtures used to validate the contract, not Doctor output captured
from a running binary.

Normative contracts: [`DOCTOR_CORE_V1_DESIGN.md`](DOCTOR_CORE_V1_DESIGN.md) and
[`schemas/doctor-v1.schema.json`](schemas/doctor-v1.schema.json).

## 1. Harness and evidence policy

The Rust implementation should split tests into:

1. **Pure contract tests** — target/status/exit aggregation, reason codes,
   redaction, resolver precedence using injected values, serialization bounds.
   These require no process, Session, filesystem, network, or OS sandbox changes.
2. **Isolated fake-runtime tests** — temporary fixture directories and fake
   process runners; use only test-owned roots. Never point tests at the real
   Codex CLI, real home directory, a production Session store, or a real
   project.
3. **CLI contract tests** — invoke a test-built binary with captured pipes;
   assert streams, exit codes, help, and no Session/Goal operations.
4. **Platform contract checks** — execute on native CI runners only. A Linux
   or macOS build/test is not evidence for Windows sandbox parity; packaging
   checks are not runtime sandbox tests.

A test that cannot safely perform an observation returns UNKNOWN/not-run. It
must not be skipped and then recorded as PASS. Fixtures may represent a
hypothetical authoritative observation to exercise a status branch; fixture
validation is not an E2E test and does not establish that Doctor can produce
that observation.

## 2. Required cases

Exit codes below use the V1 rule: 1 for required BLOCK, 2 for required UNKNOWN
without BLOCK, 0 when required baseline has neither. Optional-only BLOCK or
UNKNOWN does not change the exit code.

| # | Case | Setup / assertion | Expected result |
| ---: | --- | --- | --- |
| 1 | Windows Experimental | Windows x86_64 fixture; atomic helper and required core contracts present; no sandbox probe. | `platform.support_status=experimental`; Windows warning emitted; `sandboxed_execution.requirement=not_applicable`, status UNKNOWN, `SANDBOX_PARITY_NOT_APPLICABLE`; never claim Unix sandbox PASS; otherwise complete required baseline exits 0 (WARN is allowed). |
| 2 | Linux Bubblewrap missing | Linux x86_64 PATH fixture has no `bwrap`; sibling `codex-linux-sandbox` is independently represented. | `BWRAP_NOT_FOUND`; `sandboxed_execution=BLOCK`; MCP transport/session statuses are unchanged; required BLOCK exit 1. |
| 3 | Linux Bubblewrap below 0.12.0 | Explicit safe `--probe-bubblewrap-version`, fake output `0.11.9`; fake is launched only through a test-only runner. | Reuse strict shared comparator; `BWRAP_VERSION_UNSUPPORTED`; sandbox capability BLOCK, no sandbox/helper execution or host fallback; exit 1. |
| 4 | Linux Bubblewrap 0.12.0+ | Explicit safe version probe with `0.12.0` and `1.0.0` fixtures. Do not run a namespace/sandbox smoke test. | Version-gate check PASS, but namespace/kernel/seccomp runtime check UNKNOWN (`BWRAP_RUNTIME_PREREQUISITES_UNKNOWN`); `sandboxed_execution` is not PASS; required UNKNOWN exit 2 unless a required BLOCK also exists. |
| 5 | Required helper missing | Remove `atomic-publish` from a test-owned sibling fixture (second subcase: Linux `codex-linux-sandbox`). | Missing helper BLOCKs only its capability (`atomic_publication` or `sandboxed_execution`); no in-process helper or PATH fallback; required BLOCK exit 1. |
| 6 | Helper in wrong directory | Place required helper in a child directory or parent directory, not beside observed `current_exe`. | Same result as absent sibling; candidate count remains exactly one; `INSTALL_REQUIRED_HELPER_MISSING`; no broad search; exit 1 when required capability is blocked. |
| 7 | PATH-only helper | Place fake required helper only in a test-owned PATH directory; separately keep the executable sibling candidate absent. | Production Doctor never searches PATH for helpers. Report `INSTALL_REQUIRED_HELPER_MISSING` solely from the absent sibling; do not claim that PATH-only presence was detected; no helper process launched; exit 1. |
| 8 | Codex CLI missing | No explicit override, no macOS bundled binary, no `codex` PATH candidate. | `goal_model_execution=BLOCK`, `CODEX_CLI_NOT_FOUND`; `mcp_transport`, `session_management`, and atomic capability unchanged; optional-only BLOCK exit 0 if required baseline passes. |
| 9 | Codex CLI override precedence | Provide all candidates in injected resolver data. Subcases: local override, generic override, macOS bundle, PATH. | Selected source is exactly the first present source in order: `LOCAL_MCP_CODEX_CLI_PATH`, `CODEX_CLI_PATH`, macOS bundle, PATH. No selected path value is serialized. |
| 10 | Malformed Codex CLI path | Set the highest-precedence override to empty, directory, non-regular, or otherwise invalid; populate lower-priority candidates too. Separate subcase uses a relative override while Doctor cwd differs from the hypothetical Goal Session cwd. | Invalid override remains selected; no fallthrough. `CODEX_CLI_PATH_INVALID` or statically proven non-executable BLOCKs `goal_model_execution` only; relative path is UNKNOWN with `CODEX_CLI_PATH_CONTEXT_UNKNOWN`; optional-only BLOCK exit 0. Permission/metadata ambiguity is UNKNOWN, not an invented absence. |
| 11 | Version probe timeout | Explicit Codex version probe with a test process that stalls. | Per-process deadline, process-group termination and reap; output discarded; `CODEX_CLI_VERSION_TIMEOUT`, version null, check UNKNOWN. Optional capability UNKNOWN does not alter required exit 0. No retry or login/API/model command. |
| 12 | Unsupported architecture | Inject an OS/architecture pair outside the checked install matrix. | `host_target=null`, `support_status=unsupported`, `HOST_TARGET_UNSUPPORTED`; all affected required capabilities UNKNOWN (not PASS and not guessed to another target); exit 2 absent a required BLOCK. |
| 13 | Symlink behavior UNKNOWN | Do not create/relaunch a symlink. Inject only the platform's observed `current_exe` metadata. | Sibling resolution uses the observed executable directory only; origin check UNKNOWN with `INSTALL_SYMLINK_ORIGIN_UNKNOWN`; no symlink claim or mutation; advisory-only result does not fail required exit. |
| 14 | Nix build not verified | Nix provenance/layout cannot be confirmed; no `nix build` or `nix run`. | `INSTALL_NIX_UNVERIFIED`, status UNKNOWN, never PASS by static flake inference. If optional diagnostic only, exit unchanged. |
| 15 | Multiple WARN aggregation | Two or more required WARN checks, no required BLOCK/UNKNOWN. | Required aggregate WARN; warning count matches; exit 0. Individual reason codes remain distinct. |
| 16 | BLOCK and UNKNOWN aggregation | One required BLOCK plus one required UNKNOWN; inverse subcase: required UNKNOWN plus WARN and no BLOCK. | First subcase aggregate BLOCK, exit 1 (BLOCK precedence). Inverse aggregate UNKNOWN, exit 2. No UNKNOWN-to-PASS upgrade. |
| 17 | JSON Schema invalid fields | Validate fixtures with missing required property, invalid status enum, wrong scalar/object type, malformed reason code, overlong message, and excessive check count. | Every invalid fixture is rejected by Draft 2020-12 validator. Unknown additive object property is accepted; unknown status/reason is not silently interpreted as PASS. |
| 18 | Unexpected stdout contamination | Run JSON mode while injecting internal logging/warnings. | stdout contains exactly one valid JSON object plus trailing newline and nothing else; successful diagnostic stderr is empty. Logs/errors must be redirected to stderr or suppressed. Any prefix/suffix/banners fail the test. |
| 19 | No Session/Goal creation | Use test seams that fail if Session create/load/save, approval listener, MCP serve, Goal store, Task store, or model transport is called. | `doctor`, JSON mode, and help make zero such calls; no state directory/session/goal file is created; current `start` default and `mcp` regressions still pass. |
| 20 | No filesystem mutations | Snapshot a test-owned tree and relevant environment values before/after static Doctor; explicit external probe without verified read-only containment uses a runner that records if spawned. | No creates/writes/deletes/chmod/symlink changes; PATH/config/environment unchanged; unsafe probe is refused before spawn; output contains no project/home paths. Hash/mode/symlink snapshots are identical. |

## 3. Additional contract tests

### CLI / stream / compatibility

- `local-mcp doctor`, `doctor --json`, and `doctor --help` map to the same
  report semantics; JSON affects formatting only.
- Help exits 0 and does not call any probe even when a probe flag is also
  present.
- Unknown/invalid Doctor arguments return 64, no diagnostic JSON, concise
  stderr. Doctor-specific error mapping must not change existing Clap behavior
  for `start` or `mcp`.
- Internal serialization/size errors return 70, stdout remains empty (no
  partial JSON), stderr contains no raw path/child output.
- `start` with no subcommand remains the existing Session-start default;
  `start [SESSION_ID]` and `mcp` behavior remain unchanged.
- JSON is valid UTF-8, one object, one trailing newline, no ANSI codes, no
  duplicate keys, and stable field names. Human output is non-interactive,
  bounded, plain text, and redacted.
- Unknown additive properties are accepted by consumers. Existing enum and
  reason-code meanings cannot be redefined in a patch release.

### Status / schema / bounds

- Truth-table property test covers every status combination, no-applicable-check
  result, and required/optional/not-applicable partition.
- Required BLOCK > required UNKNOWN > required WARN > PASS. Optional-only BLOCK
  or UNKNOWN never changes the required exit; it remains in the report.
- `not_run`, timeout, malformed output, unsupported architecture, and OS N/A
  are never PASS.
- Formal and runtime status aggregation is independent. Formal PASS with runtime
  UNKNOWN remains runtime UNKNOWN; no code path copies formal status into
  runtime status.
- A report at the configured byte/check/message limits serializes whole; one
  byte/item over fails closed without partial stdout.
- If static collection exceeds its 10-second deadline, Doctor emits a complete bounded UNKNOWN report with `DOCTOR_STATIC_CHECK_TIMEOUT`; it does not emit partial output or start an external process.
- Paths, PATH values, environment values, raw external output, usernames,
  repository paths, and credentials do not appear in any fixture/report/message.

### Resolver / platform isolation

- Install helper lookup calls the shared `install_contract::sibling_candidates`
  rule (or the narrow public seam extracted from it), and never uses cwd/PATH.
- Codex resolver follows the current precedence and never falls through after a
  selected explicit override proves invalid.
- Linux version parser accepts `0.12.0` and later strict numeric triplets and
  rejects below-minimum, malformed, overflow, extra components, or unsupported
  suffixes exactly as the shared support gate does. Bubblewrap nonzero exit,
  timeout, output overflow, and malformed output stay UNKNOWN; only a parsed
  below-minimum version is BLOCK.
- Codex version fixtures cover one accepted version token, empty/malformed
  output, multiple/oversized output, nonzero exit, spawn permission failure,
  and timeout. Only the parsed version token may be retained; raw stdout/stderr
  and path/error text must not appear in JSON or human output.
- Explicit Codex/Bubblewrap probe on Windows, or on any host without a verified
  no-write/no-network execution boundary, returns UNKNOWN before process spawn.
  A recording fake runner must show zero launches; direct host-native fallback
  is a test failure.
- A selected Codex override that exists but is a directory, unreadable, or
  non-executable never falls through to the ChatGPT bundle/PATH. A relative
  override remains context UNKNOWN rather than being resolved against Doctor's
  cwd. macOS bundled resolution is considered only after both environment
  overrides are absent.
- Forbidden platform helper presence is BLOCK for installation-contract
  integrity; absent forbidden helpers are a passing negative check. An
  unreadable required helper is BLOCK only when unreadability is authoritative;
  ACL/metadata ambiguity remains UNKNOWN.
- macOS uses the current Seatbelt executable path for static evidence only; no
  sandbox-exec test command is run.
- Windows has no Unix sandbox capability PASS; named-pipe DACL is not treated as
  process sandboxing.
- Probe process uses only fixed argv, null stdin, explicit opt-in, bounded
  output/time, process-tree cleanup, and read-only/no-network/no-user-data
  containment. If that cannot be established, the process runner is not called.

## 4. Fixture mapping

| Fixture | Status exercised | Purpose |
| --- | --- | --- |
| `examples/doctor-v1-pass.json` | PASS | Required baseline passes; optional/advisory statuses remain separate. Synthetic only. |
| `examples/doctor-v1-warn.json` | WARN | Known experimental/platform limitation without required block/unknown. |
| `examples/doctor-v1-block.json` | BLOCK | Required sandbox dependency is below the minimum. |
| `examples/doctor-v1-unknown.json` | UNKNOWN | Required evidence was not safely obtained. |

Fixtures are schema/contract artifacts, not tests that execute Doctor. CI for a
future implementation must separately run pure contract tests and native CLI
checks. Doctor E2E must remain **NOT RUN / NOT IMPLEMENTED** until PR-I02B adds
the production command; a fixture's `PASS` is not an E2E PASS.

## 5. Verification plan for PR-I02B (future)

1. Add status/exit pure tests and schema-shaped serialization tests.
2. Add injected resolver/probe runner tests; ensure tests cannot reach real
   Codex, shell, network, or user files.
3. Add CLI stream/exit/help tests and negative Session/Goal/filesystem tests.
4. Run JSON Schema validation using an already-installed Draft 2020-12 tool;
   do not add a validator dependency to production. If unavailable, report that
   explicitly and still run JSON syntax plus fixture round-trip checks.
5. Run platform-native CI. Do not equate compile success with sandbox parity.
6. Keep the safe external-probe gate disabled until a separate review proves an
   existing no-write/no-network boundary and tests its negative paths.
