# Installing GoalLatch

GoalLatch Public v1 keeps the `local-mcp` executable and archive names for
compatibility. This is the per-platform install guide for the Public v1 archives. For what the
project is and what it does, see the [release
notes](RELEASE_NOTES_v0.1.0.md). For the security model, read
[`SECURITY.md`](../../SECURITY.md) — it is the authoritative description, and
this page only tells you how to unpack a download.

## Before you start: verify what you downloaded

Every release ships a `SHA256SUMS` file next to the archives. Check it before
you run anything:

```sh
# Linux
sha256sum -c SHA256SUMS

# macOS
shasum -a 256 -c SHA256SUMS
```

Every line should read `OK`. A mismatch means the download is incomplete or
altered; do not run the binary.

Pick the archive that matches your platform:

> **Linux aarch64 is experimental.** Its binary is built natively and its
> package/architecture checks run in CI, but the repository's Linux sandbox
> evidence documents x86_64 only. Do not treat an ARM64 build as sandbox
> closure validation.

| Platform | Archive |
| --- | --- |
| Linux x86_64 | `local-mcp-v0.1.0-linux-x86_64.tar.gz` |
| Linux aarch64 (ARM) | `local-mcp-v0.1.0-linux-aarch64.tar.gz` |
| macOS Intel | `local-mcp-v0.1.0-macos-x86_64.tar.gz` |
| macOS Apple Silicon | `local-mcp-v0.1.0-macos-aarch64.tar.gz` |
| Windows x86_64 | `local-mcp-v0.1.0-windows-x86_64.zip` |
| Windows ARM64 | `local-mcp-v0.1.0-windows-aarch64.zip` |

Each archive contains a single versioned directory with the binaries, project
`LICENSE`, `README.md`, `SECURITY.md`, `THIRD_PARTY_NOTICES.md`, and the
verbatim Apache-2.0 `CODEX-LICENSE.txt` and `CODEX-NOTICE.txt` from the pinned
Codex revision. Extracting it does not scatter files into your current directory.

## Linux

### What is in the archive

Two executables, and **both are required**:

- `local-mcp` — the MCP server and approval UI.
- `codex-linux-sandbox` — the Bubblewrap sandbox helper.

They must be installed in the **same directory**. `local-mcp` locates the
helper as a sibling of its own executable and refuses to run a sandboxed
command if it is missing. Copying only `local-mcp` to your `PATH` produces an
install that appears to work and then fails the first time it tries to run a
command.

### Install

```sh
tar -xzf local-mcp-v0.1.0-linux-x86_64.tar.gz
sudo install -d /usr/local/lib/local-mcp
sudo install -m 0755 local-mcp-v0.1.0-linux-x86_64/local-mcp \
  local-mcp-v0.1.0-linux-x86_64/codex-linux-sandbox /usr/local/lib/local-mcp/
```

Both were installed with mode `0755`; the archive preserves that. Put them on
your `PATH` with a symlink, or add the directory to `PATH` directly:

```sh
sudo ln -sf /usr/local/lib/local-mcp/local-mcp /usr/local/bin/local-mcp
```

Note that `local-mcp` itself finds the helper relative to its own path, so if
you symlink only `local-mcp` into `PATH` the helper must still be resolvable
as a sibling. Installing both into one directory and adding that directory to
`PATH` is the reliable arrangement.

### Bubblewrap is a hard requirement

Linux needs upstream **Bubblewrap 0.12.0 or newer** on `PATH`:

```sh
bwrap --version   # must report 0.12.0 or newer
```

