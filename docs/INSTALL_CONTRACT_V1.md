# Install Contract V1

Status: implemented by `scripts/release/install_contract.py` and
`scripts/release/test_install_contract.py`. This document is the installed-runtime
contract for GoalLatch Public v1 release artifacts. It describes what a verified
archive must produce after extraction, what the runtime resolves, and which
claims were observed on a real host rather than inferred from an archive header.

This is not a package-manager registration and not a Doctor CLI. Doctor V1 is a
future consumer of the same checks; the diagnostic contract at the end defines
what it may rely on.

## Supported platforms

| Target key | OS | Architecture | Archive | Status |
| --- | --- | --- | --- | --- |
| `linux-x86_64` | Linux | x86_64 | `.tar.gz` | Supported |
| `linux-aarch64` | Linux | aarch64 | `.tar.gz` | Experimental (sandbox closure evidence is x86_64 only) |
| `macos-x86_64` | macOS | x86_64 | `.tar.gz` | Supported |
| `macos-aarch64` | macOS | aarch64 | `.tar.gz` | Supported |
| `windows-x86_64` | Windows | x86_64 | `.zip` | Experimental (no Unix-equivalent process sandbox) |
| `windows-aarch64` | Windows | aarch64 | `.zip` | Experimental (no Unix-equivalent process sandbox) |

Archive names remain `local-mcp-v<version>-<os>-<arch>.<suffix>` for
compatibility with the existing release assets.

## Required executables

| Target | Required executables | Forbidden executables |
| --- | --- | --- |
| Linux x86_64 / aarch64 | `local-mcp`, `codex-linux-sandbox`, `atomic-publish` | none |
| macOS x86_64 / aarch64 | `local-mcp`, `atomic-publish` | `codex-linux-sandbox` |
| Windows x86_64 / aarch64 | `local-mcp.exe`, `atomic-publish.exe` | `codex-linux-sandbox`, `codex-linux-sandbox.exe` |

`atomic-publish` is required on every platform. It performs the Writer commit
inside the platform boundary on Unix and is the shell-free commit mechanism on
Windows. The runtime refuses to publish when it is missing; there is no
unsandboxed in-process fallback.

`codex-linux-sandbox` is required only on Linux. It is a Linux-only helper and
must not be installed on macOS or Windows.

## Install layout

A verified archive extracts to exactly one versioned top-level directory:

```text
local-mcp-v<version>-<os>-<arch>/
  local-mcp[.exe]
  atomic-publish[.exe]
  codex-linux-sandbox          # Linux only
  CODEX-LICENSE.txt
  CODEX-NOTICE.txt
  LICENSE
  README.md
  SECURITY.md
  THIRD_PARTY_NOTICES.md
```

Rules:

1. The top-level directory name must equal the archive stem for the version.
2. Every required executable and documentation file must be present.
3. No other file may be present. A stray source file, cache, or helper is a
   contract failure.
4. On Unix, required executables must carry the executable bit and documentation
   must not.
5. Helpers must be direct siblings of `local-mcp`. A helper in a subdirectory is
   not an install.
6. The Linux helper must be absent from macOS and Windows installs.
7. An install that contains only `local-mcp` is structurally invalid even though
   `local-mcp --version` may still run.

The installer is a plain extraction plus a copy into a directory. GoalLatch does
not currently ship an installer that mutates the user's `PATH`; the user adds the
install directory to `PATH` or creates a symlink.

## Helper resolution

The runtime resolves helpers from `std::env::current_exe()`:

```text
helper = current_exe().parent() / helper_name
```

There is exactly one production candidate. The runtime does **not** search:

- `PATH`;
- the current working directory;
- a configuration file or session value;
- the model's output;
- a parent directory above the install prefix.

If the candidate is absent, the operation that needs it fails closed:

- a missing `atomic-publish` refuses publication;
- a missing `codex-linux-sandbox` refuses a sandboxed command.

No fallback lowers containment when a helper is missing.

