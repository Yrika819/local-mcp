# Quick Start

This guide installs the released GoalLatch v0.1.1 binaries and checks that they
start. It does not claim that the complete interactive MCP workflow was tested
on every listed OS. Platform status and security boundaries are in
[SECURITY.md](../SECURITY.md); archive details are in
[the install guide](release/INSTALL.md).

## 1. Download and verify

Download the archive for your OS/architecture and `SHA256SUMS` from the
[GoalLatch v0.1.1 release](https://github.com/Yrika819/local-mcp/releases/tag/v0.1.1).
Use the matching checksum command before running the binaries.

Linux:

```sh
sha256sum -c SHA256SUMS
```

macOS:

```sh
shasum -a 256 -c SHA256SUMS
```

Windows PowerShell: calculate the archive hash and compare it to the exact
archive line in the downloaded `SHA256SUMS` file (PowerShell does not verify
that manifest format with `Get-FileHash -c`).

```powershell
Get-FileHash -Algorithm SHA256 .\local-mcp-v0.1.1-windows-x86_64.zip
Get-Content .\SHA256SUMS
```

A mismatch means stop; do not run the binary.

## 2. Install

### Windows x86_64 (Experimental)

Extract the archive and place **both** `local-mcp.exe` and
`atomic-publish.exe` together in a directory on `PATH`. This example uses
`C:\tools`:

```powershell
Expand-Archive -Path .\local-mcp-v0.1.1-windows-x86_64.zip -DestinationPath .
New-Item -ItemType Directory -Force C:\tools | Out-Null
Copy-Item .\local-mcp-v0.1.1-windows-x86_64\local-mcp.exe C:\tools\
Copy-Item .\local-mcp-v0.1.1-windows-x86_64\atomic-publish.exe C:\tools\
```

This is an install walkthrough based on the release install instructions; the
full Windows runtime/security behavior is **Experimental**. Windows has no
Unix-equivalent process sandbox. Use the ordinary approval flow and do not
enable `/permission yolo` unless you accept the host-native authority it grants.

### Linux x86_64

Requires upstream Bubblewrap **0.12.0 or newer**, user/PID/network namespaces,
seccomp, and glibc providing `GLIBC_2.34` or newer. Install all three
executables from the archive together:

```sh
tar -xzf local-mcp-v0.1.1-linux-x86_64.tar.gz
sudo install -d /usr/local/lib/local-mcp
sudo install -m 0755 local-mcp-v0.1.1-linux-x86_64/local-mcp \
  local-mcp-v0.1.1-linux-x86_64/codex-linux-sandbox \
  local-mcp-v0.1.1-linux-x86_64/atomic-publish /usr/local/lib/local-mcp/
sudo ln -sf /usr/local/lib/local-mcp/local-mcp /usr/local/bin/local-mcp
bwrap --version
local-mcp --version
```

The `bwrap` version must be 0.12.0 or newer. If the host requirements are
missing, sandboxed operations fail; do not disable host security controls to
work around that refusal. Linux aarch64 is experimental and is not covered by
this x86_64 walkthrough.

### macOS x86_64 / ARM64

The v0.1.1 archive names are `local-mcp-v0.1.1-macos-x86_64.tar.gz` and
`local-mcp-v0.1.1-macos-aarch64.tar.gz`. Extract the matching archive and
install both `local-mcp` and `atomic-publish` together. Example for Apple
Silicon:

```sh
tar -xzf local-mcp-v0.1.1-macos-aarch64.tar.gz
sudo install -m 0755 local-mcp-v0.1.1-macos-aarch64/local-mcp /usr/local/bin/local-mcp
sudo install -m 0755 local-mcp-v0.1.1-macos-aarch64/atomic-publish /usr/local/bin/atomic-publish
local-mcp --version
```

Use the `macos-x86_64` directory for Intel. Native Intel release-closure
validation is documented; Apple Silicon is a CI target and is **not** claimed
to be locally release-closure validated. Release binaries are not notarized or
code-signed; see the install guide before deciding how to handle Gatekeeper.

## 3. Verify the CLI

Run:

```sh
local-mcp --version
local-mcp --help
```

On Windows use `local-mcp.exe` if the executable is not on `PATH`. The
expected version is `0.1.1`. These checks prove only that the executable starts
and reports its CLI; they do not prove sandbox readiness or MCP client
compatibility.

## 4. Start a project session

Open a terminal in a disposable or otherwise appropriate project directory.
Start the session UI there:

```sh
cd /path/to/your/project
local-mcp start
```

PowerShell example:

```powershell
Set-Location C:\path\to\your\project
local-mcp.exe start
```

Record the session ID displayed by the UI. The working directory is the
location where `start` ran. The UI's `permitted_directories` are the filesystem
boundary; inspect them before letting an agent access the project. Do not paste
a real session ID into public logs/issues.

## 5. Connect your MCP client

Run `local-mcp mcp` as the client's stdio MCP server process, while leaving
`local-mcp start` running separately. Configure the MCP client using its own
current documentation and the executable path; GoalLatch does not publish a
verified, client-specific JSON configuration in this guide. See
[Connecting an MCP client](MCP_CLIENTS.md) for the protocol flow and limits.

After the client reports the server/tools are available, call `session_info`
with the session ID from the UI. Confirm the returned working directory and
permitted roots. Then follow [Example A](examples/README.md#example-a-read-only-project-inspection).

## Goal-model execution prerequisites

The stdio MCP connection, `tools/list`, `session_info`, direct file and directory
reads, and durable Goal creation/status do not require Codex CLI. `goal_start`
records a Goal but does not run it. `goal_run` invokes the external Codex CLI
for model-backed planning and work; the CLI is not bundled in GoalLatch's
release archives.

GoalLatch locates Codex in this order: `LOCAL_MCP_CODEX_CLI_PATH`, then
`CODEX_CLI_PATH`, then (on macOS, if present)
`/Applications/ChatGPT.app/Contents/Resources/codex`, then `codex` on `PATH`.
The Goal model is selected by `LOCAL_MCP_GOAL_MODEL`, falling back to
`LOCAL_MCP_CODEX_FALLBACK_MODEL`; the current code default is `gpt-5.6-luna`.
That string is a configured default, not evidence that the model is currently
available to your Codex CLI or account. Check current Codex availability and
pricing before running a Goal that invokes a model.

The host invokes `codex exec` with the selected model, medium reasoning effort,
an ephemeral run, ignored user config, and an explicit read-only sandbox. Each
model invocation also requires approval through the local GoalLatch permission
UI. Codex CLI must already be installed and authenticated for the same user;
GoalLatch does not inspect, create, or modify Codex credentials. See the
[Codex CLI documentation](https://developers.openai.com/codex/cli/) for its
current installation and sign-in instructions. Model requests may consume the
account's available usage or incur charges under its plan/provider terms. If a
model call is denied or fails, inspect `goal_status` and `goal_result`; never
interpret a returned error or nonterminal state as successful completion.

## What is and is not verified

- v0.1.1 release artifacts and platform requirements are listed in the
  [installation guide](release/INSTALL.md); the version and help commands are
  safe local smoke checks.
- macOS Intel has native release-closure evidence; Linux x86_64 is the
  documented supported sandbox target when host requirements are met.
- Windows is Experimental and has no Unix-equivalent process sandbox.
- macOS ARM64 and Linux ARM64 must not be represented as having the same local
  validation as their x86_64 counterparts.
- This campaign's documentation checks do not constitute an end-to-end test
  with a third-party MCP client.
