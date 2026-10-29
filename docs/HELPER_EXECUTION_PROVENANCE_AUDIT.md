# Helper Execution Provenance Security Audit — V2

Status: **PARTIAL**. The final-entry mitigation is in place. A bounded
**Mitigation Closure** is supportable only for the trusted-install assumptions
below and after all declared platform tests pass. **Full Execution Identity
Closure is NOT achieved.** No runtime code or OS guarantee binds the file
metadata check to the image opened by `Command::new(path)`.

## Scope and authority

This audit covers `atomic-publish` on all supported runtime platforms and
`codex-linux-sandbox` on Linux. Both production resolution paths use
`current_exe()` and the shared Install Contract helper selector. No PATH, cwd,
configuration, or arbitrary parent-directory fallback is permitted. The
`#[cfg(test)]` profile-parent helper location exists only to let Cargo tests
find built test helpers; it is not present in production.

No sandbox, Session/Goal/Writer permissions, publication permissions, helper
fallback, Doctor implementation, release artifact, or package channel changed.
The `local-mcp` launcher symlink is distinct from a helper symlink: the OS's
reported `current_exe()` remains the anchor; how that path is reported is
platform-dependent and is not used to trust a helper target.

## Threat model and attack conditions

The ordinary OS security boundary applies: installation files and every ancestor
must be protected from principals outside the trusted installer/runtime
administrative domain. A malicious process able to modify the helper under the
same effective user token is considered an attacker for helper provenance. In
that case it can directly rewrite the helper or often the running user's other
files; this runtime does not provide same-user malware isolation. Such a
configuration is **NO-GO** for any helper-execution claim. An administrator/root
or equivalent trusted package manager remains in the trust base.

| Case | Preconditions and attack path | Assessment |
| --- | --- | --- |
| A — Trusted install | Executable and helpers plus all ancestors are protected against untrusted writes/replacement for runtime duration. | A path check followed by spawn does not by itself prevent a trusted updater from changing the entry, but an untrusted attacker cannot win that race under this precondition. No hostile TOCTOU is established in this case. A trusted updater may cause availability failure or choose a different trusted release image; authenticity/version policy belongs to that updater. |
| B — Writable install | Attacker can write the helper entry or bytes, or replace an ancestor. | Symlink rejection is insufficient. An attacker can place an ordinary file, alter bytes in place, replace it after checking, or redirect a parent. NO-GO; current runtime does not certify directory owners/ACLs or protect writable installs. Same-token malware is included here. |
| C — Trusted entry, untrusted symlink target | A helper entry is a symlink whose target is across a trust boundary. | Existing `symlink_metadata` final-entry policy rejects it without intentionally following the leaf. This closes the stable final-link selection case. It does not close a replace-after-check race or a link/reparse point in an ancestor. |
| D — Concurrent trusted updater | A trusted updater atomically replaces a helper during startup or execution. | A path race can cause launch failure or start either old/new trusted content depending on OS/open timing. This is a compatibility/availability race if both images are trusted; it is not automatically malicious execution. If updater provenance is not trusted, classify it as Case B. In-place writes may expose partially updated content or fail to launch; updater atomicity is separate. |
| E — Ancestor redirection | Any ancestor is a symlink, junction/reparse point, mount, or replaceable directory; or path identity changes. | The current selector checks only the final helper component. The candidate is resolved through ancestors. It does not validate canonical ancestry, mount identity, directory identity, or pin a directory handle. Parent redirection is not prevented. Under Case A ancestors are protected by assumption; otherwise NO-GO. |
| Hard-link/alias | A regular helper path is a hard link to an inode also named elsewhere. | It passes the regular-file check. Hard-link count or alternate names do not prove maliciousness; file identity and bytes are only as trustworthy as all writers to that inode. No hostile hard-link attack is demonstrated. |

**Severity:** High for Case B/E when an untrusted principal can modify or redirect
the helper path: GoalLatch may run attacker-controlled code with its user
privileges. This is conditional on actual write/replace authority, not on the
mere existence of a symlink. In Case A, no attacker-controlled helper execution
is established; residual trusted-updater races are primarily availability and
release-consistency concerns. No exploit has been demonstrated.

## Current runtime audit

- `src/install_contract.rs` uses `symlink_metadata`, requires a regular file,
  rejects a final symlink, and on Windows rejects the final
  `FILE_ATTRIBUTE_REPARSE_POINT` bit.
- `src/execution.rs` uses the shared rule for `atomic-publish` and refuses when
  missing. Its test-only `deps` profile candidate is cfg-gated out of production.
- Linux `src/sandbox.rs` uses the same selector for `codex-linux-sandbox`, with
  a test-only Cargo profile helper candidate. Production does not search PATH,
  cwd, or a different install directory.
- macOS uses the system Seatbelt executable and has no `codex-linux-sandbox`
  helper; publication still uses the shared helper selector.
