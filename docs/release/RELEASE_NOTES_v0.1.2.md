# GoalLatch v0.1.2

GoalLatch is an independent project. It is not affiliated with or endorsed by
OpenAI, and it is not an official release of
[upstream `nakasyou/local-mcp`](https://github.com/nakasyou/local-mcp), which it
continues under the MIT License with substantial rework of the orchestrator,
sandbox, and fallback boundaries.

## What changed since v0.1.1

v0.1.2 is a security and diagnostics release. It contains the changes that
landed on `main` after the v0.1.1 tag (`1af317a1aa201964bfe6f81d23b3b6a155ab58c6`)
up to the release commit:

- **Security V2 — final-helper symlink/reparse refusal.** The runtime helper
  selector (`src/install_contract.rs`) now rejects a helper whose final entry
  is a symlink (`symlink_metadata`, regular-file requirement) and, on Windows,
  any final entry carrying the `FILE_ATTRIBUTE_REPARSE_POINT` attribute. This
  applies to `codex-linux-sandbox` on Linux and to `atomic-publish` on every
  platform. This is a final-entry mitigation only; it does not pin the opened
  image and it does not validate ancestors. See "Residual risks" below.
- **Install Contract V1 enforcement.** A single sibling-only helper resolution
  rule is shared by the runtime, the install-layout checks, and release CI.
  Extracted archives, native launch modes, checksums, and fail-closed
  negative cases are verified. The contract and the verification procedure
  are in [Install Contract V1](https://github.com/Yrika819/local-mcp/blob/v0.1.2/docs/INSTALL_CONTRACT_V1.md).
- **Doctor Core V1.** New read-only diagnostic surface: `local-mcp doctor`
  prints a typed `PASS / WARN / BLOCK / UNKNOWN` report of the host, install
  layout, and dependency assumptions. Exit codes: `0` (PASS/WARN),
  `1` (required BLOCK), `2` (required UNKNOWN), `64` (Doctor usage error),
  `70` (internal serialization failure). No process probes are run; an unrun
  probe reports `attempted: false` and `UNKNOWN`, never PASS. See
  [Doctor Core V1](https://github.com/Yrika819/local-mcp/blob/v0.1.2/docs/DOCTOR_CORE_V1_DESIGN.md).
- **Doctor JSON Schema V1.** The report contract is defined by
  [`docs/schemas/doctor-v1.schema.json`](https://github.com/Yrika819/local-mcp/blob/v0.1.2/docs/schemas/doctor-v1.schema.json)
  (version 1.0.0), with synthetic fixtures for all four outcomes under
  `docs/examples/`.
- **Schema/Producer CI verification.** CI validates the schema document and
  the fixtures with a dedicated Draft 2020-12 validator
  (`scripts/doctor/schema-validator`), and validates the JSON emitted by
  `local-mcp doctor --json` against the same schema. Both gates are part of
  the quality lane.
- **Public OSS readiness documentation** and link fixes.

Security V2 and Doctor Core V1 are **not** part of v0.1.1; they landed on
`main` after the v0.1.1 tag and are first released here.

## CLI compatibility

No subcommands were removed or renamed. `local-mcp doctor`, `local-mcp
doctor --json`, `local-mcp doctor --help`, and `local-mcp help doctor` are
additive. All v0.1.1 subcommands and flags behave as before. `--version`
reports `0.1.2`.

## Supported platforms

| Platform | Status | Sandbox |
| --- | --- | --- |
| Linux x86_64 | Supported | Bubblewrap 0.12.0+, namespaces, seccomp |
| Linux aarch64 | **Experimental**; native build in CI, sandbox not closure-validated | Bubblewrap 0.12.0+, namespaces, seccomp |
| macOS x86_64 | Supported; native Intel release evidence | Seatbelt (`sandbox-exec`) |
| macOS Apple Silicon | Native build/test target in CI; not locally release-closure validated | Seatbelt (`sandbox-exec`) |
| Windows x86_64 | **Experimental** | None; host-native with approval |
| Windows aarch64 | **Experimental** | None; host-native with approval |

**Windows remains experimental and is not covered by the Linux or macOS
security model.** It has no process sandbox equivalent, so command and file
mutation paths run host-native and are approval-gated. Named-pipe IPC is
restricted to the current user with an explicit DACL, but that is an IPC
boundary, not a substitute for a sandbox. Do not read successful macOS or
Linux validation as Windows security validation.

## Nix

The Nix flake is updated to report `0.1.2` but is **not release-closure
validated** for this version. The published archives and `cargo build
--release --locked` are the supported installation paths; Nix users build at
their own risk and should verify what they run.

## Residual risks

- **Full Execution Identity Closure is NOT achieved.** No runtime code or OS
  guarantee binds the file-metadata check to the image that
  `Command::new(path)` opens. A check-to-open (TOCTOU) window remains for
  `atomic-publish` on every platform and, inside the Bubblewrap gate, for
  `codex-linux-sandbox` on Linux. The Linux publication path has the wider
  window.
- **Trusted-install assumption.** Helper-execution claims are supportable
  only when the executable, the helpers, and every ancestor directory are
  protected against untrusted writes/replacement for the runtime duration
  (trusted installer/package manager). A writable install — an attacker who
  can write the helper entry, its bytes, or an ancestor — is a **NO-GO**:
  final-entry symlink rejection is insufficient against in-place byte
  rewrites, ancestor replacement, and check-to-open races. This includes
  same-token malware.
- **Ancestor redirection is not prevented.** The selector checks only the
  final helper component; it does not validate canonical ancestry, mount
  identity, directory identity, or pin a directory handle.
- Trusted-updater replacement of a helper during startup can cause launch
  failure or select old/new trusted content depending on OS/open timing.
  That is a compatibility/availability race when both images are trusted.
- Doctor Core V1 is a **diagnostic provider, not an install authorization**.
  A `PASS` from `local-mcp doctor` must not be treated as permission to
  install, and neither `UNKNOWN` nor `BLOCK` is overridable from the
  diagnostic surface. Version probes are deliberately unrun, so
  `dependencies.bubblewrap.minimum_version` and
  `dependencies.codex_cli.version_probe` report `UNKNOWN`; on Linux and
  macOS, when the sandbox capability is required, that UNKNOWN is required,
  so `doctor` exits `2` even on a healthy host. Exit `2` means "not
  established", not "broken".
- Known inherited dependency advisories remain disclosed; see
  [SECURITY.md](https://github.com/Yrika819/local-mcp/blob/v0.1.2/SECURITY.md).
  This release is not a claim of zero known vulnerabilities.
- Activity and approval messages can include your command arguments, paths,
  and file diff content. Do not put secrets in commands or files being edited.
- Long-running mixed Goal/Job load and tail latency under high load are not
  fully verified.
- No code signing or notarization.

## Known dependency advisories

The locked dependency graph inherits advisories from the pinned upstream Codex
revision, including `quick-xml 0.38.4` and `hickory-proto 0.25.2`, plus
`lru 0.16.4` (unsound) and several unmaintained transitive packages. They are
not selectable away without an upstream pin change, and GoalLatch does not
exercise the affected code paths. **These remain known findings, not a
resolved set.**

## Verification

- Test suite: see the `quality` and `test` jobs on the release commit; the
  suite result for this candidate is recorded in the release checklist.
- CI: `quality`, four `test` legs (ubuntu-x64, macos-intel, macos-arm64,
  windows-x64), and six `compat` legs, all required green for the release
  candidate.
- Doctor Schema CI gates (schema/fixture validation and `doctor --json`
  producer validation) run in the quality lane.
- 6-platform release builds are exercised by the release workflow dry run;
  the dry run publishes nothing.

## Upgrade & rollback

- Upgrade from v0.1.1 or v0.1.0: replace binaries; existing data remains
  compatible. Back up state before upgrading.
- Goal/Session/Job schemas are unchanged; no migration required.
- v0.1.1 and v0.1.2 binaries can coexist in separate directories.
- Roll back to v0.1.1 if regressions appear; data remains readable by v0.1.1.

## Installation

Download the archive for your platform, verify it against `SHA256SUMS`, and
extract it. Full per-platform instructions are in the
[installation guide](https://github.com/Yrika819/local-mcp/blob/v0.1.2/docs/release/INSTALL.md).

```sh
sha256sum -c SHA256SUMS          # Linux
shasum -a 256 -c SHA256SUMS      # macOS
```

Source builds remain supported:

```sh
git clone https://github.com/Yrika819/local-mcp.git
cd local-mcp
cargo build --release --locked
```

## License and attribution

MIT. See [LICENSE](https://github.com/Yrika819/local-mcp/blob/v0.1.2/LICENSE) for the copyright notice and
[THIRD_PARTY_NOTICES.md](https://github.com/Yrika819/local-mcp/blob/v0.1.2/THIRD_PARTY_NOTICES.md) for dependency license
evidence. The original upstream copyright notice is preserved unchanged.
