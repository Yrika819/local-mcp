# Linux sandbox support

Local MCP's Linux command sandbox is the Bubblewrap path from the pinned Codex
revision in `Cargo.toml`, followed inside Bubblewrap by the helper's `no_new_privs`
and seccomp restrictions. The ordinary production path does not switch to
Landlock if Bubblewrap setup fails.

## Requirements

- Linux x86_64 is the currently documented release target. Native release evidence
  is from Ubuntu 26.04.1 LTS on x86_64, kernel 7.0.0-34-generic, ext4.
- Bubblewrap upstream 0.12.0 or newer is required. Bubblewrap 0.12.0 is the first
  upstream release fixing [CVE-2026-87766](https://github.com/containers/bubblewrap/security/advisories/GHSA-pxhw-h44j-8pfx).
  The sibling Linux helper checks `bwrap --version` before normal sandbox setup
  and exits 126 on a missing, malformed, or older version. It does not start the
  requested command after a failed check.
- User and PID namespaces, mount namespaces, seccomp filters, and the kernel
  filesystem support required by Bubblewrap must be available. Restricted
  networking also depends on network namespaces.
- AppArmor may remain enabled. Bubblewrap must be permitted to create the
  namespaces and proc/device mounts it uses. Generic `unshare` outcomes depend on
  the calling process's AppArmor context and do not by themselves establish
  Bubblewrap support.

[Ubuntu USN-8779-1](https://ubuntu.com/security/notices/USN-8779-1) initially
backported the fix to `0.11.1-1ubuntu0.2`, then [USN-8779-2](https://ubuntu.com/security/notices/USN-8779-2)
reverted that fix in `0.11.1-1ubuntu0.3` after a symlink-resolution regression. Consequently the upstream-version check intentionally rejects that
package until Ubuntu ships a fixed package whose Bubblewrap-reported upstream
version is at least 0.12.0. Do not remove AppArmor restrictions or use an
unofficial system repository to work around this requirement.

## Enforcement and boundaries

The helper uses a filesystem namespace assembled from the pinned Codex sandbox
policy. Typical mounts include a read-only root or a temporary root, scoped
read-only binds, writable-root binds, `/dev`, and `/proc`. It requests user and
PID namespaces and a network namespace for restricted-network profiles. The
seccomp filter denies IP socket creation and network operations for ordinary
restricted commands while allowing Unix sockets where required. Child processes
inherit these restrictions.

Bubblewrap is required for the filesystem view. Landlock remains an upstream
legacy option, but Local MCP does not select it. The pinned legacy implementation
permits broad read access and writable roots and rejects some restricted-read
policies; it does not establish the full network namespace and mount contract.
It is not a supported substitute when Bubblewrap is missing or too old.

Sandbox setup errors are terminal for the sandboxed attempt. There is no
automatic host-native fallback. Explicit host-native execution follows the
separate approval boundary described in the main README.

## Host capability for restricted networking

Restricted networking needs the helper to own an isolated network namespace
whose loopback device it can bring up, which requires privileges inside the user
namespace the helper just created. The only route to those privileges is a user
namespace: upstream removed setuid support outright, and the helper now dies
with `setuid use of bubblewrap is not supported` as soon as the real and
effective uids differ. A host that refuses an unprivileged write to
`/proc/<pid>/uid_map` therefore cannot build this sandbox at all — the
namespaces are created but the privileges never arrive, and every sandboxed
request fails during setup with `setting up uid map: Permission denied` or, when
a network namespace is also requested, with
`loopback: Failed RTM_NEWADDR: Operation not permitted`.

This is a kernel property, not a Bubblewrap property, and the hosted Linux
images disagree about it. Measured with the pinned upstream runtime, built from
source and run unprivileged:

| image | kernel | filesystem sandbox | restricted network |
| --- | --- | --- | --- |
| `ubuntu-22.04` | 6.8.0-1064-azure | works | works |
| `ubuntu-24.04` | 6.17.0-1022-azure | refused | refused |
| `ubuntu-24.04-arm` | 6.17.0-1022-azure | refused | refused |
| `ubuntu-26.04` | 7.0.0-1012-azure | refused | refused |

The same table holds for the `v0.12.0` and `v0.13.0` release tags, so it is not
a regression in one upstream revision, and the distribution's own Bubblewrap
0.11.1 on `ubuntu-26.04` succeeds where the pinned build fails — which is why a
`unshare -Ur` probe is not a usable proxy for the capability either.

The Linux test job therefore runs on `ubuntu-22.04`, the hosted image that can
build the sandbox. Running the suite on a host that can do it is more coverage
than skipping the sandboxed tests on one that cannot, and `ubuntu-24.04` keeps
the full `quality` job: formatting, `cargo check`, and both clippy passes over
all targets, tests included. The Bubblewrap build step also reports
`sandbox_capability=restricted_sandbox_available` or `sandbox_capability=unavailable`
so the log states what the runner can do instead of leaving the suite to
discover it.

Production is restricted on every platform and cannot be configured otherwise.
The test build is the only place that can answer differently, and it answers
from the host: a memoized, fail-closed probe runs the real helper with the real
restricted profile and reads only the helper's own exit status. Only an observed
successful restricted run counts as supported. A missing or version-vulnerable
runtime, a helper that cannot be constructed, and a failure of any kind all
leave the production policy in place, so the tests that genuinely need a
sandbox fail loudly on the real cause instead of being quietly relaxed. Nothing
about the probe comes from a caller, an environment variable, or a model.

The restricted-network isolation test is gated on that probe rather than on an
environment variable, and is no longer `#[ignore]`d. On a host that can build a
restricted sandbox it proves the isolation; on a host that cannot it reports
that there is none to observe and passes, instead of satisfying its first
assertions with the helper's own setup failure while the requested command never
ran. Filesystem confinement is unaffected either way: it is a separate half of
the contract and is asserted on every host.

## Native evidence on 2026-09-23

The validation host reported Ubuntu 26.04.1 LTS, kernel 7.0.0-34-generic,
x86_64, ext4, AppArmor enabled, `kernel.apparmor_restrict_unprivileged_userns=1`,
`user.max_user_namespaces=29127`, seccomp filter support, and Landlock kernel
support. The Codex process itself reported `unconfined`, `Seccomp: 0`, and
`NoNewPrivs: 0`; this process context is not evidence about every desktop or
terminal AppArmor profile. The host's Bubblewrap package was
`0.11.1-1ubuntu0.3`, which the helper correctly rejects. A separately built
upstream Bubblewrap was used only from a disposable `/tmp` prefix for compatibility
checks; `/usr/bin/bwrap` was not replaced.

The final locked all-target suite passed on Rust 1.96.0 and 1.98.1: 504 tests
passed, 0 failed, and 1 intentionally ignored in each run; the separate Linux
helper harness also passed its strict version parser test. The ignored restricted-
network test was run explicitly with `LOCAL_MCP_TEST_ALLOW_LINUX_NETWORK` unset
and passed. Formatting and production/all-target clippy checks passed on both
toolchains; `cargo check --locked` passed on 1.98.1. Release compilation and CLI
version/help inspection passed on Rust 1.96.0.

This section records a single dated run and is not current guidance. The
restricted-network test it describes as ignored is no longer `#[ignore]`d, and
`LOCAL_MCP_TEST_ALLOW_LINUX_NETWORK` no longer exists: the policy that test runs
under is now chosen by the host-owned probe described above, which reads no
environment variable. See "Host capability for restricted networking" for how
the suite behaves now.

## CVE-2026-87766 applicability

This issue applies to the sandbox setup path. The pinned Codex Bubblewrap builder
in `linux-sandbox/src/bwrap.rs` constructs mounts using `--bind`, `--ro-bind`,
`--tmpfs`, `--dev`, `--proc`, and `--dir`; helper code creates parent directory
targets for mounts. The host policy supplies the mount roots and restrictions,
but workspace-derived path destinations and the filesystem's symlink layout can
influence path traversal while Bubblewrap constructs its new root. The vulnerable
resolution happens before the requested child process starts, so command-level
seccomp does not mitigate it. No exploit was attempted.

The local system package reported Bubblewrap 0.11.1 and package version
`0.11.1-1ubuntu0.3`. The helper now rejects reported versions below upstream
0.12.0 before sandbox setup. Compatibility and confinement tests used a separate
upstream 0.12.0 build at commit
`2a76602a8c71f36c1527cf9fc3417d9149822e0c`, isolated under `/tmp`; the system
binary and packages were left unchanged. The check uses Bubblewrap's upstream
version string, so a downstream fixed backport that still reports a version
below 0.12.0 will also be rejected until the gate is deliberately extended with
verified distro-specific backport metadata.

## Remaining release gates

- RustSec audit of the locked dependency graph reports four vulnerabilities in
  transitive dependencies: `hickory-proto 0.25.2` (RUSTSEC-2026-0118 and
  RUSTSEC-2026-0119; the latter is fixed in 0.26.1 or newer) and `quick-xml
  0.38.4` (RUSTSEC-2026-0194 and RUSTSEC-2026-0195; fixes require 0.41.0 or
  newer). It also reports `lru 0.16.4` as unsound and `derivative`, `fxhash`, and
  `paste` as unmaintained. These are inherited dependency-graph findings; this
  campaign did not change Cargo dependencies.
- `cargo-deny` has no repository configuration. Its default license policy rejects
  the current graph's license expressions; this is not evidence of missing
  license metadata. `cargo metadata --locked` found 503 packages and the license
  inventory found no missing metadata. A maintainer must define the intended
  license allowlist before license-policy validation can pass.
- Nix is not installed on the validation host, so the edited flake was not parsed
  or built. The pinned Nixpkgs Bubblewrap is older than 0.12.0; the flake now
  relies on a host-provided Bubblewrap meeting the runtime gate.
- Only the current unconfined Codex process context was directly exercised.
  Remote CI and native macOS/Windows validation remain outside this Ubuntu run.
