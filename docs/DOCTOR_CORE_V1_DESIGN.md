# GoalLatch Doctor Core V1 — Design & Contract

Status: **implemented** as `local-mcp doctor` (Doctor Core V1). This document is
the design and contract for a session-independent, read-only diagnostic surface.
The JSON contract is defined by
[`schemas/doctor-v1.schema.json`](schemas/doctor-v1.schema.json); the contract
test matrix, including which cases are implemented and which remain open, is in
[`DOCTOR_CORE_V1_TEST_PLAN.md`](DOCTOR_CORE_V1_TEST_PLAN.md).

Two artifacts are easy to confuse:

- `local-mcp doctor --json` prints one report produced by this code on this host.
- The JSON files in [`examples/`](examples/) are **synthetic schema fixtures**.
  They exercise the schema surface, including evidence shapes this build never
  emits (for example an executed Bubblewrap version probe, which is always
  `attempted: false` here). They are not captured output, not observations, and
  not proof of runtime behaviour.

## 0. Implementation status

| Item | Status |
| --- | --- |
| `doctor`, `doctor --json`, `doctor --help`, `help doctor` | implemented |
| Exit codes `0` (PASS/WARN), `1` (required BLOCK), `2` (required UNKNOWN) | implemented |
| Exit code `64` (Doctor usage error), `70` (internal serialization failure) | implemented |
| Read-only static and formal checks | implemented |
| Version probes (`--probe-codex-version`, `--probe-bubblewrap-version`) | deliberate refusal: no process is started, the probe reports `attempted: false` and UNKNOWN |
| JSON Schema CI conformance gate | implemented in CI (`scripts/doctor/schema-validator`) |
| Runtime execution evidence, ACL/ancestor enforcement, image identity | not implemented by design; see the runtime audit |

Because no probe runs, `dependencies.bubblewrap.minimum_version` and
`dependencies.codex_cli.version_probe` are UNKNOWN on Linux and macOS even on a
healthy host. When the sandbox capability is required, that UNKNOWN is a
**required** UNKNOWN, so `doctor` exits `2` on those hosts. This is deliberate:
an unrun probe is not evidence, and treating it as PASS would advertise a
capability nobody observed. Use the exit code as "not established", not as
"broken", and do not add an opt-in that turns an unrun probe into PASS.

Audited base: `6f39cc871a760bdd6183353681246f53a4eac9a5` (main; PR #18 and #19
integrated). The Doctor design must reuse the current runtime contracts, not
reimplement a second version of them.

## 1. Scope and compatibility boundary

Doctor Core V1 answers: *which declared GoalLatch capabilities are available,
blocked, warned, or not established on this host, and what bounded evidence
supports that answer?* It is not an installer, repair tool, sandbox test
harness, session manager, Goal runner, authentication checker, or model client.

Existing CLI behavior is frozen:

- `local-mcp` and `local-mcp start [SESSION_ID]` continue to start a Session and
  show the existing permission UI.
- `local-mcp mcp` continues to run the stdio MCP server.
- Neither `start` nor `mcp` changes its arguments, output, defaults, or runtime
  behavior as part of Doctor.
- `local-mcp doctor` is an explicit subcommand. It requires no Session ID,
  current project, MCP client, or Goal. It creates no Session.
- `local-mcp doctor --help` prints help and exits before collecting diagnostics.

The CLI must parse/dispatch Doctor without calling `approvals::start`,
`mcp::serve`, Session load/save, Goal APIs, or model invocation. The existing
no-subcommand default remains `Start`; adding Doctor must not turn it into a
new default. `doctor` should use the existing `clap` dependency and the existing
`Cargo.toml` package version. Do not add a dependency solely for this design.

## 2. CLI and stream contract

V1 invocations:

```text
local-mcp doctor
local-mcp doctor --json
local-mcp doctor --help
local-mcp doctor --probe-bubblewrap-version
local-mcp doctor --json --probe-bubblewrap-version
local-mcp doctor --probe-codex-version
local-mcp doctor --json --probe-codex-version
```

