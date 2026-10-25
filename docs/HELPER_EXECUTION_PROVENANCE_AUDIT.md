# Helper Execution Provenance Security Audit

Status: **PARTIAL; Security Closure BLOCKED**

This campaign is limited to runtime helper resolution and execution. It does not
change Doctor Core, Setup Preview, release channels, sandbox policy, session
permissions, or publication authority.

## Finding and conditions

Both `atomic-publish` and (on Linux) `codex-linux-sandbox` are resolved as
siblings of `current_exe()`; production has no PATH or cwd fallback and missing
helpers fail closed. Before this change, `Path::is_file()` followed a final
symlink, and the returned path was later passed to `Command::new()`. An
attacker-controlled helper target therefore could be selected when an attacker
can arrange a helper symlink in the executable directory. The mere presence of
a symlink is not proof of a remotely exploitable vulnerability: impact requires
an attacker to control or replace the installed helper entry/target, or to win a
race in an otherwise protected directory. No exploit has been demonstrated.

| Threat | Assessment |
| --- | --- |
| Writable install directory | Critical trust-boundary failure if an attacker can replace helper files (or the host binary). Read-only/existence checks do not establish ownership or ACL trust. This patch does not validate owner, ACL, or directory write access. |
| Helper symlink to same/external target | Final helper symlinks are rejected. This is conservative; legitimate helper symlinks may stop working. The normal `local-mcp` launcher symlink remains distinct: resolution remains anchored to OS `current_exe()`. |
| Check-to-spawn replacement | Not solved. `Command::new(path)` reopens a path and does not bind execution to the earlier metadata result. No race exploit has been demonstrated. |
| Windows reparse points / junctions | Final reparse entries are rejected using the reparse attribute. Parent path traversal and replacement are not pinned. Native Windows validation is still required. |
| Hard links / aliases | Hard links are ordinary regular files and pass the check. The same inode may have names outside the install directory; path-based checks cannot establish its provenance. No hostile hard-link execution was demonstrated. |
| Legitimate updater | An updater that replaces the helper concurrently can race the runtime. Update coordination or immutable/versioned helper directories are not implemented. |
| Nix / package managers | Store-managed symlink layouts can be incompatible with the no-final-link policy. Compatibility has not been tested; an explicit trusted immutable-store policy would need separate design/evidence. |

Severity: **High (provisional)** where an attacker can modify the install
location or helper target, because the attacker may cause host-owned code to be
executed with GoalLatch's user authority. Severity is conditional on that write
capability; this audit does not claim a symlink alone grants it. The unresolved
race is also security-significant, but exploitability depends on a concurrent
writer able to replace the path or an ancestor. Severity is not a claim of
confirmed remote exploitability.

## Decision and mitigation

Adopted A as a mitigation: inspect the final path entry with
`symlink_metadata`, require a regular file, reject symlinks, and on Windows
reject any final entry carrying `FILE_ATTRIBUTE_REPARSE_POINT`. This prevents
following an already-present final symlink/reparse entry. It does not prove
ownership, non-writability, code signature, content identity, or race freedom.

B is not implemented. Checking that a directory exists or can be read is not a
trust proof; reliable owner/ACL policy is platform-specific and package-manager
specific, and the current release/install contract does not establish such a
policy.

C is not implemented to closure. Linux can in principle execute an opened file
descriptor using `execveat(AT_EMPTY_PATH)`/`fexecve`, but this requires replacing
or extending the current Tokio/Command spawn path and preserving process-group,
sandbox, argv, and descriptor semantics. macOS has descriptor-based exec APIs,
but the same spawn/lifecycle integration and platform policy work is required.
Windows `CreateProcessW` takes an image path; safely binding a verified file
handle to the created image needs a separately reviewed native implementation
and guarantees. No portable `std::process::Command` API pins file identity.
Therefore this patch explicitly does **not** claim TOCTOU remediation.

D remains a separate gate: SHA-256 checks on release archives verify archive
integrity at download/package time, not the identity of an installed helper at
runtime.

## Platform guarantees

- Linux: no-final-symlink preflight; Bubblewrap minimum remains 0.12.0. No
  descriptor-pinned helper execution; race remains.
- macOS: same no-final-link rule for `atomic-publish`; Seatbelt behavior is
  unchanged. No descriptor-pinned execution; race remains.
- Windows: final reparse-point attribute rejected for `atomic-publish.exe`;
  Windows remains Experimental and unsandboxed. Parent redirection and race
  remain.

No change was made to atomic publication permissions, sandbox fallback
behavior, or Session/Goal/Writer authority. Missing helpers remain a hard error.

## Validation and closure gates

The install-contract tests include regular sibling, missing helper, same-directory
and external symlinks, broken symlinks, directory symlink, and Unix hard-link
alias behavior. Symlink fixtures may be unavailable on Windows hosts without
symlink privilege. Parent redirection, writable-directory policy, permission
denial, concurrent helper replacement, arbitrary reparse tags/junctions, and
unexpected-but-regular file identity are not proven by the current fixture
suite. A metadata-only fake runner would not prove the OS image identity; a
race-safe native integration test must verify which image actually ran.

Native Linux and macOS test runs are required before closure. Windows results
alone cannot close Linux/macOS. Runtime and Doctor diagnostics have not been
changed: no new Doctor finding is claimed, and no Doctor worktree content is
included. Any later Doctor integration must be additive and preserve its existing
uncommitted work.

**GO/NO-GO: NO-GO.** Do not publish a draft PR as a security-closure claim. A
reviewed race-resistant launch design, platform-native implementation and tests,
install/update/package compatibility evidence (including Nix consideration),
and Linux/macOS native CI are required first. Existing v0.1.1 release artifacts
are unchanged; the runtime fix, if later accepted, applies only to a subsequent
build/release. Do not modify or reissue the existing tag/assets as part of this
campaign.