The name is platform-specific: `atomic-publish.exe` on Windows and
`atomic-publish` elsewhere. `codex-linux-sandbox` has no `.exe` suffix and only
exists on Linux.

### What is tested

`src/install_contract_tests.rs` covers the candidate rule directly:

- same-directory helper is found;
- a missing helper produces no candidate;
- a helper in another directory or on `PATH` is never a candidate;
- the platform-specific helper name is correct;
- launching the same test binary through a symlink observes what
  `current_exe()` actually returns, and helper resolution is anchored to that
  observed directory, never to `PATH`.

The symlink observation is intentionally an observation, not an assumption. The
answer is defined by the OS and can differ between Linux, macOS, and Windows.
`install_contract.py` separately launches the installed `local-mcp` directly,
through `PATH`, and through a symlink on the native host.

## Symlink constraints

- The reliable install is: put `local-mcp` and every required helper in the same
  directory, then add that directory to `PATH`.
- A symlink to `local-mcp` works only if the helper is still resolvable from the
  directory `current_exe()` reports. On some systems that is the symlink's
  directory; on others it is the symlink target's directory.
- Observed on the Windows x86_64 host used for this campaign: a file symlink
  launched with `current_exe()` reporting the **symlink path** (the `link-bin`
  directory), not the real target path. Helper resolution therefore looked in
  the symlink's directory. Linux and macOS were not executed in this campaign;
  their `current_exe()` behavior is unverified here.
- A symlink-only install that puts `local-mcp` in one directory and helpers in
  another is not supported. It may pass `--version` and then fail the first
  sandboxed command or file commit. This is exactly why `--version` success is
  not used as evidence that helper resolution works.
- On Windows, creating a symlink requires Developer Mode or
  `SeCreateSymbolicLinkPrivilege`. If the host cannot create one, the symlink
  check is reported as `not-run`, not as a pass.
- Do not use a symlink to redirect helper discovery to a directory outside the
  install. The runtime has no such search and adding one would widen authority.

## Version consistency

- `Cargo.toml` `[package].version` is the source of truth for the build.
- Archive names and the versioned root use that version.
- `install_contract.py` compares `local-mcp --version` with the version the
  archive declares. A mismatch is a failure.
- `flake.nix` must declare the same version. A static test enforces that.
- The release workflow already checks the requested version against
  `Cargo.toml` before building.

## Formal verification versus runtime verification

These are different claims and are reported separately.

| Check | Kind | Where |
| --- | --- | --- |
| Exact archive member set, root directory, exec bits, denied content | Formal (archive bytes) | Existing `verify_package.py`; reused by `install_contract.py` |
| ELF/Mach-O/PE architecture headers | Formal (archive bytes) | Existing `verify_package.py`; reused |
| Post-extraction layout, sibling helpers, forbidden helpers | Formal (extracted tree) | `install_contract.py` |
| `local-mcp --version` direct | Runtime (native host) | `install_contract.py` |
| `local-mcp --version` through `PATH` | Runtime (native host) | `install_contract.py` |
| `local-mcp --version` through a symlink | Runtime (native host, if the host allows symlinks) | `install_contract.py` |
| `atomic-publish` starts and rejects a malformed frame | Runtime (native host) | `install_contract.py` |
| Helper resolution rule and symlink `current_exe()` observation | Runtime (unit test on the host) | `src/install_contract_tests.rs` |
| Real published archive download, SHA256SUMS, extraction | Formal + network | `install_contract.py published` |

A structural PASS is not a claim that the binary ran. A native PASS is not a
claim about a different OS or architecture. Foreign-architecture binaries are
never executed.

## Platform verification status

As of 2026-10-24, on the Windows x86_64 host used for this campaign:

| Platform | Published v0.1.1 formal | Native execution |
| --- | --- | --- |
| Linux x86_64 | PASS (SHA256SUMS matched) | not-run on Windows host |
| Linux aarch64 | PASS (SHA256SUMS matched) | not-run (foreign architecture) |
| macOS x86_64 | PASS (SHA256SUMS matched) | not-run on Windows host |
| macOS aarch64 | PASS (SHA256SUMS matched) | not-run (foreign architecture) |
| Windows x86_64 | PASS (SHA256SUMS matched) | PASS: direct, PATH, symlink, helper startup |
| Windows aarch64 | PASS (SHA256SUMS matched) | not-run (foreign architecture) |