The first three are the required public entry points. The two probe switches
are explicit opt-ins, not defaults. Unknown options and incompatible/duplicate
options are usage errors. `--help` must not run a probe even if it appears with
one. V1 has no `--fix`, `--repair`, `--session`, `--goal`, `--output-file`, or
arbitrary command/argument option.

### Human output

- UTF-8, plain text, no ANSI escape sequences, no startup banner, and no raw
  environment values or absolute paths.
- Show platform/target and experimental classification; each capability's
  requirement and status; concise reason codes; whether evidence is formal,
  static, runtime, or not run; then summary and exit code.
- A version string may be shown only after strict parsing and must be capped at
  64 printable characters. Never print raw child stdout/stderr.
- Maximum human stdout: 32 KiB. Do not silently truncate a report; fail with an
  internal error on a bound violation.

### JSON output

- `--json` writes exactly one UTF-8 JSON object followed by one newline to
  stdout. No log, banner, warning, progress message, or other text may appear
  on stdout. Successful diagnostic results with `BLOCK`, `WARN`, or `UNKNOWN`
  are still complete JSON documents.
- Serialize to a bounded in-memory buffer first, enforce 64 KiB maximum, then
  write once. Never emit a partial/truncated JSON document. If serialization or
  the cap fails, stdout stays empty and a short sanitized error goes to stderr
  with exit code 70.
- On a successful diagnostic result stderr is empty. Clap usage/help errors
  and internal errors go to stderr. Help text goes to stdout and exits 0.
- JSON consumers must ignore unknown object properties and unknown check IDs.
  Producers keep V1 field names and meanings stable. New optional fields and
  new check IDs are additive; changing a type, enum meaning, required field,
  status rule, or existing reason-code meaning requires a new incompatible
  contract/schema major. Consumers must not reject an otherwise valid report
  just because an object contains an unknown property. Unknown enum values are
  not silently mapped to PASS: consumers preserve/report them as unknown and
  fail closed for decisions they do not understand.

### Non-interactive execution and bounds

Doctor never prompts, reads stdin, waits for a terminal, starts a permission
listener, or requests elevation. Standard input is not consumed. Static checks
have a 10-second overall deadline. Each opted-in child probe has a hard
per-process timeout (Bubblewrap: 5 seconds, matching the current runtime gate;
Codex: 2 seconds), 4 KiB stdout and 4 KiB stderr capture caps, null stdin, and a
500 ms termination/cleanup grace. There are at most two probes and no retries
except a separately reviewed, shared runtime policy. A timeout kills/reaps the
whole child process group and reports `UNKNOWN`; it does not continue the probe.

Report limits: 64 KiB JSON, 32 KiB human output, at most 64 checks, at most 512
characters per message, at most 16 reason codes per capability, at most 4 KiB
retained from either child stream, and at most 64 characters of parsed version
text. Any cap breach is an explicit bounded error, never partial success.

## 3. Versioned JSON contract

The authoritative schema is JSON Schema Draft 2020-12 at
[`schemas/doctor-v1.schema.json`](schemas/doctor-v1.schema.json).

- `schema_version`: semantic schema revision, initially `"1.0.0"`.
- `diagnostic_contract_version`: integer `1`; stable public status/exit/output
  semantics. This is distinct from the install contract's version.
- `doctor_version`: Doctor implementation version, independently versioned.
- `goallatch_version`: package version from the build (`Cargo.toml`, currently
  `0.1.1`), not a value inferred from a CLI path or environment override.
- Every check has stable `id`, `status`, `requirement`, `evidence_type`,
  `observed`, `reason_code`, `message`, and `capability` fields.
- `observed` must contain only bounded, non-secret facts. Paths and environment
  values are never serialized. Unknown/not-run is explicit (`status: UNKNOWN`,
  `evidence_type: not_run`, `observed: null` or a bounded observation, and a
  stable reason code); it is never represented as an empty PASS.
