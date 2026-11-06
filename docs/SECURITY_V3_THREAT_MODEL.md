# Security V3 — Threat Model

Status: **DESIGN-LEVEL / NOT IMPLEMENTED** (2026-11-04). Companion to
`docs/SECURITY_V3_DESIGN.md` and `docs/SECURITY_V3_TEST_PLAN.md`. Adversary
classes are unchanged from Security V2: an attacker with a foothold as the
same user token is in scope for analysis but **out of complete defense**
(R-4); an attacker with write authority over the install root or helpers is
Case B (NO-GO today).

## 1. Assets

- B-1: integrity of the helper image at the moment it executes.
- B-2: integrity of the path ancestry used to resolve the helper.
- B-3: the install root's continued invariants (ownership, mode, ACLs).
- B-4: correctness of any verdict consumed by runtime/validator/Doctor.
- B-5: availability of the legitimate exec path (denial is acceptable to
  fail closed; silent weakening is never acceptable).

## 2. Adversaries

- External same-host attacker with write access to some ancestor of the
  install directory (Case E).
- External attacker racing an atomic trusted updater (Case D).
- Compromised package/updater or CI releasing a forged archive (V2 scope
  unchanged; V3 does not solve archive authenticity end-to-end — that is
  release signing, a separate track).
- Local same-user malware (out of complete defense, R-4).
- Honest-but-buggy installer/updater (availability races, mislabeled
  verdicts).

## 3. Threats and required mitigations

| ID | Threat | Mitigation (design) | Test |
| --- | --- | --- | --- |
| T-V3-1 | Helper image swapped between check and spawn (path re-resolution) | Open-then-exec on a pinned object (D-V3-1); no path re-parse at exec on Linux; `/dev/fd` on macOS post-spike; residual documented on Windows | N-V3-2, N-V3-3 |
| T-V3-2 | Ancestor directory replaced/redirected after open (renamed root, junction flip, mount change) | Per-level `fstat(fd)` verification against values observed at each open (device+inode+type); legitimate mount boundaries are evaluated as new levels under the same policy; `O_NOFOLLOW`/`FILE_FLAG_OPEN_REPARSE_POINT`; filesystem type via `fstatfs`; fail closed on any change | N-V3-4 |
| T-V3-3 | Final-entry symlink/reparse substituted between verification and exec | fd-pinning makes final-name irrelevant at exec; existing final-entry rejection retained as defense in depth | N-V3-1, N-V3-2 |
| T-V3-4 | In-place byte rewrite of a verified helper after open | Identity binding makes the pinned inode the image; bytes may still change for an authorized writer — accepted and documented; immutability is trusted-root policy (D-V3-4, chattr/ACL option) | N-V3-5 |
| T-V3-5 | Mount/replace attack at an ancestor (tmpfs over install dir) | `st_dev` comparison per component; fd opened before verification cannot be re-pointed; exec of pinned inode persists | N-V3-4 |
| T-V3-6 | Runtime/validator/Doctor reach different helper verdicts | Single shared verdict function (D-V3-3); parity tests over one matrix; any divergence is a BLOCK | N-V3-8 |
| T-V3-7 | Trusted updater swaps generations during spawn | On fd-pinned platforms (Linux; macOS only post-spike): the pinned inode executes or exec fails; never an attacker generation. On Windows no fd-pinned exec exists — no determinism is claimed; the only real control is the trusted-install-root precondition (D-V3-4) plus the documented re-parse residual. Generation selection remains updater policy (Case D), out of scope for authenticity claims | N-V3-6, N-V3-9 |
| T-V3-8 | Doctor/validator fabricates PASS for a helper V3 would refuse | Doctor never enforces; verdict source is the shared function; parity + negative tests assert agreement; Doctor's role is reporting only | N-V3-6, N-V3-8 |
| T-V3-9 | Windows DACL/owner unparsable or ambiguous (silent allow) | Unparsable descriptor, denied retrieval, or owner SID outside policy → UNKNOWN → no exec (Windows strict); owner and DACL are independent gates (owner holds implicit `READ_CONTROL`/`WRITE_DAC`); no allow-by-default | N-V3-7, N-V3-13 |
| T-V3-10 | Race between DACL check and `CreateProcessW` re-parse (Windows) | Cannot be closed with public APIs; detected by immediate re-check but never bypassed; residual documented; Experimental classification retained | N-V3-9 |
| T-V3-11 | Descriptor leak: helper fd inherited by an unrelated child, pinning wrong object | fd opened `O_CLOEXEC`/`O_PATH`; strict launcher closes or explicitly passes only intended fds; test asserts no extra inherited fds | N-V3-10 |
| T-V3-12 | Fail-closed converted to fail-open by future edit | Every branch in the verdict table is exhaustive; compiler-enforced (no wildcard arm in security paths); regression tests per branch | N-V3-11 |
| T-V3-13 | Performance regression masks an availability DoS | Measured budget (Design §8); overhead reported with measurements before default-on; no silent latency creep in strict mode | N-V3-12 |
| T-V3-14 | Release archive substituted before install (V2 gap, adjacent) | Out of V3 scope; Setup Preview displays digest verification states only; archive authenticity requires release signing (separate track) — referenced, not solved here | n/a |
| T-V3-15 | Owner-approved "weakening for convenience" | No weakening path exists in V3; any such change is contract-major and requires Security review; explicitly forbidden here | review gate |
| T-V3-16 | macOS ACL `allow` entry grants write access invisible to POSIX mode checks (mode looks clean, ACL grants write) | macOS trusted-root policy evaluates `ACL_TYPE_EXTENDED` entries on every ancestor and the install dir (D-V3-4); allow-write to non-root/non-installer → BLOCK; retrieval failure → UNKNOWN fail-closed; native behavior **NOT VERIFIED** until the D-V3-7 spike covers ACLs; strict mode disabled on macOS until then | A-M5 |