0.12.0 is the first release fixing
[CVE-2026-87766](https://github.com/containers/bubblewrap/security/advisories/GHSA-pxhw-h44j-8pfx).
The helper checks the version before starting anything and exits `126` on a
missing, malformed, or older one — the requested command never starts. Ubuntu
currently ships a version the check rejects on purpose; see
[docs/linux_sandbox.md](../linux_sandbox.md) for why, and do not work around it
by disabling AppArmor or using an unofficial repository.

The kernel must also permit unprivileged user namespaces. If
`/proc/<pid>/uid_map` cannot be written, sandbox setup fails and that is a
host property, not something you can configure around.

### Other requirements

The Linux executables dynamically link against the GNU/Linux runtime. The
release build measured these highest required GLIBC symbol versions for both
executables in each archive:

| Architecture | Required glibc | Build runner | Status |
| --- | --- | --- | --- |
| x86_64 | `GLIBC_2.34` or newer | Ubuntu 22.04 (glibc 2.35) | Supported target |
| aarch64 | `GLIBC_2.39` or newer | Ubuntu 24.04 ARM (glibc 2.39) | Experimental; sandbox closure not validated |

Use a distribution that provides at least the listed glibc symbol version. The
vendored OpenSSL does not require a separate system OpenSSL library. Exact
`ldd` output and the symbol checks are in the linked public Release Actions run.

## macOS

### What is in the archive

One executable, `local-mcp`. There is no helper to install: macOS sandboxes
through the system `sandbox-exec`, which is part of the OS.

### Install

```sh
tar -xzf local-mcp-v0.1.0-macos-aarch64.tar.gz
install -m 0755 local-mcp-v0.1.0-macos-aarch64/local-mcp /usr/local/bin/local-mcp
```

The binary links to system libraries as listed by `otool -L` in the release
build log. It is not notarized or code-signed as part of this release.
Gatekeeper may therefore refuse to run it the first time. If it does, either use the
source build below or remove the quarantine attribute from the file you
extracted:

```sh
xattr -d com.apple.quarantine /usr/local/bin/local-mcp
```

Removing the attribute is your decision; it only suppresses the first-run
check for a file you already downloaded from a public release and verified
against `SHA256SUMS`.

## Windows

> **Experimental.** Windows support in this release has **no Unix-equivalent
> process sandbox**. Command and file mutation paths run host-native and are
> approval-gated; named-pipe IPC is restricted to the current user with an
> explicit DACL, but that is an IPC boundary, not a sandbox. Windows is not
> covered by the Linux or macOS security model, and successful validation on
> those platforms is not Windows security validation.

### What is in the archive

One executable, `local-mcp.exe`.

### Install

Extract the zip, then either add the directory to `PATH` or copy the binary
somewhere already on it:

```powershell
Expand-Archive -Path local-mcp-v0.1.0-windows-x86_64.zip -DestinationPath .
Copy-Item local-mcp-v0.1.0-windows-x86_64\local-mcp.exe C:\tools\local-mcp.exe
```

```powershell
local-mcp.exe --version
local-mcp.exe mcp
```

Verify a download on Windows with:

```powershell
Get-FileHash -Algorithm SHA256 local-mcp-v0.1.0-windows-x86_64.zip
```

and compare against the line for that file in `SHA256SUMS`. SmartScreen may
warn on first run for the same unsigned-binary reason as macOS.

## Building from source instead

Every platform can build from source with the locked dependency graph. This is
the path to take if you need a binary your distribution would accept, or want
to avoid the unsigned-binary prompt.

```sh
git clone https://github.com/Yrika819/local-mcp
cd local-mcp
cargo build --release --locked
./target/release/local-mcp --version
```

Nix users can use the flake, which supports Linux and macOS on x86_64 and
aarch64:

```sh
nix run github:Yrika819/local-mcp
```

The flake does not bundle Bubblewrap, because the pinned Nixpkgs version is
older than the 0.12.0 security fix. Provide Bubblewrap 0.12.0+ on your host
`PATH`. On Nix, `--locked` and a pinned nixpkgs revision give you a
reproducible dependency graph.

## After installing

Start a session in the directory you want the agent to work in, and run a
separate MCP server process:

```sh
cd ./some-project
local-mcp start
```

Give the printed session ID to the agent so it can include it in each tool
call. The session working directory is wherever you ran `local-mcp start`;
there is no separate persistent `cwd` setting.