- OS-inapplicable checks use `requirement: not_applicable`,
  `applicability: not_applicable`, `status: UNKNOWN`, and
  `CHECK_NOT_APPLICABLE`. This is not a success state and is excluded from
  applicable-capability aggregation.
- The schema intentionally permits unknown properties for forward compatibility
  but constrains all V1 fields, status enums, IDs, counts, and output shapes.
  The producer additionally enforces the total serialized-byte cap, which JSON
  Schema cannot express.

Capability fields distinguish `formal_status` from `runtime_status`. Formal
status reports only checks whose evidence is a static observation or a checked-in
contract. Runtime status reports only actual bounded runtime evidence. The
aggregate `status` is not permission to claim runtime success: if a capability
requires runtime evidence and that evidence is skipped/unavailable, its status
is UNKNOWN. A formal PASS must not overwrite a runtime UNKNOWN. Capability
requirements mean:

- `required`: part of the declared host baseline; a BLOCK or UNKNOWN affects
  the diagnostic exit code.
- `optional`: independently useful feature/dependency; its BLOCK/UNKNOWN remains
  visible in the report but cannot by itself fail the host's required baseline.
- `not_applicable`: the feature has no meaning on this target; it is excluded
  from aggregation and is never PASS.

V1 capability requirement defaults:

| Capability | Linux x86_64 | Linux aarch64 | macOS x86_64 | macOS aarch64 | Windows |
| --- | --- | --- | --- | --- | --- |
| `mcp_transport` | required | required | required | required | required |
| `session_management` | required | required | required | required | required |
| `atomic_publication` | required | required | required | required | required |
| `sandboxed_execution` | required | required, experimental closure | required | required, local closure unknown | not_applicable (no Unix-equivalent sandbox) |
| `goal_model_execution` | optional | optional | optional | optional | optional |

The implementation must emit an explicit check for every required baseline
capability. A capability with no applicable evidence is UNKNOWN, not PASS.

## 4. Status model and aggregation

### Definitions

- **PASS** — every applicable required check for the stated capability/claim
  has positive, authoritative evidence. A PASS is scoped to those checks only;
  it does not mean the whole product, another OS, an unchecked runtime path, or
  an external service works.
- **WARN** — a known limitation or degraded-but-supported condition is present;
  no applicable required check is BLOCK or UNKNOWN. Examples: Windows is
  Experimental, Linux aarch64 sandbox closure is not validated, or macOS ARM64
  is a CI target without local release-closure evidence.
- **BLOCK** — authoritative evidence proves a required condition is unsatisfied
  or the relevant operation must refuse. Examples: a missing required sibling
  helper, or Bubblewrap below 0.12.0.
- **UNKNOWN** — evidence is absent, ambiguous, skipped, timed out, malformed,
  unreadable, or not safely obtainable. UNKNOWN never becomes PASS by inference.

`UNKNOWN` is an evaluated state, not a missing field. `not_run` is always
UNKNOWN. A known platform limitation can be WARN while an individual untested
runtime check remains UNKNOWN; the latter is retained in `runtime_status` and
check records.

### Truth table

Aggregation applies to applicable checks in a scope. `not_applicable` checks are
excluded; optional capabilities are evaluated separately from required baseline
capabilities.

| Any applicable BLOCK? | Else any applicable UNKNOWN? | Else any applicable WARN? | Aggregate |
| --- | --- | --- | --- |
| yes | any | any | BLOCK |
| no | yes | any | UNKNOWN |
| no | no | yes | WARN |
| no | no | no | PASS |
| no applicable checks | — | — | UNKNOWN |

Concretely:

