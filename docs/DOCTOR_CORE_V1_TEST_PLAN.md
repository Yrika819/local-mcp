# GoalLatch Doctor Core V1 — Contract Test Plan

Status: **implemented**. `local-mcp doctor` exists and this plan is the contract
test matrix for it. Cases marked *implemented* below are backed by a test in
`src/doctor.rs` or `tests/doctor_cli.rs`; cases marked *open* are contract
requirements that the current build does not exercise and that must not be
described as passing. The JSON files in `examples/` are synthetic serialization
fixtures, not Doctor output captured from a running binary.

Normative contracts: [`DOCTOR_CORE_V1_DESIGN.md`](DOCTOR_CORE_V1_DESIGN.md) and
[`schemas/doctor-v1.schema.json`](schemas/doctor-v1.schema.json).

### Implementation status of the required cases

| # | Case | Status | Evidence |
| ---: | --- | --- | --- |
| 1 | Windows Experimental | implemented | `windows_is_experimental_and_sandbox_parity_is_not_applicable` |
| 2 | Linux Bubblewrap missing | implemented | `linux_bubblewrap_missing_is_a_required_sandbox_block` |
| 3 | Bubblewrap below 0.12.0 | implemented | `bubblewrap_old_version_uses_the_shared_minimum_gate` |
| 4 | Bubblewrap 0.12.0+ | implemented | `unprobed_bubblewrap_is_unknown_even_when_the_binary_is_present`; the version gate passes but runtime prerequisites stay UNKNOWN |
| 5 | Required helper missing | implemented | `missing_required_helper_blocks_only_its_capability_and_path_never_falls_back` |
| 6 | Helper in wrong directory | implemented | same test; candidate count stays exactly one |
| 7 | PATH-only helper | implemented | `path_codex_resolution_is_distinct_from_helper_resolution` |
| 8 | Codex CLI missing | implemented | `non_windows_missing_codex_blocks_only_the_optional_goal_model_capability`, `windows_default_codex_resolution_is_unknown_and_does_not_block_required_capabilities` |
| 9 | Codex override precedence | implemented | `codex_resolver_precedence_is_exact_and_override_invalidity_does_not_fall_through` |
| 10 | Malformed Codex path | implemented | `malformed_codex_path_is_blocked_without_exposing_the_path`, `nul_in_codex_override_is_malformed_and_does_not_fall_through` |
| 11 | Version probe timeout | **open** | No probe is executed in V1, so there is no probe timeout path to test. `CODEX_CLI_VERSION_TIMEOUT` exists in the contract but is unreachable until a reviewed probe is added. |
| 12 | Unsupported architecture | partial | Host facts are injected and an unsupported pair maps to UNKNOWN, but there is no dedicated per-pair test. Do not claim it as covered. |
| 13 | Symlink origin UNKNOWN | implemented | `doctor_json_is_pure_redacted_bounded_and_does_not_mutate_state` plus the symlink-origin reason code |
| 14 | Nix not verified | implemented | `installation.nix_validation` is UNKNOWN with `INSTALL_NIX_UNVERIFIED` |
| 15 | Multiple WARN aggregation | implemented | `status_truth_table_covers_every_three_check_combination` |
| 16 | BLOCK and UNKNOWN aggregation | implemented | `required_unknown_and_block_precedence_are_deterministic` |
| 17 | JSON Schema invalid fields | implemented in CI | `scripts/doctor/schema-validator` (Draft 2020-12, 8 negative controls) |
| 18 | stdout contamination | implemented | `report_json_is_single_bounded_object_with_trailing_newline`, `doctor_json_is_pure_redacted_bounded_and_does_not_mutate_state` |
| 19 | No Session/Goal creation | implemented | `doctor_cli.rs` state snapshot before/after each command |
| 20 | No filesystem mutations | implemented for the static path | `doctor_cli.rs` snapshot includes files, sizes, modes and symlinks |
| 21 | Helper symlink/reparse | implemented | `runtime_and_doctor_both_reject_helper_symlink_entries`; a host that refuses the fixture records NOT RUN, not PASS |

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
| 8 | Codex CLI missing | No explicit override, no macOS bundled binary, no `codex` PATH candidate. | On Linux/macOS, `goal_model_execution=BLOCK`, `CODEX_CLI_NOT_FOUND`; MCP/session/atomic capabilities unchanged; optional-only BLOCK exit 0 if required baseline passes. On Windows, default `Command::new` searches additional OS directories, so incomplete static resolution is UNKNOWN, not a guessed missing result. |
| 9 | Codex CLI override precedence | Provide all candidates in injected resolver data. Subcases: local override, generic override, macOS bundle, PATH. | Selected source is exactly the first present source in order: `LOCAL_MCP_CODEX_CLI_PATH`, `CODEX_CLI_PATH`, macOS bundle, PATH. No selected path value is serialized. |
| 10 | Malformed Codex CLI path | Set the highest-precedence override to empty, directory, non-regular, or otherwise invalid; populate lower-priority candidates too. Separate subcase uses a relative override while Doctor cwd differs from the GoalLatch model launcher cwd. | Invalid override remains selected; no fallthrough. `CODEX_CLI_PATH_INVALID` or statically proven non-executable BLOCKs `goal_model_execution` only; relative path is UNKNOWN with `CODEX_CLI_PATH_CONTEXT_UNKNOWN` because the standalone Doctor cannot establish the launcher process cwd (the Session cwd is passed separately as Codex `-C`); optional-only BLOCK exit 0. Permission/metadata ambiguity is UNKNOWN, not an invented absence. |
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
| 21 | Helper symlink/reparse behavior | Feed the same test-owned sibling candidates to Runtime `first_existing` and Doctor inspection for an ordinary sibling, missing file, same-directory and external regular-file symlinks/reparse points, broken link, and directory-target link. Never execute a helper. | Runtime and Doctor both accept only the ordinary regular sibling; missing/linked/non-regular entries are refused by Runtime and BLOCKed by Doctor. Runtime's check concerns only the final entry; it does not certify ancestors, install-directory trust, or spawn-time identity. Windows checks must confirm a reparse-point entry is detected without following it; fixture creation denied by the host is recorded as NOT RUN, not PASS. |

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
- For every fixture and producer report, capability `status`, `formal_status`,
  `runtime_status`, requirement, and reason codes are recomputed from applicable
  checks; schema-valid semantic mismatches fail.
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
  selected explicit override proves invalid. Windows suffix and search-path
  tests use Rust 1.96 behavior; incomplete default OS search remains UNKNOWN.
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