- Windows uses the shared publication selector; it has no Linux sandbox helper
  and remains Experimental.
- `first_existing` returns a pathname after metadata inspection. Neither call
  site binds that inspection to the image the OS opens later. On Windows and
  macOS the publication helper is launched directly with `Command::new(path)`.
  On Linux the sandbox helper is also launched directly, but the `atomic-publish`
  helper runs as the command inside that sandbox, so its pathname is re-opened
  after the helper's Bubblewrap gate and one extra process boundary. Both shapes
  leave a check-to-open window, and the Linux publication path has the wider one.

No simpler portable `std::process::Command` mechanism is available to pin an
opened file identity across launch. A native fd/handle execution layer would
interact with Tokio process lifecycle, process-group/Job ownership, argv/env,
error reporting, descriptor inheritance, and (Linux) the helper's sandbox
bootstrap. Introducing such a layer is not justified without a separate strict
platform design review and native threat-driven tests.

## Platform design and guarantees

| Platform | Practical trust condition | Implemented | Not guaranteed / cost of stronger approach |
| --- | --- | --- | --- |
| Linux | Install tree and ancestors not writable by untrusted principals; distro/package manager is trusted. | Final helper symlink rejected. Existing ProcessGroup and Bubblewrap helper launch are unchanged; Bubblewrap minimum remains 0.12.0. | No owner/mode or mount/ancestor verification. `execveat(AT_EMPTY_PATH)`/`fexecve` can execute an opened inode on Linux, but integrating it with sandbox helper argv, process-group spawn, fd semantics and error handling needs native code and review. Atomic updater rename avoids partial bytes but cannot alone guarantee which trusted generation starts. |
| macOS | Same protected-install-tree condition; trusted package manager owns updates. | Final publication-helper symlink rejected; Seatbelt use unchanged. | No ACL/owner/ancestor verification. Descriptor-based exec variants exist but require a custom spawn lifecycle and do not solve parent-directory policy by themselves. Store symlink layouts (Nix/Homebrew Cellar links) need actual layout/runtime evidence. |
| Windows | NTFS ACLs on the executable, helper and ancestors restrict writes to trusted installer/admin principals; `C:\tools` is trusted only if its ACLs say so. | Final helper reparse attribute rejected; Job/process containment is unchanged; Windows stays Experimental. | Runtime does not inspect ACLs, owner, parent reparse points, or directory identity. `CreateProcessW` accepts an image path; a verified handle-to-image guarantee would require separately reviewed native process creation. Replacement while loaded may fail or race; exact outcomes need native tests. |

These are deployment preconditions, not properties certified by the runtime.
A directory merely existing, being readable, or being under a conventional
prefix is not evidence of trust. OS security controls, ACL/ownership review and
updater design must establish the precondition.

## Install and package compatibility

- **v0.1.1 archive:** the archive checksum authenticates downloaded archive
  bytes relative to the published checksum; it does not validate runtime
  helper identity. Existing assets are unchanged. Future installation should
  extract/copy the regular `local-mcp` and required helpers into one protected
  directory, preserving executable modes on Unix.
- **Direct extraction/manual copy:** supported when regular helper files remain
  beside the path reported by `current_exe()` and the containing directory and
  ancestors meet the trusted-install condition. Copying only the launcher or
  helper files into a user-writable shared directory does not meet it.
- **Launcher symlink:** allowed as an invocation pattern, but the OS-reported
  `current_exe()` location controls sibling lookup. Do not assume every OS
  resolves the launcher symlink identically; test the actual installed layout.
- **Helper symlink:** deliberately rejected even if it points to a same-directory
  or package-store file. This can break deployments that symlink helpers.
- **Windows reparse:** final helper reparse point is rejected. Parent junctions
  and other ancestor redirections are not rejected by current code; only protected
  ACLs satisfy the deployment precondition.
- **Nix flake:** static inspection shows `buildRustPackage` builds the package,
  wraps `local-mcp` in `$out/bin`, and adds bash/curl to PATH; it does not
  explicitly link or relocate helper binaries in `postInstall`. It appears
  intended for sibling binaries in the same output, but wrapper/current_exe and
  store layout behavior has not been established by a native `nix build` and
  runtime probe. Do not infer build success, runtime compatibility, or change
  `cargoHash` from this inspection. Nix is **unverified**.
- **Homebrew/future managers:** no current formula was audited as an existing
  package. A supported formula must keep regular helper siblings beside the
  executed binary, use a protected/versioned Cellar tree and atomic trusted
  activation, then prove behavior on each platform. Do not rely on symlinked
  helper entries.

## Negative-test matrix and evidence

Pure metadata/contract tests must not be presented as proof of spawn-time image
identity. Current tests use temporary fixtures and do not launch attacker
payloads. The native test `current_exe` symlink probe launches only the test
harness and observes its OS-reported executable path.