- Required `BLOCK` + required `UNKNOWN` => required aggregate `BLOCK`.
- Required `UNKNOWN` + required `WARN`, no BLOCK => `UNKNOWN`.
- Required PASS + required WARN => `WARN`.
- Optional-only BLOCK/UNKNOWN never changes required aggregate; it remains
  visible as that capability's state and in `advisory_status`/counts.
- Windows sandbox parity is `not_applicable`/UNKNOWN, not PASS. The separate
  Windows Experimental check is WARN.
- Codex CLI missing is BLOCK for `goal_model_execution` only; it does not change
  `mcp_transport`, `session_management`, or their statuses.
- A required helper missing is BLOCK for the capability that requires it.
- Linux Bubblewrap below `0.12.0` is BLOCK for `sandboxed_execution` and must
  never trigger unsandboxed fallback.

`summary.status` and `summary.required_status` are the aggregate of required
baseline checks/capabilities and are equal; they drive the diagnostic exit code.
`advisory_status` aggregates optional capabilities. `runtime_status` aggregates
runtime-evidence checks separately. `counts` counts every emitted check,
including optional and not-applicable checks; `required_blockers` and
`required_unknowns` count only applicable required checks. The per-check and
per-capability records remain authoritative; a summary must not erase a more
severe state or reclassify UNKNOWN.

## 5. Exit-code contract

Doctor result exit codes are frozen as follows:

| Code | Meaning |
| ---: | --- |
| 0 | Required baseline has no BLOCK or UNKNOWN. WARN is allowed. Optional-only BLOCK/UNKNOWN is allowed and remains in JSON. |
| 1 | At least one required baseline check/capability is BLOCK. BLOCK takes precedence over UNKNOWN. |
| 2 | No required BLOCK, but at least one required baseline check/capability is UNKNOWN or no required evidence is available. |
| 64 | CLI usage/argument error. No diagnostic report is emitted. |
| 70 | Doctor internal/serialization/bounds error. No partial report is emitted. |

The exit value is independent of presentation mode; `--json` does not change it.
`--help` exits 0. If only optional `goal_model_execution` is BLOCK because Codex
is missing, exit 0 when all required capabilities pass; consumers inspect the
capability status. If required status is WARN and optional status UNKNOWN, exit
0. If required status is UNKNOWN and optional status BLOCK, exit 2. If required
status is BLOCK with any other state, exit 1.

Clap's default parser error code is not the Doctor public contract. The Rust
implementation must map Doctor-only usage errors to 64 without changing legacy
`start`/`mcp` parsing behavior. Existing command output/error behavior is a
regression boundary.

## 6. Diagnostic inventory

### Platform

Report process OS, process architecture, corroborated host target, product
support classification, and experimental status. Do not trust an environment
variable as host evidence. `host_target` describes the running process target,
not a guessed physical CPU or a foreign binary. If OS/architecture cannot be
corroborated or is outside the six install-contract targets, use UNKNOWN; never
map an unknown target to the nearest supported target.

| Host target | Support classification | Required note |
| --- | --- | --- |
| Linux x86_64 | supported | Sandbox requires system Bubblewrap 0.12.0+, namespaces/seccomp and compatible kernel. |
| Linux aarch64 | experimental | Sandbox/release closure is not validated on ARM64; do not claim parity from packaging/CI alone. |
| macOS x86_64 | supported | Ordinary command sandbox uses existing Seatbelt dependency. |
| macOS aarch64 | CI target | CI target, but local Apple Silicon release closure is not established. |
| Windows x86_64 / aarch64 | experimental | No Unix-equivalent process sandbox. Named-pipe DACL is IPC isolation only. |
| Other/unknown | unsupported/unknown | Do not infer support; affected capabilities are UNKNOWN, not PASS. |

### Installation and helpers

Reuse `src/install_contract.rs` and `std::env::current_exe()` as the authority
for helper candidate computation. Keep the production sibling-only rule:

```text
candidate = current_exe().parent() / helper_name
```