Published archive hashes observed from the official `v0.1.1` GitHub Release:

| Archive | SHA-256 |
| --- | --- |
| `local-mcp-v0.1.1-linux-x86_64.tar.gz` | `3affc1dffcad1fc76aa0b76340a386e4b1ab86c373bc976130793fb15d3cfc56` |
| `local-mcp-v0.1.1-linux-aarch64.tar.gz` | `3a3bde6c7ac7b024a539e03c5b1381edea555f0920192cc465811e0af3a94708` |
| `local-mcp-v0.1.1-macos-x86_64.tar.gz` | `32ba392493b3485028f50116b6e808f225f162f8e245b4a8b50bf385829ae7e1` |
| `local-mcp-v0.1.1-macos-aarch64.tar.gz` | `6d837f6f06315d84303df042e5b0fde0b989f1b4f5dd0b1d11d2cdfbc26cd3bc` |
| `local-mcp-v0.1.1-windows-x86_64.zip` | `73f8296c7ec10e9543c5db6438c569378840dcf65dfd1215d44e77d8a5cd8a4b` |
| `local-mcp-v0.1.1-windows-aarch64.zip` | `248581dd4e27aacef284198ddd696e5610543873ca3815101687c420bb23a37c` |

The native Windows result also covered `atomic-publish.exe` startup (exit 65 on a
malformed frame). `codex-linux-sandbox` was not executed: this host is Windows,
and the helper is not part of a Windows install.

CI adds native install-contract checks on each matching runner in the test
matrix (Linux x86_64, macOS Intel, macOS Apple Silicon, Windows x64). The
release build job repeats the archive-based native check on all six release
runners, including Linux ARM64 and Windows ARM64. Those two remain experimental
and are not promoted to parity by a passing install check.

## Negative tests

The contract test refuses all of the following. Each case is constructed in an
isolated temporary fixture; no existing session, Goal, project, or install is
modified.

| Case | Expected result |
| --- | --- |
| `atomic-publish` missing from the extracted install | FAIL, missing required executable |
| `codex-linux-sandbox` missing from a Linux install | FAIL, missing required executable |
| `codex-linux-sandbox` present on macOS/Windows | FAIL, forbidden platform helper |
| Helper placed in a subdirectory | FAIL, missing at root and unexpected nested file |
| Wrong versioned top-level directory | FAIL, root does not match the archive version |
| Wrong archive name / version | FAIL, name convention mismatch |
| Wrong architecture header | FAIL (existing `verify_package.py` check) |
| Corrupt/truncated archive | FAIL, extraction refused |
| Wrong SHA-256 | FAIL, checksum mismatch |
| Archive containing a symlink or hard link | FAIL, extraction refused |
| Archive escaping its destination (`..`, absolute, drive prefix) | FAIL, extraction refused |
| Non-executable required binary on Unix | FAIL, executable bit missing |
| `local-mcp --version` reporting a different version | FAIL, version mismatch |
| Missing helper on `PATH` while absent beside the executable | FAIL, no candidate (never a fallback) |
| Foreign-architecture binary | not executed; structural only |
| Windows host without symlink privilege | symlink check reported `not-run`, not PASS |

## Nix

Current state:

- `flake.nix` uses `pkgs.rustPlatform.buildRustPackage` with `pname =
  "local-mcp"`, `src = self`, and a `cargoHash`.
- The flake version was `0.1.0` while `Cargo.toml` is `0.1.1`. This campaign
  corrected the version string to `0.1.1`. `cargoHash` and dependency inputs
  were not changed because they cannot be recomputed without a Nix evaluation;
  guessing a hash would be worse than leaving it unverified.
- `buildRustPackage` is expected to install every binary target (`local-mcp`,
  `atomic-publish`, and on Linux `codex-linux-sandbox`) into `$out/bin`. This is
  a static expectation from the flake, not a build result observed in this
  campaign.