| Case | Current evidence/status |
| --- | --- |
| Valid sibling helper | Pure contract test exists; Windows focused V2 run passed. |
| Missing helper | Pure contract test exists; Windows focused V2 run passed; runtime callers fail closed. |
| Same-directory symlink | Pure negative test exists; Windows focused V2 run passed. |
| External-target symlink | Pure negative test exists; Windows focused V2 run passed. |
| Broken symlink | Pure negative test exists; Windows focused V2 run passed (symlink privilege was available). |
| Parent symlink/junction | Unix pure test records that final-entry checking still accepts an entry reached through a redirected parent; not run on Windows. Windows junction NOT RUN; no parent pinning claim. |
| Windows final reparse | Windows focused V2 run confirmed the helper symlink fixture has `FILE_ATTRIBUTE_REPARSE_POINT` and selector rejects it. Arbitrary reparse tags remain untested. |
| Writable parent | Unix fixture records that helper selection does not certify/reject a writable install directory; not run on Windows. |
| Concurrent replacement | No spawn-time identity test. Unresolved; metadata-only testing cannot close it. |
| Hard-link alias | Unix pure fixture accepts regular hard-link alias; behavior is intentional and does not attest provenance. |
| Trusted update replacement | No updater integration test; old/new trusted-generation and availability behavior unverified. |
| Permission denial | No dedicated native permission-denial matrix. Filesystem/ACL policy is not implemented. |
| Native image identity | NOT RUN / NOT PROVIDED. Must be a separate native test; fake metadata runner is insufficient. |

The focused Windows V2 run passed 7 Install Contract tests, including valid
sibling, missing, same-directory/external/broken helper symlink, final reparse
attribute rejection, and `current_exe()`-through-launcher-symlink observation.
The fixture was available (no `NOT RUN` diagnostic). Unix-only parent-link,
writable-directory and hard-link tests were not compiled/run on Windows. Full
Rust tests (1308 passed), `cargo check`, Clippy, release scripts (19 passed),
workflow policy (19 passed), formatting and diff checks passed on Windows.

Tests skipped because a host lacks symlink privilege must be reported as
**NOT RUN**, not PASS. Linux and macOS native CI remain required; Windows results
cannot substitute for them. No native test verifies OS spawn-time image identity.

## Closure decisions

### Mitigation Closure

**Conditional / bounded, not a universal runtime certification.** The known
stable final-helper symlink/reparse case is refused by code; sibling-only
production lookup, no PATH/cwd fallback and fail-closed missing-helper behavior
are retained. A trusted install directory/ancestor condition is now explicit.
Mitigation closure for a supported deployment requires: all mandatory native
negative tests available on that OS pass; privilege-dependent cases are reported
NOT RUN; supported packages install regular siblings under ACL/mode-protected
ancestors; Linux and macOS CI provide native results; and Doctor reports
unverified runtime/install conditions as UNKNOWN or BLOCK without false PASS.
Writable/attacker-controlled install trees remain **NO-GO**.

### Full Execution Identity Closure

**NOT achieved.** The checked metadata and actual image opened by the OS are not
cryptographically or kernel-handle bound. Ancestor replacement, mount changes,
hard-link provenance, concurrent replacement and native spawn identity remain.
No claim of TOCTOU repair is made. Full closure requires a reviewed OS-specific
launch/installation design and identity-verifying native tests; it is not a
prerequisite for every ordinary use if the trusted-install precondition is
actually maintained, but any deployment that allows attacker changes stays
NO-GO.

## Doctor integration contract

The Doctor implementation is not part of this change and is not present in this
branch, so this section records the properties a Doctor integration must satisfy
instead of observations about its code. An integrated Doctor must classify a
helper entry that this selector refuses as BLOCK, must report unverified runtime
execution as UNKNOWN, must keep Windows Experimental, must not BLOCK MCP when the
optional Codex CLI is absent, and must never report an unrun probe as PASS.
Security V2 also does not claim that Doctor proves ACL, ancestor, or image trust.

The integration must stay additive: start from the then-current `main`, feed the
same helper entry states to runtime resolution and to Doctor classification, and
assert that both reach the same accept/refuse decision for an ordinary sibling, a
missing entry, a same-directory or external file symlink, a broken link, and a
directory-target link. Any Doctor reading of an entry must not be described as
enforcing runtime containment, because the two currently answer different
questions: the runtime asks whether a usable regular sibling exists, and Doctor
asks whether the directory entry itself is a regular file.

## Release and publication decision

The v0.1.1 release is historical and unchanged. This change ships no release,
tag, or publication step and does not modify an existing release artifact. A
build from this branch may be published only after the trusted-install
preconditions, OS matrix results, regression tests and Nix/package compatibility
statements in this document are reviewed, and only if the published material does
not claim Full Execution Identity Closure. A draft must not be published as if
closure were complete.