No search of PATH, cwd, configuration, parent directories, symlink target
heuristics, or in-process replacement is permitted. The only existing extra
candidate in `execution.rs`/`sandbox.rs` is `#[cfg(test)]` and must not leak into
Doctor or production behavior.

Report that `current_exe()` and its parent directory were observed, plus whether
the executable basename matches the platform's fixed `local-mcp` name. Mark
`path_redacted: true`; never emit the executable path, directory basename, or
any user-supplied path component (even a basename can reveal a username or
project). Helper checks include required/forbidden name, sibling candidate
result, regular-file status,
readability, and executable status where the host can authoritatively inspect
it. If an OS ACL or a race prevents a reliable answer, use UNKNOWN. Do not run a
helper merely to establish executable status.

Install Contract V1 baseline (from `docs/INSTALL_CONTRACT_V1.md`):

| OS | Required helpers | Forbidden helpers |
| --- | --- | --- |
| Linux | `atomic-publish`, `codex-linux-sandbox` | none |
| macOS | `atomic-publish` | `codex-linux-sandbox` |
| Windows | `atomic-publish.exe` | `codex-linux-sandbox`, `codex-linux-sandbox.exe` |

Missing/unreadable/non-executable required helpers BLOCK their relevant
capability. A forbidden helper present is BLOCK for the installation-contract
check and is not silently ignored. A helper available only on PATH or in a
subdirectory is still missing. This agrees with the release checker and
runtime; do not add PATH fallback.

A symlink executable's behavior is OS-specific. The Doctor reports exactly the
path returned by `current_exe()` in redacted form and anchors the one helper
candidate to it. It does not create a symlink or relaunch itself. If
current-executable target-vs-link origin cannot be proven without such a
relaunch, the symlink-origin check is UNKNOWN
(`INSTALL_SYMLINK_ORIGIN_UNKNOWN`). Required helper regular-file status is about
the sibling directory entry itself: symlink/reparse-point entries are not
reported as regular helpers and BLOCK the install-contract check. This is
intentionally conservative and it agrees with the runtime: since Security V2 the
runtime rejects a final helper symlink or reparse entry through the shared
`install_contract::is_plain_file` predicate, and Doctor calls that same predicate
(`src/doctor.rs` `inspect_helper_entry`). Both sides refuse a linked entry. What
neither side does is bind the inspected directory entry to the image the OS opens
later, so Doctor does not claim runtime image identity, ancestor containment, or
ACL enforcement. External helper-link targets and replacement races remain a
Runtime provenance concern and are not changed by Doctor. A passing direct
`--version` invocation is not proof of helper resolution. Nix's expected
`$out/bin` layout/wrapper behavior is documented but `nix build`/`nix run` remain
unverified; report `INSTALL_NIX_UNVERIFIED` as UNKNOWN, never PASS.

`install_contract_version` is the checked-in contract version (`1`), not proof
that an archive signature, checksum, release origin, or formal archive member
set has been verified by this local runtime command. Doctor Core V1 does not
inspect/download an archive.

### Codex CLI

Reuse the exact current resolver in `src/fallback.rs::codex_path()` and preserve
this precedence:

1. `LOCAL_MCP_CODEX_CLI_PATH`
2. `CODEX_CLI_PATH`
3. On macOS, `/Applications/ChatGPT.app/Contents/Resources/codex` if it is a file
4. `codex` resolved by PATH

A present higher-priority override is authoritative. If it is empty, malformed,
not a regular file, unreadable, or non-executable, report that selected source
and the corresponding BLOCK/UNKNOWN; do **not** fall through to a lower-priority
source. A relative explicit override is context-dependent. The Goal model launcher passes
no child `current_dir`; Rust resolves a path with components from the GoalLatch
process's inherited working directory. The Session cwd is passed separately to
Codex as `-C` and does not choose the executable path. A standalone Doctor cannot
establish the MCP server process's working directory, so report UNKNOWN with
`CODEX_CLI_PATH_CONTEXT_UNKNOWN`; do not resolve against Doctor's cwd or fall
through. The current code returns an explicit override as-is and does not fall
through when it is invalid. Do not display the value, resolved absolute path,
PATH entries, or ChatGPT app path in a user-specific report. `PATH`-only Codex
resolution is distinct from helper resolution; only Codex CLI uses PATH.