- `postInstall` runs `wrapProgram $out/bin/local-mcp`. `makeWrapper` renames the
  real binary to `$out/bin/.local-mcp-wrapped` and installs a wrapper at
  `$out/bin/local-mcp`. Because `current_exe()` points at the wrapped binary in
  `$out/bin`, the sibling helpers remain resolvable. This is a static inference;
  `nix run` and `nix build` were not executed here.
- The flake does not bundle Bubblewrap. Linux users must provide Bubblewrap
  0.12.0 or newer on `PATH`; this is already documented in the install guide.
- macOS Nix uses the system `sandbox-exec` and ships `atomic-publish`; no Linux
  sandbox helper is installed.
- `nix` was not available on the campaign host. No claim of a successful Nix
  build is made.

Remaining Nix work, if pursued separately:

1. Evaluate the flake and recompute `cargoHash` if the pinned dependency graph
   requires it.
2. Confirm `nix build` places all three Linux binaries in `$out/bin`.
3. Confirm `nix run` starts the wrapped binary and that helper resolution still
   uses the wrapper's directory.
4. Decide whether the Nix package should wrap or expose `atomic-publish`
   explicitly (it is currently installed as a normal sibling).
5. Validate the Linux sandbox dependency contract on a NixOS host with
   Bubblewrap 0.12.0+.

This is a separate, larger task and should be its own change if it requires
dependency or hash changes.

## Doctor V1 diagnostic contract

Doctor V1 may reuse this contract but must not invent authority. The following
fields are the stable diagnostics it can rely on:

```json
{
  "contract_version": 1,
  "target": "windows-x86_64",
  "host_target": "windows-x86_64",
  "native": true,
  "experimental": true,
  "install_root": "local-mcp-v0.1.1-windows-x86_64",
  "required_executables": ["local-mcp.exe", "atomic-publish.exe"],
  "forbidden_executables": ["codex-linux-sandbox", "codex-linux-sandbox.exe"],
  "helper_resolution": "current_exe-sibling",
  "formal": "PASS",
  "runtime": {
    "direct": "local-mcp.exe --version = 0.1.1",
    "path": "local-mcp.exe --version = 0.1.1",
    "symlink": "local-mcp.exe --version = 0.1.1",
    "atomic_publish_startup": "atomic-publish.exe starts and rejects a malformed request (exit 65)"
  }
}
```

Doctor V1 rules:

1. Report structural and runtime results separately. Never upgrade a structural
   PASS to a runtime PASS.
2. A non-native target is `not-run`, never a pass.
3. A missing helper is a failure. Do not suggest adding `PATH` search or an
   in-process fallback.
4. Report the observed `current_exe()` directory when explaining helper
   resolution, especially for symlink installs.
5. Report `not-run` for symlink checks the host cannot perform.
6. Do not mutate the install, the user's `PATH`, or the user's data to make a
   diagnostic pass.
7. Treat an experimental target's success as evidence for that target only, not
   as parity with Linux/macOS sandbox claims.

## Running the checks

```sh
# Fast, network-free contract and negative tests
python3 scripts/release/test_install_contract.py

# Matrix consistency only
python3 scripts/release/install_contract.py contract

# Verify a locally built archive and its extracted install
python3 scripts/release/install_contract.py archive \
  --target linux-x86_64 --version 0.1.1 \
  --archive dist/local-mcp-v0.1.1-linux-x86_64.tar.gz

# Stage target/release binaries and observe the native install
python3 scripts/release/install_contract.py native \
  --version 0.1.1 --binary-dir target/release

# Verify the real published artifact (downloads from the official release URL)
python3 scripts/release/install_contract.py published \
  --target windows-x86_64 --version 0.1.1
```

`published` uses the official URL
`https://github.com/Yrika819/local-mcp/releases/download/v<version>/<asset>`,
compares the archive against `SHA256SUMS`, extracts into a temporary directory,
and executes only when the target matches the host.