Fixtures are schema/contract artifacts, not observations. CI validates them with
the Draft 2020-12 validator in `scripts/doctor/schema-validator`, and separately
validates the JSON that `local-mcp doctor --json` actually prints on the native
runner. A fixture's `PASS` is not an E2E PASS, and the `pass` fixture's executed
version probe (`attempted: true`) is a shape the current producer never emits.

The three evidence classes are deliberately distinct:

- **Schema-valid synthetic fixtures** — exercise the schema surface, including
  shapes the producer does not emit.
- **Current producer-compatible JSON** — what this build prints; additionally
  checked for the producer profile (null/boolean observations, redaction flags,
  64 KiB bound, exit code agreement).
- **Runtime-observed evidence** — a real probe or launch. Doctor V1 produces
  none, so it reports UNKNOWN rather than PASS.

The schema is permissive about `observed` on purpose (forward compatibility for
additive fields). Schema validity therefore does **not** prove that strings are
redacted or that an observation is truthful; that is enforced by the typed
producer and checked by the producer-profile assertions above.

## 5. Verification status

Implemented in CI:

1. Status/exit pure tests and schema-shaped serialization tests.
2. Injected resolver/probe runner tests; no test reaches real Codex, shell,
   network, or user files.
3. CLI stream/exit/help tests and negative Session/Goal/filesystem tests.
4. Draft 2020-12 schema validation of the schema document, the four fixtures,
   corrupted fixtures (negative control) and the real producer output, using a
   pinned validator crate with its own lock file. The runtime package gains no
   dependency and no Python tooling.
5. Platform-native CI. Compile success is never equated with sandbox parity.

Still open, and therefore not claimed:

- Case 11 (probe timeout): unreachable while no probe runs.
- Case 12 (unsupported architecture): no dedicated per-pair test.
- Any external probe: the safe external-probe gate stays disabled until a
  separate review proves and tests a no-write/no-network boundary.