On Windows the default bare `Command::new("codex")` resolution is wider than
PATH: with the current Rust 1.96 toolchain it checks the executable directory,
System directory, Windows directory, then the parent PATH, and appends `.exe`
when the bare name has no extension. Doctor does not reproduce all those OS
search locations, so it reports the default Windows PATH resolution as UNKNOWN
with `RUNTIME_VERIFICATION_NOT_RUN`; a PATH-only candidate or absence is not
proof of the executable selected by Runtime. This does not launch Codex. The
pure PATH resolver accepts only the suffix Rust adds (`.exe`) for a bare
Windows name and skips empty Windows PATH entries.

Default Doctor mode performs only static metadata checks. It does not invoke
Codex, check authentication, inspect login state, or make a model/API request.
A statically established missing or unusable Codex CLI BLOCKs
`goal_model_execution` only; it does not block MCP transport, Session
management, or atomic publication. On Windows, incomplete default `Command`
search knowledge remains UNKNOWN rather than being reported as missing.

`--probe-codex-version` is explicit user consent to request a bounded version
probe, not proof that arbitrary `codex --version` is harmless. The only argv is
`--version`; stdin is null; timeout/output/process-group bounds above apply; the
output parser accepts one bounded version token and discards all raw output.
No login/auth command, model name, prompt, project content, or API request is
permitted. The process may run only inside an already available, host-enforced,
read-only, network-denied execution boundary that has **no writable project,
home, config, or user-data roots**. The Doctor must not create a temporary
sandbox, probe namespace capability, or fall back to direct/host-native
execution to satisfy this condition. If that boundary cannot be proved from an
existing trusted runtime path, do not spawn Codex; return UNKNOWN with
`CODEX_CLI_VERSION_PROBE_REFUSED`. On Windows V1 has no Unix-equivalent
containment, so the version probe is refused and remains UNKNOWN. Permission
failure is BLOCK only when static inspection proves the selected binary is not
executable; a child `EACCES`, timeout, nonzero status, or malformed output is
UNKNOWN. A valid parsed version is a version observation, not proof of
authentication or model execution.

The design intentionally makes the opt-in probe fail closed where safe
containment is absent. PR-I02B must not weaken this invariant to make the version
field look complete.

### Sandbox

**Linux**

- Static detection locates `bwrap` on PATH exactly as current
  `src/bubblewrap_support.rs::resolve_bubblewrap()` does; this PATH search is
  only for the system Bubblewrap runtime, never for the sibling helper.
- Bubblewrap version comparison reuses
  `src/bubblewrap_support.rs::require_supported_version()` and its strict
  numeric `major.minor.patch` rule. Minimum is exactly `0.12.0` (the current
  security gate for CVE-2026-87766).
- If `--probe-bubblewrap-version` is absent, version is UNKNOWN; do not infer
  `0.12.0+` from file presence. With the flag, invoke only `bwrap --version`
  under the explicit-probe safety gate, matching the runtime's 5-second/4-KiB
  limits. Malformed output, nonzero exit, timeout, or unsafe launch context is
  UNKNOWN. A known version below `0.12.0` is BLOCK.
- Presence/version is not namespace, user-namespace, seccomp, Landlock, network,
  or kernel-capability evidence. Doctor V1 performs no sandbox smoke test,
  namespace creation, `unshare`, capability escalation, test command, network
  probe, or temporary-file probe. Unobserved runtime prerequisites remain
  UNKNOWN; never report `sandboxed_execution` PASS from a version string alone.
- A missing `codex-linux-sandbox` sibling is independently BLOCK for
  `sandboxed_execution`, even if a system `bwrap` is on PATH.