## 4. Residual risks (accepted, must be documented, never silently)

- R-V3-1: **In-place inode modification** between open and exec: possible for
  a principal authorized to write the helper. Mitigation is trusted-root
  policy (root-owned, immutable option). Identity is bound; bytes are not
  guaranteed unchanged.
- R-V3-2: **Windows path re-parse window**: `CreateProcessW(path)` re-resolves
  the path; a handle-pinned verification cannot be enforced at exec with
  public APIs. Detection (pre-spawn recheck) + trusted root + Experimental
  classification only. No identity claim on Windows.
- R-V3-3: **macOS `/dev/fd` semantics and ACL evaluation**: pending the
  D-V3-7 spike; until native evidence exists, macOS carries the same class
  of residual as Windows, documented as such, and strict mode must not be
  enabled on macOS. All macOS-specific closure claims are **NOT VERIFIED**
  (not PASS) until the spike records native evidence on both runner types.
- R-V3-4: **Same-token malware** (unchanged R-4).
- R-V3-5: **Network filesystems / user-writable ancestors** are NO-GO
  absent explicit Owner attestation.
- R-V3-6: **Trusted updater availability races** remain (Case D); fd-pinning
  makes them deterministic, not eliminated as a policy matter.
- R-V3-7: **Kernel/LSM refusal of fd-exec** (SELinux policy, binfmt
  restrictions): strict mode must fail closed with a clear verdict; an
  Owner-accepted documented fallback may exist for such hosts, never silent.

## 5. Out of scope

- Release/archive signing and transparency (separate track).
- Sandbox parity claims for Windows (none).
- Same-token malware isolation (none possible in this architecture).
- Helper *content* authenticity (code signing verification) — adjacent
  control, tracked under supply-chain work; V3 binds identity to the object,
  not the provenance of its bytes beyond install-time sealing.