**macOS**

- Ordinary command sandboxing retains the existing
  `codex_sandboxing::seatbelt::MACOS_PATH_TO_SEATBELT_EXECUTABLE` dependency
  (`sandbox-exec`). Static file/executable inspection is distinct from a
  successful Seatbelt runtime test.
- Doctor does not launch `sandbox-exec` with a probe policy or test command.
  Static absence is BLOCK for `sandboxed_execution`; actual sandbox behavior
  not safely established by static evidence stays UNKNOWN.
- Do not require/ship the Linux `codex-linux-sandbox` helper on macOS.

**Windows**

- Always report Experimental. Current code executes argv directly with a
  restricted environment; that is not a filesystem/network sandbox.
- `sandboxed_execution` for Unix-equivalent parity is `not_applicable` with
  UNKNOWN status and an explicit reason; never PASS. Named-pipe current-user
  DACL is not process containment.
- No active sandbox/version probe is used to imply parity. Session and
  host-native paths remain governed by existing approval behavior; Doctor does
  not start a Session or request approval.

## 7. Security invariants

Doctor has no authority to create, change, or repair state. It must not:

- create/load a Session, Goal, Task, approval listener, MCP server, or job;
- invoke Codex model execution, authentication/login, API requests, or prompts;
- install, repair, publish, change PATH, change environment variables, or edit
  configuration;
- write/delete user/project files, create temporary files/symlinks, alter
  permissions, or run filesystem-mutating probes;
- bypass sandboxing, retry sandbox refusal host-native, elevate privileges, or
  expand any Session/project root;
- expose raw PATH, Codex override values, absolute paths, environment variable
  values, child stdout/stderr, tokens, credentials, or project contents.

All path-like observations are redacted by construction, not post-hoc log
filtering. Messages use stable reason codes and generic remediation. Probe
children receive fixed argv, null stdin, bounded capture, bounded lifetime, no
network, and no writable user/project data roots. A child can still be an
arbitrary executable; therefore explicit consent plus a verified read-only
containment boundary is mandatory. If either is absent, the probe is not run.

The Doctor JSON report itself is diagnostic data and may identify OS, version,
helper presence, and capability states. It must not include host/user names,
repository names, cwd, absolute paths, PATH contents, environment values, or
raw command output. Do not include the report in telemetry; no telemetry is
added by this design.

## 8. Machine-readable reason-code registry

Reason codes are stable uppercase identifiers; messages may improve without
changing a code's meaning. Unknown future codes are preserved, not treated as
success. Initial codes:

| Code | Meaning |
| --- | --- |
| `CAPABILITY_AVAILABLE` | Applicable required contract checks passed. |
| `CHECK_NOT_APPLICABLE` | Check does not apply to this OS/target. |
| `PROBE_NOT_RUN` | Optional process/runtime probe was not requested. |
| `RUNTIME_VERIFICATION_NOT_RUN` | Runtime behavior was intentionally not exercised. |
| `DOCTOR_STATIC_CHECK_TIMEOUT` | Static diagnostics exceeded the overall deadline; affected observations remain UNKNOWN. |
| `HOST_TARGET_UNKNOWN` | OS/architecture evidence was insufficient. |
| `HOST_TARGET_UNSUPPORTED` | Target is outside the verified install matrix. |
| `PLATFORM_WINDOWS_EXPERIMENTAL` | Windows is Experimental; no Unix sandbox parity. |
| `PLATFORM_LINUX_AARCH64_UNVERIFIED` | Linux ARM64 sandbox closure is not validated. |
| `PLATFORM_MACOS_AARCH64_CI_ONLY` | macOS ARM64 CI target; local release closure not established. |
| `INSTALL_CURRENT_EXE_UNAVAILABLE` | `current_exe()` or its parent was unavailable. |
| `INSTALL_REQUIRED_HELPER_MISSING` | Required sibling helper is absent. |
| `INSTALL_REQUIRED_HELPER_NOT_REGULAR_FILE` | The sibling helper entry is not a regular file. |
| `INSTALL_REQUIRED_HELPER_UNREADABLE` | Required helper is not readable. |
| `INSTALL_REQUIRED_HELPER_NOT_EXECUTABLE` | Required helper is not executable. |
| `INSTALL_FORBIDDEN_HELPER_PRESENT` | Platform-forbidden helper is present. |
| `INSTALL_SYMLINK_ORIGIN_UNKNOWN` | Link-vs-target behavior was not safely established. |
| `INSTALL_NIX_UNVERIFIED` | Nix build/run/install layout has not been validated. |
| `CODEX_CLI_NOT_FOUND` | No selected Codex CLI file was found. |
| `CODEX_CLI_PATH_INVALID` | Selected higher-precedence override is invalid. |
| `CODEX_CLI_PATH_CONTEXT_UNKNOWN` | Relative override cannot be resolved without the Goal Session cwd. |
| `CODEX_CLI_NOT_EXECUTABLE` | Selected CLI is statically known not executable. |
| `CODEX_CLI_VERSION_PROBE_REFUSED` | Probe could not be safely contained; no process was run. |
| `CODEX_CLI_VERSION_TIMEOUT` | Opted-in version process timed out. |
| `CODEX_CLI_VERSION_MALFORMED` | Opted-in output did not contain a recognized version. |
| `CODEX_CLI_VERSION_NONZERO` | Opted-in version process returned nonzero. |
| `BWRAP_NOT_FOUND` | Linux system Bubblewrap was not found on PATH. |
| `BWRAP_VERSION_UNPROBED` | Bubblewrap version was not explicitly probed. |
| `BWRAP_VERSION_UNSUPPORTED` | Bubblewrap is below 0.12.0. |
| `BWRAP_VERSION_UNKNOWN` | Bubblewrap version could not be established. |
| `BWRAP_RUNTIME_PREREQUISITES_UNKNOWN` | Namespace/kernel/seccomp behavior was not tested. |
| `MACOS_SANDBOX_EXEC_NOT_FOUND` | Existing Seatbelt executable is absent/unusable. |
| `SANDBOX_PARITY_NOT_APPLICABLE` | Target does not provide Unix-equivalent sandbox parity. |

## 9. Deliberate non-claims

Doctor Core V1 does not prove archive checksum/signature or formal release
closure; execute helpers; validate Nix; validate Linux ARM64; exercise Linux
namespace/seccomp, macOS Seatbelt, or Windows sandbox parity; test Codex auth,
network, model availability, or execution; test an MCP client handshake; create a
Session/Goal; or establish that a symlink install will find helpers on another
OS. These remain UNKNOWN or explicit known limitations, not inferred PASS.

## 10. Runtime reuse and PR-I02B implementation guardrails

The implementation campaign must:

1. add a dedicated Doctor module/typed result model and CLI branch only;
2. reuse the current sibling candidate helper from `install_contract.rs`; it must not search PATH to determine whether a missing sibling helper exists elsewhere. A PATH-only helper is therefore reported as a missing sibling, without claiming that it exists only on PATH;
3. reuse the exact Codex precedence from `fallback::codex_path()` without
   exposing path values or changing it;
4. reuse `bubblewrap_support` version parsing/minimum and existing process
   cleanup/output-bound helpers if the explicit probe's safety boundary is
   independently proved;
5. preserve the current `start`/`mcp` behavior and all existing security gates;
6. add pure aggregation/schema-fixture tests before CLI wiring;
7. not implement any unsupported probe as a direct host-native fallback;
8. leave `src/mcp.rs`, Session/Goal runtime semantics, release workflow, and
   installation behavior unchanged.

This is a design-only commit. No Doctor production Rust implementation or
Doctor command E2E PASS claim is included.
